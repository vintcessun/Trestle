//! 多 agent 协同层：在场感知、会话级资源、留言板。
//!
//! 一个 agent 看不见别的 agent 在干什么，就会撞车——同时抢同一张卡、同时改同一个
//! 目录、同时重启同一个服务。这一层的全部目的就是让「谁在干什么」可见。

use std::collections::BTreeMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use serde::{Deserialize, Serialize};
use tokio::sync::Mutex;

use trestle_core::event::{AgentId, Event, EventKind, ForwardCloseReason, Level};

use crate::events::{EventBus, now_ms};

/// 一个连上来的客户端：一个 MCP 会话，或者一次 CLI 调用。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentSession {
    pub id: String,
    pub label: String,
    pub started_ms: u64,
    /// 最近做了什么，给别的 agent 看。
    pub last_action: String,
    pub last_target: String,
    pub last_ms: u64,
}

/// 一条转发通道的声明。**属于开它的那个会话**。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ForwardRecord {
    pub owner: String,
    pub target: String,
    pub remote_port: u16,
    pub local_port: u16,
    pub opened_ms: u64,
}

/// 一条留言。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Note {
    pub scope: String,
    pub text: String,
    pub author: String,
    pub at_ms: u64,
    /// 到期时刻。**写入时必填**——没有过期时间的留言板会变成一堆没人清的垃圾。
    pub expires_ms: u64,
}

#[derive(Default, Serialize, Deserialize)]
pub struct PersistedRegistry {
    #[serde(default)]
    pub forwards: Vec<ForwardRecord>,
    #[serde(default)]
    pub notes: Vec<Note>,
}

pub struct AgentRegistry {
    bus: EventBus,
    next: AtomicU64,
    sessions: Mutex<BTreeMap<String, AgentSession>>,
    forwards: Mutex<Vec<ForwardRecord>>,
    notes: Mutex<Vec<Note>>,
}

impl AgentRegistry {
    pub fn new(bus: EventBus) -> Self {
        Self {
            bus,
            next: AtomicU64::new(1),
            sessions: Mutex::new(BTreeMap::new()),
            forwards: Mutex::new(Vec::new()),
            notes: Mutex::new(Vec::new()),
        }
    }

    pub async fn connect(&self, label: &str) -> String {
        let id = format!("a{}", self.next.fetch_add(1, Ordering::SeqCst));
        let session = AgentSession {
            id: id.clone(),
            label: label.to_string(),
            started_ms: now_ms(),
            last_action: String::new(),
            last_target: String::new(),
            last_ms: now_ms(),
        };
        self.sessions.lock().await.insert(id.clone(), session);
        self.bus.publish(Event {
            at_ms: now_ms(),
            level: Level::Info,
            agent: AgentId(id.clone()),
            kind: EventKind::AgentConnected {
                label: label.to_string(),
            },
        });
        id
    }

    /// 会话结束：**它开的转发全部关掉、端口还回去**。
    ///
    /// 一条开了一次就没人用的转发不该一直占着，而「没人用」最可靠的判据就是
    /// 「开它的那个会话没了」。
    pub async fn disconnect(&self, id: &str) -> Vec<ForwardRecord> {
        let label = self
            .sessions
            .lock()
            .await
            .remove(id)
            .map(|s| s.label)
            .unwrap_or_default();

        let mut forwards = self.forwards.lock().await;
        let (mine, rest): (Vec<_>, Vec<_>) = forwards.drain(..).partition(|f| f.owner == id);
        *forwards = rest;
        drop(forwards);

        for f in &mine {
            self.bus.publish(Event {
                at_ms: now_ms(),
                level: Level::Info,
                agent: AgentId(id.to_string()),
                kind: EventKind::ForwardClosed {
                    target: f.target.clone(),
                    local_port: f.local_port,
                    reason: ForwardCloseReason::SessionEnded,
                },
            });
        }

        self.bus.publish(Event {
            at_ms: now_ms(),
            level: Level::Info,
            agent: AgentId(id.to_string()),
            kind: EventKind::AgentDisconnected { label },
        });
        mine
    }

    pub async fn touch(&self, id: &str, action: &str, target: &str) {
        if let Some(s) = self.sessions.lock().await.get_mut(id) {
            s.last_action = action.to_string();
            s.last_target = target.to_string();
            s.last_ms = now_ms();
        }
    }

    pub async fn sessions(&self) -> Vec<AgentSession> {
        self.sessions.lock().await.values().cloned().collect()
    }

    pub async fn remember_forward(&self, record: ForwardRecord) {
        self.bus.publish(Event {
            at_ms: now_ms(),
            level: Level::Info,
            agent: AgentId(record.owner.clone()),
            kind: EventKind::ForwardOpened {
                target: record.target.clone(),
                remote_port: record.remote_port,
                local_port: record.local_port,
            },
        });
        self.forwards.lock().await.push(record);
    }

    pub async fn forwards(&self) -> Vec<ForwardRecord> {
        self.forwards.lock().await.clone()
    }

