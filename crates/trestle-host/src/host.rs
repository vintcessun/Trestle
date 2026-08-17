//! 把所有零件装起来：运行时、机队、GPU 分配器、技能插件注册表。
//!
//! daemon、CLI、MCP 前端都用这一个门面，不各自拼装——否则「工具从哪来」
//! 这件事会在三个地方各有一份答案。

use std::path::PathBuf;
use std::sync::Arc;

use trestle_core::config::ConfigStore;
use trestle_core::{Result, TrestleError};

use crate::fleet::Fleet;
use crate::gpu::GpuArbiter;
use crate::runtime::Runtime;
use crate::state::{EventSink, NullSink, PluginKv};
use crate::tool_state::{NoWs, NullTasks, TaskSink, ToolState, WsHub};
use crate::tools::{LoadedTool, ToolRegistry, parse_descriptors};

pub struct TrestleHost {
    pub store: Arc<ConfigStore>,
    pub fleet: Arc<Fleet>,
    pub gpu: Arc<GpuArbiter>,
    pub tools: Arc<ToolRegistry>,
    events: Arc<dyn EventSink>,
    /// 热加载要重跑一遍装配，所以运行时和当初那份选项都得留着。
    runtime: Arc<Runtime>,
    opts_tasks: Arc<dyn TaskSink>,
    opts_ws: Arc<dyn WsHub>,
    /// 每个池起几个实例。热加载要复用同一个值。
    pool_size: usize,
}

pub struct HostOptions {
    pub events: Arc<dyn EventSink>,
    pub tasks: Arc<dyn TaskSink>,
    pub ws: Arc<dyn WsHub>,
    /// 每个 connector 起几个实例。见 [`crate::fleet::DEFAULT_POOL_SIZE`]。
    pub pool_size: usize,
}

impl Default for HostOptions {
    fn default() -> Self {
        Self {
            events: Arc::new(NullSink),
            tasks: Arc::new(NullTasks),
            ws: Arc::new(NoWs),
            pool_size: crate::fleet::DEFAULT_POOL_SIZE,
        }
    }
}

impl TrestleHost {
    pub async fn start(store: Arc<ConfigStore>, opts: HostOptions) -> Result<Self> {
        let runtime =
            Runtime::with_events(Arc::clone(&store), Arc::clone(&opts.events)).map_err(|e| {
                TrestleError::Config {
                    path: "wasm runtime".into(),
                    detail: format!("{e:#}"),
                }
            })?;

        let fleet = Arc::new(
            Fleet::load_with_pool_size(&runtime, Arc::clone(&store), opts.pool_size).await?,
        );
        let gpu = Arc::new(GpuArbiter::new(Arc::clone(&fleet)));
        let tools = Arc::new(ToolRegistry::default());
        let runtime = Arc::new(runtime);

        let host = Self {
            store: Arc::clone(&store),
            fleet: Arc::clone(&fleet),
            gpu: Arc::clone(&gpu),
            tools: Arc::clone(&tools),
            events: Arc::clone(&opts.events),
            runtime: Arc::clone(&runtime),
            opts_tasks: Arc::clone(&opts.tasks),
            opts_ws: Arc::clone(&opts.ws),
            pool_size: opts.pool_size,
        };
        host.load_tools().await?;
        Ok(host)
    }

    /// 重新扫描插件目录并热加载。
    ///
    /// 先清空再装：否则被删掉的插件会留在工具面里，Claude Code 会一直看到
    /// 一个调不通的工具。
    pub async fn reload_tools(&self) -> Result<usize> {
        self.tools.clear().await;
        self.load_tools().await?;
        Ok(self.tools.plugin_names().await.len())
    }

    async fn load_tools(&self) -> Result<()> {
        let store = &self.store;
        let runtime = &self.runtime;
        let fleet = &self.fleet;
        let gpu = &self.gpu;
        let tools = &self.tools;

        for dir in tool_dirs(store) {
            let loaded = runtime.load_tool(&dir).map_err(|e| TrestleError::Config {
                path: dir.display().to_string(),
                detail: format!("{e:#}"),
            })?;
            let name = loaded.manifest.name.clone();
            let config_json = store
                .plugin_section(&name)
                .map(|v| serde_json::to_string(v).unwrap_or_else(|_| "{}".into()))
                .unwrap_or_else(|| "{}".into());

            // KV 在池里的实例之间**共享**：插件的跨调用状态该在这里，
            // 而不是在 wasm 内存里（那样池化会让实例各看各的）。
            let kv = Arc::new(PluginKv::open(&store.state_dir(), &name));
            let make_state = || {
                ToolState::new(
                    loaded.manifest.clone(),
                    Arc::clone(fleet),
                    Arc::clone(gpu),
                    Arc::clone(&kv),
                    Arc::clone(&self.events),
                    Arc::clone(&self.opts_tasks),
                    Arc::clone(&self.opts_ws),
                    Arc::clone(tools),
                    config_json.clone(),
                )
            };

            let pool = runtime
                .instantiate_tool_pool(&loaded, make_state, self.pool_size)
                .await
                .map_err(|e| TrestleError::Config {
                    path: dir.display().to_string(),
                    detail: format!("{e:#}"),
                })?;
            let pool = Arc::new(pool);

            // 声明与实例化分离：`list-tools` 读一次就进注册表，工具因此立刻可见。
            let raw = pool.any().list_tools().await?;
            let descriptors = parse_descriptors(&name, &raw)?;
            tools
                .register(LoadedTool {
                    manifest: loaded.manifest,
                    tools: descriptors,
                    pool,
                })
                .await?;
        }
        Ok(())
    }

