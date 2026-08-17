# 09 · 源码地图

**每个文件是干什么的、里面有什么骨架，以及审阅时该盯哪里。**

按依赖顺序排（`core ← {transport, host}`，`host ← daemon`，`daemon ← {mcp, cli}`），
从上往下读不会遇到「这个类型是哪来的」。

标记：
* 🔸 = 本轮（弹性实例池 + connector 驱动改名 + 前置条件走配置）改动过
* ✅ = 你已经审过
* ⚙️ = 大部分是机械代码，扫一眼就行
* ❗ = 决定性的地方，值得慢读

行数是审阅当天的实际值。

---

## 0 · 先看这两个

| 文件 | 行 | 作用 |
|---|---|---|
| ❗ `wit/trestle.wit` | 271 | **插件接口的唯一真相。** 两个世界（`connector` / `tool-plugin`），共用 `types` 与 `host-services`。host 侧用 `bindgen!` 生成，插件侧用 `wit_bindgen::generate!` 生成，两边指的是同一个文件——所以这份 WIT 一改，两边一起动。骨架：`types`（error / error-kind / target-info / health）· `transport`（只给 connector 的传输工具箱）· `host-services`（两边都有）· `base` / `plugins` / `tasks` / `gpu` / `ws`（只给技能插件）。 |
| 🔸❗ `config/trestle.toml` | 161 | **统一配置。** 分节 `[daemon]` `[defaults]` `[connectors.<实例名>]` `[targets.<机器名>]`。本轮多了 `pool_max` / `pool_idle_secs`（池的伸缩）、`plugin = "ssh-socks5"`（驱动与实例分家）、`allow_exec`（本机命令授权）、`[connectors.*.ready]`（前置条件）。**不含任何凭据**，可以安全入库。 |
| ⚙️ `config/secrets.example.toml` | — | 凭据的形状示例。真的那份 `secrets.toml` 已 gitignore，每次提交前都核对过它不在。 |

---

## 1 · `trestle-core` —— 类型与配置，不含任何行为

谁都依赖它，它不依赖任何人。1450 行。

| 文件 | 行 | 作用与骨架 |
|---|---|---|
| `lib.rs` | 123 | 七个基本操作的枚举 `Op`（`FromStr` / `Display` / `is_idempotent`）。**幂等性判定在这**——重试逻辑靠它。 |
| ❗ `error.rs` | 237 | `TrestleError`。**错误消息是给 agent 看的产物**，不是日志。每个变体的 `#[error(...)]` 都在说「发生了什么 + 下一步能做什么」。两个格式化辅助：`Where`（省掉「failed on :」这种空 target）、`CommaList`（未知机器时列出所有可选名）。重点看 `UnknownTarget` / `ShellTimeout` / `UnknownState` 三条。 |
| 🔸 `config.rs` | 499 | `ConfigStore`：唯一入口，所有运行期文件都落在**程序目录**。结构：`Config` → `DaemonConfig`（🔸 新增 `pool_max` / `pool_idle_secs`）· `Defaults` · `ConnectorConfig`（🔸 新增 `allow_exec`；`plugin` 的语义变成「驱动」）· `TargetConfig`。`SecretRef` 支持 `env:` / `file:` / 明文，**`Display` 会把明文脱敏**，反序列化时不解析（这样 Web UI 回写配置不会把明文写回文件）。 |
| `target.rs` | 248 | `Target` / `TargetRegistry`。解析顺序：主名 → 别名 → host 精确匹配；失败时错误里列出所有名字。`grouped()` 按 connector 分组，`targets_list` 的形状由它决定。 |
| ⚙️ `ops.rs` | 271 | 七个操作的请求/响应结构体。纯 serde，没有逻辑。`ShellResponse` 是个 enum（同步结果 / detach 结果），`EditOp` 四种（literal / regex / lines / insert）。 |
| `event.rs` | 219 | `Event` / `EventKind`。事件是协同层与 Web UI 的共同语言，`ForwardCloseReason` 区分 timeout / job_finished / host_shutdown。 |

---

## 2 · `trestle-transport` —— 字节搬运，不含任何编排

wasm 沙箱里跑不了的那部分（russh 建在 tokio 上，tokio 的 net 在 wasi target 上不支持）。
**这里没有一个「什么时候该重连」之类的决定**——那些全在 connector 插件里。2258 行。

