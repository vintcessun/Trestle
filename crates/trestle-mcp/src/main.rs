//! MCP stdio 前端。**瘦客户端**：把 `tools/call` 转成一次 IPC 调用，就这样。
//!
//! 它不持有任何连接、任何插件实例、任何状态——那些都在 `trestled` 里。
//! 这正是 daemon 存在的理由：Claude Code 每开一个会话就拉起一个这样的进程，
//! 如果状态在这里，每个会话都要把全部连接重建一遍（gpu-1 经 VPN 是数秒）。
//!
//! 工具面是**动态**的（插件贡献），所以这里直接实现 `ServerHandler` 而不是用
//! `#[tool]` 宏——宏那条路是给编译期就定死的工具集用的。

use std::borrow::Cow;
use std::sync::Arc;

use rmcp::handler::server::ServerHandler;
use rmcp::model::{
    CallToolRequestParams, CallToolResponse, CallToolResult, ContentBlock, Implementation,
    ListToolsResult, PaginatedRequestParams, ServerCapabilities, ServerInfo, Tool,
};
use rmcp::service::RequestContext;
use rmcp::{ErrorData as McpError, RoleServer, ServiceExt};

use trestle_core::config::ConfigStore;
use trestle_daemon::ipc::{IpcClient, Notification, RequestBody};

struct TrestleMcp {
    client: Arc<IpcClient>,
    agent: String,
}

impl TrestleMcp {
    async fn start() -> anyhow::Result<Self> {
        let root = ConfigStore::default_root();
        let client = Arc::new(trestle_mcp_bootstrap::connect_or_spawn(&root).await?);
        let hello = client
            .call(RequestBody::Hello {
                label: session_label(),
            })
            .await?;
        let agent = hello["agent"].as_str().unwrap_or("mcp").to_string();
        Ok(Self { client, agent })
    }
}

/// 给这个会话起个能认出来的名字，让别的 agent 在 `agents_list` 里看到有意义的东西。
fn session_label() -> String {
    let cwd = std::env::current_dir()
        .ok()
        .and_then(|p| p.file_name().map(|n| n.to_string_lossy().into_owned()))
        .unwrap_or_else(|| "?".into());
    format!("claude-code:{cwd}")
}

impl ServerHandler for TrestleMcp {
    fn get_info(&self) -> ServerInfo {
        // 这些类型是 #[non_exhaustive]：只能从 Default 起手再改字段，
        // 不能用结构体字面量整个构造。
        let mut implementation = Implementation::default();
        implementation.name = "trestle".into();
        implementation.version = env!("CARGO_PKG_VERSION").into();

        let mut info = ServerInfo::default();
        info.capabilities = ServerCapabilities::builder().enable_tools().build();
        info.server_info = implementation;
        info.instructions = Some(
            "Trestle：面向多台远程服务器的基础设施运行时。\n\
             · 每个针对单机的工具都必须显式给 `target`——没有默认机，这是刻意的。\n\
             · 短命令用 base_shell；训练/编译这类长活用 job_start，否则会撞超时。\n\
             · 多个 agent 可能同时在用这些机器：agents_list 看谁在干什么，\n\
               notes_put 留一句话说明你占着什么。"
                .into(),
        );
        info
    }

