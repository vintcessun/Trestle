# 04 · 插件

除了七个基本操作，**所有能力都是插件**。

| 插件 | 工具 | 额外权限 |
|---|---|---|
| `job` | job_start / job_list / job_logs / job_wait / job_stop | call_plugins=[gpu] |
| `fs` | fs_list / fs_find / fs_stat / fs_tree / fs_disk | （无） |
| `fleet` | targets_list / fleet_status / fleet_run | （无） |
| `gpu` | gpu_status / gpu_find / gpu_acquire / gpu_release | arbitrate=[gpu] |
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
arbiter.acquire / release / bind-job / claims   // 需要 arbitrate 权限
ws.publish(filter, timeout-secs)                // 需要 ws 权限
```

**够不到的**：SSH、本机进程、网络、文件系统、别的插件的数据、凭据。
那不是疏漏，是权限模型。

## capability

manifest 里声明，host 在每个导入的入口处强制。wasm 组件没有 syscall，所以这些导入是
插件唯一能碰到外界的地方——强制因此是真的，不是约定。

```toml
name = "gpu"
kind = "tool"

[capabilities]
arbitrate = ["gpu"]           # 仲裁哪些**资源种类**
# stateless = true            # 准起多个实例（见下）
# tasks = true                # 注册周期任务
# ws = true                   # 开 WebSocket 端点
# call_plugins = ["gpu"]      # 调别的插件（job 就是这么要卡的）
# local_exec = ["docker"]     # 本机命令（只有 connector 该要这个，而且授权在配置里）
```

几条都写成**清单**而不是 bool，理由是同一条：一个能仲裁 GPU 的插件不该顺手
把别人的许可证席位还掉，一个能调 `gpu` 的插件不该顺手去调 `xfer`。

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

池是**弹性**的：

```
起来时      1 个实例
撞上并发    1 → 2 → 4 → 8 …  指数长，上限默认 = CPU 核心数
闲够了      8 → 7 → 6 …      线性收，每分钟还一个
```

三段各有各的理由：

* **从 1 起**——绝大多数插件这辈子不会被并发调用，为它们预留实例是纯浪费
  （一个 componentize-py 产出的组件就是 18 MB）。
* **指数长**——第一次撞上说明还会再撞，一次补到位比一次加一个少绕好几圈。
* **线性收**——收得比长得慢是刻意的：「刚才忙过」比「此刻闲着」更能预测下一秒。

「忙」不靠计数器，靠 `Arc` 的引用计数：`pick()` 交出去的实例调用方握着，
所以「只有池自己持有」就是「没人在用」。不需要记账，也不会因为忘了归还而泄漏。

上限在 `[daemon]` 里：

```toml
pool_max = 0          # 0 = 跟 CPU 核心数走
pool_idle_secs = 600  # 连续这么久没撞上并发就开始收
```

核心数只是个不至于离谱的默认值——实例被占住的时间绝大部分在**等网络**而不是烧 CPU，
所以下限钉在 2，一核的机器也照样能并发。

### 只有无状态的插件准长

声明了 `stateless = true` 的插件上限才 > 1，否则钉死在一个实例上：

```toml
[capabilities]
stateless = true    # 我不在 wasm 内存里存跨调用的状态
```

**只有真的无状态才能声明。** 如果插件把东西存在 wasm 全局变量里（`static mut`、
`thread_local!`），池里的实例会各看各的，而且是**静默**出错——host 没有办法替你验证。
要跨调用记东西，用 `host.state-*`（per-plugin KV，池里的实例共享同一份）。

现有六个 Rust 插件都开了。`hello-py` 刻意没开：它本身确实无状态，但 componentize-py
产出的组件是 18 MB，长起来就是 ×N 内存。

（connector 驱动里那个 `thread_local` 的 ready 缓存是这条规则的一个**有意**的例外：
它只是缓存，实例之间分裂的代价是多探一次端口。真正的跨调用状态——连接句柄——
在 host 的 session 表里。）

实测：两个并发的 2 秒调用，池化前 4 秒，池化后 2.05 秒。

**宁可工具语义清晰而多，不要为了省数量把语义糊在一起。** Claude Code 对 MCP 工具做
deferred loading——先知道有哪些工具，需要时才拉 schema，所以工具多不是问题。
上一代的反例：把短命令和长任务塞进同一个 `shell`，结果 agent 拿它跑训练然后撞超时。

**错误消息是给 agent 看的产物。** `detail` 说清楚发生了什么，`remedy` 说下一步能做什么，
两个都不是客套话。

## 加接口时的兼容性

WIT 是接口的唯一真相，而组件模型按**接口与函数**解析导入，不是按整个世界。所以：

| 改动 | 旧插件 | 要不要重编 |
|---|---|---|
| 加一个 import 接口（比如本轮的 `arbiter`） | 照常工作 | **不用** |
| 往已有接口里加一个函数 | 照常工作 | 不用 |
| 删掉/改名一个接口或函数 | 装不上 | 要 |
| 改 record 字段、enum 分支、函数签名 | 装不上（或更糟：ABI 对不齐） | 要 |
| 加一个**必须导出**的函数（`world` 的 export） | 装不上 | 要 |

实测过：把 `arbiter` 整个从 WIT 里删掉编出来的一个插件，塞进现在这个提供
`arbiter` 的 host，加载正常、工具照出。反过来，一个要 host 没有的接口的插件会得到：

```
component imports instance `trestle:plugin/future-thing@0.1.0`,
but a matching implementation was not found in the linker
```

**装不上的插件被跳过，不会让 daemon 起不来**，而且会带着原因和下一步出现在
`trestle plugin list` 里。这条很要紧：接口对不上最容易发生在「host 升级了、插件没重编」
的时刻，如果那时 daemon 直接不启动，你连查是哪一个坏了的工具都没有。

connector 同理：一个装不上的驱动只让**它那一组机器**不可达，别组照常。

所以往前加东西是安全的，改已有的东西不是。真要做破坏性改动，正确做法是**加一个新世界**
（`tool-plugin-v2`）让 host 两个都试，而不是原地改签名。

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
`job` 与 `gpu` 各有一个真面板，可以照抄。
