//! `fleet`：全队视角——状态概览、广播执行、跨机挑卡。
//!
//! 这个插件是 `base.call-many` 存在的理由：顺序问整支机队，冷启动时就是六倍延迟。
//! 并发编排由 host 做，插件只说要打哪几台。
//!
//! GPU 相关的部分建在 host 的**单点分配器**上，而不是自己维护一张占用表——
//! 分配器看到的是 `nvidia-smi` 的真实占用，所以别人绕过 Trestle 直接 ssh 上去
//! 占的卡也照样可见。

#[allow(warnings)]
mod bindings {
    wit_bindgen::generate!({
        path: "../../../wit",
        world: "tool-plugin",
    });
}

use bindings::trestle::plugin::base;
use bindings::trestle::plugin::gpu;
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

/// 留空 = 全部机器。这是「全队」语义，不是「默认机」。
fn selected(v: &serde_json::Value) -> Vec<String> {
    v.get("targets")
        .and_then(|t| t.as_array())
        .map(|a| {
            a.iter()
                .filter_map(|x| x.as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default()
}

fn shell_payload(command: &str, timeout: u64) -> String {
    serde_json::json!({"command": command, "timeout_secs": timeout}).to_string()
}

fn stdout_of(raw: &str) -> String {
    serde_json::from_str::<serde_json::Value>(raw)
        .ok()
        .and_then(|v| v["stdout"].as_str().map(str::to_string))
        .unwrap_or_default()
}

struct Component;

impl Guest for Component {
    fn list_tools() -> String {
        serde_json::json!([
            {
                "name": "targets_list",
                "description": "有哪些机器、归哪个 connector、用途是什么。不连接任何机器，秒回。",
                "input_schema": {"type": "object", "properties": {}}
            },
            {
                "name": "fleet_status",
                "description": "全队概览：GPU 占用与空闲卡、磁盘、负载。留空 targets 表示全部。",
                "input_schema": {
                    "type": "object",
                    "properties": {"targets": {"type": "array", "items": {"type": "string"}}}
                }
            },
            {
                "name": "fleet_run",
                "description": "一条命令并发打多台机器。留空 targets 表示全部。",
                "input_schema": {
                    "type": "object", "required": ["command"],
                    "properties": {
                        "command": {"type": "string"},
                        "targets": {"type": "array", "items": {"type": "string"}},
                        "timeout_secs": {"type": "integer"}
                    }
                }
            },
            {
                "name": "gpu_find",
                "description": "跨机找空闲卡：哪台机器上有几张卡是空的。",
                "input_schema": {
                    "type": "object",
                    "properties": {
                        "targets": {"type": "array", "items": {"type": "string"}},
                        "need": {"type": "integer", "description": "至少要几张连着的空卡"}
                    }
                }
            },
            {
                "name": "gpu_status",
                "description": "一台机器上每张卡的真实占用。别人绕过 Trestle 占的卡也看得见。",
                "input_schema": {
                    "type": "object", "required": ["target"],
                    "properties": {"target": {"type": "string"}}
                }
            }
        ])
        .to_string()
    }

    fn call(tool: String, args: String) -> Result<String, Error> {
        let v: serde_json::Value = serde_json::from_str(&args)
            .map_err(|e| bad(format!("arguments are not valid JSON: {e}")))?;

        match tool.as_str() {
            "targets_list" => {
                // 不建任何连接 —— 这个工具存在的意义就是秒回。
                let mut by_connector: std::collections::BTreeMap<String, Vec<serde_json::Value>> =
                    Default::default();
                for t in host::targets() {
                    by_connector
                        .entry(t.connector.clone())
                        .or_default()
                        .push(serde_json::json!({
                            "name": t.name, "host": t.host, "port": t.port, "user": t.user,
                            "workdir": t.workdir, "note": t.note, "aliases": t.aliases,
                        }));
                }
                Ok(serde_json::to_string(&by_connector).unwrap_or_default())
            }

            "fleet_status" => {
                let targets = selected(&v);
                // 一条命令把要问的全问掉，省往返。
                let probe = "echo '--gpu--'; \
                     nvidia-smi --query-gpu=index,name,memory.used,memory.total,utilization.gpu \
                        --format=csv,noheader,nounits 2>/dev/null; \
                     echo '--disk--'; df -h / $HOME 2>/dev/null | tail -n +2; \
                     echo '--load--'; uptime";
                let results = base::call_many(&targets, "shell", &shell_payload(probe, 40));
                let names = resolve_names(&targets);

                let rows: Vec<serde_json::Value> = names
                    .iter()
                    .zip(results.iter())
                    .map(|(name, r)| match r {
                        Ok(raw) => parse_status(name, &stdout_of(raw)),
                        Err(e) => serde_json::json!({
                            "target": name, "ok": false,
                            "error": e.detail, "remedy": e.remedy
                        }),
                    })
                    .collect();
                Ok(serde_json::to_string(&rows).unwrap_or_default())
            }

            "fleet_run" => {
                let command = v
                    .get("command")
                    .and_then(|c| c.as_str())
                    .ok_or_else(|| bad("fleet_run needs a `command`"))?;
                let timeout = v.get("timeout_secs").and_then(|t| t.as_u64()).unwrap_or(60);
                let targets = selected(&v);
                let results = base::call_many(&targets, "shell", &shell_payload(command, timeout));
                let names = resolve_names(&targets);

                let rows: Vec<serde_json::Value> = names
                    .iter()
                    .zip(results.iter())
                    .map(|(name, r)| match r {
                        Ok(raw) => {
                            let v: serde_json::Value =
                                serde_json::from_str(raw).unwrap_or_default();
                            serde_json::json!({
                                "target": name, "ok": v["exit_code"] == 0,
                                "exit_code": v["exit_code"],
                                "stdout": v["stdout"], "stderr": v["stderr"],
                                "timed_out": v["timed_out"],
                            })
                        }
                        Err(e) => serde_json::json!({
                            "target": name, "ok": false, "error": e.detail, "remedy": e.remedy
                        }),
                    })
                    .collect();
                Ok(serde_json::to_string(&rows).unwrap_or_default())
            }

            "gpu_find" => {
                let need = v.get("need").and_then(|n| n.as_u64()).unwrap_or(1) as usize;
                let targets = resolve_names(&selected(&v));
                let mut rows = Vec::new();
                for name in targets {
                    // 走分配器的视图：它的判据是真实显存占用，不是我们自己的账本。
                    let Ok(raw) = gpu::view(&name) else { continue };
                    let devices: Vec<serde_json::Value> =
                        serde_json::from_str(&raw).unwrap_or_default();
                    let free: Vec<u64> = devices
                        .iter()
                        .filter(|d| d["processes"].as_u64().unwrap_or(1) == 0)
                        .filter_map(|d| d["index"].as_u64())
                        .collect();
                    rows.push(serde_json::json!({
                        "target": name,
                        "free": free,
                        "free_count": free.len(),
                        "enough": free.len() >= need,
                        "total": devices.len(),
                    }));
                }
                // 空卡多的排前面，agent 一眼就知道该去哪台。
                rows.sort_by_key(|r| std::cmp::Reverse(r["free_count"].as_u64().unwrap_or(0)));
                Ok(serde_json::to_string(&rows).unwrap_or_default())
            }

            "gpu_status" => {
                let target = v.get("target").and_then(|t| t.as_str()).ok_or_else(|| {
                    bad("gpu_status needs a `target`; there is no default machine")
                })?;
                gpu::view(target)
            }

            other => Err(err(
                ErrorKind::NotFound,
                format!("unknown tool '{other}'"),
                "targets_list, fleet_status, fleet_run, gpu_find, gpu_status",
            )),
        }
    }

    fn on_tick(_name: String, _payload: String) {}

    /// Web UI 上的显卡面板。
    fn ui_panel() -> String {
        r#"<div class="group">
  <h2>显卡</h2>
  <div id="gpu-rows"><p class="empty">读取中…</p></div>
</div>
<script>
(function () {
  const box = document.getElementById("gpu-rows");
  async function refresh() {
    try {
      const rows = await (await fetch("/api/tool/gpu_find", {
        method: "POST", headers: {"content-type": "application/json"}, body: '{"need":1}'
      })).json();
      if (!Array.isArray(rows) || !rows.length) { box.innerHTML = '<p class="empty">没有显卡信息</p>'; return; }
      box.innerHTML = rows.map(r => `
        <div class="card"><div class="row">
          <span class="name">${r.target}</span>
          <span class="pill">${r.free_count} / ${r.total} 空闲</span>
          <span class="addr">${r.free.join(", ") || "全部占用"}</span>
        </div></div>`).join("");
    } catch (e) { box.innerHTML = '<p class="empty">拿不到显卡信息</p>'; }
  }
  refresh(); setInterval(refresh, 10000);
})();
</script>"#
            .to_string()
    }

    fn config_schema() -> String {
        serde_json::json!({"type": "object", "properties": {}}).to_string()
    }
}

/// 留空表示全部机器；否则原样返回（host 那边会做别名解析）。
fn resolve_names(selected: &[String]) -> Vec<String> {
    if selected.is_empty() {
        host::targets().into_iter().map(|t| t.name).collect()
    } else {
        selected.to_vec()
    }
}

fn parse_status(name: &str, out: &str) -> serde_json::Value {
    let mut section = "";
    let mut gpus = Vec::new();
    let mut disks = Vec::new();
    let mut load = String::new();

    for line in out.lines() {
        match line.trim() {
            "--gpu--" => section = "gpu",
            "--disk--" => section = "disk",
            "--load--" => section = "load",
            _ => match section {
                "gpu" => {
                    let c: Vec<&str> = line.split(',').map(str::trim).collect();
                    if c.len() >= 5 {
                        let used: u64 = c[2].parse().unwrap_or(0);
                        gpus.push(serde_json::json!({
                            "index": c[0].parse::<u32>().unwrap_or(0),
                            "name": c[1],
                            "memory_used_mb": used,
                            "memory_total_mb": c[3].parse::<u64>().unwrap_or(0),
                            "util_percent": c[4].parse::<u32>().unwrap_or(0),
                            "free": used <= 512,
                        }));
                    }
                }
                "disk" => {
                    let c: Vec<&str> = line.split_whitespace().collect();
                    if c.len() >= 6 {
                        disks.push(serde_json::json!({
                            "filesystem": c[0], "size": c[1], "used": c[2],
                            "available": c[3], "use_percent": c[4], "mounted_on": c[5],
                        }));
                    }
                }
                "load" if !line.trim().is_empty() => {
                    load = line.trim().to_string();
                }
                _ => {}
            },
        }
    }

    let free = gpus.iter().filter(|g| g["free"] == true).count();
    serde_json::json!({
        "target": name,
        "ok": true,
        "gpus": gpus,
        "gpus_free": free,
        "disks": disks,
        "load": load,
    })
}

bindings::export!(Component with_types_in bindings);
