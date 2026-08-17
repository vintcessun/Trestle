//! M3 的验收：技能插件只靠七个基本操作就能把长任务管起来。
//!
//! ```text
//! $env:TRESTLE_HOME = "<repo>\config"
//! cargo test -p trestle-host --test tools -- --include-ignored --test-threads=1 --nocapture
//! ```

use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use trestle_core::config::ConfigStore;
use trestle_host::host::{HostOptions, TrestleHost};
use trestle_host::state::EventSink;

#[derive(Default)]
struct Collector {
    events: Mutex<Vec<String>>,
}

impl EventSink for Collector {
    fn emit(&self, _plugin: &str, _level: &str, kind: &str, _fields: &str) {
        self.events.lock().unwrap().push(kind.to_string());
    }
}

impl Collector {
    fn saw(&self, kind: &str) -> bool {
        self.events.lock().unwrap().iter().any(|k| k == kind)
    }
}

/// 整个测试二进制共用一个 host。
///
/// 每个测试各建一个的话，7 个插件（含 18 MB 的 Python 那个）会被 wasmtime
/// 编译 N 遍——并行跑的时候直接卡住。共用之后只编一次。
async fn host() -> &'static (TrestleHost, Arc<Collector>) {
    static HOST: tokio::sync::OnceCell<(TrestleHost, Arc<Collector>)> =
        tokio::sync::OnceCell::const_new();
    HOST.get_or_init(build_host).await
}

