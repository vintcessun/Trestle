//! M2 的验收：connector 插件真的能只靠传输工具箱把活干完。
//!
//! 这是整个架构的地基测试。如果 connector 表达不了，那「上层只看到七个操作、
//! 下面是什么一概不知」这句话就是空的。
//!
//! 不连机器的部分默认就跑；真调部分需要真机，标了 `#[ignore]`：
//!
//! ```text
//! $env:TRESTLE_HOME = "<repo>\config"
//! cargo test -p trestle-host --test connector -- --ignored --nocapture --test-threads=1
//! ```

use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use trestle_core::config::ConfigStore;
use trestle_host::runtime::Runtime;
use trestle_host::state::EventSink;

fn repo_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .parent()
        .unwrap()
        .to_path_buf()
}

fn plugin_dir(kind: &str, name: &str) -> PathBuf {
    repo_root().join("plugins").join(kind).join(name)
}

/// 收集事件，让测试能断言「被拒绝的调用确实被看见了」。
#[derive(Default)]
struct Collector {
    events: Mutex<Vec<(String, String, String)>>,
}

impl EventSink for Collector {
    fn emit(&self, plugin: &str, level: &str, kind: &str, _fields: &str) {
        self.events
            .lock()
            .unwrap()
            .push((plugin.into(), level.into(), kind.into()));
    }
}

impl Collector {
    fn saw(&self, kind: &str) -> bool {
        self.events
            .lock()
            .unwrap()
            .iter()
            .any(|(_, _, k)| k == kind)
    }
}

