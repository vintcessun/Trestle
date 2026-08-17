# 05 · Monitor 与 Web UI

## 为什么 Monitor 必须独立于 MCP

两条硬约束交汇在这里：

1. **rmcp 3.x 没有 WebSocket transport**（只有 stdio 与 Streamable HTTP）。
   WebSocket 从来不是 MCP 标准 transport。
2. **Claude Code 的 Monitor 只接受两种事件源**：`command`（本地 shell 命令，stdout 每一行
   = 一条通知）和 `ws`（WebSocket URL，每个 text frame = 一条通知，socket close 结束监视）。

所以 MCP 归 MCP，Monitor 走 daemon 自己的 HTTP 服务上的 `/monitor/ws/<id>`。

这条路是通的，而且是相对上一代最大的摩擦削减：agent 只需要拿到一个 URL 就能挂上监视，
不用再拼一条带 Windows 路径和正则转义的命令行。

## 生命周期规则

```
ws 端点由 trestled 管
   ├─ daemon 退出        → 所有 ws 关闭
   ├─ 开启时必须传 timeout_secs → 到期 host 主动关闭
   └─ 关闭前推最后一帧，说明为什么关
```

`timeout_secs` **必填**是刻意的：一个没有过期时间的监视端点会悄悄泄漏——任务早就
结束了，ws 还挂在那里占着轮询。强制传值让调用方每次都想一下「这个任务大概跑多久」。

关闭时必须先推一帧再 close：

```json
{"type":"closing","reason":"timeout","detail":"monitor m3 expired; whatever it was watching is still running"}
{"type":"closing","reason":"job_finished","exit_code":0}
{"type":"closing","reason":"host_shutdown"}
```

**区分 `timeout` 和 `job_finished` 是这套设计的重点**：前者意味着任务还在跑但没人盯了，
agent 需要重新开一个；后者才是真正结束。如果只是静默 close，两种情况看起来一模一样。

URL 必须是 `ws://` 而不是 `http://`——Monitor 拿 http 的 URL 是连不上的，
而它失败的样子和「任务很安静」几乎一样，非常难查。有一个测试专门守着这条。

## 覆盖面：静默不等于正常

Monitor 工具文档里的这条警告，对默认 filter 是硬要求：

> 只匹配成功标志的监视器，在崩溃/挂死时保持沉默——而沉默看起来和「还在跑」一模一样。

所以默认 alert 规则**宁宽勿窄**，覆盖所有终态：

```
Traceback | FAIL | ERROR | OOM | Killed | CUDA out of memory | AssertionError | Segmentation fault
```

而且 **alert 压过 quiet**：一条既被压制又该告警的行必须推出去，压制规则不能盖掉故障。

## 事件模型

daemon 内部一条 EventBus，两个订阅者：`tracing`（落日志）和 WebSocket 广播。

```
AgentConnected / AgentDisconnected
ConnectorEnsureReady / ConnectorFailed
SessionConnected / SessionLost / SessionRecovered / SessionReattached
OpStarted / OpFinished / OpUnknownState
JobStarted / JobFinished
ForwardOpened / ForwardClosed
GpuAllocated / GpuReleased / GpuUnavailable
PluginLoaded / PluginCallDenied / PluginCallFailed
ToolCalled / ToolFinished
```

每条事件都带 **agent id**——这是多 agent 协同的基础：任一 agent 的 monitor 都能看到
别人在干什么。

慢的订阅者会丢事件而不是把发送方拖住（掉几条日志远好过让一次 `base_shell` 卡住），
丢了会推一帧 `lagged` 说明丢了多少。

## Web UI

`http://127.0.0.1:<http_port>/`，端口在 daemon 启动日志里。

它由两部分组成：

* **host 外壳**：机器（按 connector 分组）、在场的 agent、端口转发、留言板、工具清单、实时事件流；
* **插件面板**：每个插件导出 `ui-panel()` 返回一段 HTML，host 拼起来挂在 `/ui/panels`。

所以「加一个插件，它自己带着自己的那块界面进来」——不需要动前端工程，因为根本没有
前端工程：单文件 HTML，零构建工具链。

面板可用的接口：

```
GET  /api/targets     机器（按 connector 分组）
GET  /api/tools       全部工具声明
GET  /api/agents      会话 / 转发 / 留言板
POST /api/tool/<name> 调一个工具，body 是参数 JSON
WS   /events          实时事件流（不过滤）
```
