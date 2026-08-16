# 02 · 核心抽象

四个类型撑起整个系统。写代码时如果发现某个东西不属于这四个之一，先停下来想想它是不是不该存在。

```
Target       一台可达的机器（配置里的一项）
Connector    Target → Session 的方法      「怎么进去」
Session      一条活着的连接 + 远端 agent 句柄
Tool         Session 上的领域动作          「进去干什么」
```

---

## Target

```rust
pub struct Target {
    pub name: String,            // "gpu-4" —— 全局唯一，工具调用里用它
    pub host: String,
    pub port: u16,
    pub user: String,
    pub connector: String,       // 绑定的 connector 名，如 "lab-vpn"
    pub workdir: String,         // 默认工作目录
    pub aliases: Vec<String>,    // IP / hostname / 别名，解析时也认
    pub note: String,            // 给 agent 看的用途说明（会进 fleet 概览）
    pub agent_dir: String,       // 远端 agent 落点，默认 ~/.trestle
}
```

**没有默认机。** 每个针对单机的工具，`target` 都是必填参数。

这条是上一代实测后由用户拍板的：默认机会制造"打错机器"这类静默事故——你以为在 gpu-4 上删文件，
其实在 gpu-1。多写一个词，换掉一整类事故。面向全队的操作（状态概览、广播、找空闲卡）用可选的
`targets`，留空表示全部——那是"全队"语义，不是"默认机"。

解析规则：名字 → 别名 → host 精确匹配。解析失败的错误消息里**必须列出所有可选名字**
（`unknown target 'x36'; known: gpu-1, gpu-2, gpu-3, gpu-4`），这在 agent 手里比"未找到"有用得多。

---

## Connector

```rust
#[async_trait]
pub trait Connector: Send + Sync {
    fn name(&self) -> &str;

    /// 幂等地确保前置条件就绪：拨 VPN、拉起容器、刷新凭据……
    /// 会在每次 dial 前被调用，实现方自己做缓存（别每次真的去查 docker）。
    async fn ensure_ready(&self) -> Result<()>;

    /// 返回一条已经连到 (host, port) 的双向流。SSH 层不关心它从哪来。
    async fn dial(&self, host: &str, port: u16) -> Result<Box<dyn AsyncStream>>;

    /// 健康自检，给 `trestle doctor` 用
    async fn diagnose(&self) -> ConnectorHealth;
}
```

内置两个：

| connector | ensure_ready | dial |
|---|---|---|
| `direct-ssh` | no-op | `TcpStream::connect` |
| `socks5` | 检查 SOCKS 端口；不通则按配置启动 VPN 容器并等待就绪 | SOCKS5 CONNECT 握手（no-auth）后返回流 |

`socks5` 的 `ensure_ready` 要有**短缓存**（确认过一次就在 N 秒内直接返回），否则每次 dial 都去
`docker ps` 会明显拖慢。上一代用的是 30s，够用。

未来的 `jump-host` / `tailscale` / `teleport` 都是同一个 trait 的实现，或者 v0.3 之后是
`connectors/*.wasm`。**加一种新的进入方式，不需要动 SSH 层、不需要动任何 Tool。**

---

## Session

```rust
pub struct Session {
    pub target: Target,
    transport: SshTransport,     // russh
    agent: AgentHandle,          // 远端常驻 agent 的多路复用句柄
    stats: SessionStats,         // connects/calls/reconnects/last_latency
}
```

生命周期由 `SessionPool` 管，语义在 Python 原型上验证过：

* **一台机器一条连接**，常驻复用。第一次调用建链（含部署/拉起远端 agent），之后每次调用只是在
  已有通道上发一行、收一行。实测 **冷 0.7–5s / 热 33–52ms**。
* **lazy**：没人用就不建。
* **自愈**：连接死了、agent 被 kill 了，下次调用自动重建（实测 0.7–1.6s 恢复），上层无感。
* **keepalive + 后台探活**：睡眠唤醒/VPN 抖动后主动发现失效，而不是等下次调用才发现。

