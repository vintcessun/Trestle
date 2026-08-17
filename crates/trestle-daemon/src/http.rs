//! HTTP 服务：Monitor 的 WebSocket、事件流、Web UI。
//!
//! **和 MCP 完全分开**：rmcp 没有 WebSocket transport，而 Claude Code 的 Monitor
//! 只认两种事件源——本地 shell 命令，或者一个 ws URL。所以 Monitor 必须走这里，
//! 不是走 MCP。
//!
//! ws 端点的生命周期规则（都是刻意的）：
//!   * 开的时候**必须**给超时——没有过期时间的监视端点会悄悄泄漏；
//!   * 到期由 host 主动关，关之前**推最后一帧说明为什么关**；
//!   * daemon 退出 → 所有 ws 关闭。
//!
//! 最后一条尤其重要：区分「任务结束了」和「监视超时了但任务还在跑」。
//! 只是静默 close 的话，这两种情况在 agent 眼里一模一样。

use std::collections::BTreeMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use axum::Router;
use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::{Path, State};
use axum::response::{Html, IntoResponse};
use axum::routing::get;
use serde::{Deserialize, Serialize};
use tokio::sync::Mutex;

use trestle_core::event::Event;
use trestle_core::{Result, TrestleError};
use trestle_host::tool_state::WsHub;

use crate::events::{EventBus, now_ms};

/// 一个 ws 端点的订阅条件。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Filter {
    /// 只看这台机器。
    #[serde(default)]
    pub target: Option<String>,
    /// 只看这个任务。
    #[serde(default)]
    pub job_id: Option<String>,
    /// 命中就压掉，别推给 Monitor。
    #[serde(default)]
    pub quiet: Vec<String>,
    /// 命中就一定推出去。
    ///
    /// 默认值必须**宁宽勿窄**：只匹配成功标志的监视器在崩溃时保持沉默，
    /// 而沉默看起来和「还在跑」一模一样。
    #[serde(default)]
    pub alert: Vec<String>,
}

impl Filter {
    /// 默认 alert 规则：覆盖所有终态。
    pub fn default_alerts() -> Vec<String> {
        [
            "Traceback",
            r"\bFAIL\b",
            r"\bERROR\b",
            "OOM",
            "Killed",
            "CUDA out of memory",
            "AssertionError",
            "Segmentation fault",
        ]
        .into_iter()
        .map(String::from)
        .collect()
    }

    pub fn with_defaults(mut self) -> Self {
        if self.alert.is_empty() {
            self.alert = Self::default_alerts();
        }
        self
    }

    /// 这条事件要不要推出去。
    pub fn accepts(&self, text: &str, target: Option<&str>) -> bool {
        if let (Some(want), Some(got)) = (&self.target, target)
            && want != got
        {
            return false;
        }
        if self.alert.iter().any(|p| contains_like(text, p)) {
            return true;
        }
        if self.quiet.iter().any(|p| contains_like(text, p)) {
            return false;
        }
        true
    }
}

/// 极简的「正则」匹配：只支持 `\b词\b` 与纯子串。
///
/// 不拖一个正则库进来是刻意的——过滤规则是给人写的，复杂正则在这里没有价值，
/// 而一条写错的正则会让整个监视变哑。
fn contains_like(text: &str, pattern: &str) -> bool {
    let cleaned = pattern.replace(r"\b", "");
    if cleaned.is_empty() {
        return false;
    }
    text.to_lowercase().contains(&cleaned.to_lowercase())
}

/// 关闭原因。区分它们是这套设计的重点。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ClosingReason {
    /// 监视到期了，**任务还在跑**。agent 需要重新开一个。
    Timeout,
    /// 任务真的结束了。
    JobFinished,
    /// daemon 退出。
    HostShutdown,
}

struct Endpoint {
    id: String,
    /// 谁开的。关闭事件与 /api 里要认得出来。
    #[allow(dead_code)]
    plugin: String,
    filter: Filter,
    expires_ms: u64,
}

#[derive(Clone)]
pub struct HttpState {
    pub bus: EventBus,
    endpoints: Arc<Mutex<BTreeMap<String, Arc<Endpoint>>>>,
    next: Arc<AtomicU64>,
    pub base_url: Arc<Mutex<String>>,
    pub registry: crate::registry::SharedRegistry,
    pub host: Arc<trestle_host::host::TrestleHost>,
}

impl HttpState {
    pub fn new(
        bus: EventBus,
        registry: crate::registry::SharedRegistry,
        host: Arc<trestle_host::host::TrestleHost>,
    ) -> Self {
        Self {
            bus,
            endpoints: Arc::new(Mutex::new(BTreeMap::new())),
            next: Arc::new(AtomicU64::new(1)),
            base_url: Arc::new(Mutex::new(String::new())),
            registry,
            host,
        }
    }

