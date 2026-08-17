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

/// 按**驱动**找 connector，而不是按名字。
///
/// 名字是用户给自己那组机器起的——写死在这里等于把某个人的机队名字印进一个
/// 公开仓库。驱动名（`ssh-socks5` / `ssh-direct`）是这个项目自己的东西，
/// 而且它才是测试真正关心的：一个走代理，一个直连。
fn connector_using(driver: &str) -> String {
    let store = config_store();
    store
        .config()
        .connectors
        .iter()
        .find(|(_, c)| c.plugin == driver && c.enabled)
        .map(|(name, _)| name.clone())
        .unwrap_or_else(|| panic!("no enabled connector uses the {driver} driver"))
}

/// 这个 connector 管的第一台机器。真调测试拿它当靶子。
fn first_target_of(connector: &str) -> String {
    let store = config_store();
    let registry = store.targets().expect("targets");
    registry
        .iter()
        .find(|t| t.connector == connector)
        .map(|t| t.name.clone())
        .unwrap_or_else(|| panic!("{connector} manages no machines"))
}

/// 一个 connector 只实例化一次，整个测试二进制共用。
/// 每个测试各建一个的话 wasmtime 要把同一个组件反复编译。
async fn shared(driver: &'static str) -> &'static trestle_host::runtime::ConnectorInstance {
    static PROXY: tokio::sync::OnceCell<trestle_host::runtime::ConnectorInstance> =
        tokio::sync::OnceCell::const_new();
    static DIRECT: tokio::sync::OnceCell<trestle_host::runtime::ConnectorInstance> =
        tokio::sync::OnceCell::const_new();
    let cell = if driver == "ssh-direct" { &DIRECT } else { &PROXY };
    cell.get_or_init(|| async {
        instantiate(&connector_using(driver), Arc::new(Collector::default())).await
    })
    .await
}

/// 按**配置里的 connector 名**实例化——驱动是哪个由配置说了算。
///
/// 这个间接层就是 `[connectors.gpu-cluster] plugin = "ssh-socks5"` 那一行：
/// 驱动是通用的，「gpu-cluster」是它的一个实例。
async fn instantiate(
    name: &str,
    events: Arc<Collector>,
) -> trestle_host::runtime::ConnectorInstance {
    let store = config_store();
    let runtime = Runtime::with_events(Arc::clone(&store), events).expect("runtime");
    let section = store
        .connector_section(name)
        .unwrap_or_else(|e| panic!("no [connectors.{name}] in the config: {e}"));
    let dir = plugin_dir("connectors", &section.plugin);
    let mut loaded = runtime.load_connector(&dir).unwrap_or_else(|e| {
        panic!(
            "cannot load {} (driver of {name}): {e:#}\n(run scripts/build-plugins.ps1 first)",
            section.plugin
        )
    });
    // host 做的同一件事：配置里的授权并进 manifest 的白名单。
    for prog in &section.allow_exec {
        loaded.manifest.capabilities.local_exec.push(prog.clone());
    }

    let registry = store.targets().expect("targets");
    let mine: Vec<_> = registry
        .iter()
        .filter(|t| t.connector == name)
        .cloned()
        .collect();
    let config_json = serde_json::to_string(&section.settings).unwrap();

    runtime
        .instantiate_connector(&loaded, mine, config_json)
        .await
        .unwrap_or_else(|e| panic!("cannot instantiate {name}: {e:#}"))
}

/// 一个 ready 配置：代理指向一个没人听的端口，逼 `ensure-ready` 走到
/// 「去把它拉起来」那一步。
fn config_that_forces_a_start() -> String {
    serde_json::json!({
        "socks": "127.0.0.1:9",
        "ready": {
            "check": ["docker", "ps", "-a", "--filter", "name=^nope$", "--format", "{{.Names}}"],
            "check_expect": "nope",
            "start": ["docker", "start", "nope"],
            "timeout_secs": 1,
            "cache_secs": 0
        }
    })
    .to_string()
}

#[tokio::test]
async fn a_connector_component_loads_and_reports_its_machines() {
    let events = Arc::new(Collector::default());
    let c = instantiate(&connector_using("ssh-socks5"), Arc::clone(&events)).await;

    let targets = c.targets().await.expect("targets");
    let names: Vec<_> = targets.iter().map(|t| t.name.as_str()).collect();
    // 报的机器必须正好是配置里归它管的那些——名字是什么无所谓，
    // 「不多不少」才是这条要守的东西。
    let store = config_store();
    let mine = connector_using("ssh-socks5");
    let expected: Vec<String> = store
        .targets()
        .expect("targets")
        .iter()
        .filter(|t| t.connector == mine)
        .map(|t| t.name.clone())
        .collect();
    assert!(!expected.is_empty());
    for want in &expected {
        assert!(names.contains(&want.as_str()), "missing {want}: {names:?}");
    }
    // 这个 connector 不该看到别的 connector 的机器。
    let other = first_target_of(&connector_using("ssh-direct"));
    assert!(
        !names.contains(&other.as_str()),
        "leaked another connector's machines: {names:?}"
    );

    assert!(events.saw("plugin_loaded"));
}

#[tokio::test]
async fn a_connector_publishes_a_config_schema_for_the_web_ui() {
    let c = shared("ssh-socks5").await;
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
    let lab = shared("ssh-socks5").await;
    let mine = shared("ssh-direct").await;

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
async fn the_config_is_what_carries_the_start_command() {
    // 拉起代理的那条命令以前写死在驱动里。现在它在配置里，所以这条断言守着
    // 那根线还接着——驱动通用了，但这一组机器仍然知道自己该怎么被叫醒。
    //
    // 断言的是**形状**不是那个容器叫什么：容器名是你的，这里跑的可能是你的
    // trestle.toml，也可能是 clone 下来只有样例。名字对不对由下面那条
    // 「check 和 start 说的是同一个东西」来管。
    let store = config_store();
    let lab = store.connector_section(&connector_using("ssh-socks5")).expect("section");
    assert_eq!(lab.plugin, "ssh-socks5");
    assert!(
        lab.allow_exec.iter().any(|p| p == "docker"),
        "allow_exec = {:?}",
        lab.allow_exec
    );

    let ready = &lab.settings["ready"];
    let start: Vec<&str> = ready["start"]
        .as_array()
        .expect("ready.start")
        .iter()
        .map(|v| v.as_str().unwrap())
        .collect();
    assert_eq!(&start[..2], ["docker", "start"], "{start:?}");
    // check 找的和 start 拉的必须是同一个东西，否则「它在不在」这一步白做。
    let container = start[2];
    let expect = ready["check_expect"].as_str().unwrap_or("");
    assert_eq!(expect, container, "check_expect and start disagree");
    let check = ready["check"].as_array().expect("ready.check");
    assert!(
        check.iter().any(|v| v.as_str().unwrap_or("").contains(container)),
        "check does not mention {container}: {check:?}"
    );
    // 「不存在时怎么办」必须给出创建命令——报错却不说下一步，等于没报。
    let remedy = ready["missing_remedy"].as_str().unwrap_or("");
    assert!(remedy.contains("docker run"), "{remedy}");

    // 直连那一组没有前置条件，所以也不该有任何本机命令的授权。
    let mine = store.connector_section(&connector_using("ssh-direct")).expect("section");
    assert_eq!(mine.plugin, "ssh-direct");
    assert!(mine.allow_exec.is_empty(), "{:?}", mine.allow_exec);
    assert!(mine.settings.get("ready").is_none());
}

#[tokio::test]
async fn a_generic_driver_ships_with_no_local_commands_allowed() {
    // 通用驱动不知道你的代理是 docker 起的还是别的，所以它没资格替你声明
    // 「我需要跑 docker」。manifest 里那份白名单必须是空的。
    let store = config_store();
    let runtime = Runtime::new(Arc::clone(&store)).expect("runtime");
    for driver in ["ssh-socks5", "ssh-direct"] {
        let loaded = runtime
            .load_connector(&plugin_dir("connectors", driver))
            .unwrap_or_else(|e| panic!("cannot load {driver}: {e:#}"));
        assert!(
            loaded.manifest.capabilities.local_exec.is_empty(),
            "{driver} self-granted {:?} in its manifest",
            loaded.manifest.capabilities.local_exec
        );
    }
}

#[tokio::test]
async fn without_allow_exec_the_start_command_is_blocked_and_says_so() {
    // 权限模型的验收项：没在配置里授权就跑不了本机命令，而且这次拒绝必须
    // **看得见**——否则出问题时没人知道是被挡了还是根本没调。
    let events = Arc::new(Collector::default());
    let store = config_store();
    let sink: Arc<dyn EventSink> = Arc::clone(&events) as Arc<dyn EventSink>;
    let runtime = Runtime::with_events(Arc::clone(&store), sink).expect("runtime");
    // 原样加载 = 没有任何 allow_exec。
    let loaded = runtime
        .load_connector(&plugin_dir("connectors", "ssh-socks5"))
        .expect("load");

    let c = runtime
        .instantiate_connector(&loaded, Vec::new(), config_that_forces_a_start())
        .await
        .expect("instantiate");

    let err = c.ensure_ready().await.expect_err("must be denied");
    let msg = err.to_string();
    assert!(msg.contains("docker"), "{msg}");
    // 错误必须指向**配置**，不是指向 docker——被挡住和命令失败是两件事。
    assert!(msg.contains("allow_exec"), "{msg}");
    assert!(
        events.saw("plugin_call_denied"),
        "a denied call must be visible in the event stream"
    );
}

#[tokio::test]
async fn allow_exec_in_the_config_is_what_grants_it() {
    // 另一个方向：配置里授权之后，同一次调用不再是「被拒绝」。
    // 它多半还是会失败（这里那个容器根本不存在），但失败的**原因**不同了——
    // 这正是这两条测试要区分的东西。
    let store = config_store();
    let runtime = Runtime::new(Arc::clone(&store)).expect("runtime");
    let mut loaded = runtime
        .load_connector(&plugin_dir("connectors", "ssh-socks5"))
        .expect("load");
    loaded
        .manifest
        .capabilities
        .local_exec
        .push("docker".into());

    let c = runtime
        .instantiate_connector(&loaded, Vec::new(), config_that_forces_a_start())
        .await
        .expect("instantiate");

    let err = c.ensure_ready().await.expect_err("that container is fake");
    let msg = err.to_string();
    assert!(
        !msg.contains("allow_exec"),
        "still being denied after the config granted it: {msg}"
    );
}

#[tokio::test]
#[ignore = "needs real servers"]
async fn a_wasm_connector_drives_the_seven_operations_end_to_end() {
    let events = Arc::new(Collector::default());
    let c = instantiate(&connector_using("ssh-socks5"), Arc::clone(&events)).await;
    let target = std::env::var("TRESTLE_TEST_TARGET")
        .unwrap_or_else(|_| first_target_of(&connector_using("ssh-socks5")));

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
    let c = shared("ssh-socks5").await;
    let target = std::env::var("TRESTLE_TEST_TARGET")
        .unwrap_or_else(|_| first_target_of(&connector_using("ssh-socks5")));

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
    let c = shared("ssh-direct").await;
    let target = std::env::var("TRESTLE_MY_TARGET")
        .unwrap_or_else(|_| first_target_of(&connector_using("ssh-direct")));

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
