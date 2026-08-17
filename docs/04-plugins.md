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

**打多台机器用 `call-many`。** 自己写循环调六次，冷启动时就是六倍延迟：一个 wasm 实例
同时只能进一个调用，并发只能由 host 做。

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