    /// 开一个 ws 端点。返回可以直接交给 Claude Code Monitor 的 URL。
    pub fn open(&self, plugin: &str, filter: Filter, timeout_secs: u32) -> Result<String> {
        let id = format!("m{}", self.next.fetch_add(1, Ordering::SeqCst));
        let endpoint = Arc::new(Endpoint {
            id: id.clone(),
            plugin: plugin.to_string(),
            filter: filter.with_defaults(),
            expires_ms: now_ms() + timeout_secs as u64 * 1000,
        });
        let endpoints = Arc::clone(&self.endpoints);
        let id2 = id.clone();
        tokio::spawn(async move {
            endpoints.lock().await.insert(id2, endpoint);
        });
        let base = self
            .base_url
            .try_lock()
            .map(|b| b.clone())
            .unwrap_or_default();
        if base.is_empty() {
            return Err(TrestleError::Config {
                path: "daemon.http_bind".into(),
                detail: "the HTTP service has not bound a port yet".into(),
            });
        }
        // **必须是 ws:// 而不是 http://**：Claude Code 的 Monitor 拿 http:// 的 URL
        // 是连不上的，而它失败的样子和「任务很安静」很像，非常难查。
        let ws_base = base
            .strip_prefix("http://")
            .map(|rest| format!("ws://{rest}"))
            .unwrap_or_else(|| base.replace("https://", "wss://"));
        Ok(format!("{ws_base}/monitor/ws/{id}"))
    }
}

/// 给插件用的 ws 开口。
pub struct DaemonWs(pub HttpState);

impl WsHub for DaemonWs {
    fn publish(&self, plugin: &str, filter: &str, timeout_secs: u32) -> Result<String> {
        let filter: Filter = serde_json::from_str(filter).unwrap_or_default();
        self.0.open(plugin, filter, timeout_secs)
    }
}

pub fn router(state: HttpState) -> Router {
    Router::new()
        .route("/monitor/ws/{id}", get(monitor_ws))
        .route("/events", get(events_ws))
        .route("/", get(index))
        .route("/api/targets", get(api_targets))
        .route("/api/tools", get(api_tools))
        .route("/api/agents", get(api_agents))
        .route("/api/config", get(api_config))
        // 插件贡献的面板。Web UI 是插件的一部分，不是独立前端工程。
        .route("/ui/panels", get(api_panels))
        // 面板里调工具用的。
        .route("/api/tool/{name}", axum::routing::post(api_call_tool))
        .with_state(state)
}

async fn monitor_ws(
    ws: WebSocketUpgrade,
    Path(id): Path<String>,
    State(state): State<HttpState>,
) -> impl IntoResponse {
    ws.on_upgrade(move |socket| monitor_loop(socket, id, state))
}

async fn monitor_loop(mut socket: WebSocket, id: String, state: HttpState) {
    let Some(endpoint) = state.endpoints.lock().await.get(&id).cloned() else {
        let _ = socket
            .send(Message::Text(
                serde_json::json!({
                    "type": "closing", "reason": "unknown_endpoint",
                    "detail": format!("monitor {id} does not exist (it may have already expired)")
                })
                .to_string()
                .into(),
            ))
            .await;
        return;
    };

    let mut rx = state.bus.subscribe();
    let deadline = tokio::time::Instant::now()
        + std::time::Duration::from_millis(endpoint.expires_ms.saturating_sub(now_ms()));

    loop {
        tokio::select! {
            _ = tokio::time::sleep_until(deadline) => {
                // 关之前一定要说清楚是「到期」而不是「结束」——
                // 静默 close 的话，agent 分不出任务是完了还是没人盯了。
                let _ = socket.send(Message::Text(serde_json::json!({
                    "type": "closing",
                    "reason": "timeout",
                    "detail": format!(
                        "monitor {} expired; whatever it was watching is still running",
                        endpoint.id
                    ),
                }).to_string().into())).await;
                break;
            }
            got = rx.recv() => {
                match got {
                    Ok(event) => {
                        let (text, target) = render(&event);
                        if !endpoint.filter.accepts(&text, target.as_deref()) {
                            continue;
                        }
                        if socket.send(Message::Text(text.into())).await.is_err() {
                            break; // 对面走了
                        }
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                        let _ = socket.send(Message::Text(serde_json::json!({
                            "type": "lagged", "dropped": n,
                            "detail": "this monitor could not keep up; some events were dropped"
                        }).to_string().into())).await;
                    }
                    Err(_) => {
                        let _ = socket.send(Message::Text(serde_json::json!({
                            "type": "closing", "reason": "host_shutdown"
                        }).to_string().into())).await;
                        break;
                    }
                }
            }
        }
    }

    state.endpoints.lock().await.remove(&endpoint.id);
    // 先推完最后一帧再关。上面每条退出路径都已经推过 closing 帧了，
    // 这里只是把 socket 收掉。
    let _ = socket.send(Message::Close(None)).await;
}

/// Web UI 的实时事件流：不过滤，全推。
async fn events_ws(ws: WebSocketUpgrade, State(state): State<HttpState>) -> impl IntoResponse {
    ws.on_upgrade(move |mut socket| async move {
        let mut rx = state.bus.subscribe();
        while let Ok(event) = rx.recv().await {
            let (text, _) = render(&event);
            if socket.send(Message::Text(text.into())).await.is_err() {
                break;
            }
        }
    })
}

