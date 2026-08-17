//! `job`：长任务的管理。
//!
//! 整个插件只用**基本操作**做事——`shell`（detach 起任务、普通模式查状态）、
//! `read`（读日志）——加上 host 的 KV 和事件。它没有 SSH、没有本机进程、
//! 没有网络：那些它都够不到，也不需要。
//!
//! 这是对插件模型最直接的一次检验：如果连「后台任务管理」这种有状态的东西都能
//! 只靠七个原语写出来，那插件模型就是真的成立的。
//!
//! ## 为什么偏移量记在 host
//!
//! `job_logs(since="last")` 要接着上次读。让调用方（agent）自己记偏移量是纯粹的
//! 摩擦——它得把一个数字在对话里传来传去。所以偏移量放在插件的 KV 里，
//! agent 只说「接着上次」。

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

use serde::{Deserialize, Serialize};

/// 一个任务的登记。
#[derive(Debug, Clone, Serialize, Deserialize)]
struct Job {
    job_id: String,
    target: String,
    name: String,
    command: String,
    pid: u32,
    pgid: u32,
    log_path: String,
    rc_path: String,
    started_ms: u64,
    #[serde(default)]
    gpus: Vec<u32>,
}

fn err(kind: ErrorKind, detail: impl Into<String>, remedy: impl Into<String>) -> Error {
    Error {
        kind,
        detail: detail.into(),
        remedy: remedy.into(),
    }
}

fn bad_args(detail: impl Into<String>) -> Error {
    err(
        ErrorKind::InvalidRequest,
        detail,
        "check the tool's input schema",
    )
}

fn json(v: &serde_json::Value, key: &str) -> Option<String> {
    v.get(key).and_then(|x| x.as_str()).map(str::to_string)
}

/// 单机工具的 `target` 一律必填——没有默认机。
fn need_target(v: &serde_json::Value) -> Result<String, Error> {
    json(v, "target").ok_or_else(|| {
        bad_args("this tool needs a `target`; there is no default machine (that is deliberate)")
    })
}

fn job_key(target: &str, job_id: &str) -> String {
    format!("job:{target}:{job_id}")
}

fn offset_key(target: &str, job_id: &str) -> String {
    format!("offset:{target}:{job_id}")
}

fn load_job(target: &str, job_id: &str) -> Result<Job, Error> {
    let raw = host::state_get(&job_key(target, job_id)).ok_or_else(|| {
        err(
            ErrorKind::NotFound,
            format!("no job '{job_id}' on {target}"),
            "job_list",
        )
    })?;
    serde_json::from_str(&raw)
        .map_err(|e| err(ErrorKind::Internal, format!("corrupt job record: {e}"), ""))
}

fn all_jobs() -> Vec<Job> {
    host::state_list("job:")
        .into_iter()
        .filter_map(|k| host::state_get(&k))
        .filter_map(|raw| serde_json::from_str::<Job>(&raw).ok())
        .collect()
}

/// 任务当前的退出码：rc 文件存在就说明结束了。
fn exit_code(job: &Job) -> Option<i32> {
    let payload = serde_json::json!({
        "command": format!("cat {} 2>/dev/null", shq(&job.rc_path)),
        "timeout_secs": 20
    })
    .to_string();
    let out = base::call(&job.target, "shell", &payload).ok()?;
    let v: serde_json::Value = serde_json::from_str(&out).ok()?;
    let text = v["stdout"].as_str().unwrap_or("").trim().to_string();
    if text.is_empty() {
        None
    } else {
        text.parse().ok()
    }
}

fn shq(s: &str) -> String {
    format!("'{}'", s.replace('\'', r"'\''"))
}

struct Component;

