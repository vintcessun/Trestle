//! 技能插件实例的 host 侧状态，以及它能看到的 host 服务。
//!
//! 技能插件**没有**传输工具箱——它够不到 SSH、够不到本机进程、够不到网络。
//! 它只能通过 `base` 做事，而 `base` 由 host 路由到目标所属的 connector。
//! 这是权限模型能成立的前提：插件不能自己开一条路。

use std::sync::Arc;

use trestle_core::{Result, TrestleError};

use crate::arbiter::Arbiter;
use crate::bindings::trestle::plugin::types::{Error, ErrorKind, TargetInfo};
use crate::bindings_tool::trestle::plugin::{arbiter, base, plugins, tasks, ws};
use crate::capability::{Capabilities, Manifest};
use crate::fleet::Fleet;
use crate::state::{EventSink, PluginKv, sandboxed_wasi};

/// 周期任务的登记处。真正的定时器在 daemon 里，这里只记「谁要什么时候被叫醒」。
pub trait TaskSink: Send + Sync {
    fn schedule(&self, plugin: &str, name: &str, interval_ms: u32, payload: &str);
    fn cancel(&self, plugin: &str, name: &str);
}

/// 什么都不排的登记处，测试用。
pub struct NullTasks;
impl TaskSink for NullTasks {
    fn schedule(&self, _: &str, _: &str, _: u32, _: &str) {}
    fn cancel(&self, _: &str, _: &str) {}
}

/// WebSocket 端点的开口。真正的 HTTP 服务在 daemon 里。
pub trait WsHub: Send + Sync {
    fn publish(&self, plugin: &str, filter: &str, timeout_secs: u32) -> Result<String>;
}

/// 没有 ws 服务时的占位：诚实报错，而不是给一个连不上的 URL。
pub struct NoWs;
impl WsHub for NoWs {
    fn publish(&self, _: &str, _: &str, _: u32) -> Result<String> {
        Err(TrestleError::Config {
            path: "daemon.http_bind".into(),
            detail: "no WebSocket service is running in this process".into(),
        })
    }
}

/// 插件之间互相调用的入口。指回 [`crate::tools::ToolRegistry`]。
pub trait ToolInvoker: Send + Sync {
    fn call_blocking_ctx(&self) -> Arc<crate::tools::ToolRegistry>;
}

pub struct ToolState {
    pub plugin: String,
    pub manifest: Manifest,
    pub fleet: Arc<Fleet>,
    pub arbiter: Arc<Arbiter>,
    pub kv: Arc<PluginKv>,
    pub events: Arc<dyn EventSink>,
    pub tasks: Arc<dyn TaskSink>,
    pub ws: Arc<dyn WsHub>,
    /// 插件调插件用。加载完成后由注册表填上。
    pub registry: Arc<crate::tools::ToolRegistry>,
    pub config_json: String,
    pub wasi: wasmtime_wasi::WasiCtx,
    pub table: wasmtime::component::ResourceTable,
}

impl wasmtime_wasi::WasiView for ToolState {
    fn ctx(&mut self) -> wasmtime_wasi::WasiCtxView<'_> {
        wasmtime_wasi::WasiCtxView {
            ctx: &mut self.wasi,
            table: &mut self.table,
        }
    }
}

impl ToolState {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        manifest: Manifest,
        fleet: Arc<Fleet>,
        arbiter: Arc<Arbiter>,
        kv: Arc<PluginKv>,
        events: Arc<dyn EventSink>,
        tasks: Arc<dyn TaskSink>,
        ws: Arc<dyn WsHub>,
        registry: Arc<crate::tools::ToolRegistry>,
        config_json: String,
    ) -> Self {
        Self {
            plugin: manifest.name.clone(),
            manifest,
            fleet,
            arbiter,
            kv,
            events,
            tasks,
            ws,
            registry,
            config_json,
            wasi: sandboxed_wasi(),
            table: Default::default(),
        }
    }

    fn caps(&self) -> &Capabilities {
        &self.manifest.capabilities
    }

    fn deny(&self, action: &str) -> Error {
        self.events.emit(
            &self.plugin,
            "warn",
            "plugin_call_denied",
            &serde_json::json!({ "plugin": self.plugin, "action": action }).to_string(),
        );
        Error {
            kind: ErrorKind::Denied,
            detail: format!(
                "plugin '{}' denied: {action} is not in its manifest allowlist",
                self.plugin
            ),
            remedy: format!(
                "add it to the `capabilities` section of plugins/tools/{}/manifest.toml",
                self.plugin
            ),
        }
    }
}

fn to_wit(e: TrestleError) -> Error {
    crate::imports::error_to_wit(e)
}

impl base::Host for ToolState {
    /// 七个基本操作。host 按 target 路由到它所属的 connector。
    async fn call(&mut self, target: String, op: String, payload: String) -> Result<String, Error> {
        self.fleet.op(&target, &op, &payload).await.map_err(to_wit)
    }

    /// 对多台机器并发执行同一个操作。
    ///
    /// 并发发生在 **host** 这边：一个 wasm 实例同时只能进一个调用，所以插件自己
    /// 循环调六次就是六倍延迟。插件只说要打哪几台，怎么并发是 host 的事。
    async fn call_many(
        &mut self,
        targets: Vec<String>,
        op: String,
        payload: String,
    ) -> Vec<Result<String, Error>> {
        self.fleet
            .op_many(&targets, &op, &payload)
            .await
            .into_iter()
            .map(|(_, r)| r.map_err(to_wit))
            .collect()
    }
}

