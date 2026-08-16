# Trestle

**An extensible infrastructure runtime for coding agents.**

> Trestle lets agents turn infrastructure friction into reusable capabilities.

给 Coding Agent（Claude Code / Codex 等）用的远程基础设施运行时。它不是"又一个 SSH MCP 包装"——
那条赛道已经很拥挤（`mcp-ssh-manager`、`bridge-mcp` 337 个工具）。Trestle 的赌注是另一个东西：

```
Agent 遇到摩擦  →  解决一次  →  固化成 capability  →  永久复用
        ↑                                                  │
        └──────────────── 下一个摩擦 ←────────────────────┘
```

两条正交的抽象撑起整个系统：

```
                     Connector          │          Tool
                  「怎么进去」          │      「进去干什么」
        direct-ssh / socks5-vpn /       │   docker / nvidia / slurm /
        jump-host / tailscale / ...     │   conda / systemd / ...
```

连接方式本身是可插拔能力，而不是"假设 shell 环境已经配好了"。这来自一个真实需求：
一组组里的服务器中，`gpu-1` 在校园网内、**必须**经 VPN 才可达，其余三台公网直连——
Agent 不应该每次都重新发现这件事。

## 现在读什么

| 文档 | 内容 |
|---|---|
| [docs/01-architecture.md](docs/01-architecture.md) | 分层架构、进程模型（daemon vs 单进程）、crate 划分、数据流 |
| [docs/02-abstractions.md](docs/02-abstractions.md) | 四个核心抽象：Target / Connector / Session / Tool，以及 base 能力的精确签名 |
| [docs/03-plugins-wit.md](docs/03-plugins-wit.md) | 插件模型、WIT 接口草案、capability 权限、为什么 v0.1 先不上 WASM |
| [docs/04-mcp-surface.md](docs/04-mcp-surface.md) | 对外 MCP 工具面：命名、lazy load、list_changed、Claude Code 的具体行为 |
| [docs/05-monitor.md](docs/05-monitor.md) | Monitor WebSocket 设计（含"必须传 timeout、到期自动关"规则） |
| [docs/06-roadmap.md](docs/06-roadmap.md) | v0.1→v0.4 里程碑、每版验收标准、第一版范围边界 |
| [docs/07-fleet-lessons.md](docs/07-fleet-lessons.md) | **从 Python 原型实测来的数据、整组机器代理实测表、四个必踩的坑** |
| [docs/08-open-questions.md](docs/08-open-questions.md) | **开工前需要你拍板的 6 个决策点（都给了推荐）** |
| [docs/reference/](docs/reference/) | 原始设计对话（ChatGPT，2130 行） |

建议顺序：`01` → `07`（实测事实）→ `08`（拍板）→ `06`（范围）。其余按需。

## 配置

| 文件 | 说明 |
|---|---|
| [config/trestle.toml](config/trestle.toml) | 拓扑、connector 绑定、路径约定。**不含密码，可入库** |
| `config/secrets.toml` | 真实凭据。**已 gitignore**，内容见 `secrets.example.toml` |
| [config/secrets.example.toml](config/secrets.example.toml) | 凭据模板 |

整组机器的真实拓扑和凭据已经迁移进来了（`secrets.toml` 不在 git 里）。

## 技术选型（已核实版本，2026-08-17）

| 层 | 选择 | 版本 |
|---|---|---|
| MCP 前端 | [`rmcp`](https://crates.io/crates/rmcp)（官方 Rust SDK） | **3.1.2** |
| SSH | [`russh`](https://crates.io/crates/russh)（纯 Rust，async） | **0.62.6** |
| 插件运行时（v0.3+） | [`wasmtime`](https://crates.io/crates/wasmtime) + Component Model | **47.0.3** |
| 异步 / HTTP / WS | `tokio` 1.53 / `axum` 0.8.9 / `tokio-tungstenite` 0.30 | — |
| 本机工具链 | cargo 1.97.1 / rustc 1.97.1，`cross` 已装（**musl target 未装**，见 Q2） | — |

完整依赖表在 [`Cargo.toml`](Cargo.toml) 的 `[workspace.dependencies]`，**已用 probe crate 跑通
`cargo fetch`**（365 个包 resolve 干净）。workspace 骨架 `cargo check` 通过。
各 crate 目前尚未引用这些依赖，所以 `cargo check` 不会去下载——开工时逐个启用。

⚠️ `docs/reference/` 里那份设计对话给的 rmcp 示例代码是 **2.x** 的，**已过时**（现在是 3.1.2，2.x→3.x 有
breaking change）。宏名 `#[tool]` / `#[tool_router]` / `#[tool_handler]` 仍在，transport 只有
**stdio** 和 **Streamable HTTP**（没有 WebSocket——所以 Monitor 的 ws 必须独立于 MCP）。
开工时以 `cargo add rmcp` 后的实际签名为准，不要照抄那份文档里的代码。

## 状态

**尚未开工**——本仓库当前只有设计文档、配置和接口草案。上一代 Python 实现在
`D:\Scripts\fleet`（已从全局 MCP 注册移除，保留作移植参考，CLI 仍可应急使用）。
