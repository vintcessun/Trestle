//! `trestled`：真正的状态都在这里，MCP 和 CLI 都是它的瘦客户端。
//!
//! 为什么要有 daemon：MCP server 由 Claude Code 按 stdio 拉起，**每个会话一个进程**。
//! 没有 daemon 的话，每开一个会话就要重新建全部连接（gpu-1 经 VPN 冷启动数秒）。
//! 有了它，连接真正只建一次，跨会话、跨 CLI 复用；ws 端点、后台任务、插件实例
//! 也都有唯一归属。
//!
//! **lazy 启动**：客户端连不上就自己把它拉起来，用户永远不需要手动 `trestled start`。
//! **idle 退出**：没人连着也没活儿干，超过一段时间就自己退。

use trestle_daemon::{events, http, ipc, registry, tasks};

use std::sync::Arc;
use std::time::Duration;

use clap::Parser;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

use trestle_core::config::ConfigStore;
use trestle_host::host::{HostOptions, TrestleHost};

use events::{EventBus, PluginEventSink, now_ms};
use ipc::{DaemonInfo, Request, RequestBody, Response};
use registry::AgentRegistry;

#[derive(Parser)]
#[command(name = "trestled", about = "Trestle 的常驻进程", version)]
struct Cli {
    /// 配置与状态所在目录。默认是程序所在目录（可用 TRESTLE_HOME 覆盖）。
    #[arg(long)]
    home: Option<String>,
    /// 前台运行并把日志打到 stderr。
    #[arg(long)]
    foreground: bool,
}

struct Daemon {
    host: Arc<TrestleHost>,
    /// 主动推给所有客户端的东西（目前只有「工具集变了」）。
    notifications: tokio::sync::broadcast::Sender<ipc::Notification>,
    bus: EventBus,
    registry: Arc<AgentRegistry>,
    token: String,
    /// 最近一次有人说话的时间。idle 退出看它。
    last_activity: Arc<std::sync::atomic::AtomicU64>,
    shutdown: tokio::sync::watch::Sender<bool>,
}

#[tokio::main]
async fn main() -> std::process::ExitCode {
    let cli = Cli::parse();
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_env("TRESTLE_LOG")
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .with_writer(std::io::stderr)
        .init();

    match run(cli).await {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("{e}");
            std::process::ExitCode::FAILURE
        }
    }
}

