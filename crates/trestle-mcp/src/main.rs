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
///
/// 前端是谁由 `TRESTLE_AGENT` 说（安装脚本给每个客户端各设一个）。硬编码成
/// `claude-code` 会让 Codex 的会话在留言板上冒充 Claude Code——多 agent 协同的
/// 全部意义就是知道对面是谁，这里说错就白搭了。
fn session_label() -> String {
    let who = std::env::var("TRESTLE_AGENT")
        .ok()
        .filter(|s| !s.trim().is_empty())
        .unwrap_or_else(|| "mcp".into());
    let cwd = std::env::current_dir()
        .ok()
        .and_then(|p| p.file_name().map(|n| n.to_string_lossy().into_owned()))
        .unwrap_or_else(|| "?".into());
    format!("{who}:{cwd}")
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
        // 这段每个会话都会被注入，所以它得配得上占的位置：只放**不知道就会做错**
        // 的事，不放能从工具描述里读出来的东西。
        //
        // Claude Code 那边还有一个 skill 讲得更细，但 Codex 之类没有 skill——
        // 对它们来说这里就是唯一的机会。
        info.instructions = Some(
            "Trestle：面向多台远程服务器的基础设施运行时。你自己的本地命令与文件工具\
             对这些机器无效，远端的活儿都从这里走。\n\
             · 先看 targets_list：机器叫什么、`note` 里写了什么（哪个盘满了、东西该放哪）。\n\
             · 每个针对单机的工具都必须显式给 `target`——**没有默认机**，这是刻意的。\n\
             · 短命令用 base_shell；训练/编译这类长活用 job_start，否则会撞超时被杀掉，\n\
               然后用 job_logs / job_wait / job_stop 跟进。\n\
             · 要 GPU 用 gpu_acquire（或 job_start 的 gpus=\"auto:N\"），别自己看 nvidia-smi\n\
               然后开跑——那正是两个 agent 抢同一张卡的方式。\n\
             · 跨机传文件用 xfer_between，不要「下到本地再传上去」。\n\
             · 多个 agent 可能同时在用这些机器：agents_list 看谁在干什么，\n\
               note_put 说明你占着什么（ttl_secs 必填）。\n\
             · 错误消息分两段：发生了什么 + 下一步做什么，值得读完。看到 unknown state\n\
               不要重试——请求可能已经执行过了，先查状态。"
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
        //
        // `ttl_ms` / `cache_scope` 必须显式给：Default 把它们留成 None，而 None 是
        // 不序列化的；但我们同时在报 `resultType: complete`（新规范的形状），
        // 于是 Claude Code 按 2026-07-28 的 schema 校验，发现这两个字段不见了，
        // 整个 tools/list 就被判无效——服务器"连上了但一个工具都没有"。
        //
        // **ttl 是 0：这份清单不许缓存。** 工具面是动态的（插件热加载会改它），
        // 缓存住就等于 `plugin reload` 之后还得重连，而那正是我们花力气避免的事。
        // private：它取决于这台机器的配置与插件，不是谁都能共用的东西。
        let mut result = ListToolsResult::with_all_items(tools);
        result.ttl_ms = Some(0);
        result.cache_scope = Some(rmcp::model::CacheScope::Private);
        Ok(result)
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
            // 协同层也是 host 自己的面，不经过插件。
            None if name == "agents_list" => RequestBody::Agents,
            None if name == "notes_list" => {
                let v: serde_json::Value = serde_json::from_str(&args).unwrap_or_default();
                RequestBody::Notes {
                    scope: v["only_scope"].as_str().map(str::to_string),
                }
            }
            None if name == "note_put" => {
                let v: serde_json::Value = serde_json::from_str(&args).unwrap_or_default();
                // TTL 必填，而且必须在这里挡住：留言板一旦允许「永不过期」，
                // 几个星期后它就是一堆没人看的垃圾。
                let Some(ttl_secs) = v["ttl_secs"].as_u64() else {
                    return Ok(CallToolResult::error(vec![ContentBlock::text(
                        "note_put needs `ttl_secs`; a note without an expiry is how a \
                         noticeboard turns into a junk drawer nobody reads",
                    )])
                    .into());
                };
                RequestBody::PutNote {
                    agent: self.agent.clone(),
                    scope: v["scope"].as_str().unwrap_or_default().to_string(),
                    text: v["text"].as_str().unwrap_or_default().to_string(),
                    ttl_secs,
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

        // 等它就绪。**第一次**要把全部插件从头编一遍——componentize-py 产出的那个
        // 组件是 18 MB，冷缓存下这一步就要一分多钟，而它恰好发生在「刚装完、
        // 第一次用」的时刻。给 30 秒等于保证第一次必然失败。之后有编译缓存，
        // 都是一两秒。
        let deadline = std::time::Instant::now() + Duration::from_secs(180);
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
            "started {} but it never became reachable within 180s.
             If this is the first run after installing, it was probably still compiling              plugins. Run it with --foreground to watch.",
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
