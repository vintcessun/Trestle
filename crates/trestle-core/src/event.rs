//! EventBus 上流动的事件。
//!
//! 两个订阅者：`tracing`（落日志）和 WebSocket 广播（给 Monitor 插件 / Web UI）。
//!
//! 每条事件都带 `agent` —— 这是多 agent 协同的基础：任一 agent 的 monitor 都能看到
//! 别人在干什么，从而不互相踩。

use serde::{Deserialize, Serialize};

/// 事件级别。monitor 插件的默认 alert 规则按它做第一层筛选。
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Level {
    Debug,
    Info,
    Warn,
    Error,
}

/// 谁触发的。CLI 与每个 MCP 会话各是一个。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentId(pub String);

impl std::fmt::Display for AgentId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Event {
    /// Unix 毫秒。由 daemon 打，插件不自己取时间。
    pub at_ms: u64,
    pub level: Level,
    pub agent: AgentId,
    #[serde(flatten)]
    pub kind: EventKind,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum EventKind {
    // ── 会话 ──
    AgentConnected {
        label: String,
    },
    AgentDisconnected {
        label: String,
    },

    // ── 连接 ──
    ConnectorEnsureReady {
        connector: String,
        cached: bool,
        took_ms: u64,
    },
    ConnectorFailed {
        connector: String,
        detail: String,
    },
    SessionConnected {
        target: String,
        cold_ms: u64,
        agent_deployed: bool,
    },
    SessionLost {
        target: String,
        detail: String,
    },
    SessionRecovered {
        target: String,
        took_ms: u64,
    },
    /// 懒恢复时接管了一个还活着的远端 agent，而不是重新部署。
    SessionReattached {
        target: String,
        agent_uptime_s: u64,
    },

    // ── 基本操作 ──
    OpStarted {
        target: String,
        op: String,
    },
    OpFinished {
        target: String,
        op: String,
        took_ms: u64,
        ok: bool,
    },
    /// 请求已发出但没拿到响应——**不会**自动重放。
    OpUnknownState {
        target: String,
        op: String,
    },

    // ── 任务 ──
    JobStarted {
        target: String,
        job_id: String,
        pid: u32,
        command: String,
    },
    JobFinished {
        target: String,
        job_id: String,
        exit_code: i32,
        elapsed_s: u64,
    },

    // ── 端口转发（会话级资源）──
    ForwardOpened {
        target: String,
        remote_port: u16,
        local_port: u16,
    },
    ForwardClosed {
        target: String,
        local_port: u16,
        reason: ForwardCloseReason,
    },

    // ── GPU 单点分配 ──
    GpuAllocated {
        target: String,
        devices: Vec<u32>,
        job_id: String,
    },
    GpuReleased {
        target: String,
        devices: Vec<u32>,
    },
    /// 要卡但没要到。带上当前占用情况，让 agent 知道在等什么。
    GpuUnavailable {
        target: String,
        wanted: u32,
        free: u32,
    },

    // ── 插件 ──
    PluginLoaded {
        plugin: String,
        world: String,
    },
    /// capability 拒绝**必须**能被看见，否则权限模型是个黑盒。
    PluginCallDenied {
        plugin: String,
        action: String,
    },
    PluginCallFailed {
        plugin: String,
        tool: String,
        detail: String,
    },

    // ── 工具面 ──
    ToolCalled {
        tool: String,
    },
    ToolFinished {
        tool: String,
        took_ms: u64,
        ok: bool,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ForwardCloseReason {
    /// 开它的那个会话断了——通道随之回收，端口还回池子。
    SessionEnded,
    /// 调用方主动关。
    Requested,
    /// daemon 退出。
    HostShutdown,
    /// 底层连接没了。
    TargetLost,
}

impl Event {
    pub fn new(at_ms: u64, level: Level, agent: AgentId, kind: EventKind) -> Self {
        Self {
            at_ms,
            level,
            agent,
            kind,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn events_serialize_with_a_flat_type_tag() {
        let ev = Event::new(
            1_700_000_000_000,
            Level::Info,
            AgentId("cc-session-1".into()),
            EventKind::SessionConnected {
                target: "gpu-4".into(),
                cold_ms: 812,
                agent_deployed: false,
            },
        );
        let json = serde_json::to_value(&ev).unwrap();
        // Monitor 的 ws 每帧一个 JSON，扁平的 type 字段让客户端过滤最省事。
        assert_eq!(json["type"], "session_connected");
        assert_eq!(json["target"], "gpu-4");
        assert_eq!(json["agent"], "cc-session-1");
    }

    #[test]
    fn levels_are_ordered_so_filters_can_compare() {
        assert!(Level::Error > Level::Warn);
        assert!(Level::Warn > Level::Info);
    }
}