async fn run(cli: Cli) -> trestle_core::Result<()> {
    let root = cli
        .home
        .map(std::path::PathBuf::from)
        .unwrap_or_else(ConfigStore::default_root);

    // 已经有一个在跑就别抢。
    if let Some(existing) = DaemonInfo::read(&root) {
        if tokio::net::TcpStream::connect(("127.0.0.1", existing.port))
            .await
            .is_ok()
        {
            tracing::info!(
                port = existing.port,
                pid = existing.pid,
                "a daemon is already running"
            );
            return Ok(());
        }
        // 陈旧的文件，清掉。
        DaemonInfo::remove(&root);
    }

    let store = Arc::new(ConfigStore::load(&root)?);
    if store.from_example() {
        // 样例里的地址是 RFC 5737 文档保留段，一台都连不上。静默跑下去的话
        // 每一次操作都会超时，而人会去查网络而不是查配置。
        tracing::warn!(
            root = %root.display(),
            "no trestle.toml here — running on trestle.example.toml, whose machines are \
             documentation addresses and will not connect. Copy it to trestle.toml and edit it."
        );
    }
    std::fs::create_dir_all(store.state_dir()).ok();

    let bus = EventBus::new();
    let registry = Arc::new(AgentRegistry::new(bus.clone()));
    let scheduler = Arc::new(tasks::TaskScheduler::new());

    // 先用一个占位的 ws 开口把 host 起起来，等 HTTP 服务真的绑上端口后再换成真的。
    let ws_state_holder: Arc<tokio::sync::OnceCell<http::HttpState>> = Default::default();
    let host = Arc::new(
        TrestleHost::start(
            Arc::clone(&store),
            HostOptions {
                events: Arc::new(PluginEventSink::new(bus.clone())),
                tasks: Arc::clone(&scheduler) as Arc<dyn trestle_host::tool_state::TaskSink>,
                ws: Arc::new(tasks::DeferredWs(Arc::clone(&ws_state_holder))),
                policy: trestle_host::pool::PoolPolicy::default()
                    .with_max(store.config().daemon.pool_max)
                    .with_idle_secs(store.config().daemon.pool_idle_secs),
            },
        )
        .await?,
    );

    // 恢复落盘的协同状态（留言板等）。转发通道要由各自的会话重开——
    // 它们是会话级资源，owner 会话已经不在了。
    if let Some(persisted) = load_persisted(&store) {
        registry.restore(persisted).await;
    }

    // ── HTTP：Monitor ws + 事件流 + Web UI ──
    let http_state = http::HttpState::new(bus.clone(), Arc::clone(&registry), Arc::clone(&host));
    let http_bind = store.config().daemon.http_bind.clone();
    let http_listener = tokio::net::TcpListener::bind(&http_bind)
        .await
        .map_err(|e| trestle_core::TrestleError::Config {
            path: "daemon.http_bind".into(),
            detail: format!("cannot bind {http_bind}: {e}"),
        })?;
    let http_port = http_listener.local_addr().map(|a| a.port()).unwrap_or(0);
    *http_state.base_url.lock().await = format!("http://127.0.0.1:{http_port}");
    let _ = ws_state_holder.set(http_state.clone());

    let app = http::router(http_state.clone());
    tokio::spawn(async move {
        if let Err(e) = axum::serve(http_listener, app).await {
            tracing::error!(%e, "the HTTP service stopped");
        }
    });

    // ── IPC ──
    let (listener, port) = ipc::bind(&store.config().daemon.ipc_bind).await?;
    let token = ipc::new_token();
    DaemonInfo {
        port,
        token: token.clone(),
        pid: std::process::id(),
        version: env!("CARGO_PKG_VERSION").into(),
        started_ms: now_ms(),
    }
    .write(&root)
    .map_err(|e| trestle_core::TrestleError::Config {
        path: root.join(ipc::DAEMON_FILE).display().to_string(),
        detail: format!("cannot write the daemon file: {e}"),
    })?;

    let (shutdown_tx, mut shutdown_rx) = tokio::sync::watch::channel(false);
    let (notifications, _) = tokio::sync::broadcast::channel(16);
    let daemon = Arc::new(Daemon {
        host: Arc::clone(&host),
        notifications,
        bus: bus.clone(),
        registry: Arc::clone(&registry),
        token,
        last_activity: Arc::new(std::sync::atomic::AtomicU64::new(now_ms())),
        shutdown: shutdown_tx,
    });

    scheduler.run(Arc::clone(&host));

    // 实例池的巡检。池在撞上并发时会自己长，长完不会自己缩——一次调用不该
    // 为了归还内存去等一轮回收。所以收回放在这条独立的定时器上，闲够了
    // 一次还一个。
    {
        let host = Arc::clone(&host);
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(Duration::from_secs(
                trestle_host::pool::SWEEP_INTERVAL_SECS,
            ));
            loop {
                tick.tick().await;
                let reclaimed = host.sweep_pools().await;
                if reclaimed > 0 {
                    tracing::debug!(reclaimed, "reclaimed idle plugin instances");
                }
            }
        });
    }

    tracing::info!(
        ipc = port,
        http = http_port,
        ui = %format!("http://127.0.0.1:{http_port}/"),
        "trestled is up"
    );

    // idle 退出。
    let idle = store.config().daemon.idle_timeout_secs;
    if idle > 0 {
        let d = Arc::clone(&daemon);
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(Duration::from_secs(30)).await;
                let quiet_ms = now_ms()
                    .saturating_sub(d.last_activity.load(std::sync::atomic::Ordering::Relaxed));
                let busy = !d.registry.sessions().await.is_empty();
                if !busy && quiet_ms > idle * 1000 {
                    tracing::info!("idle for {}s, exiting", quiet_ms / 1000);
                    let _ = d.shutdown.send(true);
                    break;
                }
            }
        });
    }

    // 接受连接。
    let accept_daemon = Arc::clone(&daemon);
    let mut accept_shutdown = accept_daemon.shutdown.subscribe();
    loop {
        tokio::select! {
            _ = accept_shutdown.changed() => break,
            _ = shutdown_rx.changed() => break,
            _ = tokio::signal::ctrl_c() => break,
            accepted = listener.accept() => {
                let Ok((stream, _)) = accepted else { continue };
                let d = Arc::clone(&accept_daemon);
                tokio::spawn(async move { serve_client(d, stream).await });
            }
        }
    }

    // 收摊：保存状态、清掉 daemon.json。
    save_persisted(&store, registry.snapshot().await);
    DaemonInfo::remove(&root);
    tracing::info!("trestled is down");
    Ok(())
}

