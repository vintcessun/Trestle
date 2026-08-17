//! `gpu-cluster` connector：组里那几台 GPU 服务器，统一经 VPN 容器的 SOCKS5 出去。
//!
//! 这份代码是从 M1 那个临时 native 编排器**原样搬过来**的——逻辑一行没改，
//! 只是把 `trestle_transport::*` 的直接调用换成了 host 导入的传输工具箱。
//! 这正是 M2 要验证的事：connector 能不能只靠工具箱把活全干了。
//!
//! 它做的决定（host 一个都不替它做）：
//!   * 前置条件怎么算就绪：探 11080，不通就 `docker start` 再轮询等
//!   * 走哪条路：SOCKS5
//!   * 用什么认证：密码（引用配置里的一项，明文不进 wasm）
//!   * 连接什么时候算死、死了怎么办
//!   * 远端 agent 装在哪

#[allow(warnings)]
mod bindings {
    // 直接引用仓库根那一份 WIT，不在插件里复制——复制出来的接口迟早会走样。
    wit_bindgen::generate!({
        path: "../../../wit",
        world: "connector",
    });
}

use std::cell::RefCell;

use bindings::trestle::plugin::host_services as host;
use bindings::trestle::plugin::transport as tp;
use bindings::trestle::plugin::types::{Error, ErrorKind, Health, TargetInfo};
use bindings::Guest;

/// 一台机器的活连接。
struct Live {
    session: u64,
    agent: u64,
}

thread_local! {
    /// `ensure-ready` 的短缓存到期时刻（host 的单调毫秒）。
    static READY_UNTIL: RefCell<u64> = const { RefCell::new(0) };
}

/// 配置里属于我的那一节。
#[derive(serde::Deserialize)]
struct Config {
    #[serde(default = "default_socks")]
    socks: String,
    #[serde(default = "default_container")]
    container: String,
    #[serde(default = "default_ready_timeout")]
    ready_timeout_secs: u64,
    #[serde(default = "default_ready_cache")]
    ready_cache_secs: u64,
}

fn default_socks() -> String {
    // ⚠️ 11080 而不是 1080：本机 clash/mihomo 占着 1080，Docker Desktop(WSL2)
    // 发布到 127.0.0.1:1080 会「静默失败」——docker run 返回成功但端口实际不通。
    "127.0.0.1:11080".into()
}
fn default_container() -> String {
    "vpn-proxy".into()
}
fn default_ready_timeout() -> u64 {
    40
}
fn default_ready_cache() -> u64 {
    30
}

impl Config {
    fn load() -> Self {
        serde_json::from_str(&host::config_get()).unwrap_or(Config {
            socks: default_socks(),
            container: default_container(),
            ready_timeout_secs: default_ready_timeout(),
            ready_cache_secs: default_ready_cache(),
        })
    }
}

fn err(kind: ErrorKind, detail: impl Into<String>, remedy: impl Into<String>) -> Error {
    Error {
        kind,
        detail: detail.into(),
        remedy: remedy.into(),
    }
}

struct Component;

impl Guest for Component {
    fn targets() -> Vec<TargetInfo> {
        // 机器清单与称呼来自配置，不由我硬编码——host 只把归我管的那些交给我。
        host::targets()
    }

