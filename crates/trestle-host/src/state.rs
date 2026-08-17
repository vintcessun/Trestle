//! 一个插件实例的 host 侧状态：句柄表、capability、配置、KV、事件出口。

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Arc;

use tokio::sync::Mutex;

use trestle_core::Target;
use trestle_core::config::ConfigStore;

use crate::capability::{Capabilities, Manifest};
use crate::handles::HandleTable;

/// 插件往外发事件的出口。daemon 接上真的 EventBus，测试接一个收集器。
pub trait EventSink: Send + Sync {
    fn emit(&self, plugin: &str, level: &str, kind: &str, fields: &str);
}

/// 什么都不做的出口，单测用。
pub struct NullSink;
impl EventSink for NullSink {
    fn emit(&self, _: &str, _: &str, _: &str, _: &str) {}
}

/// per-plugin 持久化 KV。按插件名隔离，落在程序目录下的 `state/`。
pub struct PluginKv {
    dir: PathBuf,
    cache: Mutex<BTreeMap<String, String>>,
}

impl PluginKv {
    pub fn open(state_dir: &std::path::Path, plugin: &str) -> Self {
        let dir = state_dir.join("plugins").join(plugin);
        let mut cache = BTreeMap::new();
        if let Ok(raw) = std::fs::read_to_string(dir.join("kv.json"))
            && let Ok(map) = serde_json::from_str::<BTreeMap<String, String>>(&raw)
        {
            cache = map;
        }
        Self {
            dir,
            cache: Mutex::new(cache),
        }
    }

    pub async fn get(&self, key: &str) -> Option<String> {
        self.cache.lock().await.get(key).cloned()
    }

    pub async fn set(&self, key: String, value: String) {
        let mut cache = self.cache.lock().await;
        cache.insert(key, value);
        self.persist(&cache);
    }

    pub async fn delete(&self, key: &str) {
        let mut cache = self.cache.lock().await;
        cache.remove(key);
        self.persist(&cache);
    }

    pub async fn list(&self, prefix: &str) -> Vec<String> {
        self.cache
            .lock()
            .await
            .keys()
            .filter(|k| k.starts_with(prefix))
            .cloned()
            .collect()
    }

    /// 原子写：先写临时文件再改名，崩在半路也不会留下坏掉的 JSON。
    fn persist(&self, cache: &BTreeMap<String, String>) {
        let _ = std::fs::create_dir_all(&self.dir);
        let tmp = self.dir.join("kv.json.tmp");
        let final_path = self.dir.join("kv.json");
        if let Ok(raw) = serde_json::to_string_pretty(cache)
            && std::fs::write(&tmp, raw).is_ok()
        {
            let _ = std::fs::rename(&tmp, &final_path);
        }
    }
}

/// 同一个 connector 的所有实例共享的东西。
///
/// 一个 wasm 实例在被调用期间是独占的，所以 host 给每个 connector 起一个实例池，
/// 让「打整支机队」能真并发。但**连接不该跟着实例走**——gpu-4 的连接只该有一条。
/// 所以句柄表和「哪台机器对应哪条连接」都放在这里，按 connector 共享。
#[derive(Default)]
pub struct SharedConnector {
    pub handles: Mutex<HandleTable>,
    /// target → (session, agent)
    pub sessions: Mutex<BTreeMap<String, (u64, u64)>>,
}

/// 一个插件实例的全部 host 侧状态。每个 wasmtime `Store` 装一份。
pub struct PluginState {
    pub plugin: String,
    pub manifest: Manifest,
    pub shared: Arc<SharedConnector>,
    pub kv: Arc<PluginKv>,
    pub events: Arc<dyn EventSink>,
    pub store: Arc<ConfigStore>,
    /// 这个 connector 管辖的机器（connector 插件用）。
    pub targets: Vec<Target>,
    /// 属于我的那一节配置，JSON 形式（插件自己解析）。
    pub config_json: String,
    /// **空的** WASI 上下文。
    ///
    /// 插件编到 `wasm32-wasip2`，所以标准库会链进 `wasi:io` 之类的导入——
    /// 不给 linker 就实例化不了。但这里给的是一个**什么都没有**的上下文：
    /// 没有预开目录、没有网络、没有环境变量。插件想碰外界，只能走我们自己
    /// 那些受 capability 管的导入。
    pub wasi: wasmtime_wasi::WasiCtx,
    pub table: wasmtime::component::ResourceTable,
}

impl wasmtime_wasi::WasiView for PluginState {
    fn ctx(&mut self) -> wasmtime_wasi::WasiCtxView<'_> {
        wasmtime_wasi::WasiCtxView {
            ctx: &mut self.wasi,
            table: &mut self.table,
        }
    }
}

/// 插件用的 WASI 上下文：只留 stderr（好让 panic 有地方说话），别的全不给。
pub fn sandboxed_wasi() -> wasmtime_wasi::WasiCtx {
    let mut b = wasmtime_wasi::WasiCtxBuilder::new();
    b.inherit_stderr();
    b.build()
}

impl PluginState {
    pub fn caps(&self) -> &Capabilities {
        &self.manifest.capabilities
    }

    /// 记一次被拒绝的调用。**必须发事件**——否则权限模型是个黑盒。
    pub fn deny(&self, action: &str) -> crate::capability::CapabilityError {
        self.events.emit(
            &self.plugin,
            "warn",
            "plugin_call_denied",
            &serde_json::json!({ "plugin": self.plugin, "action": action }).to_string(),
        );
        crate::capability::deny(&self.plugin, action)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn kv_survives_a_reopen() {
        let dir = std::env::temp_dir().join("trestle-kv-test");
        let _ = std::fs::remove_dir_all(&dir);

        let kv = PluginKv::open(&dir, "job");
        kv.set("offset:gpu-4:train-1".into(), "4096".into()).await;
        kv.set("offset:gpu-4:train-2".into(), "8192".into()).await;
        drop(kv);

        // 重开就是 daemon 重启后的情形：状态必须还在。
        let kv = PluginKv::open(&dir, "job");
        assert_eq!(kv.get("offset:gpu-4:train-1").await.as_deref(), Some("4096"));
        assert_eq!(kv.list("offset:gpu-4:").await.len(), 2);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn plugins_do_not_see_each_others_keys() {
        let dir = std::env::temp_dir().join("trestle-kv-isolation");
        let _ = std::fs::remove_dir_all(&dir);

        let job = PluginKv::open(&dir, "job");
        let fleet = PluginKv::open(&dir, "fleet");
        job.set("secret".into(), "job-value".into()).await;

        assert!(fleet.get("secret").await.is_none());
        assert_eq!(job.get("secret").await.as_deref(), Some("job-value"));

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn delete_removes_the_key_from_disk_too() {
        let dir = std::env::temp_dir().join("trestle-kv-delete");
        let _ = std::fs::remove_dir_all(&dir);

        let kv = PluginKv::open(&dir, "job");
        kv.set("k".into(), "v".into()).await;
        kv.delete("k").await;
        drop(kv);

        assert!(PluginKv::open(&dir, "job").get("k").await.is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
