# 01 · 架构

整个系统立在两句话上。

> **1. 基本操作只有七个**：read / write / edit / shell / upload / download / forward。
>
> **2. connector 是一整块自包含的接入能力**——向上只暴露一个 name 和这七个操作，
> 向下自己管连哪些机器、怎么连、长连接怎么维持、断了怎么重试、远端 agent 怎么部署。
> **上层永远不知道下面是 SSH 还是别的。**

其余一切——任务管理、文件浏览、跨机搬运、全队概览、显卡、监视、Web UI——都是建在
这七个操作之上的 WASM 插件。

## 全局图

```
      Claude Code A      Claude Code B        终端        浏览器
           │                  │                │            │
       MCP stdio          MCP stdio           CLI        HTTP/ws
           └──────────────────┴────────────────┴────────────┘
                               │ IPC (127.0.0.1 + token)
                               ▼
┌───────────────────────────────────────────────────────────────────┐
│                            trestled                               │
│  ConfigStore · AgentRegistry（会话 + forward 归属）                │
│  GpuArbiter（单点分配）· Noticeboard（TTL）· TaskScheduler         │
│  EventBus · Monitor ws · Web UI 挂载 · 状态落盘与懒恢复            │
│                                                                   │
│  ┌──────────────── WASM Host (wasmtime 47) ────────────────────┐  │
│  │  capability 强制 · 实例池 · target → connector 路由          │  │
│  │                                                             │  │
│  │   plugins/tools/*.wasm            plugins/connectors/*.wasm │  │
│  │   job·fs·xfer·fleet·monitor         ssh-socks5            │  │
│  │        │                            ssh-direct            │  │
│  │        │ 调七个基本操作                      ▲               │  │
│  │        └────► Router: target → connector ───┘ 实现七个基本操作│  │
│  └──────────────────────────────────┬──────────────────────────┘  │
│                                     │ 传输工具箱（host native）    │
│            net.dial · socks5 · ssh.session · local.exec · deploy  │
└─────────────────────────────────────┬─────────────────────────────┘
                                      │
        ┌──────────┬──────────┬───────┴───┬────────────┬────────────┐
      gpu-1        gpu-2        gpu-3         gpu-4      198.51.100.10  198.51.100.20
        └──────────── 标准 agent-py（uv，常驻，JSON-Lines 多路复用）────────┘
```

## 四个设计支点

### 1. daemon 与前端分离

`trestle-mcp` 和 `trestle` 都是**瘦客户端**，真正的状态在 `trestled`。

这不是洁癖：MCP server 由 Claude Code 按 stdio 拉起，**每个会话一个进程**。状态放在
前端的话，每开一个会话就要把全部连接重建一遍（gpu-1 经 VPN 实测 2.4 秒）。有了 daemon：

* 连接**真正**只建一次，跨会话、跨 CLI 复用；
* Monitor 的 ws、后台任务、插件实例、资源仲裁都有唯一归属；
* 一个 agent 能看见别的 agent 在干什么。

**lazy 启动**：任一客户端连不上就自己 spawn 它，用户永远不需要手动 `trestled start`。
**idle 退出**：没人连着也没活儿干，超过 `idle_timeout_secs` 就自行退出。

### 2. host 是工具箱，插件是编排

host 提供的是**机械动作**——搬字节、做加解密、分块校验、按哈希部署。
什么时候拉容器、走哪条路、断了重试几次、agent 装在哪，全是 connector 插件的决定。

之所以 SSH 留在 host，纯粹是因为 russh 建在 tokio 之上而 tokio 的 `net` 在 wasi target
上不支持（[russh#224](https://github.com/Eugeny/russh/issues/224)、
[tokio#6526](https://github.com/tokio-rs/tokio/discussions/6526)）——**不是因为它「属于」host**。
所以架构里没有 `ssh.wasm` 这个格子：SSH 是某个 connector 的内部实现细节，不是一层。

### 3. 插件没有任何自己的 I/O

wasm 组件没有 syscall。插件唯一能碰到外界的地方就是 host 导入，而每个导入的入口处
都有 capability 检查。所以「插件只能通过基本操作做事」不是约定，是**真的强制**。

```
                          ✗ 插件自己开 socket / 起进程 / 读文件
plugin.wasm ──────────────────────────────────────────────► OS
     │
     │ ✓ 唯一出口：host 导入
     ▼
  base 七操作 / host 服务 ── capability 检查 ──► connector ──► 远程机器
```

被拒绝的调用会发一条 `plugin_call_denied` 事件——否则权限模型就是个黑盒，
出问题时没人知道是被挡了还是根本没调。

### 4. 并发在 host 侧

一个 wasm 实例被调用期间是独占的。所以：

* 每个 connector 起一个**实例池**（默认 4），实例之间**共享连接**——
  否则四个实例会对同一台机器建四条连接；
* 插件要打多台机器时调 `base.call-many`，由 host 并发扇出。
  自己写循环打整队，在冷启动时就是六倍延迟。

## Crate 划分

```
crates/
  trestle-core        抽象、错误、事件、ConfigStore
  trestle-transport   传输工具箱：TCP/SOCKS5、russh、hash 幂等部署、direct-tcpip
  trestle-host        WASM 宿主：wasmtime、capability、实例池、target→connector 路由
  trestle-daemon      trestled：IPC、EventBus、协同层、ws、Web UI、状态持久化
  trestle-mcp         MCP stdio 前端（rmcp 3.1），瘦客户端
  trestle-cli         trestle 命令行，瘦客户端
agent-py/             标准远端 agent（uv），七个操作的远端一侧
plugins/
  connectors/         ssh-socks5 · ssh-direct（驱动；配置节才是 connector 实例）
  lib/                connector-ready（两个驱动共用的前置条件状态机）
  tools/              job · fs · xfer · fleet · monitor · hello-py
  templates/rust/     `trestle plugin new` 的脚手架
wit/trestle.wit       插件接口（connector 与 tool-plugin 两个世界）
```

依赖方向严格单向：`core ← {transport, host}`，`host ← daemon`，`daemon ← {mcp, cli}`。

## 一次调用的完整数据流

以 `job_start(target="gpu-4", command="python train.py", gpus="auto:2")` 为例：

```
Claude Code
   │ tools/call job_start
   ▼
trestle-mcp ──IPC──► trestled
                        │ ToolRegistry: job_start → job.wasm
                        ▼
                    job.wasm
                        │ plugins.call(gpu, gpu_acquire, {gpu-4, 2})
                        │    └─ gpu.wasm 查 nvidia-smi → arbiter.acquire ← host 在一把锁里挑
                        │ base.call("gpu-4", "shell", {detach:true, env:{CUDA_VISIBLE_DEVICES}})
                        ▼
                    Router: gpu-4 → gpu-cluster（驱动 ssh-socks5.wasm）
                        ▼
                    ssh-socks5.wasm
                        │ session-lookup("gpu-4") → 有活连接就直接用
                        │ 没有：probe 11080 → local.exec(配置里那条 start) → dial-socks5
                        │       → ssh.connect → agent.ensure（按 hash 幂等部署）
                        ▼
                    agent.call(handle, "shell", …)
                        ▼
                    远端 agent-py：setsid 起进程，pid/rc/日志落盘
                        │
                        ├──► EventBus ──► Monitor ws / Web UI
                        ▼
                    结果一路回到 Claude Code
```

注意 `job.wasm` **没有** SSH 能力——它只是调了 `base.call`。这是权限模型成立的前提。
