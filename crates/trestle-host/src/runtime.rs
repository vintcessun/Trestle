//! 插件运行时：加载 `.wasm`、装配 linker、实例化、调用。
//!
//! **声明与实例化分离**：manifest 在 host 启动时读一次就进 registry（工具因此可见），
//! 真正的 wasm 实例延迟到第一次调用。两三百个插件不等于两三百个实例。

use anyhow::Context as _;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use tokio::sync::Mutex;
use wasmtime::component::{Component, Linker};
use wasmtime::{Config, Engine, Store};

use trestle_core::config::ConfigStore;
use trestle_core::{Result as TResult, Target, TrestleError};

use crate::bindings::Connector as ConnectorBindings;
use crate::capability::Manifest;
use crate::pool::{Factory, InstancePool, PoolPolicy};
use crate::state::{EventSink, NullSink, PluginKv, PluginState, SharedConnector, sandboxed_wasi};

/// 一个 connector 的实例池。
///
/// 所有实例共享连接（见 [`SharedConnector`]）与 KV，所以对外它们是可互换的——
/// 谁空着就用谁。这是「同时打整支机队」能真并发的原因。
pub type ConnectorPool = InstancePool<ConnectorInstance>;

/// 一个技能插件的实例池。和 [`ConnectorPool`] 是同一个东西，只是装的是技能插件。
pub type ToolPool = InstancePool<ToolInstance>;

/// 共享的编译环境。Engine 很贵，全进程一个。
pub struct Runtime {
    engine: Engine,
    store: Arc<ConfigStore>,
    events: Arc<dyn EventSink>,
}

impl Runtime {
    pub fn new(store: Arc<ConfigStore>) -> anyhow::Result<Self> {
        Self::with_events(store, Arc::new(NullSink))
    }

    pub fn with_events(
        store: Arc<ConfigStore>,
        events: Arc<dyn EventSink>,
    ) -> anyhow::Result<Self> {
        let mut config = Config::new();
        // 组件模型。异步在 wasmtime 47 里已是默认，不用再显式打开。
        // host 导入真的在做 I/O，插件那侧看到的仍是同步调用——wasmtime 用 fiber
        // 把它挂起，于是别的插件照常推进。
        config.wasm_component_model(true);
        // 一个跑飞的插件不能把整个 daemon 拖住：给它一个可被打断的执行上下文。
        config.epoch_interruption(true);

        // 编译缓存。没有它的话 daemon 每次启动都要把全部插件从头编一遍——
        // Rust 插件几百 KB 无所谓，但一个 componentize-py 产出的插件是 18 MB，
        // 单它就要几十秒。而 daemon 是 lazy 启动的，那几十秒直接加在
        // 「agent 第一次调用」的延迟上。
        //
        // 缓存落在程序目录下（portable），和别的运行期文件放在一起。
        match wasmtime::Cache::new(wasmtime::CacheConfig::new()) {
            Ok(cache) => {
                config.cache(Some(cache));
            }
            Err(e) => tracing::warn!(%e, "no compilation cache; plugin loading will be slower"),
        }

        let engine = Engine::new(&config)
            .map_err(|e| anyhow::anyhow!("cannot create the wasm engine: {e}"))?;
        Ok(Self {
            engine,
            store,
            events,
        })
    }

    pub fn engine(&self) -> &Engine {
        &self.engine
    }

    /// 从磁盘加载一个 connector 插件。只编译，不实例化。
    pub fn load_connector(&self, dir: &Path) -> anyhow::Result<LoadedConnector> {
        let manifest_path = dir.join("manifest.toml");
        let raw = std::fs::read_to_string(&manifest_path)
            .with_context(|| format!("cannot read {}", manifest_path.display()))?;
        let manifest: Manifest = toml::from_str(&raw)
            .with_context(|| format!("cannot parse {}", manifest_path.display()))?;

        let wasm_path = dir.join(format!("{}.wasm", manifest.name));
        let component = Component::from_file(&self.engine, &wasm_path)
            .map_err(|e| anyhow::anyhow!("cannot load {}: {e}", wasm_path.display()))?;

        Ok(LoadedConnector {
            manifest,
            component,
            path: wasm_path,
        })
    }

