//! `ssh-socks5`：**经一个 SOCKS5 代理连过去的 SSH**。
//!
//! 它是一个通用驱动，不是某一组机器。`[connectors.gpu-cluster]` 说
//! `plugin = "ssh-socks5"`，那一组机器就归它管；再配一组走别的代理的，
//! 同一个 `.wasm` 会被起成第二个 connector，两边各有各的配置和 KV。
//!
//! 它做的决定（host 一个都不替它做）：
//!   * 前置条件怎么算就绪：探代理端口，不通就跑**配置里那条命令**把它拉起来
//!   * 走哪条路：SOCKS5 CONNECT
//!   * 用什么认证：凭据引用（明文从不进 wasm）
//!   * 连接什么时候算死、死了怎么办
//!   * 远端 agent 装在哪
//!
//! 「拉起代理的那条命令」以前是写死的 `docker start vpn-proxy`。
//! 现在它在配置里——驱动不该知道你的代理是 docker 起的还是 systemd 起的。

#[allow(warnings)]
mod bindings {
    // 直接引用仓库根那一份 WIT，不在插件里复制——复制出来的接口迟早会走样。
    wit_bindgen::generate!({
        path: "../../../wit",
        world: "connector",
    });
}

use std::cell::RefCell;

use bindings::Guest;
use bindings::trestle::plugin::host_services as host;
use bindings::trestle::plugin::transport as tp;
use bindings::trestle::plugin::types::{Error, ErrorKind, Health, TargetInfo};
use connector_ready as ready;

/// 一台机器的活连接。
struct Live {
    session: u64,
    agent: u64,
}

thread_local! {
    /// 「刚确认过前置条件」的短缓存。
    ///
    /// 它是**每个实例一份**的：host 会给这个 connector 起一个实例池，
    /// 池里的实例各有各的 wasm 内存。这份分裂是可以接受的，因为它只是个
    /// 缓存——最坏的结果是多探一次端口。真正的跨调用状态（连接句柄）
    /// 放在 host 那边的 session 表里，见 `connect`。
    static READY: RefCell<ready::Cache> = RefCell::new(ready::Cache::default());
}

/// 配置里属于我的那一节。
#[derive(serde::Deserialize)]
struct Config {
    /// SOCKS5 代理地址。
    #[serde(default = "default_socks")]
    socks: String,
    /// 建 TCP 的超时。
    #[serde(default = "default_dial_timeout")]
    dial_timeout_ms: u32,
    /// 前置条件。不写 = 代理已经在跑，我只管连。
    #[serde(default)]
    ready: ready::ReadyConfig,
}

fn default_socks() -> String {
    // ⚠️ 11080 而不是 1080：本机 clash/mihomo 占着 1080，Docker Desktop(WSL2)
    // 发布到 127.0.0.1:1080 会「静默失败」——docker run 返回成功但端口实际不通。
    "127.0.0.1:11080".into()
}
fn default_dial_timeout() -> u32 {
    15_000
}

impl Config {
    fn load() -> Self {
        let raw = host::config_get();
        match serde_json::from_str(&raw) {
            Ok(cfg) => cfg,
            Err(e) => {
                // 读不进来却**静默**退回默认值，会让「我明明配了 start」变成一个
                // 查不出源头的问题：探不通时报的错会让你去加一条你已经写了的命令。
                // 所以这里必须出声。
                host::emit(
                    "warn",
                    "config_parse_failed",
                    &serde_json::json!({
                        "detail": e.to_string(),
                        "remedy": "检查这个 connector 的配置节；正在用默认值继续",
                    })
                    .to_string(),
                );
                Config {
                    socks: default_socks(),
                    dial_timeout_ms: default_dial_timeout(),
                    ready: Default::default(),
                }
            }
        }
    }
}

/// 把 host 导入接到 `connector-ready` 的 `Sys` 上。
///
/// 这一层只是转接，没有任何判断——判断全在 `connector-ready` 里，
/// 那样它才能在 host 上用普通 `cargo test` 测。
struct Host;

