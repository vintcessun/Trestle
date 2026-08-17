//! 周期任务：抢卡、等文件、等任务，都靠它。
//!
//! 插件注册一个 tick，host 按周期回调它的 `on-tick`。插件在回调里自己判断
//! 「够不够条件」，够了就动手然后 `cancel`。
//!
//! 为什么不是在 host 里写一个 poll+predicate 的小语言：那样每加一种等待条件
//! 就要动 host。回调进插件之后，「等什么、怎么算够」全是插件自己的事。

use std::collections::BTreeMap;
use std::sync::Arc;

use tokio::sync::Mutex;

use trestle_host::host::TrestleHost;
use trestle_host::tool_state::{TaskSink, WsHub};

#[derive(Debug, Clone)]
pub struct ScheduledTask {
    pub plugin: String,
    pub name: String,
    pub interval_ms: u32,
    pub payload: String,
    pub next_ms: u64,
}

#[derive(Default)]
pub struct TaskScheduler {
    tasks: Mutex<BTreeMap<String, ScheduledTask>>,
}

fn key(plugin: &str, name: &str) -> String {
    format!("{plugin}/{name}")
}

impl TaskScheduler {
    pub fn new() -> Self {
        Self::default()
    }

    /// 起定时循环。
    pub fn run(self: &Arc<Self>, host: Arc<TrestleHost>) {
        let me = Arc::clone(self);
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(std::time::Duration::from_millis(500)).await;
                let now = crate::events::now_ms();

                let due: Vec<ScheduledTask> = {
                    let mut tasks = me.tasks.lock().await;
                    let due: Vec<_> = tasks
                        .values()
                        .filter(|t| t.next_ms <= now)
                        .cloned()
                        .collect();
                    for t in &due {
                        if let Some(entry) = tasks.get_mut(&key(&t.plugin, &t.name)) {
                            entry.next_ms = now + entry.interval_ms as u64;
                        }
                    }
                    due
                };

                for t in due {
                    let Some(loaded) = host.tools.instance_of(&t.plugin).await else {
                        continue;
                    };
                    // 一个插件的 tick 跑飞了不该拖住别的。
                    let name = t.name.clone();
                    let payload = t.payload.clone();
                    tokio::spawn(async move {
                        if let Err(e) = loaded.instance.on_tick(&name, &payload).await {
                            tracing::warn!(task = %name, %e, "a scheduled task failed");
                        }
                    });
                }
            }
        });
    }

    pub async fn list(&self) -> Vec<ScheduledTask> {
        self.tasks.lock().await.values().cloned().collect()
    }
}

impl TaskSink for TaskScheduler {
    fn schedule(&self, plugin: &str, name: &str, interval_ms: u32, payload: &str) {
        // 太短的周期只会把远端问烂。1 秒是下限。
        let interval_ms = interval_ms.max(1000);
        let task = ScheduledTask {
            plugin: plugin.to_string(),
            name: name.to_string(),
            interval_ms,
            payload: payload.to_string(),
            next_ms: crate::events::now_ms() + interval_ms as u64,
        };
        let k = key(plugin, name);
        let tasks = &self.tasks;
        // `schedule` 是同步接口（插件那边不该 await 一次登记），所以这里
        // 用 try_lock 快路径，拿不到就丢给 runtime。
        if let Ok(mut guard) = tasks.try_lock() {
            guard.insert(k, task);
        } else {
            tracing::warn!(plugin, name, "scheduler was busy; the task was dropped");
        }
    }

    fn cancel(&self, plugin: &str, name: &str) {
        if let Ok(mut guard) = self.tasks.try_lock() {
            guard.remove(&key(plugin, name));
        }
    }
}

/// HTTP 服务绑好端口之前的 ws 开口。
///
/// host 要先起来才能建 HTTP 服务（HTTP 要用 host 来回答 /api/*），而 host 又要一个
/// ws 开口——先有鸡还是先有蛋。这里用一个延迟填充的格子把环解开。
pub struct DeferredWs(pub Arc<tokio::sync::OnceCell<crate::http::HttpState>>);

impl WsHub for DeferredWs {
    fn publish(
        &self,
        plugin: &str,
        filter: &str,
        timeout_secs: u32,
    ) -> trestle_core::Result<String> {
        let state = self
            .0
            .get()
            .ok_or_else(|| trestle_core::TrestleError::Config {
                path: "daemon.http_bind".into(),
                detail: "the HTTP service is not up yet".into(),
            })?;
        let filter: crate::http::Filter = serde_json::from_str(filter).unwrap_or_default();
        state.open(plugin, filter, timeout_secs)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn a_very_short_interval_is_clamped() {
        let s = TaskScheduler::new();
        // 10ms 的轮询只会把远端问烂，还看不出任何新东西。
        s.schedule("fleet", "grab-gpu", 10, "{}");
        let tasks = s.list().await;
        assert_eq!(tasks.len(), 1);
        assert_eq!(tasks[0].interval_ms, 1000);
    }

    #[tokio::test]
    async fn scheduling_the_same_name_replaces_it() {
        let s = TaskScheduler::new();
        s.schedule("fleet", "grab", 5000, "{}");
        s.schedule("fleet", "grab", 30000, r#"{"n":2}"#);
        let tasks = s.list().await;
        assert_eq!(
            tasks.len(),
            1,
            "a re-registration must not create a second timer"
        );
        assert_eq!(tasks[0].interval_ms, 30000);
    }

    #[tokio::test]
    async fn tasks_are_scoped_by_plugin() {
        let s = TaskScheduler::new();
        s.schedule("fleet", "grab", 5000, "{}");
        s.schedule("job", "grab", 5000, "{}");
        assert_eq!(s.list().await.len(), 2);
        s.cancel("fleet", "grab");
        let left = s.list().await;
        assert_eq!(left.len(), 1);
        assert_eq!(left[0].plugin, "job");
    }
}
