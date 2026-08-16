# 06 · 路线图

原则：**每一版都必须自己能用**。不做"等下一版才有价值"的中间态。

---

## v0.1 — 整组机器真的能用

目标：替代 Python 版 fleet 的日常使用，性能不劣于它。

**范围**

* `trestled` daemon：IPC server（localhost TCP + token）、lazy 自启、idle 退出
* `SessionPool`：一台一条常驻连接、lazy、自愈、keepalive、UnknownState 边界
* Connector：`direct-ssh` + `socks5`（含 VPN 容器 ensure_ready），**Native 实现**
* `trestle-agent`：远端常驻 agent，静态二进制，按 hash 幂等部署
* `BaseService`：read / write / edit / shell_exec / shell_spawn + list / find
* job：start / list / logs（host 记 offset）/ wait / stop
* `trestle-mcp`：stdio MCP 前端，约 20 个工具（见 `04`）
* `trestle` CLI：`serve` / `targets` / `status` / `exec` / `job *` / `doctor`
* 配置：`trestle.toml` + `secrets.toml`

**明确不做**：WASM、Monitor ws、xfer_*、插件、Streamable HTTP、Web UI。

**验收**（对标 `07` 的实测基线）

- [ ] 一组全部可达，gpu-1 经 VPN 自动拉起容器
- [ ] 热调用 ≤ 50ms，冷启动 gpu-1 ≤ 6s / 其余 ≤ 2s
- [ ] kill 远端 agent、掐 SSH transport，下次调用自动恢复 ≤ 2s（gpu-1 ≤ 5s）
- [ ] `base_shell` 超时能杀掉整个进程组（孙进程不残留，用 `[s]eq` 技巧验证）
- [ ] `job_start` 起的任务在 SSH 断开后仍在跑，退出码正确落盘
- [ ] 已发出的请求在连接中断时返回 `UnknownState`，**不自动重放**
- [ ] 每个 MCP 工具逐个真调通过 + schema 里 `target` 确实 required

---

## v0.2 — 摩擦削减

* Monitor WebSocket（`/monitor/ws`，见 `05`）：`monitor_open` 返回 URL + cli 兜底
* EventBus + tracing
* `xfer_push` / `xfer_pull`（文件目录自动识别）/ `xfer_sync`（增量）/ `xfer_between`
* `fleet_status`（GPU 占用/空闲卡/磁盘/负载）、`fleet_run` 广播、`gpu_find` 跨机选卡
* `job_start(gpus="auto:2")` 自动挑空闲卡并设 `CUDA_VISIBLE_DEVICES`

**验收**：Monitor 直接用 `ws` 源挂上一个真实训练任务，任务结束时收到带退出码的 `closing` 帧；
超时到期能收到 `reason=timeout` 且明确说明任务仍在跑。

---

## v0.3 — 插件运行时

* `trestle-plugin-host`：Wasmtime 47 + Component Model，填上 `ToolBackend::Wasm`
* WIT 绑定按 `wit/trestle.wit`，host 导出 base + events
* capability manifest 解析与强制（fs 路径白名单、shell 前缀白名单），拒绝要发事件
* `plugin list/enable/disable/reload` + `notifications/tools/list_changed`
* 第一个真插件：`nvidia`（够小、够常用、权限边界清晰）

**验收**：`nvidia.wasm` 未编译进主程序，但 `nvidia_*` 工具可用；
manifest 之外的 shell 命令被拒绝并在 Monitor 里可见；`plugin enable` 后 Claude Code 无需重连即可见新工具。

---

## v0.4 — 摩擦 → capability 闭环

* `trestle plugin new <name>`：生成**一次编译通过**的脚手架（WIT 绑定 + manifest + 示例 call）
* `trestle plugin install`：校验 + 人工确认一次 + 热加载
* Claude Code Hooks → `POST /cc/events`（可观测面板，可选）

**验收**：agent 从"遇到一个没有工具的操作"到"该操作变成常驻工具"，全程不需要人写代码，
只需要人在 install 时确认一次权限。

---

## 一条贯穿的纪律

上一代最有效的做法，继续用：

> 每个能力交付时，写一个**真调**的测试，而不是"看起来对"。
> 53 个工具逐个真调，抓到了 1 个 mock 测试永远抓不到的 bug。

以及：

> 负面结果要如实记录。`07` 里那四个坑都是踩过之后写下来的，Rust 版会以不同形式再遇到。
