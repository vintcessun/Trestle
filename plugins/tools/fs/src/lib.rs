//! `fs`：远端文件系统的查看。
//!
//! 全部建在 `base.shell` 上。它是最小的一个插件，正好说明「加一个能力有多便宜」——
//! 没有新概念、没有新权限，就是拼命令加解析输出。

#[allow(warnings)]
mod bindings {
    wit_bindgen::generate!({
        path: "../../../wit",
        world: "tool-plugin",
    });
}

use bindings::trestle::plugin::base;
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

fn shq(s: &str) -> String {
    format!("'{}'", s.replace('\'', r"'\''"))
}

fn need(v: &serde_json::Value, key: &str) -> Result<String, Error> {
    v.get(key)
        .and_then(|x| x.as_str())
        .map(str::to_string)
        .ok_or_else(|| bad(format!("this tool needs a `{key}`")))
}

fn need_target(v: &serde_json::Value) -> Result<String, Error> {
    v.get("target")
        .and_then(|x| x.as_str())
        .map(str::to_string)
        .ok_or_else(|| {
            bad("this tool needs a `target`; there is no default machine (that is deliberate)")
        })
}

/// 跑一条命令并把 stdout 拿出来。
fn run(target: &str, command: &str, timeout: u64) -> Result<String, Error> {
    let payload = serde_json::json!({"command": command, "timeout_secs": timeout}).to_string();
    let out = base::call(target, "shell", &payload)?;
    let v: serde_json::Value = serde_json::from_str(&out).map_err(|e| {
        err(
            ErrorKind::Protocol,
            format!("malformed shell response: {e}"),
            "",
        )
    })?;
    if v["exit_code"].as_i64().unwrap_or(-1) != 0 {
        let stderr = v["stderr"].as_str().unwrap_or("").trim().to_string();
        return Err(err(
            if stderr.contains("No such file") {
                ErrorKind::NotFound
            } else if stderr.contains("Permission denied") {
                ErrorKind::PermissionDenied
            } else {
                ErrorKind::Internal
            },
            stderr,
            "",
        ));
    }
    Ok(v["stdout"].as_str().unwrap_or("").to_string())
}

struct Component;

impl Guest for Component {
    fn list_tools() -> String {
        let target = serde_json::json!({"type": "string", "description": "机器名，必填"});
        serde_json::json!([
            {
                "name": "fs_list",
                "description": "列目录。带大小、修改时间、类型。",
                "input_schema": {
                    "type": "object", "required": ["target", "path"],
                    "properties": {
                        "target": target, "path": {"type": "string"},
                        "all": {"type": "boolean", "description": "包含以点开头的项"}
                    }
                }
            },
            {
                "name": "fs_find",
                "description": "按 glob 查找文件。",
                "input_schema": {
                    "type": "object", "required": ["target", "path", "glob"],
                    "properties": {
                        "target": target, "path": {"type": "string"}, "glob": {"type": "string"},
                        "max_results": {"type": "integer"}
                    }
                }
            },
            {
                "name": "fs_stat",
                "description": "看一个路径：大小、修改时间、权限、是不是目录。",
                "input_schema": {
                    "type": "object", "required": ["target", "path"],
                    "properties": {"target": target, "path": {"type": "string"}}
                }
            },
            {
                "name": "fs_tree",
                "description": "目录树概览，带每层的大小合计。",
                "input_schema": {
                    "type": "object", "required": ["target", "path"],
                    "properties": {
                        "target": target, "path": {"type": "string"},
                        "depth": {"type": "integer", "description": "默认 2"}
                    }
                }
            },
            {
                "name": "fs_disk",
                "description": "磁盘占用。默认盘普遍吃紧，写东西前值得先看一眼。",
                "input_schema": {
                    "type": "object", "required": ["target"],
                    "properties": {"target": target, "path": {"type": "string"}}
                }
            }
        ])
        .to_string()
    }