async fn serve_client(daemon: Arc<Daemon>, stream: tokio::net::TcpStream) {
    stream.set_nodelay(true).ok();
    let (reader, writer) = stream.into_split();
    let writer = Arc::new(tokio::sync::Mutex::new(writer));
    let mut lines = BufReader::new(reader).lines();
    let mut agent_id: Option<String> = None;

    // 把 daemon 的推送转给这个客户端。id = 0 的帧就是推送。
    let mut notifications = daemon.notifications.subscribe();
    let push_writer = Arc::clone(&writer);
    let pusher = tokio::spawn(async move {
        while let Ok(n) = notifications.recv().await {
            let response = Response::ok(
                ipc::NOTIFICATION_ID,
                serde_json::to_value(&n).unwrap_or_default(),
            );
            let line = serde_json::to_string(&response).unwrap_or_default();
            let mut w = push_writer.lock().await;
            if w.write_all(format!("{line}\n").as_bytes()).await.is_err() {
                break;
            }
            let _ = w.flush().await;
        }
    });

    while let Ok(Some(line)) = lines.next_line().await {
        if line.trim().is_empty() {
            continue;
        }
        let request: Request = match serde_json::from_str(&line) {
            Ok(r) => r,
            Err(e) => {
                let _ = send(&writer, Response::err(0, format!("bad frame: {e}"))).await;
                continue;
            }
        };

        // token 不对就直接断。同机别的进程也能连 127.0.0.1，这道闸不能省。
        if request.token != daemon.token {
            let _ = send(&writer, Response::err(request.id, "authentication failed")).await;
            break;
        }

        daemon
            .last_activity
            .store(now_ms(), std::sync::atomic::Ordering::Relaxed);

        let id = request.id;
        let response = match handle(&daemon, request.body, &mut agent_id).await {
            Ok(v) => Response::ok(id, v),
            Err(e) => Response::err(id, e),
        };
        if send(&writer, response).await.is_err() {
            break;
        }
    }

    pusher.abort();

    // 连接断了 = 这个会话结束了。它开的转发跟着回收。
    if let Some(id) = agent_id {
        daemon.registry.disconnect(&id).await;
    }
}

async fn send(
    writer: &Arc<tokio::sync::Mutex<tokio::net::tcp::OwnedWriteHalf>>,
    response: Response,
) -> std::io::Result<()> {
    let line = serde_json::to_string(&response).unwrap_or_default();
    let mut w = writer.lock().await;
    w.write_all(format!("{line}\n").as_bytes()).await?;
    w.flush().await
}