impl Guest for Component {
    fn list_tools() -> String {
        // `target` 在每个单机工具里都是 required —— 没有默认机是刻意的：
        // 默认机会制造「你以为在 gpu-4 上删文件、其实在 gpu-1」这类静默事故。
        serde_json::json!([
            {
                "name": "job_start",
                "description": "在目标机器上起一个长任务。SSH 断了照跑，pid/退出码/日志全部落盘。短命令请用 base_shell。",
                "input_schema": {
                    "type": "object",
                    "required": ["target", "command"],
                    "properties": {
                        "target": {"type": "string", "description": "机器名，必填"},
                        "command": {"type": "string"},
                        "name": {"type": "string", "description": "给这次运行起个名字，决定日志目录名"},
                        "cwd": {"type": "string"},
                        "gpus": {"type": "string", "description": "auto:N 表示自动挑 N 张空闲卡并设 CUDA_VISIBLE_DEVICES"},
                        "env": {"type": "object"}
                    }
                }
            },
            {
                "name": "job_list",
                "description": "列出任务：状态、退出码、时长、命令。留空 targets 表示全部机器。",
                "input_schema": {
                    "type": "object",
                    "properties": {
                        "targets": {"type": "array", "items": {"type": "string"}},
                        "state": {"type": "string", "enum": ["running", "finished", "all"]}
                    }
                }
            },
            {
                "name": "job_logs",
                "description": "读任务日志。since=\"last\" 接着上次读——偏移量由 host 记，你不用管。",
                "input_schema": {
                    "type": "object",
                    "required": ["target", "job_id"],
                    "properties": {
                        "target": {"type": "string"},
                        "job_id": {"type": "string"},
                        "since": {"type": "string", "description": "\"last\" 或 \"start\"，默认 last"},
                        "max_lines": {"type": "integer"}
                    }
                }
            },
            {
                "name": "job_wait",
                "description": "在远端等任务结束（不本地轮询）。可以给一个 until_pattern 提前返回。",
                "input_schema": {
                    "type": "object",
                    "required": ["target", "job_id", "timeout_secs"],
                    "properties": {
                        "target": {"type": "string"},
                        "job_id": {"type": "string"},
                        "timeout_secs": {"type": "integer"},
                        "until_pattern": {"type": "string"}
                    }
                }
            },
            {
                "name": "job_stop",
                "description": "停掉任务：TERM 整个进程组 → 宽限 → KILL。",
                "input_schema": {
                    "type": "object",
                    "required": ["target", "job_id"],
                    "properties": {
                        "target": {"type": "string"},
                        "job_id": {"type": "string"},
                        "force": {"type": "boolean", "description": "跳过宽限期直接 KILL"}
                    }
                }
            }
        ])
        .to_string()
    }

    fn call(tool: String, args: String) -> Result<String, Error> {
        let v: serde_json::Value = serde_json::from_str(&args)
            .map_err(|e| bad_args(format!("arguments are not valid JSON: {e}")))?;
        match tool.as_str() {
            "job_start" => job_start(&v),
            "job_list" => job_list(&v),
            "job_logs" => job_logs(&v),
            "job_wait" => job_wait(&v),
            "job_stop" => job_stop(&v),
            other => Err(err(
                ErrorKind::NotFound,
                format!("unknown tool '{other}'"),
                "job_start, job_list, job_logs, job_wait, job_stop",
            )),
        }
    }

    fn on_tick(_name: String, _payload: String) {
        // job 插件不注册周期任务。
    }

    /// Web UI 上的任务面板。
    ///
    /// Web UI 是**插件的一部分**：加一个插件，它自己带着自己的那块界面进来，
    /// host 只负责挂载。片段里用的是 host 提供的 `/api/tool/<name>`。
    fn ui_panel() -> String {
        r#"<div class="group">
  <h2>任务</h2>
  <div id="job-rows"><p class="empty">读取中…</p></div>
</div>
<script>
(function () {
  const box = document.getElementById("job-rows");
  async function refresh() {
    try {
      const rows = await (await fetch("/api/tool/job_list", {
        method: "POST", headers: {"content-type": "application/json"}, body: "{}"
      })).json();
      if (!Array.isArray(rows) || !rows.length) { box.innerHTML = '<p class="empty">没有任务</p>'; return; }
      box.innerHTML = rows.map(r => `
        <div class="card">
          <div class="row">
            <span class="name">${r.name}</span>
            <span class="pill">${r.target}</span>
            <span class="pill">${r.state}${r.exit_code === null || r.exit_code === undefined ? "" : " rc=" + r.exit_code}</span>
            <span class="addr">${r.elapsed_s}s</span>
          </div>
          <div class="note">${r.command}</div>
        </div>`).join("");
    } catch (e) { box.innerHTML = '<p class="empty">拿不到任务列表</p>'; }
  }
  refresh(); setInterval(refresh, 5000);
})();
</script>"#
            .to_string()
    }

    fn config_schema() -> String {
        serde_json::json!({"type": "object", "properties": {}}).to_string()
    }
}

