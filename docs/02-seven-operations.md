# 02 · 七个基本操作

这是整个系统唯一的原语集合。**如果一个东西不属于这七个之一，它就该是插件。**

```
read     文本读，可以只读一段
write    文本写
edit     改文件的一部分（不传整个文件）
shell    跑命令；detach=true 时脱离会话在后台跑
upload   本地 → 远端，文件与目录自动识别
download 远端 → 本地
forward  把远端一个端口映射到本地
```

由 connector 实现，host 只负责按 `target` 路由。

## 签名

```
read    (target, path, start_line?, max_lines?)      -> { content, total_lines, truncated }
write   (target, path, content, append?, make_dirs?) -> { bytes, path }
edit    (target, path, op)   op = literal | regex | lines | insert
                                                     -> { changed, occurrences, path }
shell   (target, command, cwd?, timeout_secs?, env?, detach?, name?)
          detach=false -> { exit_code, stdout, stderr, timed_out, duration_ms }
          detach=true  -> { job_id, pid, pgid, log_path, meta_path, rc_path }
upload  (target, local_path, remote_path, exclude?, sync?, dry_run?, delete?)
download(target, remote_path, local_path, exclude?, sync?, dry_run?, delete?)
                                                     -> { files, bytes, sha256?, path }
forward (target, remote_port)                        -> { local_port, url, handle }
```

## 每一条为什么长这样

### 没有默认机

每个针对单机的操作，`target` 都是**必填**。这条是上一代实测后拍板的：默认机会制造
「你以为在 gpu-4 上删文件、其实在 gpu-1」这类静默事故。多写一个词，换掉一整类事故。

面向全队的操作用可选的 `targets`，留空表示全部——那是「全队」语义，不是「默认机」。

解析规则：**名字 → 别名 → host 精确匹配**。失败时错误消息里必须列出所有可选名字：

```
✗  "target not found"
✓  "unknown target 'x36'; known: gpu-1, web-1, web-2, gpu-2, gpu-3, gpu-4"
```

### 为什么 edit 是原语而不是 read+write

组合意味着每改一行都要把整个文件传两遍。远端 agent 在本地做这件事，传输量只有 diff 大小。
这也是必须有远端 agent 的理由之一。

### shell 为什么有 detach 而不是拆成两个工具

拆成两个（`shell_exec` / `shell_spawn`）语义更清楚，但那就是五个原语了。
用一个 `detach` 参数保住「七个」这个数字，同时把两个坑沉进 base 只踩一次：

**坑一：`&` 不能跟在 `&&` 列表后面。**

```bash
# 错：调用方会一直卡到任务结束，「后台」白做
cd /work && setsid bash -c '...' > log 2>&1 &

# 对：工作目录交给进程的 cwd，别经过 shell 的后台机制
```

`&` 作用于整个 `&&` 列表，bash 会 fork 一个子 shell 去 wait 它，而**那个子 shell 一直
攥着调用方的 stdout 管道**，于是读端要等到任务结束才拿到 EOF。实测把一个 12 秒的任务
变成 12 秒阻塞。

**坑二：`$!` 拿到的不是任务 pid。** `setsid` 会 fork，`$!` 是 setsid 自己的 pid，它随即
退出，于是 pid 立刻「死了」。`agent-py` 里用 `start_new_session=True` 直接 fork，
拿到的 pid 就是新会话首进程——而它同时是 pgid，停止任务时按这个 pgid 杀整组。

**超时杀整个进程组**，不是只杀直接子进程——否则孙进程会残留。验证要用方括号技巧：

```bash
ps -eo cmd | grep -c '[s]eq 1 300'      # 不加方括号的话检查命令自己就会被匹配到
```

`shell` 的超时上限由配置定；错误消息里要指出该改用 `job_start`：

```
✓  "shell on gpu-4 timed out after 60s; process group killed.
    For long-running work use job_start instead."
```

**在错误里指出正确的工具**——agent 读到就会改用 `job_start`，而不是把 timeout 调大再撞一次。

### upload / download：产出路径必须就是入参路径

上一代在这里栽过：`shutil.make_archive(base, "gztar")` 自己决定后缀，你传 `x.tgz`，
它产出 `x.tar.gz`，调用方拿自己给的路径去解包就 404。

> **通用教训：任何「你给一个路径、我产出一个文件」的接口，产出必须就是你给的那个路径。**

这是逐个工具真调时才发现的——单元测试和「看起来对」都抓不到。现在
`smoke.rs::an_output_path_is_always_the_path_that_was_asked_for` 专门守着它。

**增量同步保留源端 mtime。** 判据是 size + mtime，如果远端记的是「写入时刻」，
两台机器之间哪怕一秒的时钟偏差都会让下次同步误判成「变了」，白传一遍。

### forward：本地端口由 host 分配

调用方**不能**指定本地端口，否则「上次开的转发还占着 8080、这次也要 8080」就会撞车，
而调用方通常根本不在乎是哪个口。

转发通道是**会话级资源**：开它的那个客户端会话结束，通道就关掉、端口还回去。
一条开了一次就没人用的转发不该一直占着，而「没人用」最可靠的判据就是「开它的会话没了」。

## 重试的诚实边界

```
请求还没发出去   →  重建连接后自动重放，安全
请求已经发出去   →  绝不自动重放，返回 UnknownState
```

已经发出但没拿到响应时，那条命令**可能已经在远端执行了**。自动重放意味着可能把一条
`rm -rf` 或一次训练启动跑两遍。正确做法是把不确定性如实交给上层：

```
unknown state: 'shell' on gpu-4 was sent but no response came back.
The remote side may have executed it; check state before retrying.
```

只有明确幂等的读操作（`read` / `download`）才允许自动重试。这条在
`trestle-transport` 与 `trestle-core` 里各有一个测试守着，别放松它。