    /// 实例化一个 connector，拿到可调用的句柄。名字用插件名——单测走这条。
    pub async fn instantiate_connector(
        &self,
        loaded: &LoadedConnector,
        targets: Vec<Target>,
        config_json: String,
    ) -> anyhow::Result<ConnectorInstance> {
        let kv = Arc::new(PluginKv::open(
            &self.store.state_dir(),
            &loaded.manifest.name,
        ));
        self.instantiate_connector_with(
            loaded,
            loaded.manifest.name.clone(),
            targets,
            config_json,
            Default::default(),
            kv,
        )
        .await
    }

    /// 起一个 connector 的实例池。
    ///
    /// 池里的实例**共享连接和 KV**：一个 wasm 实例被调用期间是独占的，所以
    /// 「同时打整支机队」需要多个实例；但连接不该跟着实例走，否则六个实例
    /// 就会建六条到同一台机器的连接，而 KV 会分裂成六份各写各的缓存。
    ///
    /// `instance` 是**配置里的 connector 名**（`gpu-cluster`），不是插件名
    /// （`ssh-socks5`）。同一个插件可以被配置成多个 connector，各管各的机器、
    /// 各有各的 KV——所以隔离必须按实例名来。
    pub async fn connector_pool(
        self: &Arc<Self>,
        loaded: Arc<LoadedConnector>,
        instance: String,
        targets: Vec<Target>,
        config_json: String,
        policy: PoolPolicy,
    ) -> anyhow::Result<ConnectorPool> {
        let shared: Arc<SharedConnector> = Default::default();
        let kv = Arc::new(PluginKv::open(&self.store.state_dir(), &instance));
        let runtime = Arc::clone(self);
        let name = instance.clone();

        let factory: Factory<ConnectorInstance> = Arc::new(move || {
            let runtime = Arc::clone(&runtime);
            let loaded = Arc::clone(&loaded);
            let instance = name.clone();
            let targets = targets.clone();
            let config_json = config_json.clone();
            let shared = Arc::clone(&shared);
            let kv = Arc::clone(&kv);
            Box::pin(async move {
                runtime
                    .instantiate_connector_with(&loaded, instance, targets, config_json, shared, kv)
                    .await
            })
        });

        InstancePool::new(instance, factory, policy).await
    }

    async fn instantiate_connector_with(
        &self,
        loaded: &LoadedConnector,
        instance: String,
        targets: Vec<Target>,
        config_json: String,
        shared: Arc<SharedConnector>,
        kv: Arc<PluginKv>,
    ) -> anyhow::Result<ConnectorInstance> {
        let mut linker: Linker<PluginState> = Linker::new(&self.engine);
        crate::bindings::trestle::plugin::transport::add_to_linker::<PluginState, HasSelf>(
            &mut linker,
            |s| s,
        )?;
        crate::bindings::trestle::plugin::host_services::add_to_linker::<PluginState, HasSelf>(
            &mut linker,
            |s| s,
        )?;
        // 插件编到 wasm32-wasip2，标准库会链进 wasi:io 之类的导入 —— 不挂上去
        // 就实例化不了。挂的是一个空上下文：没有目录、没有网络、没有环境变量。
        wasmtime_wasi::p2::add_to_linker_async(&mut linker)?;

        let state = PluginState {
            plugin: instance.clone(),
            manifest: loaded.manifest.clone(),
            shared: Arc::clone(&shared),
            kv,
            events: Arc::clone(&self.events),
            store: Arc::clone(&self.store),
            targets,
            config_json,
            wasi: sandboxed_wasi(),
            table: Default::default(),
        };

        let mut store = Store::new(&self.engine, state);
        store.set_epoch_deadline(u64::MAX);
        let bindings = ConnectorBindings::instantiate_async(&mut store, &loaded.component, &linker)
            .await
            .map_err(|e| anyhow::anyhow!("cannot instantiate {}: {e}", loaded.path.display()))?;

        // 事件里报的是**实例名**：用户认识的是 `gpu-cluster`，
        // 而 `ssh-socks5` 只是它碰巧用的那个驱动。
        self.events.emit(
            &instance,
            "info",
            "plugin_loaded",
            &serde_json::json!({
                "plugin": loaded.manifest.name,
                "connector": instance,
                "kind": "connector",
            })
            .to_string(),
        );

        Ok(ConnectorInstance {
            name: instance,
            inner: Mutex::new(Instance { store, bindings }),
        })
    }
}

