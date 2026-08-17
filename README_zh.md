[English](README.md) · **简体中文**

# Trestle

让 coding agent 操作远程服务器的运行时。

Trestle 在本机跑一个常驻进程，把你的服务器以 MCP 工具的形式交给 Claude Code、
Codex 或任何 MCP 客户端：跑命令、读写文件、机器之间搬数据、管长任务、抢显卡、
打通端口。

```
$ trestle targets
gpu-cluster
  gpu-1   8 × GPU，只有走代理才可达
  gpu-2     8 × GPU
  gpu-3    8 × GPU
  gpu-4    8 × GPU，盘最宽裕
cloud
  web-1    自有服务器（云厂商 A）
  web-2  自有服务器
```

同样这些能力，agent 在会话里看到的是 31 个工具（`base_shell`、`job_start`、
`gpu_acquire`、`xfer_between`…）。

## 环境要求

- Rust 1.90+，以及 `wasm32-wasip2` 目标：`rustup target add wasm32-wasip2`
- Windows（安装脚本是 PowerShell；核心是跨平台的 Rust，但只在 Windows 11 上验证过）
- 目标服务器上有 python 3.9+（远端 agent 只用标准库）
- 可选：`uv tool install componentize-py`，只有要用 Python 写插件才需要

## 安装

```powershell
git clone <this repo> && cd Trestle
copy config\trestle.example.toml config\trestle.toml
copy config\secrets.example.toml config\secrets.toml
# 编辑这两个文件：填你的机器和凭据

.\scripts\install.ps1 -Register
```

`install.ps1` 会编译（含把插件编到 wasm）、装成一个自包含目录 `dist\`、
注册给 Claude Code 与 Codex。重开一个会话，工具就在了。

常用参数：

| 参数 | 作用 |
|---|---|
| `-Register` | 注册给 Claude Code 与 Codex |
| `-Only claude` / `-Only codex` | 只注册其中一个 |
| `-SkipBuild` | 不重新编译，只重新装配目录 |
| `-Dest <路径>` | 装到别处（默认 `dist\`） |
| `-Uninstall` | 注销并停掉 daemon（配置与凭据保留） |

装出来的目录是**自包含**的——三个可执行文件、配置、凭据、插件、状态全在一起。
`trestle-mcp` 需要在自己旁边找到 `trestled`，所以不能把它们分散放。
已存在的 `trestle.toml` / `secrets.toml` 不会被覆盖。

## 配置

一个文件、一个入口：`trestle.toml`。凭据在同目录的 `secrets.toml`（已 gitignore）。

```toml
# 一组机器怎么进去
[connectors.gpu-cluster]
plugin = "ssh-socks5"              # 驱动：经 SOCKS5 代理的 SSH
socks = "127.0.0.1:11080"
allow_exec = ["docker"]            # 准它在本机跑哪些命令

[connectors.gpu-cluster.ready]  # 前置条件：探不通就把代理拉起来
check = ["docker", "ps", "-a", "--filter", "name=^vpn-proxy$", "--format", "{{.Names}}"]
check_expect = "vpn-proxy"
start = ["docker", "start", "vpn-proxy"]

# 一台机器
[targets.gpu-4]
connector = "gpu-cluster"
host = "203.0.113.31"
port = 22
user = "alice"
workdir = "/home/alice/data"
aliases = ["node-16"]
note = "8 × GPU。盘最宽裕，跑新东西优先考虑这台。"
```

`note` 会原样交给 agent，所以把「哪个盘满了」「东西该放哪」写进去——它比 agent 猜的准。
机器叫什么名字由这里决定，驱动里不写死任何称呼。

完整字段见 [config/trestle.example.toml](config/trestle.example.toml)。

## 使用

agent 在会话里直接调工具。本机也可以用 CLI：

```powershell
trestle targets                     # 有哪些机器，秒回，不建连接
trestle exec gpu-4 "nvidia-smi"       # 跑一条短命令
trestle read gpu-4 /path/to/file
trestle upload gpu-4 .\local /remote --sync
trestle forward gpu-4 8080            # 端口映射，本地口由 host 分配
trestle call job_start '{"target":"gpu-4","command":"python train.py","gpus":"auto:2"}'
trestle agents                      # 谁在线、在干什么
trestle doctor                      # 建链、量延迟、检查前置条件
```

Web UI 在 daemon 的 HTTP 端口上（`trestle doctor` 会打印地址）：机器状态、任务表、
实时事件流、配置编辑页。

## 工作原理

```
Claude Code / Codex / CLI / 浏览器
            │  MCP stdio · IPC · HTTP
            ▼
        trestled（常驻）
            │  七个基本操作，按 target 路由
            ▼
     connector 插件（wasm）  →  SSH / 代理 / 长连接
            ▼
     远端 agent（python，常驻）