    fn call(tool: String, args: String) -> Result<String, Error> {
        let v: serde_json::Value = serde_json::from_str(&args)
            .map_err(|e| bad(format!("arguments are not valid JSON: {e}")))?;
        let target = need_target(&v)?;

        match tool.as_str() {
            "fs_list" => {
                let path = need(&v, "path")?;
                let all = v.get("all").and_then(|a| a.as_bool()).unwrap_or(false);
                let flag = if all { "-A" } else { "" };
                let out = run(
                    &target,
                    &format!("ls -l --time-style=+%s {flag} {}", shq(&path)),
                    30,
                )?;
                let entries: Vec<serde_json::Value> = out
                    .lines()
                    .filter(|l| !l.starts_with("total "))
                    .filter_map(|l| {
                        let cols: Vec<&str> = l.split_whitespace().collect();
                        if cols.len() < 7 {
                            return None;
                        }
                        Some(serde_json::json!({
                            "mode": cols[0],
                            "size": cols[4].parse::<u64>().unwrap_or(0),
                            "mtime": cols[5].parse::<i64>().unwrap_or(0),
                            "name": cols[6..].join(" "),
                            "is_dir": cols[0].starts_with('d'),
                        }))
                    })
                    .collect();
                Ok(
                    serde_json::to_string(&serde_json::json!({"path": path, "entries": entries}))
                        .unwrap_or_default(),
                )
            }

            "fs_find" => {
                let path = need(&v, "path")?;
                let glob = need(&v, "glob")?;
                let max = v.get("max_results").and_then(|m| m.as_u64()).unwrap_or(200);
                let out = run(
                    &target,
                    &format!(
                        "find {} -name {} -printf '%p\\t%s\\t%T@\\n' 2>/dev/null | head -n {max}",
                        shq(&path),
                        shq(&glob)
                    ),
                    60,
                )?;
                let hits: Vec<serde_json::Value> = out
                    .lines()
                    .filter_map(|l| {
                        let mut it = l.split('\t');
                        Some(serde_json::json!({
                            "path": it.next()?,
                            "size": it.next()?.parse::<u64>().unwrap_or(0),
                        }))
                    })
                    .collect();
                let truncated = hits.len() as u64 >= max;
                Ok(serde_json::to_string(&serde_json::json!({
                    "hits": hits,
                    // 截断了必须说 —— 悄悄少给几行会让 agent 得出错误结论。
                    "truncated": truncated,
                }))
                .unwrap_or_default())
            }

            "fs_stat" => {
                let path = need(&v, "path")?;
                // `--printf` 而不是 `-c`：只有前者处理 `\t` 这类转义，
                // `-c` 会把反斜杠 t 原样打出来，于是下面的 split 什么都分不出来。
                let out = run(
                    &target,
                    &format!("stat --printf='%s\\t%Y\\t%a\\t%F' {}", shq(&path)),
                    20,
                )?;
                let cols: Vec<&str> = out.trim().split('\t').collect();
                if cols.len() < 4 {
                    return Err(err(ErrorKind::NotFound, format!("cannot stat {path}"), ""));
                }
                Ok(serde_json::to_string(&serde_json::json!({
                    "path": path,
                    "size": cols[0].parse::<u64>().unwrap_or(0),
                    "mtime": cols[1].parse::<i64>().unwrap_or(0),
                    "mode": cols[2],
                    "kind": cols[3],
                    "is_dir": cols[3].contains("directory"),
                }))
                .unwrap_or_default())
            }

            "fs_tree" => {
                let path = need(&v, "path")?;
                let depth = v.get("depth").and_then(|d| d.as_u64()).unwrap_or(2);
                let out = run(
                    &target,
                    &format!(
                        "du -h --max-depth={depth} {} 2>/dev/null | sort -k2",
                        shq(&path)
                    ),
                    120,
                )?;
                let rows: Vec<serde_json::Value> = out
                    .lines()
                    .filter_map(|l| {
                        let mut it = l.split('\t');
                        Some(serde_json::json!({"size": it.next()?, "path": it.next()?}))
                    })
                    .collect();
                Ok(
                    serde_json::to_string(&serde_json::json!({"root": path, "entries": rows}))
                        .unwrap_or_default(),
                )
            }

            "fs_disk" => {
                let path = v
                    .get("path")
                    .and_then(|p| p.as_str())
                    .unwrap_or(".")
                    .to_string();
                let out = run(&target, &format!("df -h {} | tail -n +2", shq(&path)), 20)?;
                let rows: Vec<serde_json::Value> = out
                    .lines()
                    .filter_map(|l| {
                        let c: Vec<&str> = l.split_whitespace().collect();
                        if c.len() < 6 {
                            return None;
                        }
                        Some(serde_json::json!({
                            "filesystem": c[0], "size": c[1], "used": c[2],
                            "available": c[3], "use_percent": c[4], "mounted_on": c[5],
                        }))
                    })
                    .collect();
                Ok(
                    serde_json::to_string(&serde_json::json!({"path": path, "filesystems": rows}))
                        .unwrap_or_default(),
                )
            }

            other => Err(err(
                ErrorKind::NotFound,
                format!("unknown tool '{other}'"),
                "fs_list, fs_find, fs_stat, fs_tree, fs_disk",
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