### 重试的诚实边界（这条必须实现，别偷懒）

```
请求还没发出去   →  重建连接后自动重放，安全
请求已经发出去   →  绝不自动重放，返回 UnknownState
```

已经发出但没拿到响应时，那条命令**可能已经在远端执行了**。自动重放意味着可能把一条
`rm -rf` 或一次训练启动跑两遍。正确做法是把不确定性如实交给上层：

```rust
Err(TrestleError::UnknownState {
    target, op,
    hint: "the remote side may have executed this; check state before retrying",
})
```

只对**明确幂等的读操作**（read/list/stat/hash/probe）做自动重试。

---

## Base 能力

`BaseService` 是唯一实现，MCP / WIT / CLI 三个 adapter 共用。签名如下（TS 伪码表达 schema）：

```rust
read (target, path, start_line?, max_lines?, max_bytes?) -> { content, total_lines, truncated }
write(target, path, content, append?, make_dirs?)        -> { bytes, path }
edit (target, path, edit: Edit)                          -> { changed, path }
```

```rust
pub enum Edit {
    /// 字面替换。count=0 表示全部
    Literal { old: String, new: String, count: u32 },
    /// 正则替换
    Regex   { pattern: String, replacement: String, count: u32, flags: String },
    /// 行范围替换（1-based，含两端）
    Lines   { start: u32, end: u32, replacement: String },
    /// 在某行之前插入
    Insert  { before_line: u32, content: String },
}
```

**为什么 edit 是 base 原语而不是 read+write 的组合**：组合意味着每次改一行都要把整个文件传两遍。
远端 agent 在本地做这件事，传输量只有 diff 大小。这也是必须有远端 agent 的理由之一。

### shell 必须拆成两个

```rust
shell_exec (target, command, cwd?, timeout?, env?) -> { exit_code, stdout, stderr, timed_out }
shell_spawn(target, command, cwd?, env?, name?)    -> { job_id, pid, log_path }
```

这是上一代最痛的教训之一：**只提供一个 `shell` 时，agent 会拿它去跑训练**，然后撞上超时、
进程被杀、或者更糟——超时后进程还活着但你以为它死了。

* `shell_exec`：有超时上限，超时**杀掉整个进程组**（不是只杀直接子进程，否则孙进程会残留），
  返回里明确带 `timed_out: true`。定位是"几秒到一两分钟的短命令"。
* `shell_spawn`：`setsid` 脱离会话，SSH 断了照跑；pid / 退出码 / 日志全部落盘；返回 job_id。
  定位是"训练、编译、长时间批处理"。

配套的 job 能力（`job_list` / `job_logs` / `job_wait` / `job_stop`）在 `04-mcp-surface.md`。
`job_logs` 的偏移量由 **host 侧记住**——agent 不该被迫自己管 offset，那是纯粹的摩擦。

---

## Tool

```rust
#[async_trait]
pub trait Tool: Send + Sync {
    fn namespace(&self) -> &str;              // "docker"
    fn descriptors(&self) -> Vec<ToolDescriptor>;   // 名字 + 描述 + JSON Schema
    async fn call(&self, name: &str, args: Value, ctx: &ToolCtx) -> Result<Value>;
}
```

`ToolCtx` 里给的是 **base 能力的受限句柄**，不是裸 Session——Tool 不能自己开 SSH，只能通过
base 做事。这是权限模型能成立的前提，v0.1 就要立好，否则 v0.3 接 WASM 时会发现根本收不回来。

三种 backend 共用这个 trait：

```rust
pub enum ToolBackend {
    Native(Box<dyn Tool>),   // v0.1：编译进来的内置实现
    Wasm(WasmTool),          // v0.3：Wasmtime 组件
    Process(McpTool),        // 可选：把别的 MCP server 当插件挂进来
}
```

v0.1 只实现 `Native` 分支，但**枚举和 trait 现在就定下来**——这样 v0.3 接 WASM 是加一个分支，
不是重构。这就是"预留位置"的正确做法：抽象立刻建立，实现按需推进。
