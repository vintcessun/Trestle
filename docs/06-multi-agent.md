# 06 · 多个 agent 同时用

一个 agent 看不见别的 agent 在干什么，就会撞车——同时抢同一张卡、同时改同一个目录、
同时重启同一个服务。这一层的全部目的就是让「谁在干什么」可见。

三件东西，都在 daemon 里，不属于任何插件。

## 1. 在场感知

每个 MCP 前端或 CLI 连上 daemon 时注册一个会话（agent id、标签、启动时间）。
所有 EventBus 事件都带 agent id。

```
$ trestle agents
a3  claude-code:paper   最近：base_shell gpu-4
a7  claude-code:infra   最近：job_start gpu-1
a9  cli                 最近：（还没做什么）
```

标签是有意义的：MCP 前端用 `claude-code:<当前目录名>`，所以你能一眼看出是哪个会话。

## 2. 会话级资源

**端口转发属于开它的那个会话。**

```
会话建立 ──► forward 可开，端口由 host 分配
会话断开 ──► 它开的 forward 全部关闭，端口还回池子
会话恢复 ──► 按落盘的声明重建，端口重新分配
daemon 重启 ──► 同上
```

一条开了一次就没人用的转发不该一直占着，而「没人用」最可靠的判据就是「开它的会话没了」。
端口重新分配不破坏任何约定——调用方本来就没指定过端口。

## 3. 留言板

```
$ trestle note "gpu-4:/data/exp1" "在跑 latent-v3，别动这个目录" --ttl 3600
$ trestle notes gpu-4
gpu-4:/data/exp1  在跑 latent-v3，别动这个目录  —— a3
```

**TTL 必填**。没有过期时间的留言板会变成一堆没人清的垃圾，而一堆没人清的垃圾等于没有
留言板。过期的在读取时顺手清掉，不需要单独的清理任务。

留言板是**提示**不是锁。「我在这个目录上跑实验」这种意图本来就不该用锁表达。

## GPU：单点分配，不是租约

早先的设计里有一张「谁占了哪张卡」的协作式租约表。它站不住：

* 资源 key 是自由文本——`gpu:gpu-4:0,1` 和 `gpu:gpu-4:1,0` 指不到同一个东西；
* TTL 只能靠猜——没人知道训练要跑多久，猜短了锁提前失效、猜长了卡白占；
* 又声明成 advisory 允许绕过。

于是它既不保证互斥、也不反映真实占用——**发明了一个跟现实平行的锁世界**。

换成两条：

**占用视图从真实状态推导。** `nvidia-smi` 知道每张卡上跑着什么。别人绕过 Trestle 直接
ssh 上去占的卡，照样看得见——租约表做不到这一点。

**互斥收进单点分配。** 要卡的人向同一个分配器排队，daemon 天然把他们排成序。
不需要 CAS、不需要 TTL，也没有「锁悬着」的问题。

```
job_start(target="gpu-4", gpus="auto:2")
   → gpu.allocate("gpu-4", 2, "job train-v3")
       → 查 nvidia-smi 真实占用 + 自己刚分配还没起来的预留
       → 原子选卡 → 记预留 → 设 CUDA_VISIBLE_DEVICES
```

拿不到时的错误说清楚谁占着、干什么：

```
asked for 4 free GPU(s) on gpu-4 but only 1 is free.
Busy: gpu0 (job train-v3, 40960 MiB used), gpu1 (something outside Trestle, 78000 MiB used), ...
```

**释放绑定在 job 生命周期上**，不绑在时间上：任务活多久就占多久。这才是正确的 TTL。

## 抢卡

`tasks.schedule` + 导出 `on-tick`：插件注册一个周期回调，每次醒来试一次
`gpu.allocate`——拿不到就继续睡，拿到就起任务再 `tasks.cancel`。
互斥由分配器保证，插件这边不需要任何锁逻辑。

为什么不是在 host 里写一个 poll+predicate 的小语言：那样每加一种等待条件就要动 host。
回调进插件之后，「等什么、怎么算够」全是插件自己的事。

## 状态持久化

落在**程序所在目录**下的 `state/`（不是 `%LOCALAPPDATA%`——所有运行期文件都跟程序在一起，
避免「配置在这、状态在那」的管理混乱）。

原子写：先写临时文件再改名，崩在半路也不会留下坏掉的 JSON。

恢复路径：daemon 不在 → 任一客户端调用时 lazy 自启 → 读状态 → connector `ensure-ready`
→ 建连 → **发现远端 agent 还活着就直接接管**（比对指纹与脚本哈希），不重新部署、
不重起、job 不丢。用户侧零操作，也不占开机资源。