impl ready::Sys for Host {
    fn probe_tcp(&self, addr: &str, timeout_ms: u32) -> bool {
        tp::probe_tcp(addr, timeout_ms)
    }

    fn local_exec(&self, argv: &[String]) -> Result<ready::Exec, ready::ExecError> {
        match tp::local_exec(argv) {
            Ok(out) => Ok(ready::Exec {
                exit_code: out.exit_code,
                stdout: out.stdout,
                stderr: out.stderr,
            }),
            Err(e) => Err(ready::ExecError {
                // 「host 不准我跑」和「跑了但失败了」的下一步完全不同，
                // 这个 bit 必须传下去。
                denied: matches!(e.kind, ErrorKind::Denied),
                detail: e.detail,
            }),
        }
    }

    fn now_ms(&self) -> u64 {
        host::now_ms()
    }

    fn sleep_ms(&self, ms: u32) {
        host::sleep_ms(ms);
    }

    fn emit(&self, level: &str, kind: &str, fields: &str) {
        host::emit(level, kind, fields);
    }
}

fn err(kind: ErrorKind, detail: impl Into<String>, remedy: impl Into<String>) -> Error {
    Error {
        kind,
        detail: detail.into(),
        remedy: remedy.into(),
    }
}

fn not_ready(e: ready::NotReady) -> Error {
    err(ErrorKind::NotReady, e.detail, e.remedy)
}

struct Component;

impl Guest for Component {
    fn targets() -> Vec<TargetInfo> {
        // 机器清单与称呼来自配置，不由我硬编码——host 只把归我管的那些交给我。
        host::targets()
    }

    /// 幂等地确保代理通道就绪。
    ///
    /// **只负责把配置里说的那个东西叫醒，不负责创建它**——见 `connector-ready`。
    fn ensure_ready() -> Result<(), Error> {
        let cfg = Config::load();
        READY.with(|cache| {
            ready::ensure(&Host, &cfg.ready, &mut cache.borrow_mut(), &cfg.socks)
                .map_err(not_ready)
        })
    }

    fn health(target: String) -> Health {
        match connect(&target) {
            Ok(live) => {
                let alive = tp::agent_alive(live.agent) && tp::ssh_alive(live.session);
                Health {
                    ok: alive,
                    detail: if alive {
                        format!("{target} is reachable through the configured SOCKS5 proxy")
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
                    "description": "SOCKS5 代理地址。也是 ready 的默认探测目标。"
                },
                "dial_timeout_ms": { "type": "integer", "default": default_dial_timeout() },
                "allow_exec": {
                    "type": "array", "items": {"type": "string"},
                    "description": "准这个 connector 在本机跑哪些命令。ready.start 用到什么就写什么。"
                },
                "ready": {
                    "type": "object",
                    "description": "前置条件：探不通就按这里说的把代理拉起来。不写 = 代理已经在跑。",
                    "properties": {
                        "probe": {"type": "string", "description": "探哪个地址。不写 = socks。"},
                        "probe_timeout_ms": {"type": "integer", "default": 800},
                        "check": {
                            "type": "array", "items": {"type": "string"},
                            "description": "确认它存在的命令（可选）。"
                        },
                        "check_expect": {
                            "type": "string",
                            "description": "check 的输出里必须出现这段文字，否则算不存在。"
                        },
                        "missing": {"type": "string", "description": "不存在时报什么。"},
                        "missing_remedy": {
                            "type": "string",
                            "description": "不存在时怎么办——通常是创建命令。Trestle 永远不会替你创建。"
                        },
                        "start": {
                            "type": "array", "items": {"type": "string"},
                            "description": "把它拉起来的命令。不写 = 探不通就直接报错。"
                        },
                        "timeout_secs": {"type": "integer", "default": 40},
                        "cache_secs": {"type": "integer", "default": 30}
                    }
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
    let stream = tp::dial_socks5(
        &cfg.socks,
        &format!("{}:{}", info.host, info.port),
        cfg.dial_timeout_ms,
    )?;
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
