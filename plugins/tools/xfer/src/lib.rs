//! `xfer`：跨机搬运。
//!
//! `upload`/`download` 已经是基本操作，所以这个插件**不重造分块协议**——
//! 它只做编排：服务器 A → 本地 → 服务器 B、一份文件分发到多台、批量同步。
//!
//! 两台服务器互不相通也没关系，本地就是中转站。

#[allow(warnings)]
mod bindings {
    wit_bindgen::generate!({
        path: "../../../wit",
        world: "tool-plugin",
    });
}

use bindings::trestle::plugin::base;
use bindings::trestle::plugin::host_services as host;
use bindings::trestle::plugin::types::{Error, ErrorKind};
use bindings::Guest;

fn err(kind: ErrorKind, detail: impl Into<String>, remedy: impl Into<String>) -> Error {
    Error {
        kind,
        detail: detail.into(),
        remedy: remedy.into(),
    }
}

fn bad(detail: impl Into<String>) -> Error {
    err(
        ErrorKind::InvalidRequest,
        detail,
        "check the tool's input schema",
    )
}

fn need(v: &serde_json::Value, key: &str) -> Result<String, Error> {
    v.get(key)
        .and_then(|x| x.as_str())
        .map(str::to_string)
        .ok_or_else(|| bad(format!("this tool needs a `{key}`")))
}

/// 本地中转文件的落点。
///
/// 路径由 host 给：插件在 wasm 里看到的文件系统跟 host 的不是一回事，
/// 自己拼出来的 `/tmp/x` 在 Windows 主机上根本不存在。
fn staging(name: &str) -> String {
    host::staging_path(name)
}

struct Component;

impl Guest for Component {
    fn list_tools() -> String {
        serde_json::json!([
            {
                "name": "xfer_between",
                "description": "把文件或目录从一台机器搬到另一台。两台互不相通也没关系——本地中转。",
                "input_schema": {
                    "type": "object",
                    "required": ["from", "from_path", "to", "to_path"],
                    "properties": {
                        "from": {"type": "string"}, "from_path": {"type": "string"},
                        "to": {"type": "string"}, "to_path": {"type": "string"},
                        "options": {"type": "object", "description": "exclude / sync / dry_run"},
                        "keep_local": {"type": "boolean", "description": "保留本地那份中转文件"}
                    }
                }
            },
            {
                "name": "xfer_distribute",
                "description": "把本地一份文件/目录同时发到多台机器。",
                "input_schema": {
                    "type": "object",
                    "required": ["local_path", "remote_path"],
                    "properties": {
                        "local_path": {"type": "string"}, "remote_path": {"type": "string"},
                        "targets": {"type": "array", "items": {"type": "string"},
                                    "description": "留空表示全部机器"},
                        "options": {"type": "object"}
                    }
                }
            }
        ])
        .to_string()
    }

    fn call(tool: String, args: String) -> Result<String, Error> {
        let v: serde_json::Value = serde_json::from_str(&args)
            .map_err(|e| bad(format!("arguments are not valid JSON: {e}")))?;

        match tool.as_str() {
            "xfer_between" => {
                let from = need(&v, "from")?;
                let from_path = need(&v, "from_path")?;
                let to = need(&v, "to")?;
                let to_path = need(&v, "to_path")?;
                if from == to {
                    return Err(bad(
                        "source and destination are the same machine; use base_shell with cp instead",
                    ));
                }
                let options = v.get("options").cloned().unwrap_or(serde_json::json!({}));
                let keep = v
                    .get("keep_local")
                    .and_then(|k| k.as_bool())
                    .unwrap_or(false);

                let leaf = from_path.rsplit('/').next().unwrap_or("payload");
                let local = staging(leaf);

                // 拉到本地。
                let down = base::call(
                    &from,
                    "download",
                    &serde_json::json!({
                        "remote_path": from_path, "local_path": local, "options": options
                    })
                    .to_string(),
                )?;

                // 再推上去。
                let up = base::call(
                    &to,
                    "upload",
                    &serde_json::json!({
                        "local_path": local, "remote_path": to_path, "options": options
                    })
                    .to_string(),
                )?;

                host::emit(
                    "info",
                    "xfer_between",
                    &serde_json::json!({"from": from, "to": to, "path": to_path}).to_string(),
                );

                let down: serde_json::Value = serde_json::from_str(&down).unwrap_or_default();
                let up: serde_json::Value = serde_json::from_str(&up).unwrap_or_default();
                Ok(serde_json::to_string(&serde_json::json!({
                    "from": from, "to": to,
                    "files": up["files"], "bytes": up["bytes"],
                    // 产出路径就是入参路径 —— 这条在两端都成立。
                    "path": to_path,
                    "downloaded": down["files"],
                    "staged_at": if keep { serde_json::Value::String(local) } else { serde_json::Value::Null },
                }))
                .unwrap_or_default())
            }

            "xfer_distribute" => {
                let local_path = need(&v, "local_path")?;
                let remote_path = need(&v, "remote_path")?;
                let options = v.get("options").cloned().unwrap_or(serde_json::json!({}));
                let targets: Vec<String> = v
                    .get("targets")
                    .and_then(|t| t.as_array())
                    .map(|a| {
                        a.iter()
                            .filter_map(|x| x.as_str().map(str::to_string))
                            .collect()
                    })
                    .unwrap_or_default();

                // 并发发出去。顺序发五台就是五倍时间，而这件事天然可以并行。
                let payload = serde_json::json!({
                    "local_path": local_path, "remote_path": remote_path, "options": options
                })
                .to_string();
                let results = base::call_many(&targets, "upload", &payload);
                let names: Vec<String> = if targets.is_empty() {
                    host::targets().into_iter().map(|t| t.name).collect()
                } else {
                    targets
                };

                let rows: Vec<serde_json::Value> = names
                    .iter()
                    .zip(results.iter())
                    .map(|(name, r)| match r {
                        Ok(raw) => {
                            let v: serde_json::Value =
                                serde_json::from_str(raw).unwrap_or_default();
                            serde_json::json!({
                                "target": name, "ok": true,
                                "files": v["files"], "bytes": v["bytes"], "path": v["path"]
                            })
                        }
                        Err(e) => serde_json::json!({
                            "target": name, "ok": false, "error": e.detail, "remedy": e.remedy
                        }),
                    })
                    .collect();
                Ok(serde_json::to_string(&rows).unwrap_or_default())
            }

            other => Err(err(
                ErrorKind::NotFound,
                format!("unknown tool '{other}'"),
                "xfer_between, xfer_distribute",
            )),
        }
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