async fn handle(
    daemon: &Arc<Daemon>,
    body: RequestBody,
    agent_id: &mut Option<String>,
) -> trestle_core::Result<serde_json::Value> {
    match body {
        RequestBody::Hello { label } => {
            let id = daemon.registry.connect(&label).await;
            *agent_id = Some(id.clone());
            Ok(serde_json::json!({"agent": id}))
        }

        RequestBody::Bye { agent } => {
            daemon.registry.disconnect(&agent).await;
            *agent_id = None;
            Ok(serde_json::json!({"bye": true}))
        }

        RequestBody::Op {
            agent,
            target,
            op,
            payload,
        } => {
            daemon.registry.touch(&agent, &op, &target).await;
            let out = daemon.host.op(&target, &op, &payload).await?;
            // forward 是会话级资源：记下它属于谁，会话结束时回收。
            if op == "forward"
                && let Ok(v) = serde_json::from_str::<serde_json::Value>(&out)
            {
                daemon
                    .registry
                    .remember_forward(registry::ForwardRecord {
                        owner: agent.clone(),
                        target: target.clone(),
                        remote_port: serde_json::from_str::<serde_json::Value>(&payload)
                            .ok()
                            .and_then(|p| p["remote_port"].as_u64())
                            .unwrap_or(0) as u16,
                        local_port: v["local_port"].as_u64().unwrap_or(0) as u16,
                        opened_ms: now_ms(),
                    })
                    .await;
            }
            Ok(serde_json::from_str(&out).unwrap_or(serde_json::Value::String(out)))
        }

        RequestBody::CallTool { agent, tool, args } => {
            daemon.registry.touch(&agent, &tool, "").await;
            let out = daemon.host.call_tool(&tool, &args).await?;
            Ok(serde_json::from_str(&out).unwrap_or(serde_json::Value::String(out)))
        }

        RequestBody::ListTools => Ok(serde_json::to_value(daemon.host.tool_descriptors().await)
            .unwrap_or(serde_json::Value::Null)),

        RequestBody::Targets => {
            let mut grouped: std::collections::BTreeMap<String, Vec<serde_json::Value>> =
                Default::default();
            for t in daemon.host.fleet.targets().iter() {
                grouped
                    .entry(t.connector.clone())
                    .or_default()
                    .push(serde_json::json!({
                        "name": t.name, "host": t.host, "port": t.port, "user": t.user,
                        "workdir": t.workdir, "note": t.note, "aliases": t.aliases,
                    }));
            }
            Ok(serde_json::to_value(grouped).unwrap_or_default())
        }

        RequestBody::Agents => Ok(serde_json::json!({
            "sessions": daemon.registry.sessions().await,
            "forwards": daemon.registry.forwards().await,
        })),

        RequestBody::PutNote {
            agent,
            scope,
            text,
            ttl_secs,
        } => daemon
            .registry
            .put_note(&scope, &text, &agent, ttl_secs)
            .await
            .map(|n| serde_json::to_value(n).unwrap_or_default())
            .map_err(|e| trestle_core::TrestleError::Config {
                path: "notes".into(),
                detail: e,
            }),

        RequestBody::Notes { scope } => Ok(serde_json::to_value(
            daemon.registry.notes(scope.as_deref()).await,
        )
        .unwrap_or_default()),

        RequestBody::Plugins => Ok(serde_json::to_value(daemon.host.tools.inventory().await)
            .unwrap_or(serde_json::Value::Null)),

        RequestBody::PluginReload => {
            let count = daemon.host.reload_tools().await?;
            // 推给所有连着的客户端，MCP 前端会转成 tools/list_changed ——
            // 这样 Claude Code 不用重连就能看到新工具。
            let _ = daemon.notifications.send(ipc::Notification::ToolsChanged);
            Ok(serde_json::json!({"plugins": count}))
        }

        RequestBody::Doctor { targets } => {
            let ready = daemon.host.fleet.ensure_ready_all().await;
            let names: Vec<String> = if targets.is_empty() {
                daemon
                    .host
                    .fleet
                    .targets()
                    .iter()
                    .map(|t| t.name.clone())
                    .collect()
            } else {
                targets
            };

            let probe = r#"{"command":"echo ok","timeout_secs":20}"#;
            let results = daemon.host.fleet.op_many(&names, "shell", probe).await;
            let rows: Vec<serde_json::Value> = results
                .into_iter()
                .map(|(name, r)| match r {
                    Ok(_) => serde_json::json!({"target": name, "ok": true}),
                    Err(e) => {
                        serde_json::json!({"target": name, "ok": false, "error": e.to_string()})
                    }
                })
                .collect();

            Ok(serde_json::json!({
                "connectors": ready.into_iter().map(|(name, r)| serde_json::json!({
                    "connector": name,
                    "ok": r.is_ok(),
                    "error": r.err().map(|e| e.to_string()),
                })).collect::<Vec<_>>(),
                "targets": rows,
            }))
        }

        RequestBody::Shutdown => {
            let _ = daemon.shutdown.send(true);
            Ok(serde_json::json!({"shutting_down": true}))
        }
    }
}

fn persisted_path(store: &ConfigStore) -> std::path::PathBuf {
    store.state_dir().join("registry.json")
}

fn load_persisted(store: &ConfigStore) -> Option<registry::PersistedRegistry> {
    let raw = std::fs::read_to_string(persisted_path(store)).ok()?;
    serde_json::from_str(&raw).ok()
}

fn save_persisted(store: &ConfigStore, snapshot: registry::PersistedRegistry) {
    let path = persisted_path(store);
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    // 原子写：崩在半路也不会留下坏掉的 JSON。
    let tmp = path.with_extension("json.tmp");
    if let Ok(raw) = serde_json::to_string_pretty(&snapshot)
        && std::fs::write(&tmp, raw).is_ok()
    {
        let _ = std::fs::rename(&tmp, &path);
    }
}

/// 让编译器别把 `bus` 当成没用的字段——它是 HTTP 那边的事件来源。
#[allow(dead_code)]
fn _bus_is_used(d: &Daemon) -> usize {
    d.bus.subscriber_count()
}