async fn build_host() -> (TrestleHost, Arc<Collector>) {
    // trestle-host 的测试不起 daemon，共用 config 目录没问题；
    // 但插件路径要显式指到仓库那一份，别依赖「home 的父目录就是仓库」。
    let repo = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .parent()
        .unwrap()
        .to_path_buf();
    // SAFETY: 测试进程内只写这一次。
    unsafe { std::env::set_var("TRESTLE_PLUGINS", repo.join("plugins")) };
    let root = std::env::var("TRESTLE_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|_| repo.join("config"));
    let store = Arc::new(ConfigStore::load(root).expect("config"));
    let events = Arc::new(Collector::default());
    let opts = HostOptions {
        events: Arc::clone(&events) as Arc<dyn EventSink>,
        // 池子小一点，测试起得快。
        pool_size: 2,
        ..Default::default()
    };
    let h = TrestleHost::start(store, opts).await.unwrap_or_else(|e| {
        panic!("cannot start host: {e}\n(run scripts/build-plugins.ps1 first)")
    });
    (h, events)
}

fn target() -> String {
    std::env::var("TRESTLE_TEST_TARGET").unwrap_or_else(|_| "gpu-4".into())
}

#[tokio::test]
async fn the_tool_surface_includes_base_and_plugin_tools() {
    let (h, events) = host().await;
    let names: Vec<String> = h
        .tool_descriptors()
        .await
        .into_iter()
        .map(|d| d.name)
        .collect();

    // 七个基本操作是 host 自己的面。
    for expected in [
        "base_read",
        "base_write",
        "base_edit",
        "base_shell",
        "base_upload",
        "base_download",
        "base_forward",
    ] {
        assert!(
            names.contains(&expected.to_string()),
            "missing {expected}: {names:?}"
        );
    }
    // job 插件贡献的。
    for expected in ["job_start", "job_list", "job_logs", "job_wait", "job_stop"] {
        assert!(
            names.contains(&expected.to_string()),
            "missing {expected}: {names:?}"
        );
    }
    assert!(events.saw("plugin_loaded"));
}

#[tokio::test]
async fn every_tool_that_names_a_machine_requires_it() {
    let (h, _) = host().await;
    for d in h.tool_descriptors().await {
        let props = &d.input_schema["properties"];
        if props.get("target").is_some() {
            let required = d.input_schema["required"]
                .as_array()
                .cloned()
                .unwrap_or_default();
            assert!(
                required.iter().any(|r| r == "target"),
                "{} takes a target but does not require it — that is how you delete files \
                 on the wrong machine",
                d.name
            );
        }
    }
}

#[tokio::test]
async fn calling_a_tool_that_does_not_exist_says_so() {
    let (h, _) = host().await;
    let err = h.call_tool("job_teleport", "{}").await.unwrap_err();
    assert!(err.to_string().contains("job_teleport"), "{err}");
}

#[tokio::test]
async fn a_single_machine_tool_refuses_to_guess_the_machine() {
    let (h, _) = host().await;
    let err = h
        .call_tool("job_start", r#"{"command":"echo hi"}"#)
        .await
        .unwrap_err();
    let msg = err.to_string();
    assert!(msg.contains("target"), "{msg}");
    // 错误里要说明这是有意为之，否则下一个人会去加一个默认机。
    assert!(
        msg.contains("default machine") || msg.contains("deliberate"),
        "{msg}"
    );
}

#[tokio::test]
#[ignore = "needs real servers"]
async fn a_job_survives_start_logs_wait_and_stop() {
    let (h, events) = host().await;
    let t = target();

    // 起一个会说话的任务。
    let out = h
        .call_tool(
            "job_start",
            &serde_json::json!({
                "target": t,
                "command": "for i in 1 2 3 4 5 6 7 8; do echo line-$i; sleep 1; done",
                "name": "m3-probe"
            })
            .to_string(),
        )
        .await
        .expect("job_start");
    let started: serde_json::Value = serde_json::from_str(&out).unwrap();
    let job_id = started["job_id"].as_str().expect("job_id").to_string();
    assert!(started["pid"].as_u64().unwrap_or(0) > 0);
    // 返回里要给出 CLI 兜底路径 —— daemon 挂了 ws 会断，但 CLI 子进程照跑。
    assert!(
        started["cli_command"]
            .as_str()
            .unwrap_or("")
            .contains(&job_id)
    );
    assert!(events.saw("job_started"));

    tokio::time::sleep(std::time::Duration::from_secs(3)).await;

    // 增量读日志：第二次不该把第一次那些行再给一遍。
    let first = h
        .call_tool(
            "job_logs",
            &serde_json::json!({"target": t, "job_id": job_id}).to_string(),
        )
        .await
        .expect("job_logs");
    let first: serde_json::Value = serde_json::from_str(&first).unwrap();
    let first_text = first["content"].as_str().unwrap_or("").to_string();
    assert!(first_text.contains("line-1"), "{first_text}");

    tokio::time::sleep(std::time::Duration::from_secs(3)).await;
    let second = h
        .call_tool(
            "job_logs",
            &serde_json::json!({"target": t, "job_id": job_id}).to_string(),
        )
        .await
        .expect("job_logs");
    let second: serde_json::Value = serde_json::from_str(&second).unwrap();
    let second_text = second["content"].as_str().unwrap_or("").to_string();
    assert!(
        !second_text.contains("line-1"),
        "since=last re-sent lines the caller already had:\n{second_text}"
    );
    assert!(
        second["from_line"].as_u64() > first["from_line"].as_u64(),
        "the offset did not advance"
    );

    // job_list 认得它，而且在跑。
    let listed = h
        .call_tool("job_list", &serde_json::json!({"targets": [t]}).to_string())
        .await
        .expect("job_list");
    let rows: Vec<serde_json::Value> = serde_json::from_str(&listed).unwrap();
    let row = rows
        .iter()
        .find(|r| r["job_id"] == job_id.as_str())
        .expect("job in list");
    assert_eq!(row["target"], t.as_str());

    // 在远端等它结束。
    let waited = h
        .call_tool(
            "job_wait",
            &serde_json::json!({"target": t, "job_id": job_id, "timeout_secs": 30}).to_string(),
        )
        .await
        .expect("job_wait");
    let waited: serde_json::Value = serde_json::from_str(&waited).unwrap();
    assert_eq!(waited["finished"], true, "{waited}");
    assert_eq!(waited["exit_code"], 0);
    assert_eq!(waited["reason"], "finished");
}

#[tokio::test]
#[ignore = "needs real servers"]
async fn stopping_a_job_kills_the_whole_process_group() {
    let (h, _) = host().await;
    let t = target();

    let out = h
        .call_tool(
            "job_start",
            &serde_json::json!({
                "target": t,
                // 孙进程：外层起内层，内层 sleep。只杀直接子进程的话它会残留。
                "command": "bash -c 'seq 1 400 | while read i; do sleep 1; done'",
                "name": "m3-stop"
            })
            .to_string(),
        )
        .await
        .expect("job_start");
    let job_id = serde_json::from_str::<serde_json::Value>(&out).unwrap()["job_id"]
        .as_str()
        .unwrap()
        .to_string();

    tokio::time::sleep(std::time::Duration::from_secs(2)).await;
    h.call_tool(
        "job_stop",
        &serde_json::json!({"target": t, "job_id": job_id, "force": true}).to_string(),
    )
    .await
    .expect("job_stop");
    tokio::time::sleep(std::time::Duration::from_secs(1)).await;

    // 方括号技巧：不加的话检查命令自己的命令行就含这个 pattern。
    let survivors = h
        .op(
            &t,
            "shell",
            &serde_json::json!({"command": "ps -eo cmd | grep -c '[s]eq 1 400' || true", "timeout_secs": 20})
                .to_string(),
        )
        .await
        .expect("check");
    let v: serde_json::Value = serde_json::from_str(&survivors).unwrap();
    assert_eq!(
        v["stdout"].as_str().unwrap_or("").trim(),
        "0",
        "grandchildren survived job_stop"
    );
}

#[tokio::test]
#[ignore = "needs real servers"]
async fn the_fs_plugin_sees_the_real_filesystem() {
    let (h, _) = host().await;
    let t = target();
    let dir = "/tmp/trestle-m4-fs";

    h.op(
        &t,
        "shell",
        &serde_json::json!({
            "command": format!("rm -rf {dir}; mkdir -p {dir}/sub && echo hello > {dir}/a.txt && echo x > {dir}/sub/b.log"),
            "timeout_secs": 20
        }).to_string(),
    )
    .await
    .expect("prepare");

    let listed = h
        .call_tool(
            "fs_list",
            &serde_json::json!({"target": t, "path": dir}).to_string(),
        )
        .await
        .expect("fs_list");
    let listed: serde_json::Value = serde_json::from_str(&listed).unwrap();
    let names: Vec<&str> = listed["entries"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|e| e["name"].as_str())
        .collect();
    assert!(names.contains(&"a.txt"), "{names:?}");
    assert!(names.contains(&"sub"), "{names:?}");

    let stat = h
        .call_tool(
            "fs_stat",
            &serde_json::json!({"target": t, "path": format!("{dir}/a.txt")}).to_string(),
        )
        .await
        .expect("fs_stat");
    let stat: serde_json::Value = serde_json::from_str(&stat).unwrap();
    assert_eq!(stat["size"], 6); // "hello\n"
    assert_eq!(stat["is_dir"], false);

    let found = h
        .call_tool(
            "fs_find",
            &serde_json::json!({"target": t, "path": dir, "glob": "*.log"}).to_string(),
        )
        .await
        .expect("fs_find");
    let found: serde_json::Value = serde_json::from_str(&found).unwrap();
    let hits = found["hits"].as_array().unwrap();
    assert_eq!(hits.len(), 1, "{found}");
    assert!(hits[0]["path"].as_str().unwrap().ends_with("sub/b.log"));

    let disk = h
        .call_tool("fs_disk", &serde_json::json!({"target": t}).to_string())
        .await
        .expect("fs_disk");
    assert!(disk.contains("use_percent"), "{disk}");

    h.op(
        &t,
        "shell",
        &serde_json::json!({"command": format!("rm -rf {dir}"), "timeout_secs": 20}).to_string(),
    )
    .await
    .ok();
}

#[tokio::test]
#[ignore = "needs real servers"]
async fn fleet_status_reports_every_machine_and_groups_by_connector() {
    let (h, _) = host().await;

    // targets_list 不该碰任何机器 —— 它存在的意义就是秒回。
    let started = std::time::Instant::now();
    let listed = h
        .call_tool("targets_list", "{}")
        .await
        .expect("targets_list");
    let elapsed = started.elapsed();
    assert!(
        elapsed < std::time::Duration::from_millis(500),
        "targets_list took {elapsed:?} — it must not connect to anything"
    );
    let grouped: serde_json::Value = serde_json::from_str(&listed).unwrap();
    assert!(grouped.get("gpu-cluster").is_some(), "{grouped}");
    assert!(grouped.get("cloud").is_some(), "{grouped}");

    let lab: Vec<String> = h
        .fleet
        .targets()
        .iter()
        .filter(|t| t.connector == "gpu-cluster")
        .map(|t| t.name.clone())
        .collect();

    let status = h
        .call_tool(
            "fleet_status",
            &serde_json::json!({"targets": lab}).to_string(),
        )
        .await
        .expect("fleet_status");
    let rows: Vec<serde_json::Value> = serde_json::from_str(&status).unwrap();
    assert!(!rows.is_empty());
    for row in &rows {
        assert_eq!(row["ok"], true, "{row}");
        // 这几台都是 8 卡机，GPU 那段必须解析出来。
        assert!(
            row["gpus"]
                .as_array()
                .map(|g| !g.is_empty())
                .unwrap_or(false),
            "no GPUs parsed for {}: {row}",
            row["target"]
        );
        assert!(
            row["disks"]
                .as_array()
                .map(|d| !d.is_empty())
                .unwrap_or(false),
            "{row}"
        );
    }

    let ran = h
        .call_tool(
            "fleet_run",
            &serde_json::json!({"command": "hostname", "timeout_secs": 30}).to_string(),
        )
        .await
        .expect("fleet_run");
    let rows: Vec<serde_json::Value> = serde_json::from_str(&ran).unwrap();
    let ok = rows.iter().filter(|r| r["ok"] == true).count();
    assert!(ok >= 2, "expected several machines to answer: {ran}");
}

#[tokio::test]
#[ignore = "needs real servers"]
async fn gpu_find_ranks_machines_by_free_cards() {
    let (h, _) = host().await;
    let lab: Vec<String> = h
        .fleet
        .targets()
        .iter()
        .filter(|t| t.connector == "gpu-cluster")
        .map(|t| t.name.clone())
        .collect();

    let found = h
        .call_tool(
            "gpu_find",
            &serde_json::json!({"targets": lab, "need": 1}).to_string(),
        )
        .await
        .expect("gpu_find");
    let rows: Vec<serde_json::Value> = serde_json::from_str(&found).unwrap();
    assert!(!rows.is_empty(), "{found}");
    // 空卡多的排前面 —— agent 一眼就知道该去哪台。
    let counts: Vec<u64> = rows
        .iter()
        .map(|r| r["free_count"].as_u64().unwrap_or(0))
        .collect();
    let mut sorted = counts.clone();
    sorted.sort_by(|a, b| b.cmp(a));
    assert_eq!(
        counts, sorted,
        "results are not ranked by free cards: {found}"
    );
}

#[tokio::test]
#[ignore = "needs real servers"]
async fn xfer_moves_a_file_between_two_machines_that_cannot_see_each_other() {
    let (h, _) = host().await;
    let registry = h.fleet.targets().clone();
    let from = registry
        .iter()
        .find(|t| t.connector == "gpu-cluster")
        .map(|t| t.name.clone())
        .expect("a lab machine");
    let to = registry
        .iter()
        .find(|t| t.connector == "cloud")
        .map(|t| t.name.clone())
        .expect("one of my machines");

    let src = "/tmp/trestle-m4-xfer.txt";
    let dst = "/tmp/trestle-m4-xfer-arrived.txt";
    let marker = "crossed-the-gap";

    h.op(
        &from,
        "shell",
        &serde_json::json!({"command": format!("echo {marker} > {src}"), "timeout_secs": 20})
            .to_string(),
    )
    .await
    .expect("prepare source");

    // 这两台机器互相根本连不通（一台在校园网内、一台在公网），本地就是中转站。
    let moved = h
        .call_tool(
            "xfer_between",
            &serde_json::json!({
                "from": from, "from_path": src,
                "to": to, "to_path": dst
            })
            .to_string(),
        )
        .await
        .expect("xfer_between");
    let moved: serde_json::Value = serde_json::from_str(&moved).unwrap();
    assert_eq!(
        moved["path"], dst,
        "the output path must be the one that was asked for"
    );

    let arrived = h
        .op(&to, "read", &serde_json::json!({"path": dst}).to_string())
        .await
        .expect("read at destination");
    let arrived: serde_json::Value = serde_json::from_str(&arrived).unwrap();
    assert_eq!(arrived["content"].as_str().unwrap_or("").trim(), marker);

    for (t, p) in [(&from, src), (&to, dst)] {
        h.op(
            t,
            "shell",
            &serde_json::json!({"command": format!("rm -f {p}"), "timeout_secs": 20}).to_string(),
        )
        .await
        .ok();
    }
}

#[tokio::test]
#[ignore = "needs real servers"]
async fn hitting_several_machines_happens_concurrently() {
    let (h, _) = host().await;
    let registry = h.fleet.targets().clone();
    let lab: Vec<String> = registry
        .iter()
        .filter(|t| t.connector == "gpu-cluster")
        .map(|t| t.name.clone())
        .collect();
    assert!(
        lab.len() >= 2,
        "need at least two machines to prove concurrency"
    );

    // 先把连接都建起来，这样测的是并发而不是建链。
    for name in &lab {
        h.op(name, "shell", r#"{"command":"true","timeout_secs":20}"#)
            .await
            .expect("warm up");
    }

    // 每台睡 2 秒。串行的话是 2×N 秒，并发的话接近 2 秒。
    let started = std::time::Instant::now();
    let results = h
        .fleet
        .op_many(&lab, "shell", r#"{"command":"sleep 2","timeout_secs":30}"#)
        .await;
    let elapsed = started.elapsed();

    for (name, r) in &results {
        assert!(r.is_ok(), "{name} failed: {:?}", r.as_ref().err());
    }
    println!("{} machines in {elapsed:?}", lab.len());
    assert!(
        elapsed < std::time::Duration::from_secs(2 * lab.len() as u64),
        "{} machines took {elapsed:?} — that is serial, not concurrent",
        lab.len()
    );
}