/// 一条事件渲染成一帧文本，外加它属于哪台机器（用于过滤）。
fn render(event: &Event) -> (String, Option<String>) {
    let value = serde_json::to_value(event).unwrap_or_default();
    let target = value
        .get("target")
        .and_then(|t| t.as_str())
        .map(str::to_string);
    (value.to_string(), target)
}

async fn api_targets(State(state): State<HttpState>) -> impl IntoResponse {
    let mut grouped: BTreeMap<String, Vec<serde_json::Value>> = BTreeMap::new();
    for t in state.host.fleet.targets().iter() {
        grouped
            .entry(t.connector.clone())
            .or_default()
            .push(serde_json::json!({
                "name": t.name, "host": t.host, "port": t.port,
                "user": t.user, "workdir": t.workdir, "note": t.note,
            }));
    }
    axum::Json(grouped)
}

async fn api_tools(State(state): State<HttpState>) -> impl IntoResponse {
    axum::Json(state.host.tool_descriptors().await)
}

async fn api_agents(State(state): State<HttpState>) -> impl IntoResponse {
    axum::Json(serde_json::json!({
        "sessions": state.registry.sessions().await,
        "forwards": state.registry.forwards().await,
        "notes": state.registry.notes(None).await,
    }))
}

async fn api_config(State(state): State<HttpState>) -> impl IntoResponse {
    // 配置里可能含敏感字段，这里只给结构不给 secrets —— secrets 在另一个文件里，
    // 本来就没被读进这个视图。
    axum::Json(serde_json::json!({
        "root": state.host.store.root().display().to_string(),
        "connectors": state.host.fleet.connector_names(),
    }))
}

/// 插件贡献的面板，拼在一起交给 UI 外壳。
async fn api_panels(State(state): State<HttpState>) -> impl IntoResponse {
    let panels = state.host.ui_panels().await;
    Html(
        panels
            .into_iter()
            .map(|(name, html)| format!("<!-- panel: {name} -->\n{html}"))
            .collect::<Vec<_>>()
            .join("\n"),
    )
}

/// 面板里调工具。
async fn api_call_tool(
    Path(name): Path<String>,
    State(state): State<HttpState>,
    body: String,
) -> impl IntoResponse {
    let args = if body.trim().is_empty() { "{}" } else { &body };
    match state.host.call_tool(&name, args).await {
        Ok(out) => (
            axum::http::StatusCode::OK,
            [(axum::http::header::CONTENT_TYPE, "application/json")],
            out,
        ),
        Err(e) => (
            axum::http::StatusCode::BAD_REQUEST,
            [(axum::http::header::CONTENT_TYPE, "application/json")],
            serde_json::json!({"error": e.to_string()}).to_string(),
        ),
    }
}

async fn index() -> impl IntoResponse {
    Html(include_str!("webui.html"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_default_alerts_cover_failure_not_just_success() {
        // 只匹配成功标志的监视器在崩溃时保持沉默，而沉默看起来和「还在跑」一模一样。
        let alerts = Filter::default_alerts();
        for needle in ["Traceback", "OOM", "CUDA out of memory", "Killed"] {
            assert!(
                alerts.iter().any(|a| a.contains(needle)),
                "default alerts miss {needle}"
            );
        }
    }

    #[test]
    fn an_alert_beats_a_quiet_rule() {
        let f = Filter {
            quiet: vec!["step".into()],
            alert: vec!["ERROR".into()],
            ..Default::default()
        };
        // 一条既被压制又该告警的行，必须推出去 —— 压制规则不能盖掉故障。
        assert!(f.accepts("step 42 ERROR nan loss", None));
        assert!(!f.accepts("step 42 loss 0.1", None));
    }

    #[test]
    fn a_target_filter_only_sees_its_own_machine() {
        let f = Filter {
            target: Some("gpu-4".into()),
            ..Default::default()
        }
        .with_defaults();
        assert!(f.accepts("anything", Some("gpu-4")));
        assert!(!f.accepts("anything", Some("gpu-1")));
        // 不带 target 的事件（比如 agent 上下线）照样能看到。
        assert!(f.accepts("anything", None));
    }

    #[test]
    fn monitor_urls_use_the_websocket_scheme() {
        // Monitor 拿到 http:// 的 URL 会连不上，而它失败的样子和「任务很安静」
        // 几乎一样——这条断言就是为了让那种情况没法发生。
        let to_ws = |base: &str| {
            base.strip_prefix("http://")
                .map(|rest| format!("ws://{rest}"))
                .unwrap_or_else(|| base.replace("https://", "wss://"))
        };
        assert_eq!(to_ws("http://127.0.0.1:4843"), "ws://127.0.0.1:4843");
        assert_eq!(to_ws("https://example:443"), "wss://example:443");
    }

    #[test]
    fn word_boundary_patterns_still_match() {
        assert!(contains_like("a FAIL happened", r"\bFAIL\b"));
        assert!(contains_like(
            "Traceback (most recent call last)",
            "Traceback"
        ));
        assert!(!contains_like("all good", r"\bFAIL\b"));
    }
}
