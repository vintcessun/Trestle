//! M5/M6 的验收：daemon 真的起得来、协同层真的管用、Monitor 的 ws 真的会说话。
//!
//! 这些测试**真的起一个 trestled 进程**并通过 IPC 驱动它——不 mock，因为要验的
//! 恰恰是「进程起来了没有、端口写对了没有、会话断了会不会回收」这些只有真跑
//! 才会暴露的事。
//!
//! ```text
//! $env:TRESTLE_HOME = "<repo>\config"
//! cargo test -p trestle-daemon --test daemon -- --test-threads=1 --nocapture
//! ```

use std::path::PathBuf;
use std::time::Duration;

use futures::StreamExt;

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

        // 配置优先用你自己的 `trestle.toml`，没有就退到样例。
        //
        // CI 上**一定**没有 `trestle.toml`——它是 gitignore 的机器清单。
        // 之前这里是 `if let Ok(...)`，读不到就悄悄什么都不做，于是临时 home
        // 里一个配置文件都没有，daemon 起来时零台机器，凡是断言「有 connector」
        // 的测试全挂。而 `ConfigStore` 自己的样例兜底救不了：它在 **home 目录**
        // 里找样例，而这里的 home 是临时目录。
        let raw = std::fs::read_to_string(source.join("trestle.toml"))
            .or_else(|_| std::fs::read_to_string(source.join("trestle.example.toml")))
            .unwrap_or_else(|e| {
                panic!(
                    "no trestle.toml or trestle.example.toml in {}: {e}",
                    source.display()
                )
            });
        // 把 idle 超时压短：测试结束后没人管这个 daemon，让它自己走。
        let raw = raw.replace("idle_timeout_secs = 1800", "idle_timeout_secs = 90");
        std::fs::write(dir.join("trestle.toml"), raw).expect("write the test config");
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
    p.pop(); // deps/
    p.pop(); // debug/
    p.join(if cfg!(windows) {
        "trestled.exe"
    } else {
        "trestled"
    })
}

/// 起一个 daemon 并等它就绪。跑完自己收拾。
struct Running {
    child: std::process::Child,
    home: PathBuf,
}

impl Running {
    async fn start() -> Self {
        let home = home();
        // 上一次跑剩下的先清掉。
        let _ = std::fs::remove_file(home.join("daemon.json"));

        // clippy 会提醒「spawn 之后不是每条路径都 wait」——这里是有意的：
        // 这个 daemon 的收场靠的是 idle 超时（见 Running 的 Drop 上那段注释），
        // 不是靠某条路径去 wait 它。
        #[allow(clippy::zombie_processes)]
        let child = std::process::Command::new(daemon_exe())
            .arg("--home")
            .arg(&home)
            .env("TRESTLE_PLUGINS", repo_root().join("plugins"))
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .unwrap_or_else(|e| panic!("cannot start {}: {e}", daemon_exe().display()));

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

    fn info(&self) -> DaemonInfo {
        DaemonInfo::read(&self.home).expect("daemon.json")
    }

    async fn client(&self, label: &str) -> (IpcClient, String) {
        let client = IpcClient::connect(&self.home).await.expect("connect");
        let hello = client
            .call(RequestBody::Hello {
                label: label.into(),
            })
            .await
            .expect("hello");
        let agent = hello["agent"].as_str().unwrap().to_string();
        (client, agent)
    }
}

/// ⚠️ **这个 Drop 实际上不会跑。**
///
/// `Running` 存在一个 `static OnceCell` 里，而 Rust 在进程退出时不 drop static。
/// 真正让测试起的 daemon 退场的是上面把 `idle_timeout_secs` 改成 90 秒那一手——
/// 测试结束后没人连着，它自己就走了。
///
/// 留着这个实现是为了万一有人改成非 static 的用法；但别指望它。
impl Drop for Running {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

#[tokio::test]
async fn the_daemon_serves_the_whole_tool_surface() {
    let d = daemon().await;
    let (client, _agent) = d.client("test").await;

    let tools = client.call(RequestBody::ListTools).await.expect("tools");
    let names: Vec<String> = tools
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|t| t["name"].as_str().map(str::to_string))
        .collect();

