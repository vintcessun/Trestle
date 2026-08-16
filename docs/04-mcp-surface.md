# 04 · 对外 MCP 工具面

## 命名：用 `_` 而不是 `.`

新版 MCP 规范允许工具名含 `.`（官方例子 `admin.tools.list`），但 Claude Code 有两个现实约束：

* 工具在 Claude Code 里呈现为 `mcp__<server>__<tool>`；
* plugin-bundled 场景下 `.` 会被正规化成 `_`，于是 permission matcher / hook 看到的名字和你
  声明的不一致。

**推荐统一用 `_`**：`base_read`、`docker_logs`、`job_start`。namespace 概念保留在 host 内部
（Router 按前缀分发到 BaseService / 各插件），对外只是名字前缀。

代价是"逻辑树"只存在于命名约定里——但 MCP 的 `tools/list` 本来就是**平面集合**，没有原生树，
无论怎么写都是约定。既然如此，选那个不会在 permission 配置里咬你的写法。

## v0.1 工具清单

单机工具的 `target` 一律**必填**（无默认机）；面向全队的用可选 `targets`，留空=全部。

### base（6）

| 工具 | 签名要点 |
|---|---|
| `base_read` | `(target, path, start_line?, max_lines?)` 带行号返回 |
| `base_write` | `(target, path, content, append?, make_dirs?)` |
| `base_edit` | `(target, path, op)` op = literal/regex/lines/insert，见 `02` |
| `base_shell` | `(target, command, cwd?, timeout?)` 短命令；超时杀进程组，返回带 `timed_out` |
| `base_list` | `(target, path, recursive?)` |
| `base_find` | `(target, path, glob)` |

### job（5）

| 工具 | 说明 |
|---|---|
| `job_start` | `(target, command, name?, cwd?, gpus?, env?)` → job_id + `monitor_url` + `cli_command` |
| `job_list` | `(targets?, state?)` 全队任务表：状态/退出码/时长/命令 |
| `job_logs` | `(target, job_id, since?)` `since="last"` 接着上次读——**偏移量由 host 记，不让 agent 管** |
| `job_wait` | `(target, job_id, timeout, until_pattern?)` 在**远端**等，不本地轮询 |
| `job_stop` | `(target, job_id, force?)` TERM 整个进程组 → 宽限 → KILL |

### fleet / admin（5）

| 工具 | 说明 |
|---|---|
| `targets_list` | 有哪些机器、用途、connector 绑定。**不连接任何机器，秒回** |
| `fleet_status` | `(targets?)` 全队：GPU 占用/空闲卡/磁盘/负载/连接健康 |
| `fleet_run` | `(command, targets?)` 一条命令并发打多台 |
| `fleet_doctor` | `(targets?)` 强制重查 connector + 重建连接 + 测冷热延迟 |
| `monitor_open` | `(target?, job_id?, timeout_secs)` → WebSocket URL，见 `05` |

### 传输（4）

| 工具 | 说明 |
|---|---|
| `xfer_push` | 本地→远端，**文件或目录自动识别**，目录走打包传输 |
| `xfer_pull` | 远端→本地，同上 |
| `xfer_sync` | 增量：比对清单，只传有变化的文件；`dry_run` / `delete` 可选 |
| `xfer_between` | 机器之间搬（经本地中转，两台互不相通也没关系） |

约 20 个工具。插件上线后按 namespace 增长。

## 为什么工具多不是问题

Claude Code 默认对 MCP 工具做 **deferred loading**：先知道有哪些工具，需要时才通过 `ToolSearch`
拉具体 schema。所以两三百个工具不等于两三百份 schema 挤进上下文。

这意味着设计取向应该是：**宁可工具语义清晰而多，不要为了省数量把语义糊在一起**。
上一代的实测例子——把短命令和长任务塞进同一个 `shell` 工具，结果 agent 拿它跑训练然后撞超时。
拆成 `base_shell` / `job_start` 两个反而更不容易出错。

## lazy 实例化 + list_changed

```
docker_logs 被 ToolSearch 找到
   → Claude 调用它
   → PluginRegistry 发现 docker 插件尚未实例化
   → instantiate
   → call
```

**声明与实例化分离**：manifest 里 `list()` 的结果在 host 启动时读一次就进 registry（工具可见），
真正的 wasm 实例延迟到第一次调用。

插件启停走 **host 的 CLI/admin 通道**（`trestle plugin enable docker`），然后发
`notifications/tools/list_changed`，Claude Code 会刷新工具列表、不需要断开重连。

⚠️ **不要**做成"某个 MCP 工具被调用后改变当前连接自己的工具集"。MCP 2026 规范允许工具集合随时间
变化，但要求不能因连接而异、也不该作为其他请求的隐式副作用。

## 错误消息是给 agent 看的

这条在上一代收益很大，写进规范：错误必须**可操作**。

```
✗  "target not found"
✓  "unknown target 'x36'; known: gpu-1, gpu-2, gpu-3, gpu-4"

✗  "connection failed"
✓  "SOCKS proxy unavailable for gpu-1 (203.0.113.10:2201): container
    vpn-proxy not running and `docker start` failed: <stderr>.
    Try: trestle doctor gpu-1"

✗  "timeout"
✓  "base_shell on gpu-4 timed out after 60s; process group killed.
    For long jobs use job_start instead."
```

第三条尤其重要：**在错误里指出正确的工具**。agent 读到就会改用 `job_start`，
而不是把 timeout 调大再撞一次。

## transport

v0.1 用 **stdio**（`rmcp` 的 `stdio()`）。理由：Claude Code 拉起最简单，没有端口和认证问题。

daemon 模式下 stdio 前端只是瘦客户端，所以"每会话一个 MCP 进程"的开销可以忽略——
真正的连接池在 `trestled` 里，跨会话共享。

Streamable HTTP 留给"想让 Cursor / 其他 client 也接入"的场景，rmcp 3.x 有
`StreamableHttpService`（feature `transport-streamable-http-server`）。
**rmcp 没有 WebSocket transport**——所以 Monitor 的 ws 必须是独立的 HTTP 服务，见 `05`。
