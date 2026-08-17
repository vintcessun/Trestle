//! `ssh-direct`：**直连的 SSH**。没有代理，没有中间人。
//!
//! 它和 `ssh-socks5` 的接入方式完全不同——不拨代理，认证也常常是公钥——
//! 但对上层暴露的是同一个 name + 七个操作的接口。上层调这两组机器的代码
//! 一个字都不用改，这就是 connector 抽象要证明的事。
//!
//! 它也顺带说明了「写一个 connector 有多便宜」：把 `dial-socks5` 换成 `dial`，
//! 别的一行都不用动。
//!
//! 直连**通常**没有前置条件，但不是一定没有——「先把 VPN 拨上再连」是存在的，
//! 所以它和 `ssh-socks5` 共用同一份 `[.ready]` 配置形状。不写就什么都不做。

#[allow(warnings)]
mod bindings {
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

struct Live {
    session: u64,
    agent: u64,
}

thread_local! {
    /// 见 `ssh-socks5` 里同名的那份注释：每个实例一份，但它只是个缓存。
    static READY: RefCell<ready::Cache> = RefCell::new(ready::Cache::default());
}

#[derive(serde::Deserialize)]
struct Config {
    #[serde(default = "default_dial_timeout")]
    dial_timeout_ms: u32,
    /// 前置条件。直连一般不写；要先拨 VPN 之类的就写在这。
    #[serde(default)]
    ready: ready::ReadyConfig,
}

fn default_dial_timeout() -> u32 {
    15_000
}

impl Config {
    fn load() -> Self {
        serde_json::from_str(&host::config_get()).unwrap_or(Config {
            dial_timeout_ms: default_dial_timeout(),
            ready: Default::default(),
        })
    }
}

/// 把 host 导入接到 `connector-ready` 的 `Sys` 上。判断全在那边，这里只是转接。
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

struct Component;

impl Guest for Component {
    fn targets() -> Vec<TargetInfo> {
        host::targets()
    }

    /// 没配 `[.ready]` 就什么都不做——直连的默认形态。
    fn ensure_ready() -> Result<(), Error> {
        let cfg = Config::load();
        READY.with(|cache| {
            // 直连没有「代理地址」可以当默认探测目标，所以 fallback 是空的：
            // 要探什么得在 ready.probe 里明说。
            ready::ensure(&Host, &cfg.ready, &mut cache.borrow_mut(), "")
                .map_err(|e| err(ErrorKind::NotReady, e.detail, e.remedy))
        })
    }

    fn health(target: String) -> Health {
        match connect(&target) {
            Ok(live) => {
                let alive = tp::agent_alive(live.agent) && tp::ssh_alive(live.session);
                Health {
                    ok: alive,
                    detail: if alive {
                        format!("{target} is reachable directly over SSH")
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

    fn op(target: String, op: String, payload: String) -> Result<String, Error> {
        let live = connect(&target)?;

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
        serde_json::json!({
            "type": "object",
            "properties": {
                "dial_timeout_ms": { "type": "integer", "default": default_dial_timeout() },
                "allow_exec": {
                    "type": "array", "items": {"type": "string"},
                    "description": "准这个 connector 在本机跑哪些命令。只有配了 ready.start 才需要。"
                },
                "ready": {
                    "type": "object",
                    "description": "前置条件。直连一般不需要；要先拨 VPN 之类的写在这，形状和 ssh-socks5 一样。",
                    "properties": {
                        "probe": {"type": "string", "description": "探哪个地址。直连没有默认值，要探就得写。"},
                        "start": {"type": "array", "items": {"type": "string"}},
                        "timeout_secs": {"type": "integer", "default": 40},
                        "cache_secs": {"type": "integer", "default": 30}
                    }
                }
            },
            "description": "凭据在 secrets.toml 里按机器名给（公钥或密码都行）。"
        })
        .to_string()
    }
}

fn connect(target: &str) -> Result<Live, Error> {
    // 连接记在 host 那边而不是我自己的内存里 —— host 会起多个实例来做并发，
    // 各存各的就会对同一台机器建多条连接。
    if let Some((session, agent)) = tp::session_lookup(target) {
        if tp::agent_alive(agent) && tp::ssh_alive(session) {
            return Ok(Live { session, agent });
        }
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
    let stream = tp::dial(
        &format!("{}:{}", info.host, info.port),
        cfg.dial_timeout_ms,
    )?;
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
