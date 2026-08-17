# Trestle

**An extensible infrastructure runtime for coding agents.**

> Trestle lets agents turn infrastructure friction into reusable capabilities.

给 Coding Agent（Claude Code / Codex 等）用的远程基础设施运行时。它不是"又一个 SSH MCP 包装"——
那条赛道已经很拥挤。Trestle 的赌注是另一个东西：

```
Agent 遇到摩擦  →  解决一次  →  固化成 capability  →  永久复用
        ↑                                                  │
        └──────────────── 下一个摩擦 ←────────────────────┘
```

整个系统立在两句话上：

> **1. 基本操作只有七个**：read / write / edit / shell / upload / download / forward。
>
> **2. connector 是一整块自包含的接入能力**——向上只暴露一个 name 和这七个操作，
> 向下自己管连哪些机器、怎么连、断了怎么重试、远端 agent 怎么部署。
> **上层永远不知道下面是 SSH 还是别的。**

其余一切——任务管理、文件浏览、跨机搬运、全队概览、显卡、监视、Web UI——都是建在
这七个操作之上的 **WASM 插件**。插件没有任何自己的 I/O：wasm 组件没有 syscall，
它唯一能碰到外界的地方是 host 导入，而每个导入的入口处都有 capability 检查。

## 装它

```powershell
.\scripts\install.ps1 -Register
```

编、装成一个自包含的目录（`dist\`）、注册给 Claude Code 与 Codex。重开一个会话就有了。

装出来的目录是自包含的：三个可执行文件、配置、凭据、插件、状态全在一起——
`trestle-mcp` 要能在自己旁边找到 `trestled`，散着放这条链就断了。已有的
`trestle.toml` / `secrets.toml` **绝不会被覆盖**。

怎么教 agent 用好它：Claude Code 读 `.claude/skills/trestle/SKILL.md`（`-Register`
会装到 `~\.claude\skills\`）；Codex 没有 skill，所以同样的要点放在 MCP 的
`instructions` 里，两边都拿得到。

## 现在能做什么

```
$ trestle targets                    # 整支机队，按 connector 分组，秒回
$ trestle exec gpu-4 "nvidia-smi"      # 热调用 36ms
$ trestle call job_start '{"target":"gpu-4","command":"python train.py","gpus":"auto:2"}'
$ trestle call monitor_open '{"timeout_secs":3600,"only_job":"train-..."}'
$ trestle agents                     # 谁在线、在干什么、开着哪些转发
$ trestle plugin new mytool          # 生成脚手架 → 编译 → reload → 变成常驻工具
```

**摩擦 → capability 的闭环是真的**：`plugin new` 生成的脚手架不改一个字就编译通过，
`plugin reload` 之后 Claude Code **不用重连**就能看到新工具。

## 实测（一支真实机队，2026-08-17）

| | 稳态冷启动 | 热调用 | 自愈 |
|---|---|---|---|
| gpu-4 | 566ms | 36ms | 508ms |
| gpu-1（经 VPN） | 2.4s | 55ms | 2.5s |
| web-1 / web-2 | 1.0–1.2s | 26 / 116ms | ~1s |

冷热差 36 倍——这就是为什么状态在 daemon 里而不在每个 MCP 会话里。

## 文档

| 文档 | 内容 |
|---|---|
| [01-architecture.md](docs/01-architecture.md) | 全局图、四个设计支点、一次调用的完整数据流 |
| [02-seven-operations.md](docs/02-seven-operations.md) | 七个基本操作，以及每一条为什么长这样 |
| [03-connectors.md](docs/03-connectors.md) | connector 自包含什么、传输工具箱、远端 agent |
| [04-plugins.md](docs/04-plugins.md) | 插件能看到什么、capability、怎么写一个 |
| [05-monitor-and-ui.md](docs/05-monitor-and-ui.md) | Monitor 的 ws 契约、事件模型、Web UI |
| [06-multi-agent.md](docs/06-multi-agent.md) | 在场感知、会话级资源、留言板、GPU 单点分配 |
| [07-fleet-lessons.md](docs/07-fleet-lessons.md) | **实测数据与六个坑**（唯一从上一代继承的文档） |
| [08-operating.md](docs/08-operating.md) | 构建、配置、接进 Claude Code、CLI、排查 |

## 布局

```
crates/     core · transport · host · daemon · mcp · cli
agent-py/   标准远端 agent（uv，常驻，只用标准库）
plugins/    connectors/{ssh-socks5,ssh-direct} · lib/connector-ready
            tools/{job,fs,xfer,fleet,monitor,hello-py}
            templates/rust/   ← trestle plugin new 的模板
wit/        插件接口（connector 与 tool-plugin 两个世界）
```

## 技术选型（已核实版本）

| 层 | 选择 | 版本 |
|---|---|---|
| 插件运行时 | [`wasmtime`](https://crates.io/crates/wasmtime) + Component Model | 47.0.3 |
| MCP 前端 | [`rmcp`](https://crates.io/crates/rmcp)（官方 Rust SDK） | 3.1.2 |
| SSH | [`russh`](https://crates.io/crates/russh)（纯 Rust，async） | 0.62.6 |
| 插件绑定 | `wit-bindgen` / `componentize-py` | 0.60 / 0.25 |
| 异步 / HTTP / WS | `tokio` 1.53 / `axum` 0.8 / `tokio-tungstenite` 0.30 | — |
| 远端 | python 3.9+（uv 固定版本，只用标准库） | — |

插件编到 `wasm32-wasip2`（Rust 插件约 150 KB，Python 插件约 18 MB）。

## 测试

```powershell
cargo test --workspace                              # 不需要真机
wsl python3 agent-py/test_agent.py                  # 远端 agent 协议，61 项
$env:TRESTLE_HOME = "<repo>\config"
cargo test --workspace -- --ignored --test-threads=1  # 真机验收
```

真调测试默认 `#[ignore]`，因为它们真的会连服务器、真的起进程、真的传文件。
但它们才是有价值的那部分——上一代靠「逐个工具真调」在 53 个工具里抓到过 1 个
mock 测试永远抓不到的 bug。