    // 七个基本操作 + 五个插件贡献的一批。
    for expected in [
        "base_shell",
        "base_forward",
        "job_start",
        "fs_list",
        "fleet_status",
        "monitor_open",
        "xfer_between",
    ] {
        assert!(
            names.contains(&expected.to_string()),
            "missing {expected}: {names:?}"
        );
    }
    assert!(names.len() >= 20, "only {} tools: {names:?}", names.len());
}

#[tokio::test]
async fn a_wrong_token_gets_the_door_slammed() {
    let d = daemon().await;
    let info = d.info();

    // 同机别的进程也能连 127.0.0.1，所以 token 这道闸不能是装饰。
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
    let stream = tokio::net::TcpStream::connect(("127.0.0.1", info.port))
        .await
        .expect("connect");
    let (r, mut w) = stream.into_split();
    let mut lines = BufReader::new(r).lines();

    let bogus = serde_json::json!({
        "id": 1, "token": "not-the-token", "method": "list_tools"
    });
    w.write_all(format!("{bogus}\n").as_bytes()).await.unwrap();

    let reply = lines.next_line().await.unwrap().expect("a reply");
    let reply: serde_json::Value = serde_json::from_str(&reply).unwrap();
    assert_eq!(reply["ok"], false);
    assert!(reply["error"].as_str().unwrap().contains("authentication"));

    // 而且连接会被断掉，不给第二次机会。
    let again = lines.next_line().await.unwrap();
    assert!(
        again.is_none(),
        "the daemon kept the connection open after a bad token"
    );
}

#[tokio::test]
async fn notes_need_an_expiry_and_are_visible_to_everyone() {
    let d = daemon().await;
    let (a, agent_a) = d.client("agent-a").await;
    let (b, _agent_b) = d.client("agent-b").await;

    // 没有 TTL 的留言板会变成一堆没人清的垃圾。
    let err = a
        .call(RequestBody::PutNote {
            agent: agent_a.clone(),
            scope: "demo-host".into(),
            text: "forever".into(),
            ttl_secs: 0,
        })
        .await
        .unwrap_err();
    assert!(err.to_string().contains("expiry"), "{err}");

    a.call(RequestBody::PutNote {
        agent: agent_a.clone(),
        scope: "demo-host:/data/exp1".into(),
        text: "running latent-v3, please leave this alone".into(),
        ttl_secs: 600,
    })
    .await
    .expect("put note");

    // 另一个 agent 必须看得到 —— 这就是留言板存在的全部意义。
    let notes = b
        .call(RequestBody::Notes {
            scope: Some("demo-host".into()),
        })
        .await
        .expect("notes");
    let notes = notes.as_array().unwrap();
    assert_eq!(notes.len(), 1, "{notes:?}");
    assert_eq!(notes[0]["author"], agent_a.as_str());
    assert!(notes[0]["text"].as_str().unwrap().contains("latent-v3"));
}

#[tokio::test]
async fn agents_can_see_what_the_others_are_doing() {
    let d = daemon().await;
    let (a, agent_a) = d.client("claude-code:paper").await;
    let (b, _) = d.client("cli").await;

    // a 做点事。
    a.call(RequestBody::Op {
        agent: agent_a.clone(),
        target: "demo-host".into(),
        op: "shell".into(),
        payload: r#"{"command":"true","timeout_secs":20}"#.into(),
    })
    .await
    .ok(); // 连不上机器也没关系，touch 已经记下了

    let view = b.call(RequestBody::Agents).await.expect("agents");
    let sessions = view["sessions"].as_array().unwrap();
    assert!(sessions.len() >= 2, "{sessions:?}");

    let a_row = sessions
        .iter()
        .find(|s| s["id"] == agent_a.as_str())
        .expect("agent a is listed");
    assert_eq!(a_row["label"], "claude-code:paper");
    assert_eq!(a_row["last_action"], "shell");
    assert_eq!(a_row["last_target"], "demo-host");
}

#[tokio::test]
async fn a_monitor_endpoint_says_why_it_closed() {
    let d = daemon().await;
    let (client, _agent) = d.client("test").await;

    // 三秒的监视，好让测试等得起。
    let opened = client
        .call(RequestBody::CallTool {
            agent: "test".into(),
            tool: "monitor_open".into(),
            args: r#"{"timeout_secs":3}"#.into(),
        })
        .await
        .expect("monitor_open");

    let url = opened["ws_url"].as_str().expect("ws_url");
    // Monitor 拿 http:// 是连不上的，而它失败的样子和「任务很安静」几乎一样。
    assert!(url.starts_with("ws://"), "not a websocket URL: {url}");

    let (mut socket, _) = tokio_tungstenite::connect_async(url)
        .await
        .unwrap_or_else(|e| panic!("cannot connect to {url}: {e}"));

    // 等到期。到期时必须**先推一帧说明原因**再关。
    let deadline = tokio::time::Instant::now() + Duration::from_secs(15);
    let mut closing: Option<serde_json::Value> = None;
    while tokio::time::Instant::now() < deadline {
        let Ok(Some(Ok(msg))) = tokio::time::timeout(Duration::from_secs(10), socket.next()).await
        else {
            break;
        };
        if let tokio_tungstenite::tungstenite::Message::Text(text) = msg
            && let Ok(v) = serde_json::from_str::<serde_json::Value>(&text)
            && v["type"] == "closing"
        {
            closing = Some(v);
            break;
        }
    }

    let closing = closing.expect("no closing frame arrived before the socket went away");
    // 区分「到期了但任务还在跑」和「任务真的结束了」是这套设计的重点：
    // 静默 close 的话这两种情况在 agent 眼里一模一样。
    assert_eq!(closing["reason"], "timeout", "{closing}");
    assert!(
        closing["detail"]
            .as_str()
            .unwrap()
            .contains("still running"),
        "the closing frame must say the work continues: {closing}"
    );
    drop(socket);
}

#[tokio::test]
async fn the_web_ui_and_its_api_answer() {
    let d = daemon().await;
    let (client, _) = d.client("test").await;
    // HTTP 端口不在 daemon.json 里（它是给 IPC 用的），从 monitor URL 里取。
    let opened = client
        .call(RequestBody::CallTool {
            agent: "test".into(),
            tool: "monitor_open".into(),
            args: r#"{"timeout_secs":30}"#.into(),
        })
        .await
        .expect("monitor_open");
    let url = opened["ws_url"].as_str().unwrap();
    let port: u16 = url
        .trim_start_matches("ws://127.0.0.1:")
        .split('/')
        .next()
        .unwrap()
        .parse()
        .unwrap();

    let body = http_get(port, "/").await;
    assert!(
        body.contains("<title>Trestle</title>"),
        "the UI shell did not render"
    );

    // 断言的是**形状**不是名字：connector 叫什么由用户的配置决定，写死在这里
    // 等于把某个人的机队名字印进一个公开仓库。
    let targets = http_get(port, "/api/targets").await;
    let grouped = json_body(&targets);
    let groups = grouped.as_object().expect("grouped by connector");
    assert!(!groups.is_empty(), "{targets}");
    for (connector, machines) in groups {
        assert!(!connector.is_empty());
        assert!(
            machines.as_array().is_some_and(|m| !m.is_empty()),
            "{connector} has no machines: {targets}"
        );
    }

    let tools = http_get(port, "/api/tools").await;
    assert!(
        tools.contains("base_shell"),
        "the API did not list the base tools"
    );
}

/// 极简 HTTP GET，避免为两个断言拖进一个 HTTP 客户端。
/// 从一个带 HTTP 头的响应里取出 JSON。
///
/// 头和 body 之间隔一个空行，但用它切会踩到 body 自己的空行；这里直接从
/// 第一个 `{` 起，对我们这几个只返回对象的端点足够，也不需要处理换行风格。
fn json_body(response: &str) -> serde_json::Value {
    let start = response
        .find('{')
        .unwrap_or_else(|| panic!("no JSON in the response: {response}"));
    serde_json::from_str(&response[start..]).expect("json")
}

async fn http_get(port: u16, path: &str) -> String {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let mut s = tokio::net::TcpStream::connect(("127.0.0.1", port))
        .await
        .expect("connect to the http service");
    s.write_all(format!("GET {path} HTTP/1.0\r\nHost: localhost\r\n\r\n").as_bytes())
        .await
        .unwrap();
    let mut raw = String::new();
    s.read_to_string(&mut raw).await.ok();
    // 整个响应原样返回，包括 HTTP 头。要当 JSON 解析的调用方用 [`json_body`]。
    raw
}

#[tokio::test]
#[ignore = "needs real servers"]
async fn a_forward_is_reclaimed_when_its_session_ends() {
    let d = daemon().await;
    let (owner, owner_id) = d.client("owner").await;
    let (observer, _) = d.client("observer").await;

    // 在 gpu-4 上起一个只听本机的服务，然后把它映射过来。
    owner
        .call(RequestBody::Op {
            agent: owner_id.clone(),
            target: "demo-host".into(),
            op: "shell".into(),
            payload: serde_json::json!({
                "command": "mkdir -p /tmp/trestle-fwd && cd /tmp/trestle-fwd && \
                            echo owned > p.txt && \
                            (setsid python3 -m http.server 18777 --bind 127.0.0.1 >/dev/null 2>&1 &) ; sleep 1",
                "timeout_secs": 30
            })
            .to_string(),
        })
        .await
        .expect("start a service");

    let fwd = owner
        .call(RequestBody::Op {
            agent: owner_id.clone(),
            target: "demo-host".into(),
            op: "forward".into(),
            payload: r#"{"remote_port":18777}"#.into(),
        })
        .await
        .expect("forward");
    let local_port = fwd["local_port"].as_u64().expect("local_port") as u16;
    assert_ne!(local_port, 0, "the host must allocate a concrete port");

    // 另一个 agent 看得到这条通道属于谁。
    let view = observer.call(RequestBody::Agents).await.expect("agents");
    let forwards = view["forwards"].as_array().unwrap();
    assert!(
        forwards.iter().any(|f| f["owner"] == owner_id.as_str()),
        "{forwards:?}"
    );

    // 通道确实通。
    let body = http_get(local_port, "/p.txt").await;
    assert!(body.contains("owned"), "the tunnel carried nothing: {body}");

    // owner 走了 —— 它开的转发必须跟着回收。
    owner
        .call(RequestBody::Bye {
            agent: owner_id.clone(),
        })
        .await
        .ok();
    drop(owner);
    tokio::time::sleep(Duration::from_millis(800)).await;

    let view = observer.call(RequestBody::Agents).await.expect("agents");
    let forwards = view["forwards"].as_array().unwrap();
    assert!(
        !forwards.iter().any(|f| f["owner"] == owner_id.as_str()),
        "a forward outlived the session that opened it: {forwards:?}"
    );

    observer
        .call(RequestBody::Op {
            agent: "cleanup".into(),
            target: "demo-host".into(),
            op: "shell".into(),
            payload: serde_json::json!({
                "command": "pkill -f 'http.server 18777'; rm -rf /tmp/trestle-fwd",
                "timeout_secs": 20
            })
            .to_string(),
        })
        .await
        .ok();
}

#[tokio::test]
#[ignore = "needs real servers"]
async fn the_daemon_routes_operations_to_the_right_connector() {
    let d = daemon().await;
    let (client, agent) = d.client("test").await;

    // 两台归属完全不同 connector 的机器，调用方式一模一样。
    for target in ["demo-host", "web-1"] {
        let out = client
            .call(RequestBody::Op {
                agent: agent.clone(),
                target: target.into(),
                op: "shell".into(),
                payload: r#"{"command":"echo routed","timeout_secs":30}"#.into(),
            })
            .await
            .unwrap_or_else(|e| panic!("{target}: {e}"));
        assert_eq!(
            out["stdout"].as_str().unwrap_or("").trim(),
            "routed",
            "{target}"
        );
    }
}
