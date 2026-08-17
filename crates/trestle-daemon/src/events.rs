//! EventBus：所有东西往里发，两个订阅者——`tracing`（落日志）和 WebSocket 广播。
//!
//! 每条事件都带 agent id，这是多 agent 协同的基础：任一 agent 的 monitor 都能看到
//! 别人在干什么，从而不互相踩。

use std::sync::Arc;

use tokio::sync::broadcast;

use trestle_core::event::{AgentId, Event, EventKind, Level};
use trestle_host::state::EventSink;

/// 缓冲多少条。慢的订阅者会丢事件而不是把发送方拖住——
/// Monitor 掉几条日志远好过让一次 `base_shell` 卡住。
const CAPACITY: usize = 1024;

#[derive(Clone)]
pub struct EventBus {
    tx: broadcast::Sender<Arc<Event>>,
}

impl Default for EventBus {
    fn default() -> Self {
        Self::new()
    }
}

impl EventBus {
    pub fn new() -> Self {
        let (tx, _) = broadcast::channel(CAPACITY);
        Self { tx }
    }

    pub fn subscribe(&self) -> broadcast::Receiver<Arc<Event>> {
        self.tx.subscribe()
    }

    pub fn publish(&self, event: Event) {
        log_it(&event);
        // 没有订阅者不是错误——daemon 启动早期本来就没人在听。
        let _ = self.tx.send(Arc::new(event));
    }

    pub fn subscriber_count(&self) -> usize {
        self.tx.receiver_count()
    }
}

fn log_it(e: &Event) {
    let kind = serde_json::to_value(&e.kind)
        .ok()
        .and_then(|v| v["type"].as_str().map(str::to_string))
        .unwrap_or_else(|| "event".into());
    match e.level {
        Level::Error => tracing::error!(agent = %e.agent, kind = %kind, "{:?}", e.kind),
        Level::Warn => tracing::warn!(agent = %e.agent, kind = %kind, "{:?}", e.kind),
        Level::Info => tracing::info!(agent = %e.agent, kind = %kind, "{:?}", e.kind),
        Level::Debug => tracing::debug!(agent = %e.agent, kind = %kind, "{:?}", e.kind),
    }
}

/// 插件发出来的事件的入口。
///
/// 插件用的是 `(level, kind, fields-json)` 这种松散形状——因为插件是可以外部提供的，
/// 不该被 host 的枚举卡死。这里把它包成一条 `PluginEvent` 进总线。
pub struct PluginEventSink {
    bus: EventBus,
}

impl PluginEventSink {
    pub fn new(bus: EventBus) -> Self {
        Self { bus }
    }
}

impl EventSink for PluginEventSink {
    fn emit(&self, plugin: &str, level: &str, kind: &str, fields: &str) {
        let level = match level {
            "error" => Level::Error,
            "warn" => Level::Warn,
            "debug" => Level::Debug,
            _ => Level::Info,
        };
        let fields: serde_json::Value = serde_json::from_str(fields).unwrap_or_default();
        self.bus.publish(Event {
            at_ms: now_ms(),
            level,
            agent: AgentId(plugin.to_string()),
            kind: EventKind::ToolCalled {
                tool: format!("{kind} {fields}"),
            },
        });
    }
}

pub fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn subscribers_see_what_is_published() {
        let bus = EventBus::new();
        let mut rx = bus.subscribe();
        bus.publish(Event {
            at_ms: 1,
            level: Level::Info,
            agent: AgentId("cli".into()),
            kind: EventKind::AgentConnected {
                label: "test".into(),
            },
        });
        let got = rx.recv().await.unwrap();
        assert_eq!(got.agent.0, "cli");
    }

    #[tokio::test]
    async fn publishing_with_nobody_listening_is_fine() {
        // daemon 启动早期本来就没人在听，这不该是错误。
        let bus = EventBus::new();
        bus.publish(Event {
            at_ms: 1,
            level: Level::Info,
            agent: AgentId("cli".into()),
            kind: EventKind::AgentConnected { label: "x".into() },
        });
        assert_eq!(bus.subscriber_count(), 0);
    }
}
