# Rust MCP服务器实现

**User:** Anonymous  
**Created:** 8/17/2026 1:45:33  
**Updated:** 8/17/2026 2:01:59  
**Exported:** 8/17/2026 2:02:57  
**Link:** [https://chatgpt.com/c/6a81f737-13b4-83e8-87f1-ea42e96d3f58](https://chatgpt.com/c/6a81f737-13b4-83e8-87f1-ea42e96d3f58)  

## Prompt:
2026/8/17 01:45:32

rust来写一个MCP服务器可以么？

## Response:
2026/8/17 01:45:33

> **正在搜索网页**
>
> [SDKs - What is the Model Context Protocol (MCP)?](https://modelcontextprotocol.io/docs/2026-07-28/sdk)
> [rmcp - Rust - Docs.rs](https://rust.sdk.modelcontextprotocol.io/)
> [The official Rust SDK for the Model Context Protocol](https://github.com/modelcontextprotocol/rust-sdk)
> [Rust Programming Language](https://rust-lang.org/)
> [The 2026-07-28 Specification | Model Context Protocol Blog](https://blog.modelcontextprotocol.io/posts/2026-07-28/)
> [Recently Updated - Official MCP Registry](https://registry.modelcontextprotocol.io/?q=docs)
>
> **已搜索 6 个网站**
>
> 
>
> 思考了 4s

完全可以，而且现在 **Rust 写 MCP Server 已经属于正经的一等公民方案了**。

MCP 官方目前有 Rust SDK，crate 叫 **`rmcp`**。它可以直接实现 MCP 的 `tools`、`resources`、`prompts`，也支持 server/client；传输层可以用本地的 `stdio`，也可以跑 **Streamable HTTP**。([Model Context Protocol](https://modelcontextprotocol.io/docs/2026-07-28/sdk?utm_source=chatgpt.com))

如果你的场景是给 **Claude Code / Codex / Cursor** 之类挂一个本地工具，我甚至挺推荐 Rust：

```text
Claude Code / Codex
        │
        │ MCP stdio / HTTP
        ▼
┌───────────────────┐
│   Rust MCP Server │
│       rmcp        │
├───────────────────┤
│ tool: read_sensor │
│ tool: run_cmd     │
│ tool: query_db    │
│ tool: control_xxx │
└───────────────────┘
        │
        ▼
 文件 / 串口 / DB / HTTP / 嵌入式设备
```

一个最小项目大概就是：

```bash
cargo new my-mcp
cd my-mcp

cargo add rmcp tokio serde serde_json anyhow
cargo add schemars
```

然后 Rust 侧定义工具。现在官方 SDK 本身就提供了比较舒服的宏式接口，不需要你自己手搓 JSON-RPC。官方 Rust SDK 当前还在跟进最新 MCP 规范，并兼容稳定版规范。([GitHub](https://github.com/modelcontextprotocol/rust-sdk?utm_source=chatgpt.com))

概念上代码会类似：

```rust
use rmcp::{
    handler::server::tool::ToolRouter,
    model::*,
    tool, tool_handler, tool_router,
    ServerHandler,
};

#[derive(Clone)]
struct MyMcp {
    tool_router: ToolRouter<Self>,
}

#[tool_router]
impl MyMcp {
    fn new() -> Self {
        Self {
            tool_router: Self::tool_router(),
        }
    }

    #[tool(description = "Add two integers")]
    async fn add(
        &self,
        #[tool(param)]
        a: i32,
        #[tool(param)]
        b: i32,
    ) -> String {
        (a + b).to_string()
    }
}

#[tool_handler]
impl ServerHandler for MyMcp {}
```

然后主函数把它挂到 `stdio`：

```rust
#[tokio::main]
async fn main() -> anyhow::Result<()> {
    use rmcp::{ServiceExt, transport::stdio};

    let service = MyMcp::new()
        .serve(stdio())
        .await?;

    service.waiting().await?;

    Ok(())
}
```

具体 API 在 `rmcp` 2.x 之后有一些变化，所以实际写项目时最好直接按照当前官方 example 来，我上面主要展示结构，而不是建议你硬复制版本相关代码。官方 Rust SDK 的 2.x 对 Rust API 做过 breaking change，不过 wire protocol 仍然保持兼容。([GitHub](https://github.com/modelcontextprotocol/rust-sdk/discussions/926?utm_source=chatgpt.com))

### Rust 其实特别适合你这种需求

尤其如果 MCP 后面不是单纯调几个 REST API，而是要碰：

```text
MCP
 ├─ 文件系统
 ├─ Git
 ├─ SQLite / RocksDB
 ├─ USB
 ├─ 串口
 ├─ HID
 ├─ TCP / UDP
 ├─ 本地进程
 └─ ESP32 / STM32 / 机器人
```

那 Rust 比 Python MCP Server 很有意思。

比如你完全可以做：

```text
LLM
 ↓
MCP
 ↓
Rust
 ↓
rusb / serialport / tokio
 ↓
ESP32-S3
```

直接暴露：

```text
get_device_info()
read_sensor()
set_gpio(pin, value)
capture_camera()
send_serial(command)
flash_firmware()
```

LLM 就能通过 MCP 操作硬件。

而且 Rust MCP Server 编译出来就是一个二进制：

```text
my-mcp.exe
```

客户端配置直接：

```json
{
  "mcpServers": {
    "my-device": {
      "command": "C:\\path\\my-mcp.exe"
    }
  }
}
```

不需要：

```text
python
venv
pip
node_modules
npm
```

这个对分发 **非常舒服**。

另外，如果不是本地 stdio，而是想做远程 MCP：

```text
AI Client
   │
 HTTPS
   ▼
Rust MCP Server
   │
 Axum / Tokio
```

官方 Rust SDK已经有 **Streamable HTTP Server** 示例，直接能把 MCP endpoint 挂成类似 `/mcp`。([GitHub](https://github.com/modelcontextprotocol/rust-sdk/blob/main/examples/servers/src/counter_streamhttp.rs?utm_source=chatgpt.com))

所以如果是你要做一个**真正长期用的 MCP 工具/硬件 MCP/本地系统 MCP**，我会把选择排成：

**Rust > TypeScript ≈ Python**，主要优势不是“能不能写”，而是 Rust 最后能做成一个非常干净的独立 binary，而且特别适合碰系统层和硬件层。

如果只是 50 行代码快速把某个 HTTP API 包成 MCP，那 Python/TS 会更快；但一旦涉及你前面聊的 **ESP32-S3、USB、串口、设备控制**，Rust + MCP 这个组合就很香。([GitHub](https://github.com/modelcontextprotocol/rust-sdk?utm_source=chatgpt.com))

---

**Sources:**

- [SDKs - What is the Model Context Protocol (MCP)?](https://modelcontextprotocol.io/docs/2026-07-28/sdk?utm_source=chatgpt.com)
- [The official Rust SDK for the Model Context Protocol](https://github.com/modelcontextprotocol/rust-sdk?utm_source=chatgpt.com)

## Prompt:
2026/8/17 01:50:53

rust生态如何实现插件？

## Response:
2026/8/17 01:50:58

可以，而且 Rust 的“插件生态”其实有好几条路线。关键在于你想要的“插件”到底是 **编译期插件、动态库插件，还是运行时可下载/热加载插件**。

Rust 最大的特殊点是：**Rust 没有稳定的原生 ABI**。所以不能像 C/C++ 那样随便把两个独立编译的 Rust `.dll/.so` 通过 Rust struct/trait 直接互调。这决定了 Rust 插件系统通常会采用下面几种模式。

### 1. 最简单：Trait + 静态注册

如果插件和主程序一起编译，这是最舒服的：

```rust
trait Plugin: Send + Sync {
    fn name(&self) -> &'static str;
    fn run(&self, input: &str) -> String;
}

struct HelloPlugin;

impl Plugin for HelloPlugin {
    fn name(&self) -> &'static str {
        "hello"
    }

    fn run(&self, input: &str) -> String {
        format!("hello {input}")
    }
}
```

主程序：

```rust
let plugins: Vec<Box<dyn Plugin>> = vec![
    Box::new(HelloPlugin),
];
```

目录可以搞成：

```text
workspace/
├── core/
│   └── src/lib.rs
├── plugins/
│   ├── git/
│   ├── serial/
│   ├── filesystem/
│   └── esp32/
└── app/
```

然后通过 Cargo features 控制：

```toml
[features]
git = ["plugin-git"]
serial = ["plugin-serial"]
esp32 = ["plugin-esp32"]
```

这个模式的特点是：

**类型安全、开发体验最好、性能最好，但不能运行时新增插件。**

对于你前面说的 MCP Server，如果只是：

```text
Rust MCP
 ├── GitPlugin
 ├── SerialPlugin
 ├── FilePlugin
 └── Esp32Plugin
```

我会优先这么做。

---

### 2. 自动注册：`inventory` / `linkme`

Rust 还有一个很有意思的模式：让插件自己声明“我存在”，主程序自动发现。

比如：

```rust
pub struct ToolPlugin {
    pub name: &'static str,
    pub handler: fn(&str) -> String,
}
```

插件：

```rust
inventory::submit! {
    ToolPlugin {
        name: "hello",
        handler: hello,
    }
}
```

主程序：

```rust
for plugin in inventory::iter::<ToolPlugin> {
    println!("{}", plugin.name);
}
```

这样就不需要：

```rust
plugins.push(...)
plugins.push(...)
plugins.push(...)
```

每个 crate 自己注册。

这特别适合做类似：

```text
#[mcp_tool]
async fn read_gpio(...) {}

#[mcp_tool]
async fn git_status(...) {}

#[mcp_tool]
async fn read_serial(...) {}
```

然后 procedural macro 自动帮你生成注册信息。

很多 Rust Web 框架、测试框架、CLI 框架背后的设计思想都有点类似。

---

### 3. 真正的动态插件：DLL / `.so`

如果你想达到：

```text
主程序已经编译完成

plugins/
├── git_plugin.dll
├── esp32_plugin.dll
├── camera_plugin.dll
└── whatever.dll

启动时自动加载
```

那通常使用：

```rust
libloading
```

大体结构：

```rust
use libloading::{Library, Symbol};

type PluginEntry = unsafe extern "C" fn() -> *mut PluginApi;

let lib = unsafe {
    Library::new("plugin.dll")?
};

let entry: Symbol<PluginEntry> = unsafe {
    lib.get(b"plugin_entry")?
};
```

但是这里有一个很重要的问题：

> 不建议把 Rust trait、`String`、`Vec<T>`、复杂 Rust struct 直接穿过 DLL 边界。

因为 Rust ABI 不稳定。

通常会设计成 **C ABI**：

```rust
#[repr(C)]
pub struct PluginApi {
    version: u32,
    name: extern "C" fn() -> *const c_char,
    execute: extern "C" fn(*const c_char) -> *mut c_char,
}
```

导出：

```rust
#[no_mangle]
pub extern "C" fn plugin_entry() -> *const PluginApi {
    ...
}
```

于是：

```text
Host
  │
  │ C ABI
  ▼
plugin.dll
```

这就稳定很多。

---

### 4. `abi_stable`

如果你觉得手写：

```rust
extern "C"
#[repr(C)]
*const c_char
```

太恶心，Rust 社区还有一个比较经典的思路：

```text
abi_stable
```

它就是专门解决：

> “我想用 Rust 写 Rust 动态插件，但是还想尽量保留 Rust 风格接口。”

类似可以定义 ABI-safe：

```rust
RString
RVec<T>
RBox<T>
```

而不是标准库：

```rust
String
Vec<T>
Box<T>
```

概念上就是：

```text
Rust Host
    │
    │ stable ABI abstraction
    ▼
Rust Plugin DLL
```

比纯 C ABI 舒服很多。

不过如果你要构建一个长期维护、跨版本、第三方开放的插件生态，我反而不会把它作为第一选择。

---

## 5. WebAssembly 插件

这个我觉得对你可能特别有意思。

主程序：

```text
Rust Host
   │
   ▼
Wasmtime
   │
   ├── plugin_a.wasm
   ├── plugin_b.wasm
   └── plugin_c.wasm
```

插件可以是 Rust：

```bash
cargo build --target wasm32-wasip2
```

最后得到：

```text
my_plugin.wasm
```

主程序动态加载。

这个相比 DLL 最大的优势是：

### 隔离

一个插件不能随便：

```text
读取 C:\
删你的文件
访问任意网络
访问USB
执行 shell
```

除非 Host 明确授权。

因此可以做 capability：

```text
Plugin A
├── filesystem: /workspace
├── network: no
├── serial: COM3
└── shell: no
```

这跟 MCP 的思想其实非常搭。

---

### Component Model + WIT

现代 Wasm 更有意思的地方是 **WIT / Component Model**。

你可以定义接口：

```wit
package my:plugin;

interface plugin {
    name: func() -> string;
    execute: func(input: string) -> string;
}
```

插件实现这个接口。

Rust Host 调：

```text
                    WIT
                     │
          ┌──────────┴──────────┐
          ▼                     ▼
       Host Rust            Plugin Rust
```

这相当于：

> 跨语言、ABI 稳定的 trait。

以后甚至插件不必用 Rust：

```text
Rust Host
 ├── Rust plugin
 ├── Go plugin
 ├── C plugin
 ├── JS plugin
 └── Python-ish WASM plugin
```

对于真正想做“插件平台”，这是非常漂亮的一条路。

---

## 6. 子进程插件

还有一种非常实用的方案：

```text
Host
 │
 ├── stdin/stdout JSON-RPC
 │
 ├── plugin-python
 │
 ├── plugin-rust
 │
 └── plugin-node
```

也就是插件根本不加载到同一个进程。

Host：

```rust
Command::new("plugin.exe")
    .stdin(Stdio::piped())
    .stdout(Stdio::piped())
    .spawn()
```

通信：

```json
{
  "method": "execute",
  "params": {
    "foo": "bar"
  }
}
```

这其实已经非常接近……

**MCP。**

所以你前面问 MCP Server 的话，有一个很自然的架构：

```text
                    Rust Host
                       │
            ┌──────────┼──────────┐
            │          │          │
           MCP        MCP        MCP
            │          │          │
         Git.exe   ESP32.exe   DB.exe
```

每个插件自己就是一个 MCP Server。

Host 充当：

```text
MCP Client + Router
```

这个设计的扩展性非常强。

---

# 如果让我设计一个 Rust 插件框架

我可能会分成三层：

```text
                    Application
                         │
                  Plugin Manager
                         │
        ┌────────────────┼────────────────┐
        │                │                │
   Built-in Plugin   WASM Plugin    Process Plugin
        │                │                │
    Rust trait        Wasmtime           MCP
```

定义统一抽象：

```rust
#[async_trait]
pub trait Plugin {
    fn manifest(&self) -> PluginManifest;

    async fn initialize(&mut self) -> Result<()>;

    async fn call(
        &self,
        method: &str,
        args: Value,
    ) -> Result<Value>;

    async fn shutdown(&mut self) -> Result<()>;
}
```

然后：

```rust
enum PluginBackend {
    Native(Box<dyn Plugin>),
    Wasm(WasmPlugin),
    Mcp(McpPlugin),
}
```

上层根本不用关心实现。

插件 manifest：

```toml
name = "esp32"
version = "0.1.0"
runtime = "mcp"

[permissions]
serial = true
network = false
filesystem = false
```

或者：

```toml
name = "formatter"
version = "0.2.0"
runtime = "wasm"

[permissions]
filesystem = ["./workspace"]
```

这就已经是一个挺完整的插件平台了。

---

### 各方案怎么选

| 方案 | 动态加载 | 隔离 | 开发体验 | 适合 |
|---|---:|---:|---:|---|
| Trait | ❌ | ❌ | ⭐⭐⭐⭐⭐ | 内置插件 |
| inventory/linkme | ❌ | ❌ | ⭐⭐⭐⭐⭐ | 自动注册 |
| DLL + libloading | ✅ | ❌ | ⭐⭐⭐ | 高性能 native |
| abi_stable | ✅ | ❌ | ⭐⭐⭐⭐ | Rust 动态库 |
| WASM/WASI | ✅ | ✅✅✅ | ⭐⭐⭐⭐ | 第三方插件 |
| 子进程 | ✅ | ✅✅ | ⭐⭐⭐⭐ | 多语言插件 |
| MCP 子进程 | ✅ | ✅✅ | ⭐⭐⭐⭐⭐ | AI 工具插件 |

**如果是你刚才那个 Rust MCP Server，我其实会很推荐一种很漂亮的组合：**

```text
Rust MCP Host
     │
     ├── 内置核心工具 → trait + inventory
     │
     ├── 安全第三方插件 → WASM Component
     │
     └── 重型/独立工具 → MCP subprocess
```

这样基本把 Rust 的强项全吃到了。

尤其 **“MCP 本身作为插件协议”** 这个想法非常值得用：你甚至不用自己重新发明插件 discovery、schema、RPC、capability 描述这些东西。每个插件暴露 `tools/list` 和 `tools/call`，Host 把多个 MCP Server 聚合成一个 MCP Server，直接就能做出一个 **Rust 版 MCP Plugin Hub / Router**。这个架构我觉得非常适合继续往下做。

## Prompt:
2026/8/17 01:54:01

用wasm怎么样呢？可以实现拓展性嘛？只要实现规定的接口就行了吧？然后我有一个基本的环境提供这些功能，比如我要做一个MCP服务能够管理服务器的，我在MCP具体的功能上面使用插件进行，基础服务提供read write edit shell四个功能，然后上层进行组合并且注册到MCP里面，还有提供一个命令调用之后启动一个websocket然后能够快速接入到cc的monitor里面，然后MCP是否支持树状结构？base的四个功能放在base集合里面，然后每个插件提供一个功能集合，特定加载。对于cc和MCP支不支持？不支持有没有别的方式实现W？

## Response:
2026/8/17 01:54:02

这个架构是成立的，而且我觉得你这里其实可以把 **WASM 当“插件 ABI + 沙箱”**，把 **MCP 当“对外工具协议”**，两层完全分开。关键要确认的是 MCP/Claude Code 对“工具分组、动态加载、WebSocket”到底支持到什么程度，我查一下当前规范再按你的目标给出落地结构。

我已经确认到一个很关键的点：**最新版 MCP 的工具名已经允许 `.`**，官方甚至直接给了 `admin.tools.list` 这种例子；但 MCP 协议的数据模型本身仍然是一个 `tools/list` 的**平面集合**，不是原生树。  
所以你的 `base.read / base.write / docker.logs / nginx.reload` 这种“逻辑树”完全能做，只是树是你在 Host 里维护的，MCP 对外最终还是扁平工具表。

还有一个对你这个设计非常有利的现状：**Claude Code 现在已经支持 MCP 的 `list_changed` 动态工具更新**，服务器工具集合变化后可以刷新，不需要断开重连；而且它默认还有 `ToolSearch`，大量 MCP 工具不会把完整 schema 一股脑塞进上下文。

所以你甚至不需要自己再造一套“树形工具发现协议”。真正需要自己定义的，是 **WASM 插件 ABI、插件权限和 Host 内部的 namespace**。

> **正在搜索网页**
>
> [Tools](https://modelcontextprotocol.io/specification/2026-07-28/server/tools)
> [Connect Claude Code to tools via MCP](https://docs.anthropic.com/en/docs/claude-code/mcp)
> [An Application with Plugins](https://docs.wasmtime.dev/wasip2-plugins.html)
> [Component Kft.: Professzionális irányítástechnikai megoldások](https://componentkft.hu/)
> [Claude Code の概要 - Claude Code Docs](https://code.claude.com/docs/ja/overview)
> [SPECIFICATION Definition & Meaning](https://www.merriam-webster.com/dictionary/specification)
> [‎Claude by Anthropicアプリ - App Store](https://apps.apple.com/jp/app/claude-by-anthropic/id6473753684)
> [Claudeをダウンロードする](https://claude.com/ja/download)
> [Embedding](https://en.wikipedia.org/wiki/Embedding)
> [The 2026-07-28 MCP Specification Release Candidate](https://blog.modelcontextprotocol.io/posts/2026-07-28-release-candidate/)
> [Embeddings | Machine Learning](https://developers.google.com/machine-learning/crash-course/embeddings)
> [【AI初心者向け】Claude Codeは「コードを書かない人」にこそ ...](https://www.qes.co.jp/media/claudecode/a903)
> [【2026年最新】Claudeとは？料金やChat・Cowork・Codeの ...](https://coopel.ai/column/post/what-is-claude/)
> [What is a Specification?](https://www.specright.com/what-is-a-specification/)
> [Claude](https://www.youtube.com/%40claude)
>
> **已搜索 15 个网站**
>
> 
>
> **正在搜索网页**
>
> [Connect Claude Code to tools via MCP - Claude Code Docs](https://docs.anthropic.com/en/docs/claude-code/mcp)
>
> **厘清监控架构**
>
> 我正在判断“CC monitor”是否指自定义监控面板，以及它需要接收哪些会话事件。较稳妥的设计是用一个命令启动服务，由钩子上报事件，再通过 WebSocket 推送给监控端。
>
> **厘清监控架构**
>
> 
>
> **正在搜索网页**
>
>
> **规范化动态加载**
>
> 我正在区分两种需求：外部管理命令可以热加载插件并通知客户端刷新；若由模型调用启用工具，就保持工具列表稳定，仅延迟实例化插件，避免连接级工具集合随请求改变。
>
> **规范化动态加载**
>
> 
>
> **已搜索 1 个网站**
>
> 
>
> 思考了 1m 17s

可以，而且你这个思路其实已经很接近一个相当漂亮的 **“Rust Host + WASM Plugin Runtime + MCP Frontend”** 架构了。尤其现在 Claude Code 对动态 MCP 工具的支持比以前强很多，这套东西完全能落地。

我会把整体架构定成这样：

```text
                         Claude Code
                             │
                   MCP / Streamable HTTP
                             │
                             ▼
┌──────────────────────────────────────────────────┐
│                 Rust MCP Host                    │
│                                                  │
│   ┌──────────── MCP Frontend ─────────────────┐  │
│   │ base.read                                 │  │
│   │ base.write                                │  │
│   │ base.edit                                 │  │
│   │ base.shell                                │  │
│   │ docker.logs                               │  │
│   │ docker.restart                            │  │
│   │ nginx.test                                │  │
│   │ nginx.reload                              │  │
│   └───────────────────────────────────────────┘  │
│                         │                        │
│                    Tool Router                   │
│                         │                        │
│        ┌────────────────┼───────────────┐        │
│        ▼                ▼               ▼        │
│     Base API       Plugin Registry   Monitor Bus │
│        │                │               │        │
│  read/write/edit        │               └── WS ─────► Monitor
│       /shell            ▼                        │
│                   Wasmtime Runtime               │
│                         │                        │
│              ┌──────────┼─────────┐              │
│              ▼          ▼         ▼              │
│          docker.wasm nginx.wasm postgres.wasm    │
└──────────────────────────────────────────────────┘
```

这里最重要的设计点是：**MCP 和 WASM 完全解耦。**

WASM 不需要懂 MCP，MCP 也不需要懂 Wasmtime。

---

## WASM 非常适合你这个插件模型

你说的：

> 只要实现规定的接口就行了吧？

对，**就是这个思想**。

而且现在 Wasmtime Component Model + WIT 本来就是干这个的：WIT 定义 Host 给插件什么能力，以及插件必须向 Host 导出什么接口；Wasmtime 可以根据 WIT 自动生成 Rust bindings。官方 Wasmtime 自己甚至就有一个完整的 plugin application 示例。([Wasmtime](https://docs.wasmtime.dev/wasip2-plugins.html))

比如你可以定义：

```wit
package server:plugin;

interface base {
    read: func(path: string) -> result<string, string>;

    write: func(
        path: string,
        content: string
    ) -> result<_, string>;

    edit: func(
        path: string,
        old: string,
        new: string
    ) -> result<_, string>;

    shell: func(
        command: string
    ) -> result<string, string>;
}

record tool-info {
    name: string,
    description: string,
    input-schema: string,
}

interface plugin {
    name: func() -> string;

    tools: func() -> list<tool-info>;

    call: func(
        tool: string,
        arguments-json: string
    ) -> result<string, string>;
}

world server-plugin {
    import base;
    export plugin;
}
```

于是任何插件：

```text
docker.wasm
nginx.wasm
postgres.wasm
systemd.wasm
nvidia.wasm
conda.wasm
```

只要实现：

```text
name()
tools()
call()
```

就能被你的 Host 加载。

插件内部又能调用：

```text
base.read()
base.write()
base.edit()
base.shell()
```

这些真正的系统操作。

Wasmtime 的 `Linker` 就负责把 Host 提供的函数链接给 WASM Component。([Wasmtime](https://docs.wasmtime.dev/api/wasmtime/component/struct.Linker.html?utm_source=chatgpt.com))

---

# 更漂亮的是：Base 只实现一次

我不建议：

```text
MCP base.read
    ↓
一套 read 实现

WASM base.read
    ↓
另一套 read 实现
```

而应该：

```text
                    FileService
                         │
           ┌─────────────┴────────────┐
           ▼                          ▼
      MCP adapter                WIT adapter

      base.read()                base.read()
```

例如 Rust：

```rust
struct BaseService {
    fs: FileService,
    shell: ShellService,
}
```

然后：

```text
MCP base.read
      │
      └── BaseService.read()

WASM import base.read
      │
      └── BaseService.read()
```

**真正的业务能力只有一套。**

这样非常干净。

---

# MCP 的“树状结构”：半支持

这里需要准确区分。

MCP **没有**

```text
base
├── read
├── write
└── shell

docker
├── logs
└── restart
```

这样的原生 Tool Tree 数据结构。

`tools/list` 返回的本质仍然是一个 flat list。([Model Context Protocol](https://modelcontextprotocol.io/specification/2026-07-28/server/tools))

但是最新 MCP 规范允许工具名包含 `.`，官方直接给出的合法例子就是：

```text
admin.tools.list
```

所以你完全可以：

```text
base.read
base.write
base.edit
base.shell

docker.ps
docker.logs
docker.inspect
docker.restart

nginx.status
nginx.test
nginx.reload

systemd.status
systemd.restart
```

这可以视为**逻辑树**：

```text
base.*
docker.*
nginx.*
systemd.*
```

MCP 工具名目前建议只使用字母、数字、`_`、`-`、`.`，最长建议 128 字符。([Model Context Protocol](https://modelcontextprotocol.io/specification/2026-07-28/server/tools))

所以从 MCP 角度来说，**你这个 namespace 方案完全正规。**

---

# Claude Code 这里甚至更有意思

Claude Code 本身把 MCP 工具映射为：

```text
mcp__<server>__<tool>
```

例如：

```text
server = server-manager
tool   = docker_logs

→

mcp__server-manager__docker_logs
```

这是 Claude Code 官方的工具命名方式。([Claude](https://code.claude.com/docs/en/agent-sdk/mcp?utm_source=chatgpt.com))

如果你的 MCP Server 是作为 Claude Code Plugin 一起分发，那么 Claude Code 还会进一步加入 plugin/server namespace。([Claude Platform Docs](https://docs.anthropic.com/en/docs/claude-code/mcp))

有一个小细节：**Claude Code plugin-bundled MCP server 的 callable name 会把 `.` 等字符正规化成 `_`**。所以 MCP 内部你可以叫：

```text
docker.logs
```

但 Claude Code 某些 permission/hook 场景看到的 callable name 可能类似：

```text
mcp__plugin_xxx_server__docker_logs
```

这一点做权限配置时要注意。([Claude](https://code.claude.com/docs/en/mcp?utm_source=chatgpt.com))

因此如果你特别重视 Claude Code 的 permission matcher，我甚至倾向于直接统一：

```text
base_read
base_write
base_edit
base_shell

docker_logs
docker_restart
nginx_reload
```

但如果重视协议语义和其他 MCP Client，我更喜欢：

```text
base.read
docker.logs
nginx.reload
```

两种都行。

---

# “插件特定加载”现在也能做得很好

Claude Code **已经支持 `list_changed`**。

MCP Server 的工具发生变化之后，可以通知 Claude Code：

```text
notifications/tools/list_changed
```

Claude Code 会重新获取工具列表，不需要把 MCP Server 整个断开重连。([Claude Platform Docs](https://docs.anthropic.com/en/docs/claude-code/mcp))

所以理论上可以：

```text
plugins/
├── docker.wasm      enabled
├── nginx.wasm       enabled
├── postgres.wasm    disabled
└── nvidia.wasm      disabled
```

当前：

```text
tools/list

base.read
base.write
base.edit
base.shell
docker.*
nginx.*
```

然后加载：

```bash
serverctl plugin enable postgres
```

Rust Host：

```text
postgres.wasm
      ↓
Wasmtime instantiate
      ↓
读取 plugin.tools()
      ↓
加入 ToolRegistry
      ↓
tools/list changed
      ↓
Claude Code refresh
```

这正是你想要的效果。

不过这里我建议**插件开关走 Host 的 CLI/Admin API，而不是让某个 MCP tool 调用后修改“当前连接自己的工具集”**。最新版 MCP 2026 规范允许工具集合随时间变化，但要求不能根据单个连接而变化，也不应该作为连接中其他请求的隐式副作用。([Model Context Protocol](https://modelcontextprotocol.io/specification/2026-07-28/server/tools))

所以做成：

```bash
srv plugin enable docker
srv plugin disable docker
srv plugin reload docker
```

更漂亮。

---

# 但实际上甚至不用频繁加载/卸载 WASM

因为 Claude Code 现在有个特别适合你的功能：

**MCP Tool Search。**

默认情况下，Claude Code 不会把所有 MCP 工具完整 schema 全塞进 context，而是先知道有哪些工具，在需要的时候搜索并加载具体 schema。([Claude Platform Docs](https://docs.anthropic.com/en/docs/claude-code/mcp))

也就是说以后你有：

```text
base       4 tools
docker    15 tools
nginx     10 tools
systemd   20 tools
postgres  40 tools
nvidia    20 tools
k8s       80 tools
conda     20 tools
...
```

两三百个 tool 也不意味着两三百个 schema 一开始全进入上下文。Claude Code 默认会 deferred loading，实际需要某个工具时再通过 `ToolSearch` 找。([Claude Platform Docs](https://docs.anthropic.com/en/docs/claude-code/mcp))

所以我会采用：

```text
逻辑 Enabled
        ↓
tool descriptor 注册 MCP

实际 Wasm Instance
        ↓
Lazy Load
```

也就是：

```text
docker.logs 被 ToolSearch 找到
        ↓
Claude 调 docker.logs
        ↓
PluginManager
        ↓
发现 docker.wasm 尚未实例化
        ↓
instantiate()
        ↓
call("logs", ...)
```

这就非常舒服。

---

# WebSocket / Monitor 我建议跟 MCP 分开

这个地方我反而非常明确地建议：

```text
/mcp
    → Streamable HTTP

/monitor/ws
    → WebSocket
```

而不是：

```text
整个 MCP 都走 WebSocket
```

原因是截至当前 MCP `2026-07-28` 规范，**标准 transport 是 stdio 和 Streamable HTTP；WebSocket 可以作为 custom transport，但不是 MCP 标准 transport。** ([Model Context Protocol](https://modelcontextprotocol.io/specification/2026-07-28/basic/transports?utm_source=chatgpt.com))

甚至官方 Python MCP SDK v2 已经删除了原来的 WebSocket transport，明确说明它从来不是 MCP 标准的一部分。([MCP Python SDK](https://py.sdk.modelcontextprotocol.io/whats-new/?utm_source=chatgpt.com))

但是非常有意思的是：

**Claude Code 自己目前确实支持 WebSocket MCP Server。**

配置：

```json
{
  "mcpServers": {
    "server-manager": {
      "type": "ws",
      "url": "ws://127.0.0.1:9876/mcp"
    }
  }
}
```

Claude Code 官方文档现在明确支持 `stdio / SSE / HTTP / WebSocket`。([Claude Platform Docs](https://docs.anthropic.com/en/docs/claude-code/mcp))

所以：

```text
只服务 Claude Code
        ↓
WebSocket MCP
        ✓ 能做

希望 Cursor / VS Code / ChatGPT / 其他 MCP Client 通吃
        ↓
Streamable HTTP
        ✓ 更合适
```

你的 monitor 则继续独立用 WebSocket。

---

# Monitor 可以做得非常爽

Host 自己就有一个：

```text
EventBus
```

所有事件进去：

```text
McpToolCalled
McpToolFinished

PluginLoaded
PluginUnloaded

ShellStarted
ShellStdout
ShellStderr
ShellFinished

FileRead
FileWritten

ServerConnected
ServerDisconnected
```

然后：

```text
                 EventBus
                    │
          ┌─────────┴─────────┐
          ▼                   ▼
        tracing         WebSocket Broadcast
                              │
                              ▼
                         CC Monitor
```

于是：

```bash
srv monitor
```

或者：

```bash
srv monitor --web
```

Host 启动：

```text
http://127.0.0.1:9876/
ws://127.0.0.1:9876/monitor/ws
```

浏览器 monitor 实时看到：

```text
Claude
  │
  ├─ base.read("/etc/nginx/nginx.conf")    12ms
  │
  ├─ nginx.test()
  │    └─ shell: nginx -t                  37ms
  │
  └─ nginx.reload()
       └─ shell: systemctl reload nginx    84ms
```

这个和 MCP **完全不用绑死**。

---

## 如果你还想 Monitor Claude Code 自己

比如不仅监控：

```text
MCP call
```

还想监控：

```text
Claude session start
Claude prompt
Bash
Edit
MCP tool
Subagent
Stop
```

那 Claude Code 现在还有 **Hooks**。

它能在：

```text
SessionStart
PreToolUse
PostToolUse
PostToolUseFailure
Stop
SessionEnd
...
```

这些生命周期触发 shell command 或 HTTP endpoint。([Claude Platform Docs](https://docs.anthropic.com/en/docs/claude-code/hooks))

于是可以：

```text
Claude Code
    │
    │ HTTP Hook
    ▼
POST /cc/events
    │
    ▼
Rust Host EventBus
    │
    ▼
WebSocket
    │
    ▼
Monitor
```

这就很强了。

**你甚至能做一个完整 Claude Code observability dashboard。**

---

# WASM 权限这里一定要设计好

这是这套架构里我觉得最值得从一开始就定好的地方。

不要简单：

```text
所有 WASM
    ↓
随便调用 shell()
```

不然：

```text
WASM sandbox
```

基本等于白做。

建议插件 manifest 带 capability：

```toml
name = "nginx"
version = "0.1.0"

[permissions.fs]
read = [
    "/etc/nginx/**",
    "/var/log/nginx/**"
]

write = [
    "/etc/nginx/**"
]

[permissions.shell]
allow = [
    "nginx -t",
    "systemctl status nginx",
    "systemctl reload nginx",
    "systemctl restart nginx"
]
```

那么：

```text
nginx.wasm
   │
   ├─ read("/etc/nginx/nginx.conf")       ✓
   ├─ read("/etc/shadow")                 ✗
   ├─ shell("nginx -t")                   ✓
   └─ shell("rm -rf /")                   ✗
```

**WASM 本身负责隔离，Host Capability 负责授权。**

这才真正发挥 WASM 的价值。

---

# 所以我会最终定成这一套

| 层 | 技术 |
|---|---|
| 主程序 | Rust |
| MCP | `rmcp` |
| MCP Transport | Streamable HTTP + 可选 stdio |
| 插件运行时 | Wasmtime |
| 插件 ABI | Component Model + WIT |
| 插件格式 | `.wasm` |
| 基础能力 | read / write / edit / shell |
| 插件命名空间 | `docker.*` / `nginx.*` |
| 插件发现 | manifest + directory |
| 插件加载 | lazy instantiate |
| MCP 动态更新 | `list_changed` |
| Claude 大规模工具 | ToolSearch |
| Monitor | 独立 WebSocket |
| Claude Code 生命周期 | Hooks → Rust Host |
| 权限 | capability manifest |

最终你这个东西就不是简单的“一个 MCP Server”了，而更像：

```text
                  Server Agent Runtime
                         │
       ┌─────────────────┼──────────────────┐
       │                 │                  │
      MCP             WASM Runtime       Monitor
       │                 │                  │
 Claude Code         Plugin SDK           Web UI
                         │
              ┌──────────┼───────────┐
              │          │           │
           Docker      Nginx      PostgreSQL
```

**这个架构我挺看好的。**

尤其最漂亮的一点是：第三方以后写插件，根本不用学习 MCP。只需要拿你的 `server-plugin.wit`，实现 `plugin.tools()` 和 `plugin.call()`，编译出一个 `.wasm` 扔进 `plugins/` 就结束了；而 Claude Code、Cursor 等看到的仍然是正常 MCP Server。

这就真正做到了你说的：**“只要实现规定接口，就能无限扩展。”**

---

**Sources:**

- [An Application with Plugins - Wasmtime](https://docs.wasmtime.dev/wasip2-plugins.html)
- [Tools - Model Context Protocol](https://modelcontextprotocol.io/specification/2026-07-28/server/tools)
- [Connect to external tools with MCP - Claude Code Docs](https://code.claude.com/docs/en/agent-sdk/mcp?utm_source=chatgpt.com)
- [Connect Claude Code to tools via MCP - Claude Code Docs](https://docs.anthropic.com/en/docs/claude-code/mcp)
- [What's new in v2](https://py.sdk.modelcontextprotocol.io/whats-new/?utm_source=chatgpt.com)

## Prompt:
2026/8/17 02:00:41

你给他取个名吧，我初步其实是想用这个来管理远程的服务器，因为我有四台组里的服务器，用这个东西让agent能够减少连接服务器管理服务器的摩擦这样，然后用这个架构是为了可扩展性，如果它对于连接服务器有特殊的需求比如只能通过特定的网络环境进行连接（VPN）等等可能有的需求预留位置，然后可以添加工具因为可能有时候遇到摩擦了于是就让agent写一个插件加入进来这样能够奸减少摩擦，现在有没有这样的工具了？

## Response:
2026/8/17 02:00:43

这个定位一下子清楚很多了：你不是单纯做“SSH 的 MCP 包装”，而是在做一个 **Agent 面向异构服务器环境的可演化管理层**——连接方式、VPN/跳板机、具体运维动作都可以随着遇到的摩擦长出插件。这个定位会直接影响名字，我先按这个方向找一下现有项目，看看哪里已经有人做、哪里还是空位。

我找到的现有项目里，已经有人把“Claude/Codex → SSH 多服务器管理”做到很深了，甚至有 Rust 项目直接暴露 **300+ 个 SSH 运维工具**；也已经有人做 **WASM 插件化 MCP Runtime**。但我暂时没看到一个项目把你这三个点完整合在一起：**异构连接环境抽象 + Agent 运行时按摩擦生成/安装能力 + WASM capability sandbox**。这恰好是你最值得保留的差异化。

> **正在搜索网页**
>
> [AI Agents with Machine & Workload Identity - Teleport](https://goteleport.com/docs/machine-workload-identity/use-cases/ai-agents-mwi/)
> [スーパーサイエンスハイスクール（SSH）](https://www.jst.go.jp/cpse/ssh/)
> [Security Best Practices](https://modelcontextprotocol.io/docs/tutorials/security/security_best_practices)
> [AiondaDotCom/mcp-ssh: A Model Context Protocol ...](https://github.com/aiondadotcom/mcp-ssh)
> [What is SSH (Secure Shell)? | SSH Academy](https://www.ssh.com/academy/ssh)
> [【非エンジニアでもわかる】MCPとは？仕組み、A2Aとの違いを ...](https://monstar-lab.com/dx/technology/about-mcp/)
> [Google AI - How we're making AI helpful for everyone](https://ai.google/)
> [An easy, realistic model for MCP connectivity](https://tailscale.com/blog/model-for-mcp-connectivity-lee-briggs)
> [MCP SSH Server](https://mcpservers.org/servers/mixelpixx/SSH-MCP)
> [Recently Updated](https://registry.modelcontextprotocol.io/?q=com.supabase%2Fmcp)
> [MCPとは？AIエージェントの可能性を広げよう ～LLM活用入門 ...](https://www.ntt-tx.co.jp/column/251113/)
> [Artificial intelligence](https://en.wikipedia.org/wiki/Artificial_intelligence)
> [Secure Shell](https://ja.wikipedia.org/wiki/Secure_Shell)
> [MCP とは？ LLM 向け Model Context Protocol を解説](https://www.sysdig.com/jp/learn-cloud-native/what-is-mcp-model-context-protocol)
> [What is Artificial Intelligence (AI)?](https://cloud.google.com/learn/what-is-artificial-intelligence)
> [Enabling Secure Remote Access to Home Servers with ...](https://zenn.dev/sion_pn/articles/f3bc98a32ed3ab?locale=en)
> [MCP (Model Context Protocol) サーバ](https://www.sparxsystems.jp/products/EA/tech/MCP.htm)
> [Official Google AI news and updates](https://blog.google/innovation-and-ai/technology/ai/)
> [MCP (Model Context Protocol)とは？～AIが業務で使えない ...](https://note.com/workato/n/n133148886a54)
> [What Is Artificial Intelligence (AI)?](https://www.ibm.com/think/topics/artificial-intelligence)
> [「MCP」とは？業務効率化の新常識](https://jp.ext.hp.com/techdevice/ai/ai_explained_17/)
> [What do we mean by “AI?”](https://www.peoplepowered.org/digital-guide/what-do-we-mean-by-ai)
>
> **整理命名方向**
>
> 我会把相近项目按连接管理、安全隔离、插件扩展和网关能力分类，再避开过于拥挤的名称。命名上我倾向于突出“降低远程运维摩擦、连接多台服务器、持续扩展能力”，例如 **Slipstream、Conduit、Waypoint、Dockyard、Porter**。
>
> **整理了命名方向**
>
> 
>
> **已搜索 22 个网站**
>
> 
>
> **正在搜索网页**
>
> [GitHub - microsoft/wassette: Wassette: A security-oriented runtime that runs WebAssembly Components via MCP · GitHub](https://github.com/microsoft/wassette)
> [Securing Agentic AI with Teleport Zero Trust Access | Teleport](https://goteleport.com/docs/connect-your-client/model-context-protocol/)
> [Secure AI agent connectivity](https://tailscale.com/use-cases/secure-ai-agent-connectivity)
>
> **已搜索 3 个网站**
>
> 
>
> 思考了 39s

有，而且我查完之后发现：**你的方向不是“没人做”，而是几个相邻方向已经分别有人做了，但“面向远程服务器、连接环境可插拔、遇到摩擦让 Agent 自己长插件”这个组合仍然有很清楚的位置。**

我先给它取名。我最喜欢 **Trestle**。

`Trestle` 本意是支架、栈桥的支撑结构，我觉得和这个项目很贴：

```text
               Claude Code / Codex
                       │
                     MCP
                       │
                 ┌── Trestle ──┐
                 │             │
          Connection Layer   Tool Layer
                 │             │
          SSH / VPN / Jump    WASM
          Tailscale / ...     Plugins
                 │             │
                 └──────┬──────┘
                        │
                 Server Fleet
```

它不是“SSH 工具”，而是给 Agent 搭一座通往不同服务器环境的桥架。

如果想名字更有攻击性一点，我还会考虑：**Waygate**（不同服务器/网络环境的入口）、**Jumpyard**（Jump Host + server yard）、**Relay**（很准确但太通用）、**Rivet**（插件像铆钉一样不断接能力，不过已有不少同名项目）、**Slipway**（强调降低摩擦，名字也挺工程味）。我个人排序是 **Trestle > Waygate > Jumpyard**。

---

## 但现有项目已经出现一个非常接近你的东西：Noorle

这个是我查的时候最值得你注意的。

Noorle 把自己定义成一个 Agent Runtime：对外提供**一个 MCP Endpoint**，内部聚合内置工具、WASM 插件以及外部 Connector；自定义逻辑可以用 Rust、Go、JS、TS、Python 写成 sandboxed WebAssembly。([GitHub](https://github.com/noorle))

也就是说它已经有：

```text
Agent
  │
  MCP
  │
  ▼
Runtime
 ├── builtin tools
 ├── WASM plugins
 └── connectors
```

这和我们刚才讨论的结构已经非常像了。([GitHub](https://github.com/noorle))

**但它的定位不是“远程服务器 Agent Runtime”。** 它更像通用的 managed MCP gateway/runtime，而且是托管服务。你想做的则可以明确走：

```text
local-first / self-hosted
+
remote infrastructure
+
connection abstraction
+
agent-extensible operations
```

这就明显不同了。

---

## Microsoft Wassette 更值得你研究架构

微软现在有一个 **Wassette**，定位就是：

> 用 MCP 暴露安全的 WebAssembly Component Runtime。

Agent 可以动态加载 WASM Component，然后新能力立即变成 Agent 可以调用的能力；官方已经明确支持 Claude Code、Codex CLI、Copilot 等 MCP Client。([GitHub](https://github.com/microsoft/wassette))

例如：

```text
Claude Code
    │
    ▼
 Wassette
    │
 load component
    ▼
 xxx.wasm
```

然后这个 WASM 能力就成为 Agent 的新工具。([GitHub](https://github.com/microsoft/wassette))

所以你之前那个：

> “遇到一个摩擦 → 让 Agent 写一个插件 → 装进去 → 从此以后不再有这个摩擦”

**这条技术路线已经被微软证明是合理的。**

你的差异可以是进一步把它领域化成：

```text
Wassette
    ↓
General WASM capabilities

Trestle
    ↓
Remote infrastructure capabilities
```

---

# 服务器管理这边也已经很卷了

比如 `mcp-ssh-manager` 已经支持 Claude Code/Codex 管理多 SSH 连接，可以执行命令、传文件、数据库操作、备份、健康监控等；截至 2026 年 8 月 14 日的 v3.8.0 甚至已经加入 server group。([GitHub](https://github.com/bvisible/mcp-ssh-manager))

另一个更夸张的是 **bridge-mcp**：

```text
Rust
337 tools
74 groups
SSH remote management
Docker
Kubernetes
systemd
database
Windows
cloud
compliance
...
```

而且它就是 Rust 写的。([GitHub](https://github.com/muchiny/bridge-mcp))

它添加新工具目前采用的是编译期注册：`#[mcp_tool]` + `inventory` 自动注册 handler。([GitHub](https://github.com/muchiny/bridge-mcp))

这个恰好能说明你和它的核心区别。

它基本是：

```text
遇到需求
  ↓
开发者写 Rust tool
  ↓
重新编译 MCP Server
  ↓
新版本拥有能力
```

你想要：

```text
Agent 遇到需求
      ↓
发现能力缺失
      ↓
plugin scaffold
      ↓
Agent 写插件
      ↓
cargo component build
      ↓
xxx.wasm
      ↓
Host 验证权限
      ↓
动态 load
      ↓
tools/list_changed
      ↓
Agent 立即拥有能力
```

**这个区别非常关键。**

---

# 网络连接问题反而应该成为 Trestle 的一等公民

你刚才举的 VPN 例子，我觉得不能只做成：

```text
ssh hostname
```

然后指望 shell 环境已经准备好了。

应该抽象一个：

```rust
trait Connector {
    async fn connect(&self, target: &Target) -> Result<Session>;
}
```

概念上：

```text
Target
  │
  ▼
Connection Profile
  │
  ├── Direct SSH
  ├── SSH ProxyJump
  ├── VPN + SSH
  ├── Tailscale + SSH
  ├── WireGuard + SSH
  ├── Teleport
  ├── Kubernetes exec
  ├── Docker exec
  └── Custom Connector Plugin
```

于是服务器配置不是：

```toml
host = "124.xxx.xxx.xxx"
user = "root"
```

而是：

```toml
[servers.gpu59]
connector = "lab-vpn"
host = "192.168.1.59"
user = "xxx"

[servers.gpu60]
connector = "direct-ssh"
host = "124.xxx.xxx.60"
user = "xxx"
```

再比如：

```toml
[connectors.lab-vpn]
type = "plugin"
plugin = "lab-vpn.wasm"
```

那么最有意思的事情就发生了：

> **连接服务器本身也是插件能力。**

---

## 这比只让“操作”插件化要好得多

我甚至建议你把插件分成两大类：

```text
plugins/
├── connectors/
│   ├── ssh.wasm
│   ├── wireguard.wasm
│   ├── lab-vpn.wasm
│   └── teleport.wasm
│
└── tools/
    ├── docker.wasm
    ├── systemd.wasm
    ├── nvidia.wasm
    ├── slurm.wasm
    └── conda.wasm
```

然后：

```text
                Trestle
                   │
          ┌────────┴─────────┐
          │                  │
      Connector            Skill
       Plugins             Plugins
          │                  │
     “怎么进去”          “进去干什么”
```

这句话甚至可以直接当你的架构哲学：

**Connector solves how to reach it. Tool solves what to do with it.**

---

# VPN / Bastion 这个领域已经有人解决，但不是你的这一层

Teleport 现在已经直接面向 Agent/MCP 做基础设施访问，强调不用 Agent 自己处理长期 SSH Key、VPN、bastion host，并提供细粒度访问控制和审计。([Teleport](https://goteleport.com/docs/connect-your-client/model-context-protocol/))

Tailscale 也已经明确把 AI Agent connectivity 当使用场景：通过身份和 ACL 控制哪些 Agent 可以访问哪些节点/MCP Server，同时提供流量归属和审计。([Tailscale](https://tailscale.com/use-cases/secure-ai-agent-connectivity))

所以你**不应该自己实现 VPN**。

反而应该：

```text
              Trestle
                 │
        Connector abstraction
        ┌────────┼──────────┐
        ▼        ▼          ▼
       SSH    Tailscale   Teleport
        │        │          │
        └────────┼──────────┘
                 ▼
              Server
```

你负责的是：

> **Agent 不需要知道底下是什么网络环境。**

这才是你的价值。

---

# 我觉得真正有意思的是“摩擦记忆”

你刚才说：

> 有时候遇到摩擦了，于是让 agent 写一个插件加进来。

这其实比“WASM MCP Server”本身有意思得多。

比如第一次：

```text
User:
去 59 看一下 GPU。

Agent:
ssh 59
→ connection timeout

发现：
必须先连接学校 VPN。

Agent:
配置 VPN
SSH
nvidia-smi
```

传统 Agent：

```text
下次：
又重新摸索一遍
```

Trestle：

```text
第一次发现摩擦：

        VPN prerequisite
              │
              ▼
Agent 生成 lab-network Connector
              │
              ▼
       lab-network.wasm
              │
              ▼
register connector "lab"
```

以后：

```text
server59
server60
server63
server75
   │
   └── connector = "lab"
```

下一次你说：

> 看一下 63。

Agent 直接：

```text
lab.ensure_connected()
        ↓
ssh.connect(63)
        ↓
nvidia.status()
```

**之前需要推理解决的问题，变成了以后确定性的基础设施能力。**

这个 Story 我觉得比“我们支持 WASM Plugin”强得多。

---

# 所以我会重新定义这个项目

不是：

> An extensible SSH MCP server.

那样马上就掉进 `mcp-ssh-manager` / `bridge-mcp` 已经非常拥挤的坑里了。([GitHub](https://github.com/muchiny/bridge-mcp))

而是：

> **Trestle - an extensible infrastructure runtime for coding agents.**

或者更准确：

> **Trestle lets agents turn infrastructure friction into reusable capabilities.**

核心循环就是：

```text
          Agent encounters friction
                    │
                    ▼
              Solve once
                    │
                    ▼
             Extract capability
                    │
                    ▼
              WASM Plugin
                    │
                    ▼
       Connector / Tool Registry
                    │
                    ▼
             Reuse forever
                    │
                    └──────────────┐
                                   │
                      next friction│
                                   ▼
```

这里我觉得已经有一点 **“self-evolving infrastructure interface”** 的味道了。

而且对于你的实际需求，第一版根本不用做很大：四台服务器 + SSH connector + `read/write/edit/shell` + WASM tool plugin +一个 VPN connector 接口 + monitor，就已经能真正开始用了。

顺带一提，Rust 生态里 `mcp-kit` 也在做动态 MCP 插件系统，并在 README 中描述 native/WASM plugin 支持，不过它当前文档自己存在一点不一致：功能介绍说 WASM plugin 可用，而 feature 表又把 `plugin-wasm` 标成 “coming soon”，所以我暂时不会把它作为你的底座。([GitHub](https://github.com/KSD-CO/mcp-kit))

**我反而倾向：`rmcp + wasmtime + WIT` 自己把这个很薄的 Runtime 写出来。** 这样 Trestle 的核心抽象--`Connector / Tool / Capability / Target`--完全掌握在自己手里，不会被某个 MCP framework 的插件模型反过来限制。

---

**Sources:**

- [Noorle · GitHub](https://github.com/noorle)
- [Securing Agentic AI with Teleport Zero Trust Access | Teleport](https://goteleport.com/docs/connect-your-client/model-context-protocol/)
- [Secure AI agent connectivity](https://tailscale.com/use-cases/secure-ai-agent-connectivity)



---
Powered by [ChatGPT Exporter](https://www.chatgptexporter.com)