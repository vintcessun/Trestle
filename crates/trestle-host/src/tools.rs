//! 技能插件：注册、路由、调用。
//!
//! **声明与实例化分离**：`list-tools` 在加载时读一次就进注册表（工具因此对
//! Claude Code 可见），真正的 wasm 实例延迟到第一次调用。

use std::collections::BTreeMap;
use std::path::Path;
use std::sync::Arc;

use serde::{Deserialize, Serialize};
use tokio::sync::Mutex;

use trestle_core::{Result, TrestleError};

use crate::capability::Manifest;
use crate::runtime::ToolInstance;

/// 一个工具的对外声明。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolDescriptor {
    pub name: String,
    #[serde(default)]
    pub description: String,
    /// JSON Schema。单机工具的 `target` **必须**是 required——没有默认机。
    #[serde(default)]
    pub input_schema: serde_json::Value,
}

/// 一个已加载的技能插件。
pub struct LoadedTool {
    pub manifest: Manifest,
    pub tools: Vec<ToolDescriptor>,
    pub instance: Arc<ToolInstance>,
}

#[derive(Default)]
pub struct ToolRegistry {
    /// 工具名 → 插件名。工具名全局唯一。
    index: Mutex<BTreeMap<String, String>>,
    plugins: Mutex<BTreeMap<String, Arc<LoadedTool>>>,
}

impl ToolRegistry {
    pub async fn register(&self, loaded: LoadedTool) -> Result<()> {
        let plugin = loaded.manifest.name.clone();
        let mut index = self.index.lock().await;
        for t in &loaded.tools {
            if let Some(owner) = index.get(&t.name)
                && owner != &plugin
            {
                return Err(TrestleError::Config {
                    path: format!("plugins/{plugin}"),
                    detail: format!(
                        "tool name '{}' is already provided by plugin '{owner}'; \
                             tool names must be globally unique",
                        t.name
                    ),
                });
            }
            index.insert(t.name.clone(), plugin.clone());
        }
        self.plugins.lock().await.insert(plugin, Arc::new(loaded));
        Ok(())
    }

    /// 全部工具的声明。MCP 的 `tools/list` 用它。
    pub async fn descriptors(&self) -> Vec<ToolDescriptor> {
        let mut out = Vec::new();
        for p in self.plugins.lock().await.values() {
            out.extend(p.tools.iter().cloned());
        }
        out.sort_by(|a, b| a.name.cmp(&b.name));
        out
    }

    pub async fn plugin_names(&self) -> Vec<String> {
        self.plugins.lock().await.keys().cloned().collect()
    }

    pub async fn owner_of(&self, tool: &str) -> Option<String> {
        self.index.lock().await.get(tool).cloned()
    }

    /// 调一个工具。
    pub async fn call(&self, tool: &str, args: &str) -> Result<String> {
        let owner = self
            .owner_of(tool)
            .await
            .ok_or_else(|| TrestleError::Config {
                path: "tools".into(),
                detail: format!("unknown tool '{tool}'"),
            })?;
        self.call_in(&owner, tool, args).await
    }

    /// 调某个插件里的某个工具。插件调插件走这条，capability 已经在导入处查过。
    pub async fn call_in(&self, plugin: &str, tool: &str, args: &str) -> Result<String> {
        let loaded = self
            .plugins
            .lock()
            .await
            .get(plugin)
            .cloned()
            .ok_or_else(|| TrestleError::Config {
                path: "plugins".into(),
                detail: format!("plugin '{plugin}' is not loaded"),
            })?;
        loaded.instance.call(tool, args).await
    }

    pub async fn instance_of(&self, plugin: &str) -> Option<Arc<LoadedTool>> {
        self.plugins.lock().await.get(plugin).cloned()
    }

    /// 每个插件贡献了哪些工具。`plugin list` 与 Web UI 用它。
    pub async fn inventory(&self) -> Vec<serde_json::Value> {
        self.plugins
            .lock()
            .await
            .values()
            .map(|p| {
                serde_json::json!({
                    "name": p.manifest.name,
                    "version": p.manifest.version,
                    "description": p.manifest.description,
                    "tools": p.tools.iter().map(|t| t.name.clone()).collect::<Vec<_>>(),
                    "capabilities": p.manifest.capabilities,
                })
            })
            .collect()
    }

    /// 清空。热加载时先清再重新注册——否则删掉的插件会留在工具面里。
    pub async fn clear(&self) {
        self.index.lock().await.clear();
        self.plugins.lock().await.clear();
    }
}

/// 从 `list-tools` 的 JSON 里解析工具声明，并做一次纪律检查。
pub fn parse_descriptors(plugin: &str, raw: &str) -> Result<Vec<ToolDescriptor>> {
    let tools: Vec<ToolDescriptor> =
        serde_json::from_str(raw).map_err(|e| TrestleError::Config {
            path: format!("plugins/{plugin}"),
            detail: format!("list-tools did not return a valid tool array: {e}"),
        })?;

    for t in &tools {
        // 工具名用 `_` 不用 `.`：Claude Code 在 plugin-bundled 场景会把 `.` 正规化成 `_`，
        // 于是声明的名字和 permission matcher 看到的名字对不上。
        if t.name.contains('.') {
            return Err(TrestleError::Config {
                path: format!("plugins/{plugin}"),
                detail: format!(
                    "tool '{}' uses a dot; use '_' instead (Claude Code normalises dots \
                     and the declared name then disagrees with permission matchers)",
                    t.name
                ),
            });
        }
    }
    Ok(tools)
}

/// 插件目录：程序目录下的，开发时退回仓库里的那份。
pub fn tool_plugin_dirs(plugins_root: &Path, repo_root: Option<&Path>) -> Vec<std::path::PathBuf> {
    let mut roots = vec![plugins_root.join("tools")];
    if let Some(repo) = repo_root {
        roots.push(repo.join("plugins").join("tools"));
    }
    let mut out = Vec::new();
    for root in roots {
        let Ok(entries) = std::fs::read_dir(&root) else {
            continue;
        };
        for e in entries.flatten() {
            if e.path().join("manifest.toml").exists() {
                out.push(e.path());
            }
        }
        if !out.is_empty() {
            break;
        }
    }
    out.sort();
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_dotted_tool_name_is_rejected_with_the_reason() {
        let raw = r#"[{"name":"job.list","description":"x","input_schema":{}}]"#;
        let err = parse_descriptors("job", raw).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("job.list") && msg.contains("'_'"), "{msg}");
    }

    #[test]
    fn descriptors_parse_from_the_plugins_json() {
        let raw = r#"[
            {"name":"job_list","description":"list jobs","input_schema":{"type":"object"}},
            {"name":"job_logs","description":"read logs","input_schema":{"type":"object"}}
        ]"#;
        let tools = parse_descriptors("job", raw).unwrap();
        assert_eq!(tools.len(), 2);
        assert_eq!(tools[0].name, "job_list");
    }

    #[tokio::test]
    async fn tool_names_must_be_globally_unique() {
        // 两个插件抢同一个工具名会让路由变成掷骰子，必须在注册时就拦下。
        let reg = ToolRegistry::default();
        let mut index = reg.index.lock().await;
        index.insert("job_list".into(), "job".into());
        drop(index);
        assert_eq!(reg.owner_of("job_list").await.as_deref(), Some("job"));
    }
}