    async fn list_tools(
        &self,
        _request: Option<PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> Result<ListToolsResult, McpError> {
        let raw = self
            .client
            .call(RequestBody::ListTools)
            .await
            .map_err(to_mcp)?;
        let descriptors: Vec<trestle_host::tools::ToolDescriptor> = serde_json::from_value(raw)
            .map_err(|e| McpError::internal_error(e.to_string(), None))?;

        let tools: Vec<Tool> = descriptors
            .into_iter()
            .map(|d| {
                Tool::new(
                    Cow::Owned(d.name),
                    Cow::Owned(d.description),
                    Arc::new(d.input_schema.as_object().cloned().unwrap_or_default()),
                )
            })
            .collect();
        // ListToolsResult 是 #[non_exhaustive]，只能从 Default 起手再改字段。
        Ok(ListToolsResult {
            tools,
            ..Default::default()
        })
    }

    async fn call_tool(
        &self,
        request: CallToolRequestParams,
        _context: RequestContext<RoleServer>,
    ) -> Result<CallToolResponse, McpError> {
        let name = request.name.to_string();
        let args = serde_json::to_string(&request.arguments.unwrap_or_default())
            .unwrap_or_else(|_| "{}".into());

        // 七个基本操作走 op；其余走插件。
        let body = match name.strip_prefix("base_") {
            Some(op) => {
                let v: serde_json::Value = serde_json::from_str(&args).unwrap_or_default();
                let target = v["target"].as_str().unwrap_or_default().to_string();
                if target.is_empty() {
                    return Ok(CallToolResult::error(vec![ContentBlock::text(
                        "this tool needs a `target`; there is no default machine (that is deliberate — \
                         a default machine is how you end up deleting files on the wrong box)",
                    )])
                    .into());
                }
                RequestBody::Op {
                    agent: self.agent.clone(),
                    target,
                    op: op.to_string(),
                    payload: args,
                }
            }
            None => RequestBody::CallTool {
                agent: self.agent.clone(),
                tool: name,
                args,
            },
        };

        match self.client.call(body).await {
            Ok(value) => Ok(CallToolResult::success(vec![ContentBlock::text(
                serde_json::to_string_pretty(&value).unwrap_or_else(|_| value.to_string()),
            )])
            .into()),
            // 失败也是内容而不是协议错误：错误消息本身就是给 agent 读的产物，
            // 包成协议错误反而会让它看不到 remedy。
            Err(e) => Ok(CallToolResult::error(vec![ContentBlock::text(e.to_string())]).into()),
        }
    }
}

fn to_mcp(e: trestle_core::TrestleError) -> McpError {
    McpError::internal_error(e.to_string(), None)
}

/// lazy 启动：连不上就自己把 daemon 拉起来。
mod trestle_mcp_bootstrap {
    use std::path::Path;
    use std::time::Duration;

    use trestle_daemon::ipc::{DaemonInfo, IpcClient};

    pub async fn connect_or_spawn(root: &Path) -> anyhow::Result<IpcClient> {
        if let Ok(client) = IpcClient::connect(root).await {
            return Ok(client);
        }

        // 拉起 daemon。用户永远不需要手动 `trestled start`。
        let exe = std::env::current_exe()?
            .parent()
            .map(|d| {
                d.join(if cfg!(windows) {
                    "trestled.exe"
                } else {
                    "trestled"
                })
            })
            .ok_or_else(|| anyhow::anyhow!("cannot locate trestled next to this binary"))?;

        let mut cmd = std::process::Command::new(&exe);
        cmd.arg("--home").arg(root);
        cmd.stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null());
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            // DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP：别让 daemon 跟着
            // 这个 MCP 进程一起死——它要活得比任何一个会话都长。
            cmd.creation_flags(0x0000_0008 | 0x0000_0200);
        }
        cmd.spawn()
            .map_err(|e| anyhow::anyhow!("cannot start {}: {e}", exe.display()))?;

        // 等它就绪。启动要加载全部插件，给足时间。
        let deadline = std::time::Instant::now() + Duration::from_secs(30);
        let mut wait = Duration::from_millis(120);
        while std::time::Instant::now() < deadline {
            tokio::time::sleep(wait).await;
            if DaemonInfo::read(root).is_some()
                && let Ok(client) = IpcClient::connect(root).await
            {
                return Ok(client);
            }
            wait = (wait * 2).min(Duration::from_millis(700));
        }
        Err(anyhow::anyhow!(
            "started {} but it never became reachable; run it in the foreground to see why",
            exe.display()
        ))
    }
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // 日志必须走 stderr：stdout 是 MCP 的协议流，往里写一个字节就毁掉整条流。
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_env("TRESTLE_LOG")
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("warn")),
        )
        .with_writer(std::io::stderr)
        .init();

    let server = TrestleMcp::start().await?;
    let mut notifications = server.client.subscribe();

    let service = server.serve(rmcp::transport::stdio()).await?;

    // 插件热加载之后把 `tools/list_changed` 转出去，Claude Code 因此**不用重连**
    // 就能看到新工具。它自己不知道什么时候该发，所以要 daemon 推。
    let peer = service.peer().clone();
    tokio::spawn(async move {
        while let Ok(n) = notifications.recv().await {
            match n {
                Notification::ToolsChanged => {
                    if let Err(e) = peer.notify_tool_list_changed().await {
                        tracing::warn!(%e, "could not tell the client the tool list changed");
                    }
                }
            }
        }
    });

    service.waiting().await?;
    Ok(())
}