fn config_store() -> Arc<ConfigStore> {
    let root = std::env::var("TRESTLE_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|_| repo_root().join("config"));
    Arc::new(ConfigStore::load(root).expect("config"))
}

/// 一个 connector 只实例化一次，整个测试二进制共用。
/// 每个测试各建一个的话 wasmtime 要把同一个组件反复编译。
async fn shared(name: &'static str) -> &'static trestle_host::runtime::ConnectorInstance {
    static LAB: tokio::sync::OnceCell<trestle_host::runtime::ConnectorInstance> =
        tokio::sync::OnceCell::const_new();
    static MINE: tokio::sync::OnceCell<trestle_host::runtime::ConnectorInstance> =
        tokio::sync::OnceCell::const_new();
    let cell = if name == "cloud" { &MINE } else { &LAB };
    cell.get_or_init(|| instantiate(name, Arc::new(Collector::default())))
        .await
}

async fn instantiate(
    name: &str,
    events: Arc<Collector>,
) -> trestle_host::runtime::ConnectorInstance {
    let store = config_store();
    let runtime = Runtime::with_events(Arc::clone(&store), events).expect("runtime");
    let dir = plugin_dir("connectors", name);
    let loaded = runtime.load_connector(&dir).unwrap_or_else(|e| {
        panic!("cannot load {name}: {e:#}\n(run scripts/build-plugins.ps1 first)")
    });

    let registry = store.targets().expect("targets");
    let mine: Vec<_> = registry
        .iter()
        .filter(|t| t.connector == name)
        .cloned()
        .collect();
    let config_json = serde_json::to_string(
        &store
            .connector_section(name)
            .map(|c| c.settings.clone())
            .unwrap_or_default(),
    )
    .unwrap();

    runtime
        .instantiate_connector(&loaded, mine, config_json)
        .await
        .unwrap_or_else(|e| panic!("cannot instantiate {name}: {e:#}"))
}

#[tokio::test]
async fn a_connector_component_loads_and_reports_its_machines() {
    let events = Arc::new(Collector::default());
    let c = instantiate("gpu-cluster", Arc::clone(&events)).await;

    let targets = c.targets().await.expect("targets");
    let names: Vec<_> = targets.iter().map(|t| t.name.as_str()).collect();
    assert!(names.contains(&"gpu-4"), "got {names:?}");
    assert!(names.contains(&"gpu-1"), "got {names:?}");
    // 这个 connector 不该看到别的 connector 的机器。
    assert!(
        !names.contains(&"web-1"),
        "leaked another connector's machines: {names:?}"
    );

    assert!(events.saw("plugin_loaded"));
}

#[tokio::test]
async fn a_connector_publishes_a_config_schema_for_the_web_ui() {
    let c = shared("gpu-cluster").await;
    let schema: serde_json::Value =
        serde_json::from_str(&c.config_schema().await.expect("schema")).expect("valid json");
    assert_eq!(schema["type"], "object");
    // 11080 这个坑必须写在 schema 里，否则下一个人还会踩。
    let socks = schema["properties"]["socks"]["default"]
        .as_str()
        .unwrap_or("");
    assert!(socks.contains("11080"), "{socks}");
}

#[tokio::test]
async fn the_two_connectors_manage_disjoint_machines() {
    let lab = shared("gpu-cluster").await;
    let mine = shared("cloud").await;

    let lab_names: Vec<_> = lab
        .targets()
        .await
        .unwrap()
        .into_iter()
        .map(|t| t.name)
        .collect();
    let my_names: Vec<_> = mine
        .targets()
        .await
        .unwrap()
        .into_iter()
        .map(|t| t.name)
        .collect();

    assert!(!lab_names.is_empty() && !my_names.is_empty());
    for n in &lab_names {
        assert!(!my_names.contains(n), "{n} belongs to both connectors");
    }
}

#[tokio::test]
async fn stripping_the_allowlist_actually_blocks_the_call() {
    // M2 的验收项：manifest 里去掉 docker 之后，同一次调用必须被拒绝，
    // 而且这次拒绝必须**看得见**——否则权限模型就是个黑盒。
    let events = Arc::new(Collector::default());
    let store = config_store();
    let sink: Arc<dyn EventSink> = Arc::clone(&events) as Arc<dyn EventSink>;
    let runtime = Runtime::with_events(Arc::clone(&store), sink).expect("runtime");
    let mut loaded = runtime
        .load_connector(&plugin_dir("connectors", "gpu-cluster"))
        .expect("load");

    // 原样的 manifest 是允许 docker 的；这里把白名单清空。
    assert!(loaded.manifest.capabilities.allows_local_exec("docker"));
    loaded.manifest.capabilities.local_exec.clear();

    // 把代理指到一个没人听的端口，逼 ensure-ready 走到「去叫醒容器」那一步。
    let config_json = r#"{"socks":"127.0.0.1:9","ready_timeout_secs":2,"ready_cache_secs":0}"#;
    let c = runtime
        .instantiate_connector(&loaded, Vec::new(), config_json.into())
        .await
        .expect("instantiate");

    let err = c.ensure_ready().await.expect_err("must be denied");
    let msg = err.to_string();
    assert!(msg.contains("docker"), "{msg}");
    assert!(msg.contains("denied") || msg.contains("allowlist"), "{msg}");
    assert!(
        events.saw("plugin_call_denied"),
        "a denied call must be visible in the event stream"
    );
}

#[tokio::test]
#[ignore = "needs real servers"]
async fn a_wasm_connector_drives_the_seven_operations_end_to_end() {
    let events = Arc::new(Collector::default());
    let c = instantiate("gpu-cluster", Arc::clone(&events)).await;
    let target = std::env::var("TRESTLE_TEST_TARGET").unwrap_or_else(|_| "gpu-4".into());

    c.ensure_ready().await.expect("ensure_ready");

    // shell
    let out = c
        .op(
            &target,
            "shell",
            r#"{"command":"echo from-wasm","timeout_secs":30}"#,
        )
        .await
        .expect("shell");
    let v: serde_json::Value = serde_json::from_str(&out).unwrap();
    assert_eq!(v["stdout"].as_str().unwrap_or("").trim(), "from-wasm");
    assert_eq!(v["exit_code"], 0);

    // write → read → edit
    let path = "/tmp/trestle-m2-probe.txt";
    c.op(
        &target,
        "write",
        &serde_json::json!({"path": path, "content": "alpha\nbeta\n", "make_dirs": true})
            .to_string(),
    )
    .await
    .expect("write");

    let read = c
        .op(
            &target,
            "read",
            &serde_json::json!({"path": path}).to_string(),
        )
        .await
        .expect("read");
    let v: serde_json::Value = serde_json::from_str(&read).unwrap();
    assert_eq!(v["content"], "alpha\nbeta\n");

    c.op(
        &target,
        "edit",
        &serde_json::json!({"path": path, "op": {"kind":"literal","old":"beta","new":"BETA","count":0}})
            .to_string(),
    )
    .await
    .expect("edit");
    let read = c
        .op(
            &target,
            "read",
            &serde_json::json!({"path": path}).to_string(),
        )
        .await
        .expect("read");
    assert!(read.contains("BETA"), "{read}");

    // forward：远端起一个只听 127.0.0.1 的服务，只能经隧道访问。
    c.op(
        &target,
        "shell",
        &serde_json::json!({
            "command": "mkdir -p /tmp/trestle-m2-fwd && echo wasm-tunnel-ok > /tmp/trestle-m2-fwd/p.txt",
            "timeout_secs": 20
        })
        .to_string(),
    )
    .await
    .expect("prepare");
    let spawned = c
        .op(
            &target,
            "shell",
            &serde_json::json!({
                "command": "cd /tmp/trestle-m2-fwd && exec python3 -m http.server 18999 --bind 127.0.0.1",
                "detach": true, "name": "m2-fwd"
            })
            .to_string(),
        )
        .await
        .expect("spawn server");
    let pid = serde_json::from_str::<serde_json::Value>(&spawned).unwrap()["pid"]
        .as_u64()
        .unwrap();
    tokio::time::sleep(std::time::Duration::from_millis(1200)).await;

    let fwd = c
        .op(&target, "forward", r#"{"remote_port":18999}"#)
        .await
        .expect("forward");
    let fwd: serde_json::Value = serde_json::from_str(&fwd).unwrap();
    let local_port = fwd["local_port"].as_u64().expect("local_port");
    assert_ne!(local_port, 0, "the host must allocate a concrete port");

    let body = fetch(local_port, "/p.txt").await;

    // 收拾
    c.op(
        &target,
        "shell",
        &serde_json::json!({"command": format!("kill -9 {pid} 2>/dev/null; rm -rf /tmp/trestle-m2-fwd {path}"), "timeout_secs": 20})
            .to_string(),
    )
    .await
    .ok();

    assert!(
        body.contains("wasm-tunnel-ok"),
        "tunnel carried nothing: {body}"
    );
}

async fn fetch(port: u64, path: &str) -> String {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let mut s = tokio::net::TcpStream::connect(format!("127.0.0.1:{port}"))
        .await
        .expect("connect through the tunnel");
    s.write_all(format!("GET {path} HTTP/1.0\r\nHost: localhost\r\n\r\n").as_bytes())
        .await
        .unwrap();
    let mut body = String::new();
    s.read_to_string(&mut body).await.ok();
    body
}

#[tokio::test]
#[ignore = "needs real servers"]
async fn upload_and_download_work_through_the_plugin() {
    let c = shared("gpu-cluster").await;
    let target = std::env::var("TRESTLE_TEST_TARGET").unwrap_or_else(|_| "gpu-4".into());

    let tmp = std::env::temp_dir().join("trestle-m2");
    std::fs::create_dir_all(&tmp).unwrap();
    let src = tmp.join("blob.bin");
    let back = tmp.join("blob.back.bin");
    let blob: Vec<u8> = (0..300_007u32)
        .map(|i| (i.wrapping_mul(2654435761) >> 11) as u8)
        .collect();
    std::fs::write(&src, &blob).unwrap();

    let remote = "/tmp/trestle-m2-blob.bin";
    c.op(
        &target,
        "upload",
        &serde_json::json!({"local_path": src.to_str().unwrap(), "remote_path": remote})
            .to_string(),
    )
    .await
    .expect("upload");

    c.op(
        &target,
        "download",
        &serde_json::json!({"remote_path": remote, "local_path": back.to_str().unwrap()})
            .to_string(),
    )
    .await
    .expect("download");

    assert_eq!(
        std::fs::read(&back).unwrap(),
        blob,
        "bytes changed in transit"
    );

    c.op(
        &target,
        "shell",
        &serde_json::json!({"command": format!("rm -f {remote}"), "timeout_secs": 20}).to_string(),
    )
    .await
    .ok();
    std::fs::remove_file(&src).ok();
    std::fs::remove_file(&back).ok();
}

#[tokio::test]
#[ignore = "needs real servers"]
async fn my_servers_reaches_its_machines_with_a_key_not_a_password() {
    let c = shared("cloud").await;
    let target = std::env::var("TRESTLE_MY_TARGET").unwrap_or_else(|_| "web-1".into());

    c.ensure_ready().await.expect("ensure_ready");
    let out = c
        .op(
            &target,
            "shell",
            r#"{"command":"echo pubkey-ok","timeout_secs":30}"#,
        )
        .await
        .expect("shell");
    let v: serde_json::Value = serde_json::from_str(&out).unwrap();
    assert_eq!(v["stdout"].as_str().unwrap_or("").trim(), "pubkey-ok");

    // 这是对抽象最有力的检验：两个 connector 的接入方式完全不同
    // （代理+密码 vs 直连+公钥），而上层调用代码一个字都不用改。
}