```

**基本操作只有七个**：`read` / `write` / `edit` / `shell` / `upload` / `download` /
`forward`。其余一切——任务管理、文件浏览、跨机搬运、全队概览、显卡仲裁、监视、
Web UI——都是建在这七个操作之上的 WASM 插件。

**connector 是一整块自包含的接入能力。** 它向上只暴露一个名字和这七个操作；
向下自己管连哪些机器、怎么连、断了怎么重试、远端 agent 怎么部署。上层不知道
下面是 SSH 还是别的。现在有两个驱动：`ssh-socks5`（经代理）和 `ssh-direct`（直连），
同一个驱动可以配成任意多组机器。

**插件没有自己的 I/O。** wasm 组件没有 syscall，它唯一能碰到外界的地方是 host 导入，
而每个导入的入口处都检查 capability。所以「这个插件能跑本机命令吗」是一个
manifest 里能读出来的事实，不是一句约定。

**状态在 daemon 里**，不在每个 MCP 会话里。Claude Code 每开一个会话就拉起一个
MCP 进程；连接如果跟着会话走，每次都要重建（gpu-1 经 VPN 是数秒）。放在 daemon
里之后连接真正只建一次，跨会话、跨 CLI 复用。

## 写一个插件

```powershell
trestle plugin new mytool --description "干什么的"
# 改 plugins/tools/mytool/src/lib.rs 里的 list_tools 与 call
.\scripts\build-plugins.ps1
trestle plugin reload
```

脚手架不改一个字就编译通过。`reload` 之后 Claude Code **不用重连**就能看到新工具。

这是这个项目想成立的那条闭环：遇到一个没有工具的操作，生成脚手架、填十几行、
reload，它就变成常驻工具了——下次不用再拼一遍命令。

插件也可以用 Python 写（走 componentize-py，同一份 WIT），代价是组件从 150 KB
变成 18 MB。

## 性能

一支真实机队实测（2026-08-17）：

| | 稳态冷启动 | 热调用 | 断线自愈 |
|---|---|---|---|
| gpu-4 | 566ms | 36ms | 508ms |
| gpu-1（经 VPN） | 2.4s | 55ms | 2.5s |
| web-1 / web-2 | 1.0–1.2s | 26 / 116ms | ~1s |

冷热差 36 倍。这就是状态放在 daemon 里的原因。

## 文档

| 文档 | 内容 |
|---|---|
| [01-architecture.md](docs/01-architecture.md) | 全局图、四个设计支点、一次调用的完整数据流 |
| [02-seven-operations.md](docs/02-seven-operations.md) | 七个基本操作，以及每一条为什么长这样 |
| [03-connectors.md](docs/03-connectors.md) | connector 自包含什么、传输工具箱、远端 agent |
| [04-plugins.md](docs/04-plugins.md) | 插件能看到什么、capability、实例池、接口兼容性 |
| [05-monitor-and-ui.md](docs/05-monitor-and-ui.md) | Monitor 的 ws 契约、事件模型、Web UI |
| [06-multi-agent.md](docs/06-multi-agent.md) | 在场感知、会话级资源、留言板、资源单点仲裁 |
| [07-fleet-lessons.md](docs/07-fleet-lessons.md) | 实测数据与六个坑（唯一从上一代继承的文档） |
| [08-operating.md](docs/08-operating.md) | 安装、配置、接进 Claude Code 与 Codex、排查 |
| [09-source-map.md](docs/09-source-map.md) | 每个文件干什么、里面有什么骨架 |

## 目录结构

```
crates/
  trestle-core        类型、错误、统一配置
  trestle-transport   TCP/SOCKS5 拨号、SSH、幂等部署、分块传输、端口转发
  trestle-host        wasm 宿主：capability 强制、实例池、target → connector 路由
  trestle-daemon      trestled：IPC、事件总线、协同层、ws、Web UI、状态持久化
  trestle-mcp         MCP stdio 前端
  trestle-cli         trestle 命令行
agent-py/             远端 agent（常驻，只用标准库）
plugins/
  connectors/         ssh-socks5 · ssh-direct
  lib/                connector-ready（两个驱动共用的前置条件状态机）
  tools/              job · fs · gpu · fleet · xfer · monitor · hello-py
  templates/rust/     trestle plugin new 的模板
wit/trestle.wit       插件接口（connector 与 tool-plugin 两个世界）
```

## 技术栈

| 层 | 选择 | 版本 |
|---|---|---|
| 插件运行时 | [`wasmtime`](https://crates.io/crates/wasmtime) + Component Model | 47.0.3 |
| MCP 前端 | [`rmcp`](https://crates.io/crates/rmcp)（官方 Rust SDK） | 3.1.2 |
| SSH | [`russh`](https://crates.io/crates/russh)（纯 Rust，async） | 0.62.6 |
| 插件绑定 | `wit-bindgen` / `componentize-py` | 0.60 / 0.25 |
| 异步 / HTTP / WS | `tokio` / `axum` / `tokio-tungstenite` | 1.53 / 0.8 / 0.30 |

## 测试

```powershell
cargo test --workspace                                # 不需要真机
wsl python3 agent-py/test_agent.py                    # 远端 agent 协议，61 项
$env:TRESTLE_HOME = "<repo>\config"
cargo test --workspace -- --ignored --test-threads=1  # 真机验收
```

真调测试默认 `#[ignore]`，因为它们真的会连服务器、起进程、传文件。但它们才是有价值
的那部分：上一代靠「逐个工具真调」在 53 个工具里抓到过 1 个 mock 测试永远抓不到的 bug。

## 当前状态

v0.1.0，个人项目。一支真实机队上跑通了七个基本操作、任务管理、跨机搬运、GPU 仲裁、
多 agent 协同与 Web UI。已知限制：

- 安装脚本只有 PowerShell 版，只在 Windows 11 上验证过
- 插件用 WASI Preview 2（组件模型的 async 尚未采用，见 docs/04）
- 没有发布到 crates.io，也还没有二进制发行版
