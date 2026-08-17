//! 逐个工具真调。
//!
//! 这是上一代 `mcp_smoke.py` 的等价物——那套测试在 53 个工具里抓到过 1 个
//! **mock 测试永远抓不到**的 bug（一个接口产出的文件名和入参给的不一样，
//! 于是调用方拿自己给的路径去解包就 404）。
//!
//! 形态照搬：对**每一个**对外工具，用一组安全参数真调一次，并且检查它的 schema。
//! 「看起来对」和「真的对」之间的差距，只有真调能量出来。
//!
//! ```text
//! $env:TRESTLE_HOME = "<repo>\config"
//! cargo test -p trestle-daemon --test smoke -- --ignored --nocapture --test-threads=1
//! ```

use std::path::PathBuf;
use std::time::Duration;

use trestle_daemon::ipc::{DaemonInfo, IpcClient, RequestBody};

fn repo_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .parent()
        .unwrap()
        .to_path_buf()
}

/// 每个测试二进制用自己的 home 目录，并且**共用一个 daemon**。
///
/// 每个测试各起一个 daemon 会撞 daemon.json（cargo 默认并行跑测试），而且每次都要
/// 重新加载全部插件——实测 6 个测试要 300 秒。共用一个之后是 40 秒。
fn home() -> PathBuf {
    static ONCE: std::sync::OnceLock<PathBuf> = std::sync::OnceLock::new();
    ONCE.get_or_init(|| {
        let source = std::env::var("TRESTLE_HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|_| repo_root().join("config"));
        let dir = std::env::temp_dir().join(format!(
            "trestle-test-{}-{}",
            env!("CARGO_CRATE_NAME"),
            std::process::id()
        ));
        let _ = std::fs::create_dir_all(&dir);
        let _ = std::fs::copy(source.join("secrets.toml"), dir.join("secrets.toml"));
        // 把 idle 超时压短：测试结束后没人管这个 daemon，让它自己走。
        if let Ok(raw) = std::fs::read_to_string(source.join("trestle.toml")) {
            let raw = raw.replace("idle_timeout_secs = 1800", "idle_timeout_secs = 90");
            let _ = std::fs::write(dir.join("trestle.toml"), raw);
        }
        // SAFETY: 在任何测试跑起来之前设置，之后只读。
        unsafe { std::env::set_var("TRESTLE_PLUGINS", repo_root().join("plugins")) };
        dir
    })
    .clone()
}

/// 整个测试二进制共用的那一个 daemon。
async fn daemon() -> &'static Running {
    static DAEMON: tokio::sync::OnceCell<Running> = tokio::sync::OnceCell::const_new();
    DAEMON.get_or_init(Running::start).await
}

fn daemon_exe() -> PathBuf {
    let mut p = std::env::current_exe().expect("test exe");
    p.pop();
    p.pop();
    p.join(if cfg!(windows) {
        "trestled.exe"
    } else {
        "trestled"
    })
}

struct Running {
    child: std::process::Child,
    home: PathBuf,
}

impl Running {
    async fn start() -> Self {
        let home = home();
        let _ = std::fs::remove_file(home.join("daemon.json"));
        // clippy 会提醒「spawn 之后不是每条路径都 wait」——这里是有意的：
        // 这个 daemon 由 Running::drop 统一 kill + wait，而 Running 活到进程结束。
        #[allow(clippy::zombie_processes)]
        let child = std::process::Command::new(daemon_exe())
            .arg("--home")
            .arg(&home)
            .env("TRESTLE_PLUGINS", repo_root().join("plugins"))
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .expect("start trestled");

        // 放宽到 120 秒：几个测试二进制并行跑时，机器上可能同时有两个 daemon
        // 在做首次插件编译，外加 cargo 自己在编。这是测试环境的抖动，不是产品性质
        // ——正常启动实测 1–2 秒。
        let deadline = std::time::Instant::now() + Duration::from_secs(120);
        while std::time::Instant::now() < deadline {
            if DaemonInfo::read(&home).is_some() && IpcClient::connect(&home).await.is_ok() {
                return Self { child, home };
            }
            tokio::time::sleep(Duration::from_millis(200)).await;
        }
        panic!("the daemon never became reachable");
    }
}

