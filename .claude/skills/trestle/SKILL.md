---
name: trestle
description: Operate remote servers through Trestle — run commands, read and edit files, move data between machines, start and watch long jobs, claim GPUs, and open port forwards. Use whenever work needs to happen on a machine that is not this one, or when asked about GPUs, training runs, remote logs, or which server to use.
---

# Trestle

远程机器上的活儿都从这里走。**你自己的 Bash/Read/Edit 只对本机有效**——要碰远端，用
`base_*` 与插件工具。

先看一眼 `targets_list`：机器叫什么、归哪个 connector、`note` 里写了什么。
`note` 是人写给你的（哪个盘满了、东西该放哪），**它比你猜的准**。

## 七个基本操作

```
base_read      读远端文件，可只读一段
base_write     写远端文件
base_edit      改一部分。比 read+write 便宜得多——只传 diff
base_shell     跑一条短命令
base_upload    本地 → 远端（文件与目录自动识别，sync=true 只传变化的）
base_download  远端 → 本地
base_forward   把远端端口映射到本地
```

**每个都必须显式给 `target`。没有默认机。** 这不是啰嗦：默认机会制造
「你以为在 gpu-4 上删文件、其实在 gpu-1」这类静默事故。

## 最容易做错的四件事

**1. 长任务不要用 `base_shell`。**
`base_shell` 有超时，超时会杀掉整个进程组。训练、编译、下大文件——用 `job_start`，
然后 `job_logs` 增量看、`job_wait` 等、`job_stop` 停。上一代系统就是栽在这：
agent 拿短命令工具跑训练，然后撞超时，任务死了还以为是网络问题。

**2. 要卡先 `gpu_acquire`，别自己看 `nvidia-smi` 然后开跑。**
两个 agent 同时看到「卡 0 空着」，然后同时用它——这就是抢卡。`gpu_acquire` 会
排队并把卡独占给你；拿不到时错误里写着谁占着、干什么。
跑 job 的话直接 `job_start(gpus="auto:2")`，它替你要卡、也替你在任务结束时还卡。

**3. 跨机传文件不要「下到本地再传上去」两步走。**
`xfer_between` 一次搞定，`xfer_distribute` 一份发多台。它内部就是中转，但会
做校验、会处理排除规则、失败时说得清楚在哪一步断的。

**4. 别的 agent 可能正在同一台机器上干活。**
动手之前 `agents_list` 看谁在线、在哪台机器做什么；`notes_list` 看留言板。
你要长期占用什么（一个目录、一台机器），`note_put` 留一句，**TTL 必填**。

## 常用工具

| 想干什么 | 用 |
|---|---|
| 有哪些机器 | `targets_list` |
| 全队状态一眼 | `fleet_status` |
| 同一条命令打多台 | `fleet_run` |
| 哪台有空卡 | `gpu_find` |
| 某台的卡都被谁占着 | `gpu_status` |
| 要卡 / 还卡 | `gpu_acquire` / `gpu_release` |
| 起一个长任务 | `job_start` |
| 任务在跑吗、日志、等、停 | `job_list` / `job_logs` / `job_wait` / `job_stop` |
| 列目录、找文件、看大小 | `fs_list` / `fs_find` / `fs_stat` / `fs_tree` / `fs_disk` |
| 机器之间搬东西 | `xfer_between` / `xfer_distribute` |
| 挂一个实时监视 | `monitor_open`（返回 ws URL 给 Monitor） |
| 谁在线、留言板 | `agents_list` / `notes_list` / `note_put` |

## 挑机器

不要默认用同一台。`fleet_status` 或 `gpu_find` 看一眼再决定，然后读那台的 `note`——
盘满不满、工作目录在哪，都写在里面。**东西写 `workdir`，别写 `~`**：根分区通常紧张。

## 错误消息值得读完

Trestle 的错误分两段：`detail` 说发生了什么，后面跟一句下一步能做什么。
两段都不是客套话，照着做通常就对了。

三种值得单独认一下：

* **`unknown state`** —— 请求发出去了但没拿到回音，**远端可能已经执行过了**。
  不要重试，先去查状态。
* **`not ready`** —— connector 的前置条件没就绪（代理没起来之类）。
  错误里一般带着把它拉起来的命令。
* **卡不够** —— 里面列着每张卡被谁占着。想等的话用 `job_start` 排队，
  别写循环去轮询。

## 端口转发

`base_forward(target, remote_port)` 返回一个**本地**端口——**你不能指定它**，
host 分配。指定会让旧配置和新配置抢同一个端口。

转发是**会话级**的：这个会话结束它就关掉、端口还回去。所以需要就开，不用囤着。

## 有工具做不到的事

Trestle 的能力是插件，加一个很便宜（`trestle plugin new` 生成的脚手架不改一个字就能编）。
如果你反复用 `base_shell` 拼同一串命令，那说明这里缺一个工具——跟用户说一声，
比让每个会话重新拼一遍好。
