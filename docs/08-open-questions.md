# 08 · 开工前需要拍板的决策

六个。每个都给了推荐和代价——**如果你都同意推荐项，直接说"按推荐来"即可开工**。

---

## Q1 · daemon 还是单进程？

| | daemon（推荐） | 单进程 |
|---|---|---|
| 形态 | `trestled` 常驻，MCP/CLI 都是瘦客户端经 IPC 连它 | MCP server 进程内嵌一切 |
| 连接 | **跨会话共享**，真正只建一次 | 每个 Claude Code 会话各建 4 条 SSH（gpu-1 每次 5s） |
| job/ws/插件实例 | 唯一归属，CLI 和 MCP 看到同一份状态 | 各会话各一份，互相看不见 |
| 复杂度 | 多一层 IPC，调试链路变长 | 简单直接 |

**推荐 daemon**。你明确要的"一次 CLI 或一次 MCP 调用就启动后台常驻进程，lazy 加载"就是这个形态；
而且上一代"每会话重连"的痛点只有 daemon 能解。

代价是实打实的：IPC 协议、lazy spawn 的并发锁、idle 退出、版本不匹配时怎么办（客户端比 daemon 新）。
我建议 IPC 用最笨的方式（见 Q3）把这层压到最薄。

---

## Q2 · 远端 agent 用什么？

| 方案 | 优点 | 代价 |
|---|---|---|
| **A. Rust 静态二进制（推荐）** | 无远端运行时依赖；`edit` 等原语在远端本地执行；性能最好 | 需要交叉编译到 `x86_64-unknown-linux-musl`；二进制几 MB，每台传一次 |
| B. 复用 Python agent | 已经跑通、零成本 | 依赖远端 python3（一组都有 3.10/3.13）；跨语言维护两套 |
| C. 纯 SSH exec，不放 agent | 最简单 | 每次调用一次握手+起进程（跨 VPN 几百 ms）；`edit` 要传全文；job 状态无处安放 |

**推荐 A**。但有个现实障碍要先解决：**本机没装 `x86_64-unknown-linux-musl` target**
（已装的是 `x86_64-unknown-linux-gnu`）。两条路：

* `rustup target add x86_64-unknown-linux-musl` + 一个 musl linker（Windows 上要额外装工具链）
* 用已装的 **`cross`**（走 docker，本机有 Docker Desktop）：`cross build --target x86_64-unknown-linux-musl`

我倾向 `cross`——docker 已经在跑 VPN 容器了，不多这一个依赖。

**过渡方案**：v0.1 早期可以先用 B（Python agent 现成的，一组已部署），把 daemon/连接池/MCP 面
先跑通，再换 A。这样交叉编译的坑不会挡住主线。要不要这么过渡，你定。

---

## Q3 · IPC 用什么？

| 方案 | 说明 |
|---|---|
| **localhost TCP + token（推荐）** | 跨平台一份代码；daemon 监听 `127.0.0.1:0`（随机端口），端口和 token 写进 `%LOCALAPPDATA%\Trestle\daemon.json`；客户端读文件连它 |
| Named pipe / Unix socket | 权限模型更好（不经网络栈），但 Windows/Unix 要写两套 |

**推荐 TCP + token**。注意本机其他进程也能连 `127.0.0.1`，所以 **token 必须有**
（随机 32 字节，daemon.json 文件权限设为仅当前用户）。这不是过度设计——这个 daemon 能在一组
服务器上执行任意命令。

---

## Q4 · 工具命名 `base_read` 还是 `base.read`？

**推荐 `_`**。新版 MCP 允许 `.`，但 Claude Code 在 plugin-bundled 场景会把 `.` 正规化成 `_`，
导致你声明的名字和 permission matcher / hook 看到的名字不一致。namespace 概念留在 host 内部。

代价：对外看起来"树"只存在于命名约定里。但 MCP 的 `tools/list` 本来就是平面集合，
无论怎么写都是约定。

---

## Q5 · 凭据怎么存？

现状：一组都用**密码**认证，上一代明文存在 `servers.json`（gitignore）。

| 方案 | 说明 |
|---|---|
| **`secrets.toml` + gitignore（推荐起步）** | 和上一代一致，零摩擦。已经这么迁移好了 |
| 支持 `env:VAR` / `file:path` 前缀 | 配置里写 `password = "env:TRESTLE_GPU1_PW"`，值不落盘。**建议顺手实现**，成本很低 |
| Windows Credential Manager | 最安全，但跨平台要另写一套，且 CI/远程场景不方便 |

**推荐**：`secrets.toml` 起步 + 实现 `env:` / `file:` 前缀解析（两小时的事），暂不上系统钥匙串。

---

## Q6 · 要不要顺手换成 SSH 公钥认证？

这是我主动提的，不在原设计里。

现状一组都是密码认证，意味着**四个明文密码常驻在你本机磁盘上**。改成公钥：

* 生成一把 ed25519 key，`ssh-copy-id` 到一组（一次性人工操作，几分钟）
* `secrets.toml` 里只剩 key 路径，没有明文密码
* `russh` 对公钥认证支持良好，代码量差不多

**推荐做**，但**不阻塞 v0.1**——先用密码把链路跑通，v0.1 收尾时切换。
如果你不希望动服务器上的 `authorized_keys`，就一直用密码，也没问题。

---

## 附：一个非阻塞的提醒

`Trestle` 这个名字在 crates.io / GitHub 上可能已有同名项目（设计对话里没查重）。
如果将来要开源发布，开工时顺手查一下；自用的话无所谓。