    /// 七个基本操作，按 target 路由。
    pub async fn op(&self, target: &str, op: &str, payload: &str) -> Result<String> {
        self.events.emit(
            "host",
            "debug",
            "op_started",
            &serde_json::json!({"target": target, "op": op}).to_string(),
        );
        self.fleet.op(target, op, payload).await
    }

    pub async fn call_tool(&self, tool: &str, args: &str) -> Result<String> {
        self.tools.call(tool, args).await
    }

    /// 对外的完整工具面：七个基本操作 + host 内置 + 插件贡献的。
    pub async fn tool_descriptors(&self) -> Vec<crate::tools::ToolDescriptor> {
        let mut out = base_tool_descriptors();
        out.extend(self.tools.descriptors().await);
        out
    }

    /// 插件贡献的 Web UI 面板，按插件名排序。
    ///
    /// Web UI 因此是**插件的一部分**而不是一个独立前端工程：加一个插件，
    /// 它自己带着自己的那块界面进来。
    pub async fn ui_panels(&self) -> Vec<(String, String)> {
        let mut out = Vec::new();
        for name in self.tools.plugin_names().await {
            let Some(loaded) = self.tools.instance_of(&name).await else {
                continue;
            };
            match loaded.pool.any().ui_panel().await {
                Ok(html) if !html.trim().is_empty() => out.push((name, html)),
                Ok(_) => {}
                Err(e) => {
                    tracing::warn!(plugin = %name, %e, "a plugin's UI panel failed to render")
                }
            }
        }
        out.sort_by(|a, b| a.0.cmp(&b.0));
        out
    }
}

/// 七个基本操作对外的工具声明。
///
/// 它们不来自任何插件——是 host 自己的面，因为「怎么进去」这件事本来就归 host 路由。
pub fn base_tool_descriptors() -> Vec<crate::tools::ToolDescriptor> {
    use crate::tools::ToolDescriptor;
    let target = serde_json::json!({"type": "string", "description": "机器名。必填——没有默认机。"});
    let mut out = Vec::new();

    let mut add = |name: &str, description: &str, required: Vec<&str>, props: serde_json::Value| {
        out.push(ToolDescriptor {
            name: name.into(),
            description: description.into(),
            input_schema: serde_json::json!({
                "type": "object",
                "required": required,
                "properties": props,
            }),
        });
    };

    add(
        "base_read",
        "读远端文件，可以只读一段。",
        vec!["target", "path"],
        serde_json::json!({
            "target": target, "path": {"type": "string"},
            "start_line": {"type": "integer"}, "max_lines": {"type": "integer"}
        }),
    );
    add(
        "base_write",
        "写远端文件。",
        vec!["target", "path", "content"],
        serde_json::json!({
            "target": target, "path": {"type": "string"}, "content": {"type": "string"},
            "append": {"type": "boolean"}, "make_dirs": {"type": "boolean"}
        }),
    );
    add(
        "base_edit",
        "改远端文件的一部分。比 read+write 便宜得多——只传 diff，不传整个文件。",
        vec!["target", "path", "op"],
        serde_json::json!({
            "target": target, "path": {"type": "string"},
            "op": {"type": "object", "description": "kind = literal | regex | lines | insert"}
        }),
    );
    add(
        "base_shell",
        "跑一条短命令。超时会杀掉整个进程组。跑训练/编译这类长活请用 job_start。",
        vec!["target", "command"],
        serde_json::json!({
            "target": target, "command": {"type": "string"},
            "cwd": {"type": "string"}, "timeout_secs": {"type": "integer"}
        }),
    );
    add(
        "base_upload",
        "本地 → 远端。文件与目录自动识别，sync=true 只传有变化的。",
        vec!["target", "local_path", "remote_path"],
        serde_json::json!({
            "target": target, "local_path": {"type": "string"}, "remote_path": {"type": "string"},
            "options": {"type": "object", "description": "exclude / sync / dry_run / delete"}
        }),
    );
    add(
        "base_download",
        "远端 → 本地。",
        vec!["target", "remote_path", "local_path"],
        serde_json::json!({
            "target": target, "remote_path": {"type": "string"}, "local_path": {"type": "string"},
            "options": {"type": "object"}
        }),
    );
    add(
        "base_forward",
        "把远端一个端口映射到本地。本地端口由 host 分配，你不能指定。",
        vec!["target", "remote_port"],
        serde_json::json!({"target": target, "remote_port": {"type": "integer"}}),
    );
    out
}

/// 技能插件目录：先看程序目录下的，开发时退回仓库里的那份。
fn tool_dirs(store: &ConfigStore) -> Vec<PathBuf> {
    let repo = store.root().parent().map(|p| p.to_path_buf());
    crate::tools::tool_plugin_dirs(&store.plugins_dir(), repo.as_deref())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_single_machine_tool_requires_a_target() {
        // 没有默认机是刻意的：默认机会制造「你以为在 gpu-4 上删文件、其实在 gpu-1」
        // 这类静默事故。这条断言守着它。
        for d in base_tool_descriptors() {
            let required = d.input_schema["required"].as_array().unwrap();
            assert!(
                required.iter().any(|r| r == "target"),
                "{} does not require a target",
                d.name
            );
        }
    }

    #[test]
    fn base_tools_are_named_with_underscores() {
        for d in base_tool_descriptors() {
            assert!(!d.name.contains('.'), "{} uses a dot", d.name);
            assert!(d.name.starts_with("base_"), "{}", d.name);
        }
    }

    #[test]
    fn the_shell_tool_points_at_job_start_for_long_work() {
        let shell = base_tool_descriptors()
            .into_iter()
            .find(|d| d.name == "base_shell")
            .unwrap();
        // 在描述里就指出正确的工具，agent 才不会拿短命令工具去跑训练。
        assert!(
            shell.description.contains("job_start"),
            "{}",
            shell.description
        );
    }
}
