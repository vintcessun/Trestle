//! `monitor`：把一个 WebSocket URL 交给 Claude Code 的 Monitor。
//!
//! 为什么这值得一个插件：Claude Code 的 Monitor 只认两种事件源——本地 shell 命令，
//! 或者一个 ws URL。给它一个 URL 是摩擦最低的那条路，agent 不用再拼一条带
//! Windows 路径和正则转义的命令行。
//!
//! `timeout_secs` **必填**是刻意的：一个没有过期时间的监视端点会悄悄泄漏——
//! 任务早就结束了，ws 还挂在那里占着轮询。强制传值让调用方每次都想一下
//! 「这个任务大概跑多久」。

#[allow(warnings)]
mod bindings {
    wit_bindgen::generate!({
        path: "../../../wit",
        world: "tool-plugin",
    });
}

use bindings::trestle::plugin::types::{Error, ErrorKind};
use bindings::trestle::plugin::ws;
use bindings::Guest;

fn err(kind: ErrorKind, detail: impl Into<String>, remedy: impl Into<String>) -> Error {
    Error {
        kind,
        detail: detail.into(),
        remedy: remedy.into(),
    }
}

struct Component;

impl Guest for Component {
    fn list_tools() -> String {
        serde_json::json!([
            {
                "name": "monitor_open",
                "description":
                    "开一个 WebSocket 端点并返回 URL，直接交给 Monitor 工具即可。\
                     timeout_secs 必填——到期 host 会主动关掉并推一帧说明原因，\
                     所以你能分清「任务结束了」和「监视超时了但任务还在跑」。",
                "input_schema": {
                    "type": "object",
                    "required": ["timeout_secs"],
                    "properties": {
                        "timeout_secs": {
                            "type": "integer",
                            "description": "监视多久。想一下这个任务大概跑多久再填。"
                        },
                        "only_target": {"type": "string", "description": "只看这台机器。这是过滤条件，不是操作对象——所以它不叫 target，也不是必填。"},
                        "only_job": {"type": "string", "description": "只看这个任务"},
                        "quiet": {
                            "type": "array", "items": {"type": "string"},
                            "description": "命中就压掉的模式（比如每一步的 loss）"
                        },
                        "alert": {
                            "type": "array", "items": {"type": "string"},
                            "description": "命中就一定推出去。留空则用默认规则，\
                                            它覆盖 Traceback / OOM / CUDA out of memory 这些终态。"
                        }
                    }
                }
            }
        ])
        .to_string()
    }

    fn call(tool: String, args: String) -> Result<String, Error> {
        if tool != "monitor_open" {
            return Err(err(
                ErrorKind::NotFound,
                format!("unknown tool '{tool}'"),
                "monitor_open",
            ));
        }

        let v: serde_json::Value = serde_json::from_str(&args).map_err(|e| {
            err(
                ErrorKind::InvalidRequest,
                format!("arguments are not valid JSON: {e}"),
                "",
            )
        })?;

        let timeout = v
            .get("timeout_secs")
            .and_then(|t| t.as_u64())
            .ok_or_else(|| {
                err(
                    ErrorKind::InvalidRequest,
                    "monitor_open needs a `timeout_secs`",
                    "roughly how long will the thing you are watching run? \
                     an endpoint without an expiry just leaks",
                )
            })? as u32;

        // 过滤条件原样交给 host —— 默认 alert 规则在那边补齐。
        let filter = serde_json::json!({
            "target": v.get("only_target"),
            "job_id": v.get("only_job"),
            "quiet": v.get("quiet").cloned().unwrap_or(serde_json::json!([])),
            "alert": v.get("alert").cloned().unwrap_or(serde_json::json!([])),
        });

        let url = ws::publish(&filter.to_string(), timeout)?;

        Ok(serde_json::to_string(&serde_json::json!({
            "ws_url": url,
            "expires_in_secs": timeout,
            // 兜底路径：daemon 重启的话 ws 会断，但 CLI 子进程是独立的、照跑不误。
            // 所以这不是冗余，是兜底。
            "cli_command": match v.get("only_job").and_then(|j| j.as_str()) {
                Some(job) => format!(
                    "trestle call job_logs '{{\"target\":\"{}\",\"job_id\":\"{}\"}}'",
                    v.get("only_target").and_then(|t| t.as_str()).unwrap_or(""),
                    job
                ),
                None => "trestle agents".to_string(),
            },
            "note": "到期会收到一帧 closing（reason=timeout），那表示没人盯了、\
                     任务本身还在跑；reason=job_finished 才是真的结束。",
        }))
        .unwrap_or_default())
    }

    fn on_tick(_name: String, _payload: String) {}

    /// 不需要 Web UI 面板。
    fn ui_panel() -> String {
        String::new()
    }

    fn config_schema() -> String {
        serde_json::json!({"type": "object", "properties": {}}).to_string()
    }
}

bindings::export!(Component with_types_in bindings);
