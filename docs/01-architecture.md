# 01 · 架构

## 全局图

```
   Claude Code (会话 A)      Claude Code (会话 B)         你的终端
          │                         │                        │
     MCP stdio                 MCP stdio                    CLI
          │                         │                        │
   ┌──────┴────────┐        ┌───────┴───────┐        ┌───────┴───────┐
   │ trestle-mcp   │        │ trestle-mcp   │        │   trestle     │   ← 瘦客户端
   └──────┬────────┘        └───────┬───────┘        └───────┬───────┘
          │                         │                        │
          └─────────────────────────┼────────────────────────┘
                                    │  IPC (localhost / named pipe)
                                    ▼
            ┌───────────────────────────────────────────────┐
            │                 trestled                      │   ← 常驻 daemon
            │                                               │      (lazy 启动)
            │   ToolRegistry ── Router ── EventBus ──► WS ──────► Monitor
            │        │                                      │
            │   ┌────┴─────┐                                │
            │   │ BaseSvc  │  read / write / edit / shell    │
            │   └────┬─────┘                                │
            │        │                                      │
            │   SessionPool ── 一台机器一条常驻连接         │
            │        │                                      │
            │   ConnectorRegistry                           │
            │     ├── direct-ssh                            │
            │     ├── socks5  (拉起 VPN 容器 → SOCKS5 dial) │
            │     └── <plugin>.wasm        (v0.3+)          │
            │        │                                      │
            │   PluginRegistry (tools)     (v0.3+)          │
            └────────┼──────────────────────────────────────┘
                     │
              ┌──────┴───────┬──────────┬──────────┐
              ▼              ▼          ▼          ▼
         gpu-1(VPN)          gpu-2         gpu-3        gpu-4
              │              │          │          │
         每台一个常驻 remote agent（静态二进制，自动部署）
```

## 三个设计支点

### 1. daemon 与前端分离

`trestle-mcp`（MCP stdio 服务）和 `trestle`（CLI）都是**瘦客户端**，真正的状态在 `trestled`。

这是从 Python 原型实测出来的教训：MCP server 由 Claude Code 按 stdio 拉起，**每个会话一个进程**，
于是每开一个会话就要重新建 4 条 SSH（gpu-1 经 VPN 冷启动 ~5s）。而 daemon 模式下：

* 连接**真正**只建一次，跨会话、跨 CLI 复用；
* Monitor 的 ws、后台 job、插件实例都有唯一归属，CLI 和 MCP 看到同一份状态；
* 满足你要的"一次 CLI 或一次 MCP 调用就启动后台常驻进程，lazy 加载"。

**lazy 启动**：任一客户端连不上 daemon 就自己 spawn 它（带锁防并发重复拉起），daemon 就绪后重试。
用户永远不需要手动 `trestled start`。

**idle 退出**：daemon 无客户端连接且无活跃 job 超过 N 分钟后自行退出（默认 30min，可配 0 表示常驻）。

> 代价：多一层 IPC，调试链路变长。见 `08-open-questions.md` Q1——如果你不接受这个复杂度，
> 备选是"MCP server 进程内嵌一切"，那样简单但回到 Python 版每会话重连的老问题。

### 2. base 能力只实现一次

```
                    BaseService  (read / write / edit / shell)
                          │
          ┌───────────────┼────────────────┐
          ▼               ▼                ▼
     MCP adapter     WIT adapter      CLI adapter
     base_read()     base.read()      trestle read
```

MCP 工具、WASM 插件、CLI 三条路径调的是**同一份实现**。插件不会有"第二套 read"。

### 3. Connector 与 Tool 正交

```
Connector : Target → Session          「怎么进去」
Tool      : Session → 领域动作         「进去干什么」
```

Connector 的最小契约其实只有一件事——**给我一个能连到目标的字节流**：

```rust
#[async_trait]
pub trait Connector: Send + Sync {
    fn name(&self) -> &str;
    /// 确保前置条件就绪（拨 VPN、拉容器、刷新凭据…），幂等，可被反复调用
    async fn ensure_ready(&self) -> Result<()>;
    /// 返回一条已连到 (host, port) 的双向流
    async fn dial(&self, host: &str, port: u16) -> Result<Box<dyn Stream>>;
}
```

于是 **VPN 不是特例，而是一个 dialer 装饰器**：`socks5` connector 的 `ensure_ready` 负责把
VPN 容器拉起来、`dial` 负责走 SOCKS5 CONNECT；SSH 层拿到流之后完全不关心它从哪来。
这正是 Python 原型里跑通的语义，Rust 版只是把它显式化成 trait。

## Crate 划分

```
trestle/
├── crates/
│   ├── trestle-core        # Target/Connector/Session/Tool 抽象、错误类型、事件定义
│   ├── trestle-connectors  # direct-ssh、socks5（内置实现）
│   ├── trestle-base        # BaseService：read/write/edit/shell 的唯一实现
│   ├── trestle-agent       # 远端常驻 agent（编译成静态二进制推到服务器上）
│   ├── trestle-daemon      # trestled：SessionPool、Registry、EventBus、WS、IPC server
│   ├── trestle-mcp         # MCP stdio 前端（rmcp），瘦客户端
│   ├── trestle-cli         # trestle 命令行，瘦客户端
│   └── trestle-plugin-host # Wasmtime + WIT 宿主 (v0.3+，先留空壳)
├── wit/                    # 插件接口定义
├── config/
└── docs/
```

依赖方向严格单向：`core ← {connectors, base, daemon}`，`daemon ← {mcp, cli}`（经 IPC，不是直接链接）。
`trestle-agent` **不依赖** core 以外的任何东西——它要能静态编译成一个小二进制。

## 一次调用的完整数据流

以 `docker_logs(target="gpu-4", container="foo")` 为例（v0.3 有插件之后）：

```
Claude Code
   │ tools/call docker_logs
   ▼
trestle-mcp ──IPC──► trestled
                        │ Router: 前缀 docker.* → PluginRegistry
                        │ 插件未实例化 → lazy instantiate docker.wasm
                        ▼
                    docker.wasm 调用 host 导入的 base.shell(target, "docker logs foo")
                        │
                        ▼
                    BaseService.shell
                        │ SessionPool.get("gpu-4")
                        │   └─ 无连接 → Connector("socks5").ensure_ready() → dial → SSH → 部署/拉起 agent
                        ▼
                    remote agent 执行，JSON-Lines 回传
                        │
                        ├──► EventBus: ShellStarted/ShellFinished ──► WS ──► Monitor
                        ▼
                    结果回 MCP
```

注意插件**没有**自己的 SSH 能力，它只能通过 host 导入的 base 能力做事——这是权限模型成立的前提
（见 `03-plugins-wit.md`）。

## 远端侧

每台机器上跑一个 **remote agent**：常驻进程，一条连接上跑 JSON-Lines 请求/响应，多路复用 + 并发。
这是 Python 原型验证过的形态，实测**冷启动 0.7–5s、之后每次调用 33–52ms**。

为什么要有远端 agent，而不是每次 `ssh host "command"`：

* **每次新建 SSH exec channel + 起一个进程**，跨 VPN 时是几百 ms 起步，agent 模式是一次 RTT；
* `edit` 这类原语必须在**远端本地**执行才有意义（否则要把整个文件拉过来改完再传回去）；
* 后台 job 的 pid/退出码/日志需要一个有状态的守护者。

形态选择（**待拍板，见 Q2**）：静态编译的 Rust 二进制（musl，单文件，无运行时依赖）是推荐项，
按内容哈希幂等部署——本地算 hash，远端比对不一致才重传。
