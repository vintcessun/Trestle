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
        // 上限压到 2：测试要的是「会不会长」，不是「能长多大」。
        policy: trestle_host::pool::PoolPolicy::default().with_max(2),
        ..Default::default()
    };
    let h = TrestleHost::start(store, opts).await.unwrap_or_else(|e| {
        panic!("cannot start host: {e}\n(run scripts/build-plugins.ps1 first)")
    });
    (h, events)
}

/// 按**驱动**找 connector，而不是按名字。
///
/// 名字是用户给自己那组机器起的——写死在这里等于把某个人的机队名字印进一个
/// 公开仓库。驱动名是这个项目自己的东西，而且它才是测试真正关心的。
fn connector_using(driver: &str) -> String {
    let repo = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .parent()
        .unwrap()
        .to_path_buf();
    let root = std::env::var("TRESTLE_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|_| repo.join("config"));
    let store = ConfigStore::load(root).expect("config");
    store
        .config()
        .connectors
        .iter()
        .find(|(_, c)| c.plugin == driver && c.enabled)
        .map(|(name, _)| name.clone())
        .unwrap_or_else(|| panic!("no enabled connector uses the {driver} driver"))
}

fn proxy_connector() -> String {
    connector_using("ssh-socks5")
}

fn target() -> String {
    std::env::var("TRESTLE_TEST_TARGET").unwrap_or_else(|_| {
        let c = proxy_connector();
        // 真调测试的靶子取配置里的第一台，不写死名字。
        futures_first_target(&c)
    })
}

fn futures_first_target(connector: &str) -> String {
    let repo = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .parent()
        .unwrap()
        .to_path_buf();
    let root = std::env::var("TRESTLE_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|_| repo.join("config"));
    let store = ConfigStore::load(root).expect("config");
    let registry = store.targets().expect("targets");
    registry
        .iter()
        .find(|t| t.connector == connector)
        .map(|t| t.name.clone())
        .unwrap_or_else(|| panic!("{connector} manages no machines"))
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
    // 协同层。它们曾经只有 CLI 能用，于是「多个 agent 互相知道在干什么」
    // 对 MCP 里的 agent 完全不存在——而那正是需要它的地方。
    for expected in ["agents_list", "notes_list", "note_put"] {
        assert!(
            names.contains(&expected.to_string()),
            "the coordination layer must be reachable as a tool, not just from the CLI;              missing {expected}: {names:?}"
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
async fn only_a_stateless_plugin_is_allowed_to_grow_a_pool() {
    let (h, _) = host().await;

    // 每个插件都**从一个实例起**：绝大多数插件这辈子不会被并发调用，
    // 为它们预留实例是纯浪费。区别在上限——撞上并发时准不准长。
    let job = h.tools.instance_of("job").await.expect("job is loaded");
    assert!(job.manifest.capabilities.stateless);
    assert_eq!(job.pool.size(), 1, "a pool must start at one instance");
    assert!(
        job.pool.max() > 1,
        "job declared itself stateless but its pool is capped at {}",
        job.pool.max()
    );

    // hello-py 没声明 stateless，所以上限就是 1：它可能把状态存在 wasm 内存里，
    // 多开会让实例各看各的，而且是静默出错。
    if let Some(py) = h.tools.instance_of("hello-py").await {
        assert!(!py.manifest.capabilities.stateless);
        assert_eq!(
            py.pool.max(),
            1,
            "a plugin that did not declare stateless must never be pooled — \
             it may keep state in wasm memory, and pooling would silently split it"
        );
    }
}

#[tokio::test]
#[ignore = "needs real servers"]
async fn two_agents_calling_the_same_tool_do_not_block_each_other() {
    let (h, _) = host().await;
    let t = target();

    // 先热身，把连接建起来——测的是插件实例的并发，不是建链。
    h.op(&t, "shell", r#"{"command":"true","timeout_secs":20}"#)
        .await
        .expect("warm up");

    // 两个 agent 同时调同一个工具。fs_list 里那条命令要跑一秒。
    let args = serde_json::json!({"target": t, "path": "/tmp"}).to_string();
    let slow =
        serde_json::json!({"target": t, "command": "sleep 2", "timeout_secs": 30}).to_string();

    let started = std::time::Instant::now();
    let (a, b) = tokio::join!(h.call_tool("fs_list", &args), h.call_tool("fs_list", &args),);
    a.expect("first call");
    b.expect("second call");
    let both = started.elapsed();

    // 再用一个明确会慢的调用把差别放大：两次各 2 秒，池化的话接近 2 秒。
    let started = std::time::Instant::now();
    let (a, b) = tokio::join!(h.op(&t, "shell", &slow), h.op(&t, "shell", &slow));
    a.expect("first shell");
    b.expect("second shell");
    let elapsed = started.elapsed();

    let fs = h.tools.instance_of("fs").await.expect("fs is loaded");
    let connector = h.fleet.pool(&proxy_connector()).expect("connector pool");
    println!(
        "two fs_list in {both:?}, two 2s shells in {elapsed:?}; \
         fs pool {} → {}, connector pool {} → {}",
        1,
        fs.pool.high_water(),
        1,
        connector.high_water()
    );
    assert!(
        elapsed < std::time::Duration::from_millis(3500),
        "two concurrent 2s calls took {elapsed:?} — they are serialising, not overlapping"
    );
    // 池是**遇到并发才长**的，所以这里同时也是「它真的长过」的证据：
    // 起来时只有一个实例，两路并发之后必须多过一个。
    assert!(
        connector.high_water() > 1,
        "the connector pool never grew past one instance"
    );
    assert!(
        fs.pool.high_water() > 1,
        "the fs tool pool never grew past one instance"
    );
}

#[tokio::test]
#[ignore = "needs real servers"]
async fn two_agents_asking_for_cards_at_once_get_disjoint_sets() {
    // M5 的验收项。上一版**跑不了这条**：host 的 allocate() 先取锁、再在锁里去查
    // nvidia-smi，而那条路会再取一次同一把锁——tokio 的 Mutex 不可重入，
    // 第一次真的要卡就永久挂死。没有测试走过那条路，所以没人发现。
    //
    // 现在插件查、host 挑，host 那一侧一行 I/O 都没有。
    let (h, _) = host().await;
    let t = target();

    let first = serde_json::json!({"target": t, "count": 1, "purpose": "test A"}).to_string();
    let second = serde_json::json!({"target": t, "count": 1, "purpose": "test B"}).to_string();
    let (a, b) = tokio::join!(
        h.call_tool("gpu_acquire", &first),
        h.call_tool("gpu_acquire", &second)
    );

    // 卡可能真的不够（别人在跑东西），那是合法结果——但两边都拿到时必须不重叠。
    let mut claims = Vec::new();
    let mut devices: Vec<u64> = Vec::new();
    for r in [a, b] {
        match r {
            Ok(raw) => {
                let v: serde_json::Value = serde_json::from_str(&raw).expect("json");
                claims.push(v["claim"].as_str().unwrap_or_default().to_string());
                devices.extend(
                    v["devices"]
                        .as_array()
                        .unwrap_or(&vec![])
                        .iter()
                        .filter_map(|d| d.as_u64()),
                );
            }
            Err(e) => {
                // 拿不到也得说清楚谁占着——「失败」两个字对 agent 没有用。
                let msg = e.to_string();
                assert!(msg.contains("free") || msg.contains("Busy"), "{msg}");
            }
        }
    }
    println!("claims {claims:?}, devices {devices:?}");
    let mut seen = devices.clone();
    seen.sort_unstable();
    seen.dedup();
    assert_eq!(
        seen.len(),
        devices.len(),
        "两个请求拿到了同一张卡: {devices:?}"
    );

    for c in claims {
        h.call_tool("gpu_release", &serde_json::json!({"claim": c}).to_string())
            .await
            .expect("release");
    }
}

#[tokio::test]
#[ignore = "needs real servers"]
async fn gpu_status_sees_cards_that_trestle_did_not_hand_out() {
    // 判据是真实世界，不是我们自己的账本：别人绕过 Trestle 直接 ssh 上去占的卡
    // 照样是 busy，只是 held_by 为空。
    let (h, _) = host().await;
    let raw = h
        .call_tool(
            "gpu_status",
            &serde_json::json!({"target": target()}).to_string(),
        )
        .await
        .expect("gpu_status");
    let v: serde_json::Value = serde_json::from_str(&raw).expect("json");
    let gpus = v["gpus"].as_array().expect("gpus");
    assert!(!gpus.is_empty(), "no cards reported: {raw}");
    for g in gpus {
        assert!(g["index"].is_number(), "{g}");
        assert!(g["busy"].is_boolean(), "{g}");
        // 没有 claim 的卡，held_by 必须是 null 而不是编出来的东西。
        if g["claim"].is_null() {
            assert!(g["held_by"].is_null(), "{g}");
        }
    }
    println!("{} cards, {} free", gpus.len(), v["free_count"]);
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
    assert!(
        grouped.get(proxy_connector().as_str()).is_some(),
        "{grouped}"
    );
    assert!(
        grouped
            .get(connector_using("ssh-direct").as_str())
            .is_some(),
        "{grouped}"
    );

    let lab: Vec<String> = h
        .fleet
        .targets()
        .iter()
        .filter(|t| t.connector == proxy_connector())
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
        .filter(|t| t.connector == proxy_connector())
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
        .find(|t| t.connector == proxy_connector())
        .map(|t| t.name.clone())
        .expect("a lab machine");
    let to = registry
        .iter()
        .find(|t| t.connector == connector_using("ssh-direct"))
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
        .filter(|t| t.connector == proxy_connector())
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

#[tokio::test]
async fn one_broken_plugin_does_not_take_the_others_down_with_it() {
    // 一个装不上的插件最常见的成因是它和当前 host 的接口对不上。那种情况下
    // 最糟的处理方式恰恰是「谁都别想启动」——你会连 `plugin list` 都跑不了，
    // 也就无从知道是哪一个坏了。所以它必须被跳过，而且必须**出现在清单里**。
    let repo = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .parent()
        .unwrap()
        .to_path_buf();
    let tmp = std::env::temp_dir().join("trestle-broken-plugin-test");
    let _ = std::fs::remove_dir_all(&tmp);
    std::fs::create_dir_all(tmp.join("tools/wreck")).unwrap();

    // 一个好的：整个目录照搬。
    let good = tmp.join("tools/fs");
    std::fs::create_dir_all(&good).unwrap();
    std::fs::copy(
        repo.join("plugins/tools/fs/manifest.toml"),
        good.join("manifest.toml"),
    )
    .unwrap();
    std::fs::copy(repo.join("plugins/tools/fs/fs.wasm"), good.join("fs.wasm"))
        .expect("run scripts/build-plugins.ps1 first");

    // 一个坏的：有 manifest，没有 .wasm。装不上的一种。
    std::fs::write(
        tmp.join("tools/wreck/manifest.toml"),
        "name = \"wreck\"\nkind = \"tool\"\n",
    )
    .unwrap();

    let root = std::env::var("TRESTLE_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|_| repo.join("config"));
    let store = Arc::new(
        ConfigStore::load(root)
            .expect("config")
            .with_plugins_dir(&tmp),
    );
    let h = TrestleHost::start(store, HostOptions::default())
        .await
        .expect("the host must come up even with a broken plugin in the tree");

    // 好的那个照常工作。
    let names: Vec<String> = h
        .tool_descriptors()
        .await
        .into_iter()
        .map(|d| d.name)
        .collect();
    assert!(names.contains(&"fs_list".to_string()), "{names:?}");

    // 坏的那个出现在清单里，带着原因和下一步——不是「不见了」。
    let inventory = h.tools.inventory().await;
    let wreck = inventory
        .iter()
        .find(|p| p["name"] == "wreck")
        .expect("the broken plugin must still be listed");
    assert_eq!(wreck["ok"], false);
    assert!(
        !wreck["detail"].as_str().unwrap_or("").is_empty(),
        "{wreck}"
    );
    assert!(
        !wreck["remedy"].as_str().unwrap_or("").is_empty(),
        "{wreck}"
    );

    let _ = std::fs::remove_dir_all(&tmp);
}