fn job_start(v: &serde_json::Value) -> Result<String, Error> {
    let target = need_target(v)?;
    let command = json(v, "command").ok_or_else(|| bad_args("job_start needs a `command`"))?;
    let name = json(v, "name").unwrap_or_else(|| "job".into());

    // GPU：向 host 的单点分配器要卡，而不是自己去抢。
    // 两个 agent 同时要卡时，分配器天然把他们排成序。
    let mut gpus = Vec::new();
    let mut env: Vec<(String, String)> = Vec::new();
    if let Some(spec) = json(v, "gpus") {
        if let Some(n) = spec
            .strip_prefix("auto:")
            .and_then(|n| n.parse::<u32>().ok())
        {
            gpus = gpu::allocate(&target, n, &format!("job {name}"))?;
            env.push((
                "CUDA_VISIBLE_DEVICES".into(),
                gpus.iter()
                    .map(u32::to_string)
                    .collect::<Vec<_>>()
                    .join(","),
            ));
        } else if !spec.is_empty() {
            env.push(("CUDA_VISIBLE_DEVICES".into(), spec));
        }
    }
    if let Some(extra) = v.get("env").and_then(|e| e.as_object()) {
        for (k, val) in extra {
            env.push((k.clone(), val.as_str().unwrap_or_default().to_string()));
        }
    }

    let mut payload = serde_json::json!({
        "command": command,
        "detach": true,
        "name": name,
        "env": env,
    });
    if let Some(cwd) = json(v, "cwd") {
        payload["cwd"] = serde_json::Value::String(cwd);
    }

    let out = match base::call(&target, "shell", &payload.to_string()) {
        Ok(out) => out,
        Err(e) => {
            // 起不来就把卡还回去，别让预留悬着。
            if !gpus.is_empty() {
                gpu::release(&target, &gpus);
            }
            return Err(e);
        }
    };

    let d: serde_json::Value = serde_json::from_str(&out).map_err(|e| {
        err(
            ErrorKind::Protocol,
            format!("malformed detach response: {e}"),
            "",
        )
    })?;

    let job = Job {
        job_id: d["job_id"].as_str().unwrap_or_default().to_string(),
        target: target.clone(),
        name,
        command,
        pid: d["pid"].as_u64().unwrap_or(0) as u32,
        pgid: d["pgid"].as_u64().unwrap_or(0) as u32,
        log_path: d["log_path"].as_str().unwrap_or_default().to_string(),
        rc_path: d["rc_path"].as_str().unwrap_or_default().to_string(),
        started_ms: host::now_ms(),
        gpus: gpus.clone(),
    };
    host::state_set(
        &job_key(&target, &job.job_id),
        &serde_json::to_string(&job).unwrap_or_default(),
    );
    host::emit(
        "info",
        "job_started",
        &serde_json::json!({
            "target": target, "job_id": job.job_id, "pid": job.pid,
            "command": job.command, "gpus": gpus
        })
        .to_string(),
    );

    Ok(serde_json::to_string(&serde_json::json!({
        "job_id": job.job_id,
        "target": target,
        "pid": job.pid,
        "log_path": job.log_path,
        "gpus": gpus,
        "cli_command": format!("trestle job logs {} {}", target, job.job_id),
    }))
    .unwrap_or_default())
}

