//! `cloud` connector：自己的服务器，标准 SSH 直连 + 公钥登录。
//!
//! 它和 `gpu-cluster` 的接入方式**完全不同**——没有代理、没有容器、
//! 认证方式也不一样——但对上层暴露的是同一个 name + 七个操作的接口。
//! 上层调用这两组机器的代码一个字都不用改，这就是 connector 抽象要证明的事。
//!
//! 它也顺带说明了「写一个 connector 有多便宜」：没有前置条件要准备，
//! `ensure-ready` 就是一个 `Ok(())`。

#[allow(warnings)]
mod bindings {
    wit_bindgen::generate!({
        path: "../../../wit",
        world: "connector",
    });
}

use bindings::trestle::plugin::host_services as host;
use bindings::trestle::plugin::transport as tp;
use bindings::trestle::plugin::types::{Error, ErrorKind, Health, TargetInfo};
use bindings::Guest;

struct Live {
    session: u64,
    agent: u64,
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

    /// 直连，没有任何前置条件要准备。
    fn ensure_ready() -> Result<(), Error> {
        Ok(())
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
            "properties": {},
            "description": "直连 SSH，没有需要配置的前置条件。凭据在 secrets.toml 里按机器名给。"
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

    let stream = tp::dial(&format!("{}:{}", info.host, info.port), 15_000)?;
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
