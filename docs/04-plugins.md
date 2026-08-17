# 04 · 插件

除了七个基本操作，**所有能力都是插件**。

| 插件 | 工具 | 额外权限 |
|---|---|---|
| `job` | job_start / job_list / job_logs / job_wait / job_stop | gpu |
| `fs` | fs_list / fs_find / fs_stat / fs_tree / fs_disk | （无） |
| `fleet` | targets_list / fleet_status / fleet_run / gpu_find / gpu_status | gpu |
| `xfer` | xfer_between / xfer_distribute | （无） |
| `monitor` | monitor_open | ws |
| `hello-py` | hello_py | （无，Python 写的） |

## 插件能看到什么

```wit
base.call(target, op, payload)                  // 七个基本操作
base.call-many(targets, op, payload)            // host 并发扇出
host-services.targets() / config-get / state-* / emit / now-ms / sleep-ms / staging-path
plugins.call(plugin, tool, args)                // 需要 manifest 声明被调方
tasks.schedule / cancel  + 导出 on-tick         // 需要 tasks 权限
gpu.allocate / release / view                   // 需要 gpu 权限
ws.publish(filter, timeout-secs)                // 需要 ws 权限
```

**够不到的**：SSH、本机进程、网络、文件系统、别的插件的数据、凭据。
那不是疏漏，是权限模型。

## capability

manifest 里声明，host 在每个导入的入口处强制。wasm 组件没有 syscall，所以这些导入是
插件唯一能碰到外界的地方——强制因此是真的，不是约定。

```toml
name = "job"
kind = "tool"

[capabilities]
gpu = true                    # 向 GPU 分配器要卡
# tasks = true                # 注册周期任务
# ws = true                   # 开 WebSocket 端点
# call_plugins = ["fleet"]    # 调别的插件
# local_exec = ["docker"]     # 本机命令（只有 connector 该要这个）
```

被拒绝的调用会发 `plugin_call_denied` 事件。这条不能省：否则权限模型是个黑盒，
出问题时没人知道是被挡了还是根本没调。

`local_exec` 是最要紧的一道闸——**能跑任意本机命令就等于全部权限**。
所以白名单按 argv[0] 的基名精确匹配：写 `docker` 不会顺带放行 `docker-compose`。

## 几条纪律

**工具名用 `_` 不用 `.`。** Claude Code 在 plugin-bundled 场景会把 `.` 正规化成 `_`，
于是你声明的名字和 permission matcher / hook 看到的名字对不上。注册时会直接拒掉带点的名字。

**接受 `target` 就必须 required 它。** 没有默认机。如果一个参数是**过滤条件**而不是
操作对象（比如 `monitor_open` 的「只看这台机器」），就换个名字叫 `only_target`——
让规则保持锋利，而不是放宽规则。

## 并发：一次多发，host 调度

**这是这套设计的一条基本原则，不是权宜之计。**

一个 wasm 实例在被调用期间是**独占**的——插件内部的一切都是顺序的，哪怕把它写成异步
也一样。所以并发只能发生在 host 侧，而插件表达并发的方式是**一次把要做的事全发出去**，
像 SIMD 一样：

```
✗  for t in targets { base::call(t, "shell", cmd) }     // 整队 = 六倍延迟
✓  base::call_many(&targets, "shell", cmd)              // 一次发出，host 并发
```

这样 wasm 里的逻辑保持简单（没有 future、没有执行器、没有取消语义），性能问题在
host 那边解决——host 本来就有 tokio。

组件模型的 async（WASI 0.3 / P3）能让插件自己编排并发，但在这个系统里换不来东西：
瓶颈是「一个实例不能被并发进入」，那是 P3 也解决不了的。代价倒是实打实的——
rustc 还没有 wasip3 目标，componentize-py 只到 P2，Python 那条路会退化成第二套绑定。
**所以维持 P2。**

什么时候该重新考虑：

1. 要加**第三个** host 扇出原语时（现在只有 `base.call-many`）——那说明 host 在吸收
   编排职责，边界在漂；
2. 出现真的需要**流式**的插件（`stream<T>` 是 P3 独有的，轮询替代不了）；
3. rustc 出了 wasip3 目标**且** componentize-py 跟上——代价降下来了。

WIT 是自己写的，加 `async` 是**加**不是改，现有同步签名可以原样留着。所以推迟这个
决定不欠技术债。

## 实例池：几个 agent 能同时用一个插件

既然一个实例被调用期间是独占的，那「几个实例」就等于「几个 agent 能同时用它而不互相等」。

默认**一个**实例。声明 `stateless = true` 才给池：

```toml
[capabilities]
stateless = true    # 我不在 wasm 内存里存跨调用的状态
```

**只有真的无状态才能声明。** 如果插件把东西存在 wasm 全局变量里（`static mut`、
`thread_local!`），池里的实例会各看各的，而且是**静默**出错——host 没有办法替你验证。
要跨调用记东西，用 `host.state-*`（per-plugin KV，池里的实例共享同一份）。

现有五个 Rust 插件都开了。`hello-py` 刻意没开：它本身确实无状态，但 componentize-py
产出的组件是 18 MB，池化就是 ×N 内存。

实测：两个并发的 2 秒调用，池化前 4 秒，池化后 2.05 秒。

**宁可工具语义清晰而多，不要为了省数量把语义糊在一起。** Claude Code 对 MCP 工具做
deferred loading——先知道有哪些工具，需要时才拉 schema，所以工具多不是问题。
上一代的反例：把短命令和长任务塞进同一个 `shell`，结果 agent 拿它跑训练然后撞超时。

**错误消息是给 agent 看的产物。** `detail` 说清楚发生了什么，`remedy` 说下一步能做什么，
两个都不是客套话。

## 写一个插件

```
trestle plugin new mytool --description "干什么的"
# 改 plugins/tools/mytool/src/lib.rs 的 list_tools 与 call
.\scripts\build-plugins.ps1
trestle plugin reload
```

`plugin new` 生成的脚手架**不改一个字就编译通过**——如果第一步就要人先修一遍才能编，
这条闭环就断了。`reload` 之后 daemon 会推一条 `tools_changed`，MCP 前端转成
`notifications/tools/list_changed`，**Claude Code 不用重连就能看到新工具**。

这就是「摩擦 → capability」的闭环：遇到一个没有工具的操作，生成脚手架、填十几行、
reload，它就变成常驻工具了。

## Python 也能写

`plugins/tools/hello-py/` 是一个真跑通的例子，走的是同一份 WIT、同一个 `tool-plugin` 世界。

代价是实打实的：componentize-py 产出的组件 **18 MB**（Rust 插件 150 KB），实例化也慢得多。
日常插件用 Rust；这条路留给「用 Python 写明显更顺手」的场合。

```
componentize-py -d ../../../wit -w tool-plugin componentize app -o hello-py.wasm
```

`scripts/build-plugins.ps1` 认得 `app.py`，会自动走这条路。

## Web UI 是插件的一部分

插件导出 `ui-panel()` 返回一段 HTML 片段，host 把它们拼起来挂在 `/ui/panels`。
加一个插件，它自己带着自己的那块界面进来——不需要动前端工程，因为根本没有前端工程。

片段里可以用 host 的 `/api/tool/<name>`（POST，body 是参数 JSON）和 `/events`。
`job` 与 `fleet` 各有一个真面板，可以照抄。