impl Drop for Running {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn target() -> String {
    std::env::var("TRESTLE_TEST_TARGET").unwrap_or_else(|_| "gpu-4".into())
}

/// 每个工具的一组**安全**参数——只读、或者只碰 /tmp 下自己的东西。
fn safe_args(tool: &str, t: &str) -> Option<serde_json::Value> {
    let scratch = "/tmp/trestle-smoke";
    Some(match tool {
        // ── 七个基本操作 ──
        "base_read" => serde_json::json!({"target": t, "path": format!("{scratch}/probe.txt")}),
        "base_write" => serde_json::json!({
            "target": t, "path": format!("{scratch}/probe.txt"),
            "content": "smoke\n", "make_dirs": true
        }),
        "base_edit" => serde_json::json!({
            "target": t, "path": format!("{scratch}/probe.txt"),
            "op": {"kind": "literal", "old": "smoke", "new": "smoke", "count": 1}
        }),
        "base_shell" => serde_json::json!({"target": t, "command": "true", "timeout_secs": 20}),
        "base_upload" => return None, // 要本地文件，单独测（见 tools.rs）
        "base_download" => return None, // 同上
        "base_forward" => return None, // 要远端真的有人在听，单独测

        // ── job ──
        "job_list" => serde_json::json!({"targets": [t]}),
        "job_start" => serde_json::json!({"target": t, "command": "true", "name": "smoke"}),
        "job_logs" => return None, // 需要一个已知 job，在流程测试里覆盖
        "job_wait" => return None,
        "job_stop" => return None,

        // ── fs ──
        "fs_list" => serde_json::json!({"target": t, "path": "/tmp"}),
        "fs_find" => serde_json::json!({"target": t, "path": scratch, "glob": "*.txt"}),
        "fs_stat" => serde_json::json!({"target": t, "path": "/tmp"}),
        "fs_tree" => serde_json::json!({"target": t, "path": scratch, "depth": 1}),
        "fs_disk" => serde_json::json!({"target": t}),

        // ── fleet ──
        "targets_list" => serde_json::json!({}),
        "fleet_status" => serde_json::json!({"targets": [t]}),
        "fleet_run" => serde_json::json!({"command": "true", "targets": [t], "timeout_secs": 30}),
        "gpu_find" => serde_json::json!({"targets": [t], "need": 1}),
        "gpu_status" => serde_json::json!({"target": t}),

        // ── xfer ──
        "xfer_between" => return None, // 会真的搬东西，在 tools.rs 里单独测
        "xfer_distribute" => return None,

        // ── hello-py（Python 写的，验证那条路没断）──
        "hello_py" => serde_json::json!({"target": t, "message": "smoke"}),

        // ── monitor ──
        "monitor_open" => serde_json::json!({"timeout_secs": 5, "only_target": t}),

        _ => return None,
    })
}

#[tokio::test]
async fn every_tool_declares_a_sane_schema() {
    let d = daemon().await;
    let client = IpcClient::connect(&d.home).await.expect("connect");
    client
        .call(RequestBody::Hello {
            label: "smoke".into(),
        })
        .await
        .expect("hello");

    let tools = client.call(RequestBody::ListTools).await.expect("tools");
    let tools = tools.as_array().expect("an array of tools");
    assert!(!tools.is_empty());

    let mut problems = Vec::new();
    for t in tools {
        let name = t["name"].as_str().unwrap_or("");
        let schema = &t["input_schema"];

        // 名字用 `_` 不用 `.`：Claude Code 会把 `.` 正规化成 `_`，
        // 于是声明的名字和 permission matcher 看到的名字对不上。
        if name.contains('.') {
            problems.push(format!("{name}: uses a dot in its name"));
        }
        if t["description"].as_str().unwrap_or("").trim().is_empty() {
            problems.push(format!("{name}: has no description"));
        }
        if schema["type"] != "object" {
            problems.push(format!("{name}: input_schema is not an object schema"));
        }

        // **没有默认机**：任何接受 target 的工具都必须 required 它。
        // 这一条是上一代实测后拍板的：默认机会制造「你以为在 gpu-4 上删文件、
        // 其实在 gpu-1」这类静默事故。
        let takes_target = schema["properties"].get("target").is_some();
        let requires_target = schema["required"]
            .as_array()
            .map(|r| r.iter().any(|x| x == "target"))
            .unwrap_or(false);
        if takes_target && !requires_target {
            problems.push(format!("{name}: takes a target but does not require it"));
        }
    }

    assert!(
        problems.is_empty(),
        "{} schema problem(s):\n  {}",
        problems.len(),
        problems.join("\n  ")
    );
    println!("{} tools, all schemas sane", tools.len());
}

#[tokio::test]
#[ignore = "needs real servers"]
async fn every_tool_answers_when_actually_called() {
    let d = daemon().await;
    let client = IpcClient::connect(&d.home).await.expect("connect");
    let hello = client
        .call(RequestBody::Hello {
            label: "smoke".into(),
        })
        .await
        .expect("hello");
    let agent = hello["agent"].as_str().unwrap().to_string();
    let t = target();

    // 先把探测用的东西准备好，免得只读工具因为路径不存在而失败。
    client
        .call(RequestBody::Op {
            agent: agent.clone(),
            target: t.clone(),
            op: "shell".into(),
            payload: serde_json::json!({
                "command": "mkdir -p /tmp/trestle-smoke && echo smoke > /tmp/trestle-smoke/probe.txt",
                "timeout_secs": 20
            })
            .to_string(),
        })
        .await
        .expect("prepare");

    let tools = client.call(RequestBody::ListTools).await.expect("tools");
    let tools = tools.as_array().unwrap().clone();

    let mut called = 0;
    let mut skipped = Vec::new();
    let mut failures = Vec::new();

    for tool in &tools {
        let name = tool["name"].as_str().unwrap_or("").to_string();
        let Some(args) = safe_args(&name, &t) else {
            skipped.push(name);
            continue;
        };

        let body = match name.strip_prefix("base_") {
            Some(op) => RequestBody::Op {
                agent: agent.clone(),
                target: t.clone(),
                op: op.to_string(),
                payload: args.to_string(),
            },
            None => RequestBody::CallTool {
                agent: agent.clone(),
                tool: name.clone(),
                args: args.to_string(),
            },
        };

        match client.call(body).await {
            Ok(v) => {
                called += 1;
                // 返回值必须是 JSON 而不是一坨字符串——不然调用方没法用。
                if v.is_null() {
                    failures.push(format!("{name}: returned null"));
                }
                println!("  ok   {name}");
            }
            Err(e) => {
                failures.push(format!("{name}: {e}"));
                println!("  FAIL {name}: {e}");
            }
        }
    }

    // 收拾。
    client
        .call(RequestBody::Op {
            agent: agent.clone(),
            target: t.clone(),
            op: "shell".into(),
            payload: serde_json::json!({
                "command": "rm -rf /tmp/trestle-smoke", "timeout_secs": 20
            })
            .to_string(),
        })
        .await
        .ok();

    println!(
        "\n{called} tool(s) really called, {} skipped ({}), {} failed",
        skipped.len(),
        skipped.join(", "),
        failures.len()
    );
    assert!(failures.is_empty(), "{}", failures.join("\n"));
    // 跳过的那些不是「没测」——它们在 trestle-host 的流程测试里各有覆盖。
    // 但跳过的数量要少，否则这份 smoke 就名不副实了。
    assert!(
        called >= 15,
        "only {called} tools were actually exercised; the smoke test is not earning its name"
    );
}

#[tokio::test]
#[ignore = "needs real servers"]
async fn an_output_path_is_always_the_path_that_was_asked_for() {
    // 这条单独拎出来，因为它就是上一代那个 mock 测不出的 bug 的形状：
    // 你传 `x.tgz`，接口产出 `x.tar.gz`，调用方拿自己给的路径去解包就 404。
    let d = daemon().await;
    let client = IpcClient::connect(&d.home).await.expect("connect");
    let hello = client
        .call(RequestBody::Hello {
            label: "smoke".into(),
        })
        .await
        .expect("hello");
    let agent = hello["agent"].as_str().unwrap().to_string();
    let t = target();

    // 故意用一个「后缀看起来会被人改写」的名字。
    let odd = "/tmp/trestle-smoke-suffix.tgz";
    let local = std::env::temp_dir().join("trestle-smoke-suffix.tgz");
    std::fs::write(&local, b"not really a tarball").unwrap();

    let up = client
        .call(RequestBody::Op {
            agent: agent.clone(),
            target: t.clone(),
            op: "upload".into(),
            payload: serde_json::json!({
                "local_path": local.to_str().unwrap(), "remote_path": odd
            })
            .to_string(),
        })
        .await
        .expect("upload");
    assert_eq!(up["path"], odd, "upload changed the path it was given");

    let back = std::env::temp_dir().join("trestle-smoke-back.tgz");
    let down = client
        .call(RequestBody::Op {
            agent: agent.clone(),
            target: t.clone(),
            op: "download".into(),
            payload: serde_json::json!({
                "remote_path": odd, "local_path": back.to_str().unwrap()
            })
            .to_string(),
        })
        .await
        .expect("download");
    assert_eq!(
        down["path"],
        back.to_str().unwrap(),
        "download changed the path it was given"
    );
    assert!(
        back.exists(),
        "the file is not where the caller asked for it"
    );

    std::fs::remove_file(&local).ok();
    std::fs::remove_file(&back).ok();
    client
        .call(RequestBody::Op {
            agent,
            target: t,
            op: "shell".into(),
            payload: serde_json::json!({"command": format!("rm -f {odd}"), "timeout_secs": 20})
                .to_string(),
        })
        .await
        .ok();
}
