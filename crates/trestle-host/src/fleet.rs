//! 机队：把 `target` 路由到它所属的 connector。
//!
//! 这是 host 唯一「知道 target 是什么」的地方，而且它只做一件事——查表转发。
//! 怎么连、连不上怎么办、连接什么时候算死，全在 connector 插件里。

use std::collections::BTreeMap;
use std::sync::Arc;

use trestle_core::config::ConfigStore;
use trestle_core::{Result, TargetRegistry, TrestleError};

use crate::pool::PoolPolicy;
use crate::runtime::{ConnectorPool, Runtime};

pub struct Fleet {
    store: Arc<ConfigStore>,
    pools: BTreeMap<String, Arc<ConnectorPool>>,
    registry: TargetRegistry,
    /// 装不上的 connector：名字 → 原因。它们的机器因此不可达，但别的照常。
    broken: BTreeMap<String, String>,
}

impl Fleet {
    /// 扫描 `plugins/connectors/`，把配置里启用的 connector 都加载起来。
    ///
    /// 一个 connector **配置节**是一个实例：`[connectors.gpu-cluster]` 说了
    /// `plugin = "ssh-socks5"`，那它就是 `ssh-socks5` 这个驱动的一个实例。
    /// 同一个驱动可以被配置成任意多个 connector，各管各的机器、各有各的 KV。
    pub async fn load(
        runtime: &Arc<Runtime>,
        store: Arc<ConfigStore>,
        policy: PoolPolicy,
    ) -> Result<Self> {
        let registry = store.targets()?;
        let mut pools = BTreeMap::new();
        let mut broken: BTreeMap<String, String> = BTreeMap::new();

        for (name, cfg) in &store.config().connectors {
            if !cfg.enabled {
                continue;
            }
            let dir = plugin_dir(&store, "connectors", &cfg.plugin);
            // 一个装不上的驱动只该让**它那一组机器**不可用，不该让 daemon 起不来。
            // 别组的机器和别的插件与它无关，凭什么陪葬。
            let mut loaded = match runtime.load_connector(&dir) {
                Ok(l) => l,
                Err(e) => {
                    tracing::warn!(connector = %name, driver = %cfg.plugin,
                        error = %format!("{e:#}"), "skipping a connector that will not load");
                    broken.insert(name.clone(), format!("{e:#}"));
                    continue;
                }
            };

            // 配置里的 `allow_exec` 并进 manifest 的白名单。
            //
            // 一个通用驱动（`ssh-socks5`）没法在自己的 manifest 里知道你会用
            // docker 还是 wg-quick 把代理拉起来，所以那份授权只能由**你**给。
            // manifest 里那份是插件作者说「我需要什么」，配置里这份是你说
            // 「我准你跑什么」——取并集，因为配置是你写的。
            for prog in &cfg.allow_exec {
                if !loaded.manifest.capabilities.local_exec.contains(prog) {
                    loaded.manifest.capabilities.local_exec.push(prog.clone());
                }
            }

            let mine: Vec<_> = registry
                .iter()
                .filter(|t| t.connector == *name)
                .cloned()
                .collect();
            let config_json = serde_json::to_string(&cfg.settings).unwrap_or_else(|_| "{}".into());

            let pool = match runtime
                .connector_pool(Arc::new(loaded), name.clone(), mine, config_json, policy)
                .await
            {
                Ok(p) => p,
                Err(e) => {
                    tracing::warn!(connector = %name, driver = %cfg.plugin,
                        error = %format!("{e:#}"), "skipping a connector that will not instantiate");
                    broken.insert(name.clone(), format!("{e:#}"));
                    continue;
                }
            };
            pools.insert(name.clone(), Arc::new(pool));
        }

        Ok(Self {
            store,
            pools,
            registry,
            broken,
        })
    }

    /// 巡检所有 connector 的实例池，把闲太久的实例还回去。返回收了几个。
    pub fn sweep_pools(&self) -> usize {
        self.pools.values().map(|p| p.sweep()).sum()
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
        if let Some(pool) = self.pools.get(connector) {
            return Ok(Arc::clone(pool));
        }
        // 装不上和压根没配是两回事。「未知的 connector」会让人去翻配置，
        // 而真正该做的是重编那个驱动。
        if let Some(why) = self.broken.get(connector) {
            return Err(TrestleError::ConnectorNotReady {
                connector: connector.to_string(),
                detail: format!("this connector's driver did not load: {why}"),
                remedy: "重新编一次插件（scripts/build-plugins.ps1），然后 trestle plugin reload"
                    .into(),
            });
        }
        Err(TrestleError::UnknownConnector {
            name: connector.to_string(),
            known: self.connector_names(),
        })
    }

    /// 装不上的 connector。`trestle doctor` 用它。
    pub fn broken(&self) -> &BTreeMap<String, String> {
        &self.broken
    }

    /// 把一个操作路由到 target 所属的 connector。
    pub async fn op(&self, target: &str, op: &str, payload: &str) -> Result<String> {
        let t = self.registry.resolve(target)?;
        let pool = self.pool(&t.connector)?;
        // 用解析后的**主名**调插件：插件只认主名，别名是 host 这一层的事。
        pool.pick().await.op(&t.name, op, payload).await
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

    #[test]
    fn a_connector_can_always_grow_to_at_least_two_instances() {
        // 池的上限就是「同一组机器能几路并发」。上限为 1 的机器等于没有并发，
        // fleet_status 会退化成整支机队排队——这条守着默认值不掉到那里去。
        assert!(
            PoolPolicy::default().max >= 2,
            "default pool ceiling is {}",
            PoolPolicy::default().max
        );
    }
}