| 文件 | 行 | 作用与骨架 |
|---|---|---|
| `lib.rs` | 25 | 只有 `pub mod`。 |
| ❗ `dial.rs` | 344 | `dial_direct` / `dial_socks5`。SOCKS5 CONNECT（no-auth）握手的字节序列在这，**回复要按地址类型变长读干净**——少读几个字节会让后面的 SSH 握手以一种毫无线索的方式失败。`socks5_reply_message` 把错误码翻成人话。 |
| ❗ `ssh.rs` | 417 | russh 封装。`Credentials`（密码 / 公钥，`Debug` 不泄密）· `SshSession`（exec / alive / close / direct-tcpip）· `SshTarget<'a>`（把 name/host/port/user 捆一起，否则参数列表长到会传错）· `expand_home` · `shell_quote`。❗ `exec_capture` 必须等到 `Eof` **和** `ExitStatus` 都到了才收——只等其中一个会让每条命令看起来都失败过。 |
| ❗ `agent.rs` | 433 | 远端 agent 的客户端。JSON-Lines 多路复用：一个 `read_loop` 把回包按 id 分发给等待者。❗ **`UnknownState` 的边界在这**：写失败 = 没发出去 = 可重试；发出去之后失败 = 状态未知 = **绝不重放**。 |
| ❗ `deploy.rs` | 467 | 按内容 sha256 幂等部署 + 接管。`probe_remote` 把「探测 + 启动」合并成**一条命令**（gpu-1 上每开一个 SSH channel 要 1.7 秒，所以 channel 数就是延迟）。`shell_dir` 把 `~/x` 变成 `$HOME/x`——`~` 在双引号里不展开，会造出 `/home/user/~/.trestle/` 这种目录。 |
| `transfer.rs` | 592 | 分块（512 KiB）+ sha256 + 目录增量。❗ 增量比对用的是**源文件的 mtime**（`put_chunk` 带 mtime，远端 `os.utime`），不是远端的写入时间——否则第二次同步会把所有文件重传一遍。`is_excluded` / `glob_match` 是自己写的小 glob。 |
| `forward.rs` | 130 | direct-tcpip 转发。本地端口由 host 分配，调用方不能指定。 |
| `session.rs` | 320 | 把上面几件事串成「一条到某台机器的活连接」。`ConnectOptions` / `ConnectStats`。 |

---

## 3 · `trestle-host` —— wasm 宿主

**编排逻辑一行都不在这里。** 这个边界是整个架构成立的前提。2700 行。

