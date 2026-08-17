# 07 · 实测数据与教训

> 这份文档是唯一从上一代原封不动继承下来的——它记的是**实测事实**，不是设计。
> 底部补了 Rust 版自己的测量。

# 来自 Python 原型的实测数据与教训

上一代 `D:\Scripts\fleet`（Python）在一组真机上跑通并做过三套测试（17 项 + 19 项 + 53 个 MCP 工具
逐个真调）。这份文档记录**实测事实**，不是推测——Rust 版可以直接拿来当基线和回归目标。

---

## 整组机器：代理实测表

| target | 地址 | 直连 | 经 SOCKS5(11080) | hostname | GPU |
|---|---|---|---|---|---|
| `gpu-1` | 203.0.113.10:2201 | ✗ **TCP 超时(8s)** | ✓ 3.1s | node-a | 8 × GPU |
| `gpu-2` | 203.0.113.20:2202 | ✓ 1.5s | ✓ 1.0s | dc2 | 8 × GPU |
| `gpu-3` | 203.0.113.30:2203 | ✓ 0.8s | ✓ 0.8s | node-12 | 8 × GPU |
| `gpu-4` | 203.0.113.31:2204 | ✓ 0.9s | ✓ 0.8s | node-16 | 8 × GPU |

**只有 gpu-1 必须走 VPN**（校园网内网段），其余三台公网直连也通。

但**用户拍板：一组统一走 SOCKS5**——理由是"确保连接没问题"，代价接近零（公网机走代理的额外延迟
在测量噪声内），收益是**少一整类分支**：不用维护"哪台该走哪条路"的判断，connector 绑定统一。

这正好是 Connector 抽象的第一个真实用例：`gpu-1` 的 VPN 需求不是特例代码，是配置里的一行
`connector = "lab-vpn"`。

### VPN 通道的具体形态

* docker 容器 `vpn-proxy`（镜像 `example/vpn-proxy`），
  OpenVPN3 之上的 SOCKS5 中间层，split-tunnel：`*.example.edu` / `203.0.113.x` 走 VPN，
  其余直连——所以公网机走它是安全的。
* 容器内 microsocks 监听 **1080**，宿主侧发布到 **11080**。
* ⚠️ **端口坑**：本机跑 clash/mihomo 占着 1080，Docker Desktop(WSL2) 发布 `127.0.0.1:1080` 会
  **静默失败**（`docker port` 空、主机拒连，但 `docker run` 返回成功）。换 11080 就正常。
  这个坑排查花了很久，别再踩。
* 配置卷：宿主 `%LOCALAPPDATA%\VpnClient\data` → 容器 `/root/.local/share/vpnclient`，
  需要 `--cap-add NET_ADMIN --device /dev/net/tun`。

`ensure_ready` 的实现要点：先探端口，不通再看容器状态（`docker ps -a`），按需 `start` 或 `run`，
然后**轮询等端口就绪**（上一代等 30-40s 上限）。确认成功后缓存 ~30s，避免每次 dial 都查 docker。

### 磁盘：根分区普遍吃紧

| target | `~` | 大盘（`/home/alice/data` 是软链） |
|---|---|---|
| gpu-1 | 78G free / 877G (91%) | `/mnt/sdc/.../my-project` **14G free / 7.3T (100%)** |
| gpu-2 | **13G free / 879G (99%)** | `/mnt/data/users/alice` 284G free / 14T (98%) |
| gpu-3 | **12G free / 838G (99%)** | 250G free / 879G (71%) |
| gpu-4 | 114G free / 838G (86%) | **1.4T free / 3.5T (60%)** |

写东西**默认往 `workdir`（大盘）写，不要往 `~` 写**。gpu-4 最宽裕，gpu-1 的项目盘已经满了。
`fleet_status` 应该把这个显示出来——上一代就是这么发现 gpu-1 满了的。

---

## 连接模型：实测数字

形态 = **一条常驻 SSH + 一个远端常驻 agent**（JSON-Lines over 单 channel，多线程并发处理）。

| 指标 | 实测 |
|---|---|
| 冷启动（建链 + 部署/拉起 agent） | gpu-3/gpu-4 0.7–0.9s，gpu-2 1.6s，**gpu-1 5.0s**（经 VPN） |
| 热调用（同一条连接上再发一次） | **33–52ms** |
| kill -9 远端 agent 后恢复 | 0.7s（gpu-1 4.3s） |
| 掐掉 SSH transport 后恢复 | 0.9s（gpu-1 4.9s） |
| 5 次连续 ping | avg 37–44ms |

**Rust 版的回归目标：热调用不劣于 50ms，自愈不劣于 2s（gpu-1 5s）。** 达不到说明架构走偏了。

冷热差 ~100 倍，这就是为什么 daemon 模式值得——每个 Claude Code 会话各建一次连接的话，
这 100 倍的差距会在每个新会话里重新付一遍。

---

## 四个必踩的坑

### 1. `cd DIR && cmd &` 会让"后台"调用一直阻塞

```bash
# 错：调用方会一直卡到任务结束，"后台"白做
cd /work && setsid bash -c '...' > log 2>&1 &

# 对：工作目录交给进程的 cwd，& 只作用于单条命令
setsid bash -c '...' > log 2>&1 &
```

