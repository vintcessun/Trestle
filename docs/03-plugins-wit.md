# 03 · 插件模型

## 一句话

插件**没有任何自己的 I/O 能力**。它能做的一切都必须经由 host 导入的 base 能力，
每次调用由 host 按 manifest 里的 capability 检查。

```
                          ✗ 插件自己开 socket / 起进程
plugin.wasm ──────────────────────────────────────────► OS
     │
     │ ✓ 唯一出口：host 导入的 base.read / write / edit / shell
     ▼
  BaseService ── capability 检查 ── SessionPool ──► 远程机器
```

如果这条守不住，WASM 沙箱就是白做的。这也是为什么 `ToolCtx` 给的是受限句柄而不是裸 Session
（见 `02-abstractions.md`）——**v0.1 就要立好，v0.3 再想收回来就晚了**。

接口草案见 [`wit/trestle.wit`](../wit/trestle.wit)。插件只需实现三个函数：

```
namespace() -> string
list()      -> list<tool-info>
call(name, arguments-json) -> result<string, string>
```

第三方写插件**完全不需要懂 MCP**——拿 WIT，实现三个函数，编译出 `.wasm` 扔进 `plugins/`。
对外看到的仍然是一个正常 MCP server。

## capability manifest

每个插件带一份 manifest，host 据此授权：

```toml
name = "nvidia"
version = "0.1.0"
runtime = "wasm"

# 允许在哪些 target 上工作；"*" 表示全部
targets = ["*"]

[permissions.fs]
read  = ["/proc/driver/nvidia/**", "/var/log/nvidia*"]
write = []

[permissions.shell]
# 前缀白名单。host 做 shell 命令匹配，不匹配直接拒绝并记事件
allow = [
    "nvidia-smi",
    "nvidia-smi --query-gpu=*",
]
```

于是：

```
nvidia.wasm
   ├─ shell("nvidia-smi --query-gpu=memory.used ...")   ✓
   ├─ shell("rm -rf /")                                 ✗ 拒绝 + 记事件
   ├─ read("/proc/driver/nvidia/version")               ✓
   └─ read("/home/alice/.ssh/id_rsa")                  ✗ 拒绝 + 记事件
```

**WASM 负责隔离，host capability 负责授权**——两者缺一不可。shell 白名单要做**前缀+参数模式**匹配，
不要做子串包含（`allow=["docker"]` 不能让 `docker` 后面跟任意东西，否则等于没限制）。

## ⚠️ Connector 插件比 Tool 插件难得多

设计对话里把 `connectors/*.wasm` 和 `tools/*.wasm` 并列，但两者的可行性差距很大，这点必须提前说清楚：

| | Tool 插件 | Connector 插件 |
|---|---|---|
| 需要的能力 | 只需调 base（已在 WIT 里） | 需要**建 TCP 连接**、可能还要**在本机起进程**（拉 VPN 容器） |
| WASI 支持 | 不需要额外 WASI | sockets 要 WASI p2 且 host 授权；**"在本机执行 docker 命令"根本不在 WASI 范围内** |
| 沙箱意义 | 高：限制它能碰哪些文件/命令 | 低：你已经给了它建任意连接+起本地进程的权力，还沙箱什么 |

**建议：Connector 一律 Native（编译进来），只对 Tool 开放 WASM。**

理由不只是技术难度——一个能"在你本机起任意进程"的插件，沙箱化的收益本来就接近零。
Connector 数量少（direct-ssh / socks5 / jump-host / tailscale，撑死十来个）、变更频率低、
且都需要深度系统集成，编译进主程序完全合理。

真正需要"agent 遇到摩擦就长出来"的是 **Tool**（docker/slurm/conda/某个组里自研的脚本），
那才是插件化的价值所在。

如果将来确实需要动态 connector，更合适的形态是 **Process backend**：把 connector 做成一个独立
可执行文件，用 stdio 协议通信（就是 `ToolBackend::Process`）——它本来就在进程边界外，
不需要假装自己被沙箱着。

## 为什么 v0.1 先不上 WASM

不是不做，是**排序**问题。v0.1 的价值在于"整组机器真的能用起来"，而 WASM 工具链会吃掉大量时间：

* `cargo-component` / `wit-bindgen` 与 `wasmtime 47` 的版本匹配，Component Model 仍在演进；
* WASI p2 在 Windows 宿主上的行为需要单独验证；
* 插件的构建、签名、分发链路要另外设计。

这些都值得做，但**不该挡住"gpu-1 能连上"这件事**。

正确的做法是：

```
v0.1  ToolBackend::Native  ← 唯一实现
      ToolBackend::Wasm    ← 枚举分支已存在，unimplemented!()
      ToolBackend::Process ← 同上
      WIT 文件已写好，Rust trait 按它的形状定义

v0.3  填上 Wasm 分支
```

抽象立刻建立，实现按需推进。这样 v0.3 是"加一个 backend"，不是重构——
而如果 v0.1 图快把 SSH 句柄到处传，v0.3 就会发现权限根本收不回来。

## Agent 自生成插件（v0.4 的真正目标）

这是整个项目的差异化，不是"我们支持 WASM"：

```
agent 遇到摩擦
   → trestle plugin new <name>   生成脚手架（WIT 绑定 + manifest 模板 + 示例 call）
   → agent 写实现
   → cargo component build
   → trestle plugin install <path>   host 校验 manifest / capability，人工确认一次
   → notifications/tools/list_changed
   → agent 立刻拥有新能力
```

`plugin new` 的脚手架质量直接决定这个闭环能不能跑起来——生成的模板必须能**一次编译通过**，
否则 agent 会掉进工具链泥潭。这是 v0.4 最该投入的地方。