    /// 写一条留言。`ttl_secs` 必填且必须大于 0。
    pub async fn put_note(
        &self,
        scope: &str,
        text: &str,
        author: &str,
        ttl_secs: u64,
    ) -> Result<Note, String> {
        if ttl_secs == 0 {
            return Err(
                "a note needs an expiry; pick roughly how long the thing you are warning \
                 about will last"
                    .into(),
            );
        }
        let note = Note {
            scope: scope.to_string(),
            text: text.to_string(),
            author: author.to_string(),
            at_ms: now_ms(),
            expires_ms: now_ms() + ttl_secs * 1000,
        };
        let mut notes = self.notes.lock().await;
        notes.retain(|n| n.expires_ms > now_ms());
        notes.push(note.clone());
        Ok(note)
    }

    /// 读留言。过期的顺手清掉——不需要单独的清理任务。
    pub async fn notes(&self, scope: Option<&str>) -> Vec<Note> {
        let mut notes = self.notes.lock().await;
        notes.retain(|n| n.expires_ms > now_ms());
        notes
            .iter()
            .filter(|n| scope.is_none_or(|s| n.scope == s || n.scope.starts_with(&format!("{s}:"))))
            .cloned()
            .collect()
    }

    pub async fn snapshot(&self) -> PersistedRegistry {
        PersistedRegistry {
            forwards: self.forwards.lock().await.clone(),
            notes: self
                .notes
                .lock()
                .await
                .iter()
                .filter(|n| n.expires_ms > now_ms())
                .cloned()
                .collect(),
        }
    }

    /// 从落盘状态恢复。
    ///
    /// 转发通道的**声明**恢复了，但通道本身要重建（TCP 连接跨重启必断），
    /// 而且端口会重新分配——调用方本来就没指定过端口，所以这不破坏任何约定。
    pub async fn restore(&self, persisted: PersistedRegistry) {
        *self.notes.lock().await = persisted
            .notes
            .into_iter()
            .filter(|n| n.expires_ms > now_ms())
            .collect();
        // forwards 不直接恢复到活动列表：它们的 owner 会话已经不在了。
        // 谁要用谁重新开——这正是「会话级资源」的意思。
    }
}

pub type SharedRegistry = Arc<AgentRegistry>;

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn a_session_owns_the_forwards_it_opened() {
        let reg = AgentRegistry::new(EventBus::new());
        let a = reg.connect("session-a").await;
        let b = reg.connect("session-b").await;

        reg.remember_forward(ForwardRecord {
            owner: a.clone(),
            target: "gpu-4".into(),
            remote_port: 8080,
            local_port: 41000,
            opened_ms: now_ms(),
        })
        .await;
        reg.remember_forward(ForwardRecord {
            owner: b.clone(),
            target: "gpu-4".into(),
            remote_port: 9090,
            local_port: 41001,
            opened_ms: now_ms(),
        })
        .await;

        // a 走了，只回收 a 的那条。
        let closed = reg.disconnect(&a).await;
        assert_eq!(closed.len(), 1);
        assert_eq!(closed[0].remote_port, 8080);

        let left = reg.forwards().await;
        assert_eq!(left.len(), 1);
        assert_eq!(left[0].owner, b);
    }

    #[tokio::test]
    async fn a_note_without_an_expiry_is_refused() {
        let reg = AgentRegistry::new(EventBus::new());
        // 没有过期时间的留言板会变成一堆没人清的垃圾，所以 TTL 是必填。
        let err = reg
            .put_note("gpu-4", "training", "a1", 0)
            .await
            .unwrap_err();
        assert!(err.contains("expiry"), "{err}");
    }

    #[tokio::test]
    async fn expired_notes_disappear_on_read() {
        let reg = AgentRegistry::new(EventBus::new());
        reg.put_note("gpu-4:/data/exp1", "running an experiment", "a1", 3600)
            .await
            .unwrap();
        assert_eq!(reg.notes(Some("gpu-4")).await.len(), 1);

        // 手动把它过期掉，模拟时间流逝。
        reg.notes.lock().await[0].expires_ms = 1;
        assert!(reg.notes(None).await.is_empty());
    }

    #[tokio::test]
    async fn notes_are_scoped_by_prefix() {
        let reg = AgentRegistry::new(EventBus::new());
        reg.put_note("gpu-4:/data/exp1", "mine", "a1", 3600)
            .await
            .unwrap();
        reg.put_note("gpu-1", "other", "a1", 3600).await.unwrap();
        assert_eq!(reg.notes(Some("gpu-4")).await.len(), 1);
        assert_eq!(reg.notes(None).await.len(), 2);
    }

    #[tokio::test]
    async fn sessions_show_what_each_agent_last_did() {
        let reg = AgentRegistry::new(EventBus::new());
        let a = reg.connect("claude-code-1").await;
        reg.touch(&a, "base_shell", "gpu-4").await;
        let sessions = reg.sessions().await;
        assert_eq!(sessions.len(), 1);
        assert_eq!(sessions[0].last_action, "base_shell");
        assert_eq!(sessions[0].last_target, "gpu-4");
    }
}