fn job_list(v: &serde_json::Value) -> Result<String, Error> {
    let wanted: Vec<String> = v
        .get("targets")
        .and_then(|t| t.as_array())
        .map(|a| {
            a.iter()
                .filter_map(|x| x.as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default();
    let state_filter = json(v, "state").unwrap_or_else(|| "all".into());

    let mut rows = Vec::new();
    for job in all_jobs() {
        if !wanted.is_empty() && !wanted.contains(&job.target) {
            continue;
        }
        let rc = exit_code(&job);
        let running = rc.is_none();
        match state_filter.as_str() {
            "running" if !running => continue,
            "finished" if running => continue,
            _ => {}
        }
        // 任务结束了就把卡还回去——释放绑定在 job 生命周期上，不绑在时间上。
        if !running && !job.gpus.is_empty() {
            gpu::release(&job.target, &job.gpus);
        }
        rows.push(serde_json::json!({
            "job_id": job.job_id,
            "target": job.target,
            "name": job.name,
            "command": job.command,
            "pid": job.pid,
            "state": if running { "running" } else { "finished" },
            "exit_code": rc,
            "elapsed_s": (host::now_ms().saturating_sub(job.started_ms)) / 1000,
            "gpus": job.gpus,
            "log_path": job.log_path,
        }));
    }
    Ok(serde_json::to_string(&rows).unwrap_or_default())
}

fn job_logs(v: &serde_json::Value) -> Result<String, Error> {
    let target = need_target(v)?;
    let job_id = json(v, "job_id").ok_or_else(|| bad_args("job_logs needs a `job_id`"))?;
    let job = load_job(&target, &job_id)?;
    let since = json(v, "since").unwrap_or_else(|| "last".into());
    let max_lines = v.get("max_lines").and_then(|m| m.as_u64()).unwrap_or(500) as u32;

    // 偏移量由 host 记，agent 不用管 —— 这正是「摩擦削减」的意思。
    let start_line: u32 = if since == "start" {
        1
    } else {
        host::state_get(&offset_key(&target, &job_id))
            .and_then(|s| s.parse().ok())
            .unwrap_or(1)
    };

    let payload = serde_json::json!({
        "path": job.log_path,
        "start_line": start_line,
        "max_lines": max_lines
    })
    .to_string();
    let out = base::call(&target, "read", &payload)?;
    let r: serde_json::Value = serde_json::from_str(&out).map_err(|e| {
        err(
            ErrorKind::Protocol,
            format!("malformed read response: {e}"),
            "",
        )
    })?;

    let content = r["content"].as_str().unwrap_or("");
    let returned = content.lines().count() as u32;
    let next = start_line + returned;
    host::state_set(&offset_key(&target, &job_id), &next.to_string());

    Ok(serde_json::to_string(&serde_json::json!({
        "job_id": job_id,
        "content": content,
        "from_line": start_line,
        "next_line": next,
        "total_lines": r["total_lines"],
        "exit_code": exit_code(&job),
    }))
    .unwrap_or_default())
}

fn job_wait(v: &serde_json::Value) -> Result<String, Error> {
    let target = need_target(v)?;
    let job_id = json(v, "job_id").ok_or_else(|| bad_args("job_wait needs a `job_id`"))?;
    let job = load_job(&target, &job_id)?;
    let timeout = v
        .get("timeout_secs")
        .and_then(|t| t.as_u64())
        .ok_or_else(|| bad_args("job_wait needs a `timeout_secs`"))?;
    let pattern = json(v, "until_pattern");

    // 在**远端**等，不本地轮询：一次调用而不是 N 次往返。
    let condition = match &pattern {
        Some(p) => format!(
            "grep -qE {} {} 2>/dev/null && break",
            shq(p),
            shq(&job.log_path)
        ),
        None => "true".into(),
    };
    let script = format!(
        "end=$(( $(date +%s) + {timeout} )); \
         while [ $(date +%s) -lt $end ]; do \
             if [ -f {rc} ]; then echo FINISHED; exit 0; fi; \
             {cond}; \
             sleep 1; \
         done; echo TIMEOUT",
        timeout = timeout,
        rc = shq(&job.rc_path),
        cond = condition
    );

    let payload = serde_json::json!({
        "command": script,
        // 给远端一点余量，否则 shell 的超时会先于我们的等待到期。
        "timeout_secs": timeout + 15
    })
    .to_string();
    let out = base::call(&target, "shell", &payload)?;
    let r: serde_json::Value = serde_json::from_str(&out).unwrap_or_default();
    let verdict = r["stdout"].as_str().unwrap_or("").trim().to_string();

    let rc = exit_code(&job);
    if rc.is_some() && !job.gpus.is_empty() {
        gpu::release(&target, &job.gpus);
    }

    Ok(serde_json::to_string(&serde_json::json!({
        "job_id": job_id,
        "finished": rc.is_some(),
        "exit_code": rc,
        // 区分「任务结束了」与「等超时了但任务还在跑」—— 静默返回会让两者看起来一样。
        "reason": if rc.is_some() { "finished" } else if verdict == "TIMEOUT" { "timeout" } else { "pattern_matched" },
        "note": if rc.is_none() { "the job is still running; it was not stopped" } else { "" },
    }))
    .unwrap_or_default())
}

fn job_stop(v: &serde_json::Value) -> Result<String, Error> {
    let target = need_target(v)?;
    let job_id = json(v, "job_id").ok_or_else(|| bad_args("job_stop needs a `job_id`"))?;
    let job = load_job(&target, &job_id)?;
    let force = v.get("force").and_then(|f| f.as_bool()).unwrap_or(false);

    // TERM 整个**进程组** → 宽限 → KILL。只杀直接子进程的话孙进程会残留。
    let script = if force {
        format!("kill -9 -{pgid} 2>/dev/null; echo killed", pgid = job.pgid)
    } else {
        format!(
            "kill -TERM -{pgid} 2>/dev/null; \
             for i in 1 2 3 4 5 6 7 8 9 10; do \
                 kill -0 -{pgid} 2>/dev/null || break; sleep 1; \
             done; \
             kill -9 -{pgid} 2>/dev/null; echo stopped",
            pgid = job.pgid
        )
    };
    base::call(
        &target,
        "shell",
        &serde_json::json!({"command": script, "timeout_secs": 30}).to_string(),
    )?;

    if !job.gpus.is_empty() {
        gpu::release(&target, &job.gpus);
    }
    host::emit(
        "info",
        "job_finished",
        &serde_json::json!({"target": target, "job_id": job_id, "reason": "stopped"}).to_string(),
    );

    Ok(serde_json::to_string(&serde_json::json!({
        "job_id": job_id,
        "stopped": true,
        "forced": force,
    }))
    .unwrap_or_default())
}

bindings::export!(Component with_types_in bindings);
