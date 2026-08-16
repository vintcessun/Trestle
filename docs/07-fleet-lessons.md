# 07 · 来自 Python 原型的实测数据与教训

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

| Python 文件 | Trestle 对应 | 直接可搬的东西 |
|---|---|---|
| `fleetlib/proxy.py` | `trestle-connectors::socks5` | SOCKS5 CONNECT 握手字节序列（no-auth）、容器拉起状态机、11080 的由来 |
| `fleetlib/pool.py` | `trestle-daemon::SessionPool` | 连接复用/lazy/自愈语义、**UnknownState 的判定边界**、幂等 op 白名单 |
| `remote/fleet_agent.py` | `trestle-agent` | JSON-Lines 协议帧、op 清单、job 的 `{meta.json,pid,rc,out.log}` 落盘布局、log_probe 增量协议 |
| `fleetlib/ops.py` | `trestle-base` + `xfer_*` | 分块传输(512KB)+sha256 校验、目录同步的差异算法（size+mtime 比对）、跨机中转 |
| `servers.json` | `config/trestle.toml` | 一组拓扑（已迁移） |
| `mcp_smoke.py` | `tests/` | **逐个工具真调 + 校验 schema required** 的测试形态，值得照搬 |

最后一条特别提一句：`mcp_smoke.py` 那种"对每个工具用安全参数真调一次"的测试，在 53 个工具里
抓到了 1 个真 bug（就是上面第 4 条）。Rust 版应该在 `tests/` 里保留等价物。