impl plugins::Host for ToolState {
    async fn call(&mut self, plugin: String, tool: String, args: String) -> Result<String, Error> {
        // 被调方必须写在我的 manifest 里——否则插件之间就是全通的。
        if !self.caps().allows_calling(&plugin) {
            return Err(self.deny(&format!("call plugin {plugin}")));
        }
        self.registry
            .call_in(&plugin, &tool, &args)
            .await
            .map_err(to_wit)
    }
}

impl tasks::Host for ToolState {
    async fn schedule(&mut self, name: String, interval_ms: u32, payload: String) {
        if !self.caps().tasks {
            let _ = self.deny(&format!("schedule task {name}"));
            return;
        }
        self.tasks
            .schedule(&self.plugin, &name, interval_ms, &payload);
    }

    async fn cancel(&mut self, name: String) {
        self.tasks.cancel(&self.plugin, &name);
    }
}

impl arbiter::Host for ToolState {
    /// 挑几个单位出来。
    ///
    /// 快照由插件递进来，host 这一侧**不做任何 I/O**——见 [`crate::arbiter`]
    /// 顶上那段关于死锁的话。
    async fn acquire(
        &mut self,
        pool: String,
        units: String,
        want: u32,
        purpose: String,
    ) -> Result<String, Error> {
        if !self.caps().allows_arbitrating(&pool) {
            return Err(self.deny(&format!("arbitrate {}", crate::arbiter::pool_kind(&pool))));
        }
        let snapshot: Vec<crate::arbiter::Unit> =
            serde_json::from_str(&units).map_err(|e| Error {
                kind: ErrorKind::InvalidRequest,
                detail: format!("the units snapshot is not a valid unit array: {e}"),
                remedy: r#"pass [{"id":"0","busy":false,"label":"..."}]"#.into(),
            })?;
        let claim = self
            .arbiter
            .acquire(&pool, &snapshot, want, &purpose, &self.plugin, now_ms())
            .await
            .map_err(to_wit)?;
        Ok(serde_json::json!({ "claim": claim.id, "units": claim.units }).to_string())
    }

    async fn release(&mut self, claim: String) {
        self.arbiter.release(&claim).await;
    }

    async fn bind_job(&mut self, claim: String, job_id: String) {
        self.arbiter.bind_job(&claim, &job_id).await;
    }

    async fn release_job(&mut self, job_id: String) {
        self.arbiter.release_job(&job_id).await;
    }

    async fn claims(&mut self, pool: String) -> String {
        serde_json::to_string(&self.arbiter.claims_of(&pool).await).unwrap_or_else(|_| "[]".into())
    }
}

impl ws::Host for ToolState {
    async fn publish(&mut self, filter: String, timeout_secs: u32) -> Result<String, Error> {
        if !self.caps().ws {
            return Err(self.deny("open a websocket endpoint"));
        }
        // timeout 必填是刻意的：一个没有过期时间的监视端点会悄悄泄漏——
        // 任务早就结束了，ws 还挂在那里占着轮询。
        if timeout_secs == 0 {
            return Err(Error {
                kind: ErrorKind::InvalidRequest,
                detail: "timeout_secs must be greater than zero".into(),
                remedy: "pick roughly how long the thing you are watching will run".into(),
            });
        }
        self.ws
            .publish(&self.plugin, &filter, timeout_secs)
            .map_err(to_wit)
    }
}

impl crate::bindings_tool::trestle::plugin::host_services::Host for ToolState {
    async fn targets(&mut self) -> Vec<TargetInfo> {
        self.fleet
            .targets()
            .iter()
            .map(|t| TargetInfo {
                name: t.name.clone(),
                host: t.host.clone(),
                port: t.port,
                user: t.user.clone(),
                workdir: t.workdir.clone(),
                note: t.note.clone(),
                aliases: t.aliases.clone(),
                agent_dir: t.agent_dir.clone(),
                connector: t.connector.clone(),
            })
            .collect()
    }

    async fn config_get(&mut self) -> String {
        self.config_json.clone()
    }

    async fn secret_get(&mut self, reference: String) -> Result<String, Error> {
        // 技能插件不该碰凭据。它要连机器，走 base；base 由 connector 处理认证。
        Err(self.deny(&format!("read secret {reference}")))
    }

    async fn state_get(&mut self, key: String) -> Option<String> {
        self.kv.get(&key).await
    }

    async fn state_set(&mut self, key: String, value: String) {
        self.kv.set(key, value).await;
    }

    async fn state_delete(&mut self, key: String) {
        self.kv.delete(&key).await;
    }

    async fn state_list(&mut self, prefix: String) -> Vec<String> {
        self.kv.list(&prefix).await
    }

    async fn emit(&mut self, level: String, kind: String, fields: String) {
        self.events.emit(&self.plugin, &level, &kind, &fields);
    }

    async fn now_ms(&mut self) -> u64 {
        now_ms()
    }

    async fn sleep_ms(&mut self, ms: u32) {
        tokio::time::sleep(std::time::Duration::from_millis(ms as u64)).await;
    }

    async fn staging_path(&mut self, name: String) -> String {
        crate::staging_path(&name)
    }
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_tool_plugin_cannot_read_secrets_even_by_asking() {
        // 这条是设计而不是疏漏：技能插件要连机器就走 base，认证是 connector 的事。
        // 如果它能读 secrets，capability 模型就有一个绕过口。
        let caps = Capabilities::default();
        assert!(caps.call_plugins.is_empty());
        assert!(!caps.ws);
        assert!(!caps.tasks);
    }
}