/// `add_to_linker` 需要一个「怎么从 store data 拿到实现者」的类型参数。
/// 我们的 store data 自己就是实现者，所以这里是恒等。
struct HasSelf;
impl wasmtime::component::HasData for HasSelf {
    type Data<'a> = &'a mut PluginState;
}

struct HasTool;
impl wasmtime::component::HasData for HasTool {
    type Data<'a> = &'a mut crate::tool_state::ToolState;
}

impl Runtime {
    /// 加载一个技能插件。只编译，不实例化。
    pub fn load_tool(&self, dir: &Path) -> anyhow::Result<LoadedToolPlugin> {
        let manifest_path = dir.join("manifest.toml");
        let raw = std::fs::read_to_string(&manifest_path)
            .with_context(|| format!("cannot read {}", manifest_path.display()))?;
        let manifest: Manifest = toml::from_str(&raw)
            .with_context(|| format!("cannot parse {}", manifest_path.display()))?;
        let wasm_path = dir.join(format!("{}.wasm", manifest.name));
        let component = Component::from_file(&self.engine, &wasm_path)
            .map_err(|e| anyhow::anyhow!("cannot load {}: {e}", wasm_path.display()))?;
        Ok(LoadedToolPlugin {
            manifest,
            component,
            path: wasm_path,
        })
    }

    /// 起一个技能插件的实例池。
    ///
    /// 声明了 `stateless` 的插件才允许长到 `policy.max`；没声明的钉死在一个实例上——
    /// 它可能把状态存在 wasm 内存里，那种情况下多开会让实例各看各的，**静默**出错。
    ///
    /// `make_state` 每个实例调一次：`ToolState` 里有 `WasiCtx`，不能克隆。
    pub async fn tool_pool(
        self: &Arc<Self>,
        loaded: Arc<LoadedToolPlugin>,
        make_state: Arc<dyn Fn() -> crate::tool_state::ToolState + Send + Sync>,
        policy: PoolPolicy,
    ) -> anyhow::Result<ToolPool> {
        let policy = if loaded.manifest.capabilities.stateless {
            policy
        } else {
            PoolPolicy {
                max: 1,
                ..policy
            }
        };
        let name = loaded.manifest.name.clone();
        let runtime = Arc::clone(self);

        let factory: Factory<ToolInstance> = Arc::new(move || {
            let runtime = Arc::clone(&runtime);
            let loaded = Arc::clone(&loaded);
            let state = make_state();
            Box::pin(async move { runtime.instantiate_tool(&loaded, state).await })
        });

        InstancePool::new(name, factory, policy).await
    }