| 文件 | 行 | 作用与骨架 |
|---|---|---|
| 🔸 `lib.rs` | 77 | 模块表 + 两个 `bindgen!`（`connector` 与 `tool-plugin` 世界，用 `with:` 让 `types` 只生成一份）+ `staging_path`（跨机中转的本地落点，插件不能自己拼——它眼里的文件系统和 host 的不是一回事）。 |
| ❗🔸 `capability.rs` | 180 | `Manifest` / `Capabilities`。`local_exec` 按 argv[0] 的**基名精确匹配**（写 `docker` 不放行 `docker-compose`）；`dial` 空 = 不限制（connector 天生要连任意机器）；`stateless` 决定池能不能长。测试就在文件底部。 |
| ❗🔸 `pool.rs` | 420 | **本轮新增。** 弹性实例池。`PoolPolicy { max, idle_secs }` · `InstancePool<T>`：`new`（起 1 个）· `pick`（找空闲 → 都忙就 `grow` → 到顶就轮转）· `grow`（指数，`growing` 互斥锁串行化）· `sweep`（闲够了收一个，线性）。❗ 「忙」的判定是 `Arc::strong_count == 1`——`pick` 交出去的 Arc 调用方握着，所以不需要记账也不会泄漏。9 条单测覆盖：从 1 起 / 指数长 / 到顶停 / 线性收 / 在用的不收 / 长不出来也不报错。 |
| ❗🔸 `runtime.rs` | 520 | wasmtime 装配。`Runtime`（Engine + 编译缓存，缓存省掉 daemon 每次启动重编 18 MB 的 Python 组件）· `load_connector` / `load_tool`（只编译）· 🔸 `connector_pool` / `tool_pool`（造工厂闭包交给 `InstancePool`）· `ConnectorInstance` / `ToolInstance`（每个方法 `lock().await` 后调 wasm）· `from_wit`（❗ 把插件错误翻回内部错误并**保住 kind**，尤其是 `unknown-state`）。🔸 `ConnectorPool` / `ToolPool` 现在是 `InstancePool<T>` 的别名。 |
| 🔸 `state.rs` | 203 | connector 实例的 host 侧状态。`PluginKv`（per-实例持久化 KV，原子写）· `SharedConnector`（🔸 池里实例共享的句柄表与 session 表）· `PluginState`（🔸 `plugin` 字段现在是**实例名**，事件与拒绝消息都用它）· `sandboxed_wasi`（❗ 空 WASI 上下文：没有目录、没有网络、没有环境变量）。 |
| ❗ `imports.rs` | 529 | **传输工具箱的 host 实现**，也是 capability 强制发生的地方。`transport::Host for PluginState` 的每个方法开头都是一次权限检查，被拒就发 `plugin_call_denied` 事件。❗ `local_exec` 是最要紧的一道闸。 |
| ❗ `tool_state.rs` | 316 | 技能插件的 host 侧状态与导入实现。`base`（七个操作 + `call_many` 扇出）· `plugins`（插件调插件，查 manifest）· `tasks` · `gpu` · `ws` · `host-services`。❗ 技能插件**够不到** transport——`secret_get` 对它直接拒绝，测试钉着这条。 |
| 🔸 `fleet.rs` | 182 | target → connector 路由。`Fleet::load`（🔸 按配置节实例化驱动，把 `allow_exec` 并进 manifest）· `op`（查表转发）· `op_many`（`join_all` 并发扇出）· 🔸 `sweep_pools`。 |
| ❗ `gpu.rs` | 278 | **单点 GPU 仲裁**，取代了原来的协作式租约。占用视图 = `nvidia-smi` 真实占用 + 自己的预留，所以别人绕过 Trestle 直接 ssh 占卡也看得见。释放绑 job 生命周期，不绑时间。 |
| ⚙️ `handles.rs` | 126 | u64 句柄 ↔ 真实对象。wasm 拿不到 Rust 指针，所以要这层。 |
| 🔸 `host.rs` | 351 | 门面。`TrestleHost::start` / `reload_tools` / `load_tools`（🔸 `make_state` 变成可反复调的 `Arc<dyn Fn>`，因为池随时会再长一个实例）· 🔸 `sweep_pools` · `base_tool_descriptors`（❗ 七个基本操作的对外声明，底部三条测试守着「每个单机工具都 required target」）。 |
| `tools.rs` | 222 | `ToolRegistry`：工具名 → 插件的全局唯一索引。`parse_descriptors` 在注册时就拒掉**带点的工具名**（Claude Code 会把 `.` 正规化成 `_`，于是声明的名字和 permission matcher 看到的对不上）。 |

**测试**

| 文件 | 行 | 内容 |
|---|---|---|
| 🔸 `tests/connector.rs` | 492 | 10 条。🔸 `instantiate` 现在按**配置里的 connector 名**找驱动。新增/重写：`the_real_config_is_what_carries_the_start_command`（真配置里那条 docker 命令还接着）· `a_generic_driver_ships_with_no_local_commands_allowed` · `without_allow_exec_...`（拒绝，且事件可见）· `allow_exec_in_the_config_is_what_grants_it`（授权后不再是拒绝）。3 条 `#[ignore]` 打真机。 |
| 🔸 `tests/tools.rs` | 656 | 13 条。整个二进制共用一个 host（否则 7 个插件要被编 N 遍）。🔸 `only_a_stateless_plugin_is_allowed_to_grow_a_pool`（现在断言的是**上限**，因为大家都从 1 起）· `two_agents_calling_the_same_tool_do_not_block_each_other`（🔸 加了 `high_water() > 1`，即「池真的长过」）。 |

---

## 4 · `trestle-daemon` —— 真正的状态都在这

MCP server 由 Claude Code 按 stdio 拉起，**每个会话一个进程**；没有 daemon 的话每开一个
会话就要重建全部连接。1985 行。

