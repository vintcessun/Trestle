# 05 · Monitor（WebSocket）

## 为什么必须独立于 MCP

两条硬约束交汇在这里：

1. **rmcp 3.x 没有 WebSocket transport**（只有 stdio 和 Streamable HTTP）。WebSocket 从来不是
   MCP 标准 transport——官方 Python SDK v2 甚至删掉了它。
2. **Claude Code 的 Monitor 工具只接受两种事件源**（已查工具定义核实）：
   * `command`：本地 shell 命令，**stdout 每一行 = 一条通知**；
   * `ws`：WebSocket URL，**每个 text frame = 一条通知**，socket close 结束监视。

所以：MCP 归 MCP，Monitor 走 daemon 自己的 HTTP 服务上的 `/monitor/ws`。两者不绑定。

这条路是通的——Monitor 原生支持 ws，意味着 agent 只需要拿到一个 URL 就能挂上监视，
不用再拼一条带 Windows 路径和正则转义的命令行。这是相对上一代最大的摩擦削减。

## 生命周期规则（用户明确要求）

```
ws 端点由 host 进程（trestled）管理
   ├─ host 退出        → 所有 ws 关闭
   ├─ 开启时必须传 timeout_secs → 到期 host 主动关闭
   └─ 关闭前推最后一帧，说明为什么关
```

`monitor_open` 的签名：

```
monitor_open(timeout_secs: u32,          // 必填，无默认值
             target?: string,
             job_id?: string,
             filter?: { quiet: [regex], alert: [regex] })
  -> { ws_url, expires_at, cli_command }
```

`timeout_secs` **必填**是刻意的：一个没有过期时间的监视端点会悄悄泄漏——任务早就结束了，
ws 还挂在那里占着轮询。强制传值让调用方每次都想一下"这个任务大概跑多久"。

关闭时必须先推一帧再 close，让 agent 知道发生了什么：

```json
{"type":"closing","reason":"timeout","detail":"monitor expired after 3600s; job train-xxx still running"}
{"type":"closing","reason":"job_finished","exit_code":0,"elapsed_s":4210}
{"type":"closing","reason":"host_shutdown"}
```

区分 `timeout` 和 `job_finished` 很关键：前者意味着**任务还在跑但没人盯了**，agent 需要重新开一个；
后者才是真正结束。如果只是静默 close，两种情况看起来一模一样。

## 事件模型

daemon 内部一条 EventBus，所有东西往里发：

```
SessionConnected / SessionLost / SessionRecovered
ShellStarted / ShellOutput / ShellFinished
JobStarted / JobProgress / JobFinished
FileRead / FileWritten / FileEdited
PluginLoaded / PluginCallDenied        ← capability 拒绝要能被看到
ConnectorEnsureReady / ConnectorFailed
ToolCalled / ToolFinished
```

两个订阅者：`tracing`（落日志）和 WebSocket broadcast（给 Monitor / 将来的 Web UI）。

每个 ws 连接自带一个 filter（订阅哪个 target / job、quiet 正则、alert 正则），**在服务端过滤**——
不要把原始日志全推出去，Monitor 对高频事件会自动抑制甚至停掉。

## 覆盖面：静默不等于正常

Monitor 工具文档里的这条警告，对我们的默认 filter 是硬要求：

> 只匹配成功标志的监视器，在崩溃/挂死时保持沉默——而沉默看起来和"还在跑"一模一样。

所以 `monitor_open` 的默认 alert 正则必须覆盖所有终态，宁宽勿窄：

```
Traceback | \bFAIL\b | \bERROR\b | OOM | Killed | CUDA out of memory | AssertionError
```

并且 **job 结束一定推 `closing` 帧**（无论成功失败），不依赖日志里出现某个词。

## 两条路都要保留

| 路径 | 适用 | 弱点 |
|---|---|---|
| `ws` (Monitor 直连) | 默认。摩擦最低，agent 只拿一个 URL | 端点随 trestled 存活；daemon 重启则断 |
| `cli_command` | daemon 重启/跨会话/超长任务 | 要拼命令行 |

`monitor_open` **同时返回两者**。daemon 挂了 ws 会断，但 CLI 子进程是独立的，照跑不误——
所以 `cli_command` 不是冗余，是兜底。

## Claude Code Hooks（可选，v0.4+）

Claude Code 的 Hooks 能在 `SessionStart` / `PreToolUse` / `PostToolUse` / `Stop` 等生命周期
触发 shell 命令或 HTTP 请求。指到 daemon 的 `POST /cc/events` 就能把 Claude Code 自己的行为
也汇进同一条 EventBus，做成一个完整的可观测面板。

这是很自然的延伸，但**不属于核心价值**，排在 WASM 插件之后。