    /// 幂等地确保 SOCKS5 通道就绪。
    ///
    /// **只负责把容器叫醒，不负责创建它、不拉镜像**——容器由用户手工建一次。
    fn ensure_ready() -> Result<(), Error> {
        let cfg = Config::load();

        // 短缓存：确认过一次就在 N 秒内直接返回，否则每次 dial 都去查 docker 会明显拖慢。
        let now = host::now_ms();
        if READY_UNTIL.with(|c| *c.borrow()) > now {
            return Ok(());
        }

        // 先探端口。通了就什么都不用做——绝大多数调用走的是这条路。
        if tp::probe_tcp(&cfg.socks, 800) {
            READY_UNTIL.with(|c| *c.borrow_mut() = now + cfg.ready_cache_secs * 1000);
            return Ok(());
        }

        // 端口不通：看看容器在不在。
        let listed = tp::local_exec(&[
            "docker".into(),
            "ps".into(),
            "-a".into(),
            "--filter".into(),
            format!("name=^{}$", cfg.container),
            "--format".into(),
            "{{.Names}}".into(),
        ])?;

        if listed.stdout.trim().is_empty() {
            return Err(err(
                ErrorKind::NotReady,
                format!("container '{}' does not exist", cfg.container),
                format!(
                    "create it once by hand, then Trestle will only ever start it:\n  \
                     docker run -d --name {} --restart unless-stopped \\\n    \
                     -p 127.0.0.1:11080:1080 --cap-add NET_ADMIN --device /dev/net/tun \\\n    \
                     -v \"%LOCALAPPDATA%/VpnClient/data:/root/.local/share/vpnclient\" \\\n    \
                     example/vpn-proxy:latest",
                    cfg.container
                ),
            ));
        }

        let started = tp::local_exec(&["docker".into(), "start".into(), cfg.container.clone()])?;
        if started.exit_code != 0 {
            return Err(err(
                ErrorKind::NotReady,
                format!(
                    "`docker start {}` failed: {}",
                    cfg.container,
                    started.stderr.trim()
                ),
                "check that Docker Desktop is running",
            ));
        }

        // 轮询等端口就绪：容器起来到 SOCKS 真能连上之间有几秒的窗口。
        let deadline = host::now_ms() + cfg.ready_timeout_secs * 1000;
        while host::now_ms() < deadline {
            if tp::probe_tcp(&cfg.socks, 800) {
                READY_UNTIL
                    .with(|c| *c.borrow_mut() = host::now_ms() + cfg.ready_cache_secs * 1000);
                host::emit(
                    "info",
                    "connector_ensure_ready",
                    &format!(r#"{{"container":"{}","action":"started"}}"#, cfg.container),
                );
                return Ok(());
            }
            host::sleep_ms(400);
        }

        Err(err(
            ErrorKind::NotReady,
            format!(
                "container {} is running but {} never started accepting connections within {}s",
                cfg.container, cfg.socks, cfg.ready_timeout_secs
            ),
            format!("docker logs --tail 50 {}", cfg.container),
        ))
    }

    fn health(target: String) -> Health {
        match connect(&target) {
            Ok(live) => {
                let alive = tp::agent_alive(live.agent) && tp::ssh_alive(live.session);
                Health {
                    ok: alive,
                    detail: if alive {
                        format!("{target} is reachable through the lab SOCKS proxy")
                    } else {
                        format!("the connection to {target} went stale")
                    },
                    remedy: if alive {
                        String::new()
                    } else {
                        format!("trestle doctor {target}")
                    },
                    latency_ms: 0,
                }
            }
            Err(e) => Health {
                ok: false,
                detail: e.detail,
                remedy: e.remedy,
                latency_ms: 0,
            },
        }
    }

    /// 七个基本操作。
    fn op(target: String, op: String, payload: String) -> Result<String, Error> {
        let live = connect(&target)?;

        // forward 走 SSH 通道而不是 agent —— 它要的是一条流，不是一个 JSON 请求。
        if op == "forward" {
            let port = serde_json::from_str::<serde_json::Value>(&payload)
                .ok()
                .and_then(|v| v["remote_port"].as_u64())
                .ok_or_else(|| {
                    err(
                        ErrorKind::InvalidRequest,
                        "forward needs a remote_port",
                        "pass {\"remote_port\": 8080}",
                    )
                })?;
            return tp::forward_open(live.session, "127.0.0.1", port as u16);
        }

        // upload/download 的分块与校验在 host 做——我没有本地文件系统权限，也不该有。
        if op == "upload" || op == "download" {
            let v: serde_json::Value = serde_json::from_str(&payload).map_err(|e| {
                err(
                    ErrorKind::InvalidRequest,
                    format!("{op} payload is not valid JSON: {e}"),
                    "",
                )
            })?;
            let local = v["local_path"].as_str().unwrap_or_default().to_string();
            let remote = v["remote_path"].as_str().unwrap_or_default().to_string();
            let opts = v.get("options").map(|o| o.to_string()).unwrap_or_default();
            return if op == "upload" {
                tp::agent_upload(live.agent, &local, &remote, &opts)
            } else {
                tp::agent_download(live.agent, &remote, &local, &opts)
            };
        }

        tp::agent_call(live.agent, &op, &payload)
    }

    fn config_schema() -> String {
        // Web UI 据此渲染配置表单。
        serde_json::json!({
            "type": "object",
            "properties": {
                "socks": {
                    "type": "string", "default": default_socks(),
                    "description": "SOCKS5 代理地址。用 11080 而不是 1080——1080 常被本机 clash 占着。"
                },
                "container": {
                    "type": "string", "default": default_container(),
                    "description": "VPN 容器名。Trestle 只会 start 它，不会创建它。"
                },
                "ready_timeout_secs": { "type": "integer", "default": default_ready_timeout() },
                "ready_cache_secs": {
                    "type": "integer", "default": default_ready_cache(),
                    "description": "ensure-ready 成功后缓存多久，避免每次 dial 都查 docker。"
                }
            }
        })
        .to_string()
    }
}

/// 拿一条到目标的连接，没有或已经死了就重建。
///
/// 连接记在 **host 那边**（`session-lookup` / `session-remember`）而不是我自己的内存里：
/// host 会给这个 connector 起好几个实例，好让「同时打整支机队」真并发；
/// 如果每个实例各存各的，六个实例就会对同一台机器建六条连接。
fn connect(target: &str) -> Result<Live, Error> {
    if let Some((session, agent)) = tp::session_lookup(target) {
        if tp::agent_alive(agent) && tp::ssh_alive(session) {
            return Ok(Live { session, agent });
        }
        // 死了就把旧句柄清掉，别让它们泄漏在 host 的表里。
        tp::session_forget(target);
        tp::ssh_close(session);
    }

    let info = host::targets()
        .into_iter()
        .find(|t| t.name == target)
        .ok_or_else(|| {
            err(
                ErrorKind::NotFound,
                format!("'{target}' is not one of the machines this connector manages"),
                "trestle targets",
            )
        })?;

    <Component as Guest>::ensure_ready()?;

    let cfg = Config::load();
    let stream = tp::dial_socks5(&cfg.socks, &format!("{}:{}", info.host, info.port), 15_000)?;
    // 凭据用引用而不是明文：密码从不进入 wasm。
    let session = tp::ssh_connect(
        stream,
        target,
        &info.host,
        info.port,
        &info.user,
        &format!("target:{target}"),
    )?;
    let agent = tp::agent_ensure(session, target, &info.agent_dir)?;

    tp::session_remember(target, session, agent);
    Ok(Live { session, agent })
}

bindings::export!(Component with_types_in bindings);