| 文件 | 行 | 作用与骨架 |
|---|---|---|
| 🔸 `main.rs` | 492 | 装配与主循环。`run`：抢占检查 → `ConfigStore` → `EventBus` / `AgentRegistry` / `TaskScheduler` → `TrestleHost::start`（🔸 池策略从配置读）→ 恢复落盘状态 → HTTP 服务 → IPC → 🔸 **池巡检定时器**（每 60 秒 `sweep_pools`）→ idle 退出定时器 → accept 循环。`handle` 是 IPC 请求的总分发（❗ `forward` 成功后记进 registry，会话断了要回收）。 |
| `events.rs` | 135 | `EventBus`（broadcast，容量 1024）· `PluginEventSink`（插件的 `emit` 接到总线上）。 |
| ❗ `registry.rs` | 318 | 协同层。`AgentSession`（谁在线、最近在哪台机器干了什么）· `ForwardRecord`（❗ **转发是会话级资源**：会话断了就关、端口还回池）· `Note`（❗ **TTL 必填**，写入时就带过期）· `PersistedRegistry`（落盘与恢复）。 |
| `ipc.rs` | 413 | localhost TCP + token。`DaemonInfo`（端口与 token 写 `daemon.json`，❗ `restrict_to_owner` 限权）· `Request` / `RequestBody` / `Response` · ❗ `NOTIFICATION_ID = 0`（id 为 0 的帧是 daemon 主动推送）· `IpcClient`（瘦客户端共用，含 lazy 拉起 daemon）。 |
| ❗ `http.rs` | 449 | Monitor ws + 事件流 + Web UI + `/api/*`。`Filter`（服务端过滤：`only_target` / `only_job` / alert 正则）· ❗ `ClosingReason`（timeout / job_finished / host_shutdown 分得开）· `monitor_loop`（❗ **alert 优先于安静超时**）· `DaemonWs`（`ws.publish` 的真实现，❗ 返回的 URL 必须是 `ws://` 不是 `http://`，否则 Monitor 连不上而且失败得很安静）。 |
| `tasks.rs` | 178 | `TaskScheduler`：插件注册的周期任务，到点回调 `on-tick`。`DeferredWs` 解决「host 先起、HTTP 后绑端口」的先有鸡还是先有蛋。 |
| ⚙️ `webui.html` | — | Web UI 外壳（导航 + 事件流 + 配置页）。各插件的面板由 `/api/panels` 拼进来——**没有前端工程**。 |
| `tests/daemon.rs` | 476 | 8 条。整个二进制共用一个 daemon（各起各的会抢 `daemon.json`）。含 Monitor 关闭原因、留言板 TTL、在场感知、token 认证。 |
| `tests/smoke.rs` | 402 | 移植上一代 `mcp_smoke.py` 的形态：**每个工具用安全参数真调一次 + 校验 schema 里 `target` 确实 required**。上一代靠它在 53 个工具里抓到过一个 mock 测不出的 bug。 |

---

## 5 · 对外的两个瘦客户端

| 文件 | 行 | 作用 |
|---|---|---|
| ✅ `trestle-cli/src/main.rs` | 692 | 你已审。 |
| `trestle-mcp/src/main.rs` | 246 | rmcp 3.1 stdio 前端。`ServerHandler`：`list_tools` 从 daemon 拿完整工具面，`call_tool` 转 IPC，收到 `ToolsChanged` 推送就发 `notifications/tools/list_changed`（❗ 这是「reload 之后 Claude Code 不用重连」的那根线）。`to_mcp` 把内部错误翻成 MCP 错误并**保住 remedy**。 |

---

## 6 · 插件

### connector 驱动（🔸 本轮全改）

| 文件 | 行 | 作用与骨架 |
|---|---|---|
| ❗🔸 `plugins/lib/connector-ready/src/lib.rs` | 521 | **本轮新增。** 前置条件状态机，两个驱动共用。`ReadyConfig`（probe / check / check_expect / missing / missing_remedy / start / timeout / cache）· `Sys` trait（外界能力，驱动接上自己的 host 导入）· `Cache` · `ensure()`。❗ 抽出来的真正理由是**它能在 host 上用普通 `cargo test` 测**——10 条单测覆盖「容器不存在报什么」「start 失败报什么」「被拒时指向配置而不是指向 docker」。 |
| 🔸 `plugins/connectors/ssh-socks5/src/lib.rs` | 319 | 经 SOCKS5 连过去的 SSH。骨架：`Config`（socks / dial_timeout / ready）· `Host`（把 host 导入接到 `Sys` 上，**只转接不判断**）· `Guest`（targets / ensure_ready / health / op / config_schema）· `connect`（❗ 连接记在 host 的 session 表里，不记在自己内存里——池里六个实例各存各的会建六条连接）。`thread_local` 的 ready 缓存是有意的例外，见注释。 |
| 🔸 `plugins/connectors/ssh-direct/src/lib.rs` | 257 | 直连的 SSH。和上面唯一的差别是 `dial` 而不是 `dial_socks5`——这就是「写一个 connector 有多便宜」。 |
| 🔸 `.../manifest.toml` ×2 | — | ❗ `local_exec` 现在是**空的**：通用驱动没资格替你声明「我需要跑 docker」，那份授权在配置的 `allow_exec` 里。 |