    /// 实例化一个技能插件。
    ///
    /// 它拿到的导入里**没有传输工具箱**——技能插件够不到 SSH、够不到本机进程、
    /// 够不到网络，只能通过 `base` 做事。这是权限模型能成立的前提。
    pub async fn instantiate_tool(
        &self,
        loaded: &LoadedToolPlugin,
        state: crate::tool_state::ToolState,
    ) -> anyhow::Result<ToolInstance> {
        use crate::bindings_tool::trestle::plugin as tp;
        let mut linker: Linker<crate::tool_state::ToolState> = Linker::new(&self.engine);
        tp::base::add_to_linker::<_, HasTool>(&mut linker, |s| s)?;
        tp::host_services::add_to_linker::<_, HasTool>(&mut linker, |s| s)?;
        tp::plugins::add_to_linker::<_, HasTool>(&mut linker, |s| s)?;
        tp::tasks::add_to_linker::<_, HasTool>(&mut linker, |s| s)?;
        tp::gpu::add_to_linker::<_, HasTool>(&mut linker, |s| s)?;
        tp::ws::add_to_linker::<_, HasTool>(&mut linker, |s| s)?;
        wasmtime_wasi::p2::add_to_linker_async(&mut linker)?;

        let name = state.plugin.clone();
        let mut store = Store::new(&self.engine, state);
        store.set_epoch_deadline(u64::MAX);
        let bindings = crate::bindings_tool::ToolPlugin::instantiate_async(
            &mut store,
            &loaded.component,
            &linker,
        )
        .await
        .map_err(|e| anyhow::anyhow!("cannot instantiate {}: {e}", loaded.path.display()))?;

        self.events.emit(
            &name,
            "info",
            "plugin_loaded",
            &serde_json::json!({ "plugin": name, "kind": "tool" }).to_string(),
        );

        Ok(ToolInstance {
            name,
            inner: Mutex::new(ToolInner { store, bindings }),
        })
    }
}

pub struct LoadedConnector {
    pub manifest: Manifest,
    pub component: Component,
    pub path: PathBuf,
}

struct Instance {
    store: Store<PluginState>,
    bindings: ConnectorBindings,
}

/// 一个活着的 connector 实例。
///
/// wasmtime 的 `Store` 不能并发进入，所以这里有一把锁——但**它只锁住 wasm 那一段**：
/// 插件调 host 导入去做 I/O 时，fiber 会挂起，锁仍握着。所以真正的并发要靠
/// `base.call-many`（host 侧扇出）或多个实例，而不是靠同一个实例里并发。
pub struct ConnectorInstance {
    pub name: String,
    inner: Mutex<Instance>,
}

impl ConnectorInstance {
    pub async fn targets(
        &self,
    ) -> TResult<Vec<crate::bindings::trestle::plugin::types::TargetInfo>> {
        let mut g = self.inner.lock().await;
        let Instance { store, bindings } = &mut *g;
        bindings
            .call_targets(&mut *store)
            .await
            .map_err(|e| self.trap(e))
    }

    pub async fn ensure_ready(&self) -> TResult<()> {
        let mut g = self.inner.lock().await;
        let Instance { store, bindings } = &mut *g;
        bindings
            .call_ensure_ready(&mut *store)
            .await
            .map_err(|e| self.trap(e))?
            .map_err(|e| from_wit(&self.name, e))
    }

    pub async fn health(
        &self,
        target: &str,
    ) -> TResult<crate::bindings::trestle::plugin::types::Health> {
        let mut g = self.inner.lock().await;
        let Instance { store, bindings } = &mut *g;
        bindings
            .call_health(&mut *store, target)
            .await
            .map_err(|e| self.trap(e))
    }

    /// 七个基本操作的入口。
    pub async fn op(&self, target: &str, op: &str, payload: &str) -> TResult<String> {
        let mut g = self.inner.lock().await;
        let Instance { store, bindings } = &mut *g;
        bindings
            .call_op(&mut *store, target, op, payload)
            .await
            .map_err(|e| self.trap(e))?
            .map_err(|e| from_wit(&self.name, e))
    }

    pub async fn config_schema(&self) -> TResult<String> {
        let mut g = self.inner.lock().await;
        let Instance { store, bindings } = &mut *g;
        bindings
            .call_config_schema(&mut *store)
            .await
            .map_err(|e| self.trap(e))
    }