原因：`&` 作用于整个 `&&` 列表，bash 会 fork 一个子 shell 去 wait 它，而**那个子 shell 一直攥着
调用方的 stdout 管道**，于是读端要等到任务结束才拿到 EOF。实测把一个 12 秒的任务变成 12 秒阻塞。

Rust 里同样成立——只要你是通过 shell 起后台任务，就会遇到。

### 2. `$!` 拿到的不是任务 pid

`setsid` 会 fork，`$!` 是 setsid 自己的 pid，它随即退出，于是 pid 立刻"死了"。

```bash
# 对：让最终进程自己把 pid 落盘（exec 不换 pid）
setsid bash -c 'echo $$ > pidfile; exec bash -lc "真正的命令"' > log 2>&1 &
```

如果还要拿退出码，就别 exec：`echo $$ > pidfile; bash -lc "cmd"; echo $? > rcfile`。

### 3. `pgrep -f 'pattern'` 会匹配到检查命令自己

检查命令的命令行里就含那个 pattern，于是永远至少匹配 1 个。用方括号技巧：

```bash
ps -eo cmd | grep -c '[s]eq 1 20'
```

上一代因为这个误报过一次"有孤儿进程"。

### 4. 输出名与入参名不一致

`shutil.make_archive(base, "gztar")` 自己决定后缀：你传 `x.tgz`，它产出 `x.tar.gz`，
调用方拿自己给的路径去解包就 404。

**通用教训：任何"你给一个路径、我产出一个文件"的接口，产出必须就是你给的那个路径。**
这是逐个工具真调时才发现的——单元测试和"看起来对"都抓不到。

（另一个同类：Windows 下 stdout 不显式设 UTF-8，含中文的 JSON-RPC 帧会因 GBK 编码失败而毁掉
整条协议流。Rust 默认 UTF-8，这条不适用，但 CLI 输出到 Windows 控制台时仍要注意。）

---

## 可复用资产映射

| Python 文件 | Rust/wasm 落点 |
|---|---|
| `fleetlib/proxy.py` | `plugins/connectors/ssh-socks5`（SOCKS5 握手在 `trestle-transport::dial`） |
| `fleetlib/pool.py` | connector 插件的长连接逻辑 + `trestle-transport::deploy` |
| `remote/fleet_agent.py` | `agent-py/trestle_agent.py` |
| `fleetlib/ops.py` | `agent-py` 的 put_chunk/get_chunk + `trestle-transport::transfer` |
| `servers.json` | `config/trestle.toml`（已迁移） |
| `mcp_smoke.py` | `crates/trestle-daemon/tests/smoke.rs` |

最后一条特别提一句：`mcp_smoke.py` 那种"对每个工具用安全参数真调一次"的测试，在 53 个工具里
抓到了 1 个真 bug（就是上面第 4 条）。Rust 版的等价物已经在
`crates/trestle-daemon/tests/smoke.rs`，第 4 条那个形状还专门有一个测试守着
（`an_output_path_is_always_the_path_that_was_asked_for`）。

---

# Rust 版自己的实测（2026-08-17）

## 连接

| | 稳态冷启动（接管） | 热调用 | 自愈 | 全量重装 |
|---|---|---|---|---|
| gpu-4 | 566ms | 36–44ms | 508ms | 785ms |
| gpu-3 | ~800ms | 41ms | 569ms | — |
| gpu-2 | ~900ms | 52ms | 731ms | — |
| gpu-1（经 VPN） | 2.4s | 52–57ms | 2.5s | 7.2s |
| web-1 | 1.0s | 26ms | 986ms | — |
| web-2 | 1.2s | 116ms | 1.2s | — |

冷热差在 gpu-4 上是 **36 倍**。这个比值掉下去就说明连接没被复用——那正是 daemon 模式
要解决的问题，所以验收测试断言的是**比值**而不是绝对毫秒数（一组共用一个 VPN 容器，
绝对延迟随网络窗口起伏，同一台机器见过 36ms 也见过 148ms）。

并发：4 台机器各 `sleep 2`，实例池大小 2 时耗时 4.1s（串行会是 8s）。

## 两个新踩的坑

### 5. `stat -c` 不处理转义，`stat --printf` 才处理

```bash
stat -c '%s\t%Y' f        # 输出字面量 \t，split 什么都分不出来
stat --printf='%s\t%Y' f  # 输出真的制表符
```

`find -printf` 与 `du` 都是正常的，只有 `stat -c` 这样。查这个花了一轮真调。

### 6. gpu-1 上新建一个 SSH channel 要 ~1.7 秒

gpu-4 上同一段代码是 125ms。不是 shell profile（干净环境下 `bash -c true` 是 0.00s），
是那条链路的性质。

结论不是「优化它」而是「少开几个」：部署被压到三个 channel——探测与启动合并成一条
命令，哈希对得上时远端自己就把 agent 起了。全量重装 13.5s → 7.2s。

> **通用教训：在慢链路上，往返次数比每次往返干多少活重要得多。**
> 把四个探测合并成一条命令的收益，远大于把每条命令写得更精巧。

### 一个测量陷阱

一开始量出「`bash -lc true` 是 0.00s 而 `bash -c true` 是 1.51s」，差点据此改掉整个
exec 路径。那个测量是**被污染的**：它是在 agent 已有的登录 shell 里嵌套跑的，
外层早就把 profile 的钱付过了。用 `env -i` 重测就都是 0.00s。

在一个已经初始化好的环境里测「初始化开销」，测到的永远是 0。
