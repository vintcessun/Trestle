//! 机队：把 `target` 路由到它所属的 connector。
//!
//! 这是 host 唯一「知道 target 是什么」的地方，而且它只做一件事——查表转发。
//! 怎么连、连不上怎么办、连接什么时候算死，全在 connector 插件里。

use std::collections::BTreeMap;
use std::sync::Arc;

use trestle_core::config::ConfigStore;
use trestle_core::{Result, TargetRegistry, TrestleError};

use crate::runtime::{ConnectorPool, Runtime};

/// 每个 connector 起几个实例。
///
/// 一个 wasm 实例被调用期间是独占的，所以这个数字就是「同一组机器能几路并发」。
/// 4 够覆盖常见的全队操作，又不至于让实例本身占太多内存（一个实例几百 KB）。
pub const DEFAULT_POOL_SIZE: usize = 4;

pub struct Fleet {
    store: Arc<ConfigStore>,
    pools: BTreeMap<String, Arc<ConnectorPool>>,
    registry: TargetRegistry,
}

impl Fleet {
    /// 扫描 `plugins/connectors/`，把配置里启用的 connector 都加载起来。
    pub async fn load(runtime: &Runtime, store: Arc<ConfigStore>) -> Result<Self> {
        Self::load_with_pool_size(runtime, store, DEFAULT_POOL_SIZE).await
    }

    pub async fn load_with_pool_size(
        runtime: &Runtime,
        store: Arc<ConfigStore>,
        pool_size: usize,
    ) -> Result<Self> {
        let registry = store.targets()?;
        let mut pools = BTreeMap::new();

        for (name, cfg) in &store.config().connectors {
            if !cfg.enabled {
                continue;
            }
            let dir = plugin_dir(&store, "connectors", &cfg.plugin);
            let loaded = runtime
                .load_connector(&dir)
                .map_err(|e| TrestleError::Config {
                    path: format!("connectors.{name}"),
                    detail: format!("{e:#}"),
                })?;

            let mine: Vec<_> = registry
                .iter()
                .filter(|t| t.connector == *name)
                .cloned()
                .collect();
            let config_json = serde_json::to_string(&cfg.settings).unwrap_or_else(|_| "{}".into());

            let pool = runtime
                .instantiate_pool(&loaded, mine, config_json, pool_size)
                .await
                .map_err(|e| TrestleError::Config {
                    path: format!("connectors.{name}"),
                    detail: format!("{e:#}"),
                })?;
            pools.insert(name.clone(), Arc::new(pool));
        }

        Ok(Self {
            store,
            pools,
            registry,
        })
    }

    pub fn targets(&self) -> &TargetRegistry {
        &self.registry
    }

    pub fn store(&self) -> &ConfigStore {
        &self.store
    }

    pub fn connector_names(&self) -> Vec<String> {
        self.pools.keys().cloned().collect()
    }

    pub fn pool(&self, connector: &str) -> Result<Arc<ConnectorPool>> {
        self.pools
            .get(connector)
            .cloned()
            .ok_or_else(|| TrestleError::UnknownConnector {
                name: connector.to_string(),
                known: self.connector_names(),
            })
    }

    /// 把一个操作路由到 target 所属的 connector。
    pub async fn op(&self, target: &str, op: &str, payload: &str) -> Result<String> {
        let t = self.registry.resolve(target)?;
        let pool = self.pool(&t.connector)?;
        // 用解析后的**主名**调插件：插件只认主名，别名是 host 这一层的事。
        pool.pick().op(&t.name, op, payload).await
    }

    /// 对多台机器并发执行同一个操作。
    ///
    /// 顺序执行整支机队在冷启动时是六倍延迟，所以并发这件事必须由 host 做——
    /// 插件那边一个 wasm 实例同时只能进一个调用。
    pub async fn op_many(
        &self,
        targets: &[String],
        op: &str,
        payload: &str,
    ) -> Vec<(String, Result<String>)> {
        let resolved = match self.registry.resolve_many(targets) {
            Ok(list) => list.into_iter().map(|t| t.name.clone()).collect::<Vec<_>>(),
            Err(e) => return vec![(String::new(), Err(e))],
        };

        let futures = resolved.into_iter().map(|name| async move {
            let result = self.op(&name, op, payload).await;
            (name, result)
        });
        futures::future::join_all(futures).await
    }

    /// 让所有 connector 都把前置条件准备好。`trestle doctor` 用它。
    pub async fn ensure_ready_all(&self) -> Vec<(String, Result<()>)> {
        let futures = self.pools.iter().map(|(name, pool)| {
            let name = name.clone();
            let pool = Arc::clone(pool);
            async move {
                let r = pool.any().ensure_ready().await;
                (name, r)
            }
        });
        futures::future::join_all(futures).await
    }
}

/// 插件目录：程序目录下的 `plugins/<kind>/<name>/`；开发时退回仓库里的那份。
fn plugin_dir(store: &ConfigStore, kind: &str, name: &str) -> std::path::PathBuf {
    let installed = store.plugins_dir().join(kind).join(name);
    if installed.join("manifest.toml").exists() {
        return installed;
    }
    // 开发期：配置在 config/ 下，插件在仓库的 plugins/ 下。
    store
        .root()
        .parent()
        .map(|repo| repo.join("plugins").join(kind).join(name))
        .unwrap_or(installed)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 这个常量就是「同一组机器能几路并发」，改它会直接改 fleet_status 的墙钟时间。
    /// 编译期就挡住 1 —— 池子只有一个实例等于没有并发。
    const _POOL_MUST_ALLOW_CONCURRENCY: () = assert!(DEFAULT_POOL_SIZE >= 2);

    #[test]
    fn the_pool_size_is_the_concurrency_of_a_connector() {
        // 上面那个 const 断言已经在编译期挡住了「池子只有一个」；
        // 这条只是把当前值钉住，改动时会有人看见。
        assert_eq!(DEFAULT_POOL_SIZE, 4);
    }
}
