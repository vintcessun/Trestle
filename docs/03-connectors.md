# 03 · Connector

**connector 是一整块自包含的接入能力。** 向上只暴露一个 name 和七个基本操作；
向下自己管连哪些机器、怎么连、长连接怎么维持、断了怎么重试、远端 agent 怎么部署。

它同时是**集约管理的单位**：`targets_list` 按 connector 分组，agent 挑机器时先看归属哪一组。

现在有两个，接入方式完全正交——这正是对抽象最有力的检验：

| connector | 机器 | 怎么进去 |
|---|---|---|
| `gpu-cluster` | gpu-1 / gpu-2 / gpu-3 / gpu-4 | VPN 容器的 SOCKS5(11080) + 密码 |
| `cloud` | web-1 / web-2 | 标准 SSH 直连 + `~/.ssh/id_ed25519` 公钥 |

上层调用这两组机器的代码**一个字都不用改**。

## 接口

```wit
// 插件导出
targets:       func() -> list<target-info>     // 我管哪些机器
ensure-ready:  func() -> result<_, error>      // 幂等：拉容器、拨 VPN、刷凭据
health:        func(target: string) -> health
config-schema: func() -> string                // Web UI 据此渲染配置表单
// 七个基本操作同名导出
```

## host 给 connector 的传输工具箱

```wit
net.dial(addr, timeout-ms)                  // TCP
net.dial-socks5(proxy, addr, timeout-ms)    // SOCKS5 CONNECT（no-auth）
probe-tcp(addr, timeout-ms) -> bool
local-exec(argv)                            // 本机进程，manifest argv[0] 白名单
ssh-connect(conn, target, host, port, user, creds-ref)
ssh-exec / ssh-alive / ssh-close
agent-ensure(session, target, agent-dir)    // 按 hash 幂等部署 + 接管
agent-call(agent, op, payload)
agent-upload / agent-download               // 分块与校验在 host 完成
forward-open(session, remote-host, remote-port)
session-remember / session-lookup / session-forget
```

**只有机械动作在这里。** 编排全在插件里——这条边界是整个架构成立的前提：
如果 host 开始替插件做决定，那插件就只是配置文件了。

### 为什么连接记在 host

host 会给每个 connector 起一个**实例池**（默认 4），让「同时打整支机队」能真并发。
但连接不该跟着实例走——gpu-4 的连接只该有一条。所以插件用
`session-remember` / `session-lookup` 把连接记在 host 那边，池里的实例共享它们。

### 为什么凭据是引用而不是值

`ssh-connect` 收的是 `creds-ref`（形如 `target:gpu-4`），不是密码本身。
明文因此**从不进入 wasm**。技能插件更进一步——它连 `secret-get` 都会被拒绝：
它要连机器就走 `base`，认证是 connector 的事。

## `gpu-cluster` 做了什么

```
probe 11080 端口
  ├─ 通 → 直接用（绝大多数调用走这条），成功后缓存 30 秒
  └─ 不通 → docker ps -a 看容器在不在
        ├─ 不在 → 报一条能直接照做的错误（带创建命令），不自作主张地建
        └─ 在   → docker start，然后轮询等端口就绪（上限 40s）
dial-socks5 → ssh-connect(password) → agent-ensure → 长连接
```

**只 `docker start`，不 `docker run`、不拉镜像**——容器由用户手工创建一次。

⚠️ **11080 而不是 1080**：本机 clash/mihomo 占着 1080，Docker Desktop(WSL2) 发布到
`127.0.0.1:1080` 会**静默失败**——`docker port` 空、主机拒连，但 `docker run` 返回成功。
这个坑排查花了很久，配置里的默认值和 `config-schema` 的描述都写着它。

## `cloud` 做了什么

```
dial(host:22) → ssh-connect(pubkey) → agent-ensure → 长连接
```

`ensure-ready` 就是一个 `Ok(())`。这个插件顺带说明了「写一个 connector 有多便宜」。

## 远端 agent

`agent-py/` 是仓库里的**标准件**，不隶属任何 connector。connector 负责把它送上去
并维持通道。

* **常驻在 Unix socket 上**，SSH channel 上跑的是 57 行的 `relay.py`。
  所以 agent 的生命周期与任何一条连接无关——daemon 挂了、网断了、电脑重启了，
  它都还在，下次连上来是一次**接管**而不是一次重装。
* **只用标准库**，任何 3.9+ 的解释器都能跑。uv 的作用是把解释器版本固定下来，不是拉包。
* uv 由主机自动装（探测 → curl 官方脚本 → 退回系统 python3），失败时报可操作的错误。
* 按内容 sha256 幂等部署：本地算哈希、远端比对，一致就一个字节都不传。
  agent 在握手帧里自报脚本哈希，所以「接管」这条路径连一次 `sha256sum` 往返都省了。

### gpu-1 上那条链路很贵

实测：gpu-1 经 VPN 时**新建一个 SSH exec channel 要约 1.7 秒**（gpu-4 上同一段代码是 125ms）。
这是那条链路的性质，不是实现问题。所以部署被压到了三个 channel——探测与启动合并成
一条命令，哈希对得上时远端自己就把 agent 起了。全量重装因此从 13.5s 降到 7.2s；
而正常冷启动走的是接管路径，2.4s。
