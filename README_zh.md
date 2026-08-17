# Trestle

[![CI](https://github.com/vintcessun/trestle/actions/workflows/ci.yml/badge.svg)](https://github.com/vintcessun/trestle/actions/workflows/ci.yml)
[![License: MIT](https://img.shields.io/badge/license-MIT-blue.svg)](LICENSE)

[[English](README.md)] [简体中文]

Trestle 是给 coding agent 用的基础设施运行时。它在本机跑一个常驻进程，把你的服务器
以 MCP 工具的形式交给 Claude Code、Codex 或其他 MCP 客户端：跑命令、读写文件、
机器之间搬数据、管长任务、分配显卡、打通端口。

它有七个基本操作：read、write、edit、shell、upload、download、forward。
其余一切，包括任务管理、跨机搬运、GPU 仲裁和 Web UI，都是建在这七个操作之上的
WebAssembly 插件。

```console
$ trestle targets
gpu-cluster
  gpu-1    alice@203.0.113.10:2201  /mnt/data/alice/work
           8 x GPU。只有走代理才可达。
  gpu-4    alice@203.0.113.31:2204  /home/alice/data
           8 x GPU。盘最宽裕，跑新东西优先考虑这台。
cloud
  web-1    root@198.51.100.10:22  /root

$ trestle exec gpu-4 "nvidia-smi --query-gpu=name --format=csv,noheader"
$ trestle call job_start '{"target":"gpu-4","command":"python train.py","gpus":"auto:2"}'
```

## 安装

需要 Rust 1.90 以上，以及 `wasm32-wasip2` 目标。目标服务器上要有 Python 3.9+。

```console
$ rustup target add wasm32-wasip2

# Windows
$ .\scripts\install.ps1 -Register

# Linux、macOS
$ ./scripts/install.sh --register
```

它会编译二进制与插件、装到 `dist/`、把 `trestle` 放进 `PATH`、并把 MCP server
注册给 Claude Code 与 Codex。装完新开一个 shell 和一个 agent 会话。

| 参数 | 作用 |
|---|---|
| `-Register` / `--register` | 注册给 Claude Code 与 Codex |
| `-Only codex` / `--only codex` | 只注册其中一个 |
| `-SkipBuild` / `--skip-build` | 不重新编译，只重新装配 |
| `-Dest` / `--dest` | 装到 `dist/` 以外的地方 |
| `-Uninstall` / `--uninstall` | 注销并停掉 daemon |

三个二进制必须待在同一个目录：`trestle-mcp` 会在自己旁边找 `trestled`，配置、
插件与状态也默认在同一处。已存在的 `trestle.toml` 与 `secrets.toml` 不会被覆盖。

Windows、Linux、macOS 的预编译包在每个
[release](https://github.com/vintcessun/trestle/releases) 里。

## 配置

`trestle.toml` 放机器与 connector；同目录的 `secrets.toml` 放凭据，已 gitignore。
复制样例改成自己的。

```toml
[connectors.gpu-cluster]
plugin = "ssh-socks5"                # 驱动：经 SOCKS5 代理的 SSH
socks = "127.0.0.1:11080"
allow_exec = ["docker"]              # 准它在本机跑哪些命令

[connectors.gpu-cluster.ready]       # 可选：代理没起来就把它拉起来
check = ["docker", "ps", "-a", "--filter", "name=^vpn-proxy$", "--format", "{{.Names}}"]
check_expect = "vpn-proxy"
start = ["docker", "start", "vpn-proxy"]

[targets.gpu-4]
connector = "gpu-cluster"
host = "203.0.113.31"
port = 22
user = "alice"
workdir = "/home/alice/data"
aliases = ["node-16"]
note = "8 x GPU。盘最宽裕，跑新东西优先考虑这台。"
```

`note` 会原样交给 agent，适合写「哪个盘满了」「东西该放哪」这类信息。
完整字段见 [`config/trestle.example.toml`](config/trestle.example.toml)。

## 使用

agent 直接调工具。同样的操作 CLI 也有：

```console
$ trestle targets                     # 有哪些机器，按 connector 分组
$ trestle exec gpu-4 "nvidia-smi"
$ trestle read gpu-4 /path/to/file
$ trestle upload gpu-4 ./local /remote --sync
$ trestle forward gpu-4 8080          # 本地端口由 host 分配
$ trestle agents                      # 谁连着、在干什么
$ trestle doctor                      # 建链、量延迟、打印 Web UI 地址
```

daemon 按需自启，没有 `trestled start` 这一步。Web UI 在 daemon 的 HTTP 端口上，
提供机器状态、任务表、实时事件流和配置编辑页。

## 工具

| 工具 | 用途 |
|---|---|
| `base_read` `base_write` `base_edit` `base_shell` | 文件与命令 |
| `base_upload` `base_download` `base_forward` | 传输与端口映射 |
| `job_start` `job_list` `job_logs` `job_wait` `job_stop` | 长任务 |
| `fs_list` `fs_find` `fs_stat` `fs_tree` `fs_disk` | 远端文件系统 |
| `gpu_status` `gpu_find` `gpu_acquire` `gpu_release` | GPU 仲裁 |
| `fleet_status` `fleet_run` `targets_list` | 全队 |
| `xfer_between` `xfer_distribute` | 机器之间 |
| `monitor_open` | 实时输出的 WebSocket 端点 |
| `agents_list` `notes_list` `note_put` | 多 agent 协同 |

每个针对单机的工具都必须显式给 `target`，没有默认机。

## 工作原理

```
Claude Code / Codex / CLI / 浏览器
            |  MCP stdio、IPC、HTTP
        trestled
            |  七个基本操作，按 target 路由
     connector 插件（wasm）  ->  SSH、代理、长连接
     远端 agent（python，常驻）
```

一个 connector 是一整条进入路径。它向上暴露一个名字和这七个操作，向下管连哪些
机器、怎么连、断线重连、远端 agent 怎么部署。自带两个驱动：`ssh-socks5` 与
`ssh-direct`，一个驱动可以支撑任意多个 connector。

插件没有自己的 I/O。WebAssembly 组件没有 syscall，host 导入是它唯一的出口，
而每个导入都会检查插件 manifest 里声明的 capability。

连接放在 daemon 里而不是每个 MCP 会话里，因为客户端每开一个会话就起一个 MCP 进程，
而重建连接每台机器要花掉数秒。

这些取舍的来龙去脉在 [docs/01-architecture.md](docs/01-architecture.md)。

## 写一个插件

```console
$ trestle plugin new mytool --description "干什么的"
$ cd plugins/tools/mytool && cargo build --release --target wasm32-wasip2
$ trestle plugin reload
```

脚手架不改一个字就能编译。`plugin reload` 之后 daemon 会推 `tools/list_changed`，
客户端不用重连就能看到新工具。插件也可以用 Python 写（componentize-py），
代价是组件体积大得多。

## 文档

| 文档 | 内容 |
|---|---|
| [01-architecture.md](docs/01-architecture.md) | 全局图与一次调用的完整数据流 |
| [02-seven-operations.md](docs/02-seven-operations.md) | 七个基本操作及其语义 |
| [03-connectors.md](docs/03-connectors.md) | connector、传输工具箱、远端 agent |
| [04-plugins.md](docs/04-plugins.md) | capability、实例池、接口兼容性 |
| [05-monitor-and-ui.md](docs/05-monitor-and-ui.md) | Monitor 的 ws 契约、事件模型、Web UI |
| [06-multi-agent.md](docs/06-multi-agent.md) | 在场感知、会话级资源、资源仲裁 |
| [07-fleet-lessons.md](docs/07-fleet-lessons.md) | 上一代的实测数据与踩过的坑 |
| [08-operating.md](docs/08-operating.md) | 安装、配置、接进 agent、排查 |
| [09-source-map.md](docs/09-source-map.md) | 每个文件干什么 |

## 开发

```console
$ cargo test --workspace                 # 不需要真机
$ python3 agent-py/test_agent.py         # 远端 agent 协议
$ ./scripts/check-public.ps1             # 跟踪文件里没有个人基础设施信息

$ TRESTLE_HOME=$PWD/config cargo test --workspace -- --ignored --test-threads=1
```

连真实服务器的测试默认是 `#[ignore]`。

目录结构：

```
crates/         core、transport、host、daemon、mcp、cli
agent-py/       远端 agent（常驻，只用标准库）
plugins/        connectors/、lib/、tools/、templates/
wit/            插件接口（connector 与 tool-plugin 两个世界）
```

## 许可

MIT。