### 技能插件

| 文件 | 行 | 作用 |
|---|---|---|
| `plugins/tools/job/src/lib.rs` | 553 | 长任务：start / list / logs / wait / stop。建在 `shell(detach)` 之上，任务表与日志偏移量全在 host KV 里（**wasm 内存里什么都不留**，所以它敢声明 `stateless`）。 |
| `plugins/tools/fs/src/lib.rs` | 305 | list / find / stat / tree / disk。纯粹是拼命令 + 解析输出。❗ `stat --printf` 而不是 `stat -c`——后者不处理 `\t` 转义，解析会一无所获。 |
| `plugins/tools/fleet/src/lib.rs` | 335 | 全队视角：status / run 广播 / gpu 挑卡。❗ 用 `base.call_many` 一次把整队全发出去（SIMD 式：插件一次多发，host 调度）。 |
| `plugins/tools/xfer/src/lib.rs` | 218 | 跨机搬运：服务器之间经本地中转、一份文件分发到多台。**只做编排**，分块与校验都在 `base.upload/download` 里。 |
| `plugins/tools/monitor/src/lib.rs` | 138 | 调 `ws.publish` 拿 URL 交给 Claude Code 的 Monitor。❗ `timeout_secs` 必填；过滤参数叫 `only_target` 而不是 `target`（它是**过滤条件**不是操作对象，改名比放宽「target 必填」这条规则好）。 |
| `plugins/tools/hello-py/app.py` | — | 验证 Python 也能写插件，走同一份 WIT。代价是组件 18 MB（Rust 插件 150 KB），所以刻意**不**声明 `stateless`。 |
| `plugins/templates/rust/*.tmpl` | — | `trestle plugin new` 的脚手架。❗ **不改一个字就编译通过**——第一步就要人先修一遍才能编的话，「摩擦 → capability」那条闭环就断了。 |

---

## 7 · 远端一侧

| 文件 | 行 | 作用 |
|---|---|---|
| ❗ `agent-py/trestle_agent.py` | 775 | 七个操作的远端实现，**只用标准库**。`--serve <sock>` 绑 AF_UNIX，所以 agent 的生命周期和任何一条连接无关——网断了、电脑重启了，下次连上是一次**接管**而不是一次重装。❗ 两个坑在这踩过一次就不再踩：不用 shell 的 `&`（用 `start_new_session=True`）、pid 从 `Popen` 拿而不是 `$!`（后者拿到的是 setsid 自己的 pid）。 |
| `agent-py/relay.py` | 70 | 57 行的桥：SSH stdio ↔ unix socket。 |
| `agent-py/test_agent.py` | 441 | agent 的本地单测，不需要任何服务器。 |
| ⚙️ `scripts/build-plugins.ps1` | 62 | 编所有插件到 wasm。cargo 产出下划线名（`ssh_socks5.wasm`），manifest 用连字符，所以这里要改名。认得 `app.py` 就走 componentize-py。 |

---

## 建议的审阅顺序

1. `wit/trestle.wit` + `config/trestle.toml` —— 接口和配置定了，别的都是它们的实现
2. `core/error.rs` —— 错误消息的调子在这定
3. `host/capability.rs` + `host/pool.rs` + `host/imports.rs` —— 权限与并发模型
4. `plugins/lib/connector-ready` + 两个驱动 —— 「connector 自包含」到底自包含到什么程度
5. `host/runtime.rs` + `host/host.rs` —— 装配
6. `transport/agent.rs` + `transport/deploy.rs` —— `UnknownState` 边界与幂等部署
7. `daemon/registry.rs` + `daemon/http.rs` —— 协同层与 Monitor 契约
8. 剩下的技能插件，随便挑

**其他文档**：01 架构 · 02 七个操作 · 03 connector · 04 插件 · 05 Monitor 与 UI ·
06 多 agent · 07 上一代的教训 · 08 运维。