    /// 插件自己崩了（trap）与插件返回了一个错误是两回事，必须分得开。
    fn trap(&self, e: wasmtime::Error) -> TrestleError {
        TrestleError::Protocol {
            target: String::new(),
            detail: format!("plugin '{}' trapped: {e}", self.name),
        }
    }
}

/// 一个已加载但还没实例化的技能插件。
pub struct LoadedToolPlugin {
    pub manifest: Manifest,
    pub component: Component,
    pub path: PathBuf,
}

/// 一个活着的技能插件实例。
pub struct ToolInstance {
    pub name: String,
    inner: Mutex<ToolInner>,
}

struct ToolInner {
    store: Store<crate::tool_state::ToolState>,
    bindings: crate::bindings_tool::ToolPlugin,
}

impl ToolInstance {
    pub async fn list_tools(&self) -> TResult<String> {
        let mut g = self.inner.lock().await;
        let ToolInner { store, bindings } = &mut *g;
        bindings
            .call_list_tools(&mut *store)
            .await
            .map_err(|e| self.trap(e))
    }

    pub async fn call(&self, tool: &str, args: &str) -> TResult<String> {
        let mut g = self.inner.lock().await;
        let ToolInner { store, bindings } = &mut *g;
        bindings
            .call_call(&mut *store, tool, args)
            .await
            .map_err(|e| self.trap(e))?
            .map_err(|e| from_wit(&self.name, e))
    }

    /// 周期任务的回调。
    pub async fn on_tick(&self, name: &str, payload: &str) -> TResult<()> {
        let mut g = self.inner.lock().await;
        let ToolInner { store, bindings } = &mut *g;
        bindings
            .call_on_tick(&mut *store, name, payload)
            .await
            .map_err(|e| self.trap(e))
    }

    pub async fn config_schema(&self) -> TResult<String> {
        let mut g = self.inner.lock().await;
        let ToolInner { store, bindings } = &mut *g;
        bindings
            .call_config_schema(&mut *store)
            .await
            .map_err(|e| self.trap(e))
    }

    /// 这个插件贡献的 Web UI 片段。空串表示它不要面板。
    pub async fn ui_panel(&self) -> TResult<String> {
        let mut g = self.inner.lock().await;
        let ToolInner { store, bindings } = &mut *g;
        bindings
            .call_ui_panel(&mut *store)
            .await
            .map_err(|e| self.trap(e))
    }

    fn trap(&self, e: wasmtime::Error) -> TrestleError {
        TrestleError::Protocol {
            target: String::new(),
            detail: format!("plugin '{}' trapped: {e}", self.name),
        }
    }
}

/// 把插件返回的错误翻回内部错误，**保住 kind**。
///
/// 尤其是 `unknown-state`：它一旦在边界上退化成一个普通错误，
/// 上层就可能去重放一条可能已经执行过的命令。
pub fn from_wit(plugin: &str, e: crate::bindings::trestle::plugin::types::Error) -> TrestleError {
    use crate::bindings::trestle::plugin::types::ErrorKind as K;
    match e.kind {
        K::UnknownState => TrestleError::UnknownState {
            target: String::new(),
            op: e.detail.clone(),
        },
        K::Denied => TrestleError::CapabilityDenied {
            plugin: plugin.to_string(),
            action: e.detail.clone(),
        },
        K::NotReady => TrestleError::ConnectorNotReady {
            connector: plugin.to_string(),
            detail: e.detail.clone(),
            remedy: e.remedy.clone(),
        },
        _ => TrestleError::Remote {
            target: String::new(),
            op: plugin.to_string(),
            detail: if e.remedy.is_empty() {
                e.detail
            } else {
                format!("{}\nTry: {}", e.detail, e.remedy)
            },
        },
    }
}
