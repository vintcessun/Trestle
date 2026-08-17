//! 实例池：一个插件同时能有几个 wasm 实例。
//!
//! 一个 wasm 实例在被调用期间是**独占**的（一次调用要 `&mut Store`），所以
//! 「几个实例」就等于「几个调用方能同时用这个插件而不互相等」。
//!
//! 伸缩策略是刻意的：
//!
//! * **从 1 个起。** 绝大多数插件这辈子都不会被并发调用，为它们预留实例是纯浪费——
//!   一个 componentize-py 产出的组件就是 18 MB。
//! * **撞上了才长，指数长**：1 → 2 → 4 → 8 …，上限默认是 CPU 核心数。再多没意义，
//!   实例买的是「CPU 上的一个执行上下文」，超过核心数只换来更多内存。
//! * **久了没撞上就线性收**：每次巡检收回一个。收得比长得慢是刻意的——
//!   「刚才忙过」比「此刻闲着」更能预测下一秒。
//!
//! 「忙」的判定不靠计数器，靠 `Arc` 的引用计数：[`InstancePool::pick`] 交出去的是一个
//! `Arc<T>`，调用方握着它做完整个调用。所以「只有池自己持有」（`strong_count == 1`）
//! 就是「没人在用」，不需要额外的记账，也不会因为忘了归还而泄漏。

use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use std::time::{Duration, Instant};

/// 造一个新实例。池要能在任意时刻自己长出实例来，所以它得留着造实例的那套东西。
pub type Factory<T> = Arc<
    dyn Fn() -> Pin<Box<dyn Future<Output = anyhow::Result<T>> + Send>> + Send + Sync + 'static,
>;

/// 空闲多久开始回收。「很久没用」的默认解释。
pub const DEFAULT_IDLE_SECS: u64 = 600;

/// 巡检间隔。收回是线性的——一次巡检收一个，所以这个值同时决定收得多快。
pub const SWEEP_INTERVAL_SECS: u64 = 60;

/// 池的伸缩策略。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PoolPolicy {
    /// 最多几个实例。默认 = CPU 核心数。
    pub max: usize,
    /// 连续这么久没有出现「所有实例都忙」，就开始一个一个收回。
    pub idle_secs: u64,
}

impl Default for PoolPolicy {
    fn default() -> Self {
        Self {
            max: cpu_count(),
            idle_secs: DEFAULT_IDLE_SECS,
        }
    }
}

impl PoolPolicy {
    /// 定死一个实例。没有声明 `stateless` 的技能插件走这条——
    /// 它可能把状态存在 wasm 内存里，多开会让实例各看各的。
    pub fn single() -> Self {
        Self {
            max: 1,
            ..Default::default()
        }
    }

    /// 上限。0 表示「跟 CPU 核心数走」，配置里就是这么写的。
    pub fn with_max(mut self, max: usize) -> Self {
        self.max = if max == 0 { cpu_count() } else { max };
        self
    }

    pub fn with_idle_secs(mut self, idle_secs: u64) -> Self {
        self.idle_secs = if idle_secs == 0 {
            DEFAULT_IDLE_SECS
        } else {
            idle_secs
        };
        self
    }
}

/// CPU 核心数，拿不到就按 4 算；**下限 2**。
///
/// 下限是有理由的：实例被占住的绝大部分时间是在等网络（SSH、远端 agent），
/// 不是在烧 CPU。所以哪怕只有一个核，两个实例也是有意义的——核心数只是
/// 一个「不至于离谱」的默认上限，不是这件事的真实约束。
pub fn cpu_count() -> usize {
    std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(4)
        .max(2)
}

/// 一个插件的实例池。
pub struct InstancePool<T> {
    pub name: String,
    instances: RwLock<Vec<Arc<T>>>,
    factory: Factory<T>,
    policy: PoolPolicy,
    /// 只允许一路在长。没有它的话六路并发的第一次扇出会各长各的，一下子长到上限。
    growing: tokio::sync::Mutex<()>,
    /// 上次看到「所有实例都忙」的时刻。回收按它算空闲，而不是按「上次被调用」——
    /// 一直有调用但从不重叠，恰恰说明多出来的实例是不需要的。
    last_contention: Mutex<Instant>,
    /// 都忙的时候轮转排队用。
    next: AtomicUsize,
    /// 历史最高实例数。收回之后还看得见曾经涨到过多少，排查时有用。
    high_water: AtomicUsize,
}

impl<T: Send + Sync + 'static> InstancePool<T> {
    /// 起一个池。**先造一个实例**——不是懒到第一次调用，因为那样第一次调用会
    /// 平白多等一次实例化，而实例化失败（插件坏了）应该在加载时就暴露出来。
    pub async fn new(
        name: impl Into<String>,
        factory: Factory<T>,
        policy: PoolPolicy,
    ) -> anyhow::Result<Self> {
        let first = Arc::new(factory().await?);
        Ok(Self {
            name: name.into(),
            instances: RwLock::new(vec![first]),
            factory,
            policy,
            growing: tokio::sync::Mutex::new(()),
            last_contention: Mutex::new(Instant::now()),
            next: AtomicUsize::new(0),
            high_water: AtomicUsize::new(1),
        })
    }

    /// 拿一个实例来用。拿到的 `Arc` 要**握到调用结束**——池靠它判断谁在忙。
    ///
    /// 都忙且还能长就先长一个再给；长到上限了就轮转排队。
    pub async fn pick(&self) -> Arc<T> {
        if let Some(free) = self.free_one() {
            return free;
        }

        // 都忙。记下这一刻——回收看的就是「有多久没撞上过了」。
        *self.last_contention.lock().expect("pool clock") = Instant::now();

        if self.size() < self.policy.max {
            if let Err(e) = self.grow().await {
                // 长不出来不是致命的：排队就是了。但必须说出来，否则
                // 「为什么突然变慢」会变成一个查不到源头的问题。
                tracing::warn!(pool = %self.name, error = %format!("{e:#}"),
                    "cannot grow the instance pool; calls will queue instead");
            }
            if let Some(free) = self.free_one() {
                return free;
            }
        }

        let instances = self.instances.read().expect("pool lock");
        let i = self.next.fetch_add(1, Ordering::Relaxed) % instances.len();
        Arc::clone(&instances[i])
    }

    /// 池里任意一个实例都能回答的问题用它（list-tools / ui-panel / targets）。
    pub fn any(&self) -> Arc<T> {
        Arc::clone(&self.instances.read().expect("pool lock")[0])
    }

    /// 巡检一次：空闲够久就收回**一个**实例。返回收了几个（0 或 1）。
    ///
    /// 一次只收一个，所以从 8 收回 1 要走 7 次巡检——这就是「线性递减」。
    pub fn sweep(&self) -> usize {
        let idle = self.last_contention.lock().expect("pool clock").elapsed();
        if idle < Duration::from_secs(self.policy.idle_secs) {
            return 0;
        }

        let mut instances = self.instances.write().expect("pool lock");
        if instances.len() <= 1 {
            return 0;
        }
        // 只收没人握着的那个。判定和写入在同一把写锁下，所以不存在
        // 「刚判定完就被 pick 走」——pick 要读锁。
        let Some(pos) = instances
            .iter()
            .position(|inst| Arc::strong_count(inst) == 1)
        else {
            return 0;
        };
        instances.remove(pos);
        tracing::debug!(pool = %self.name, size = instances.len(),
            "reclaimed an idle plugin instance");
        1
    }

    pub fn size(&self) -> usize {
        self.instances.read().expect("pool lock").len()
    }

    pub fn max(&self) -> usize {
        self.policy.max
    }

    pub fn high_water(&self) -> usize {
        self.high_water.load(Ordering::Relaxed)
    }

    /// 当前空闲了多久（离上次「所有实例都忙」有多远）。
    pub fn idle(&self) -> Duration {
        self.last_contention.lock().expect("pool clock").elapsed()
    }

    fn free_one(&self) -> Option<Arc<T>> {
        let instances = self.instances.read().expect("pool lock");
        instances
            .iter()
            .find(|inst| Arc::strong_count(inst) == 1)
            .map(Arc::clone)
    }

    /// 指数扩张：一次补到 `min(当前 × 2, 上限)`。
    async fn grow(&self) -> anyhow::Result<()> {
        // 串行化：并发的六路扇出会一起走到这儿，但只该有一路真的在造。
        let _one_at_a_time = self.growing.lock().await;

        let have = self.size();
        if have >= self.policy.max {
            return Ok(());
        }
        let want = have.saturating_mul(2).min(self.policy.max);

        // 造实例要 await，所以绝不能握着锁造——造好了再一次性挂进去。
        let mut made = Vec::with_capacity(want - have);
        for _ in have..want {
            made.push(Arc::new((self.factory)().await?));
        }

        let mut instances = self.instances.write().expect("pool lock");
        instances.extend(made);
        let size = instances.len();
        drop(instances);

        self.high_water.fetch_max(size, Ordering::Relaxed);
        tracing::debug!(pool = %self.name, size, max = self.policy.max,
            "grew the instance pool");
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 一个假实例：只记自己是第几个造出来的。
    struct Fake(usize);

    fn counting_factory() -> (Factory<Fake>, Arc<AtomicUsize>) {
        let made = Arc::new(AtomicUsize::new(0));
        let seen = Arc::clone(&made);
        let f: Factory<Fake> = Arc::new(move || {
            let seen = Arc::clone(&seen);
            Box::pin(async move { Ok(Fake(seen.fetch_add(1, Ordering::SeqCst))) })
        });
        (f, made)
    }

    #[tokio::test]
    async fn a_pool_starts_with_exactly_one_instance() {
        // 从 1 起是策略的一半：绝大多数插件永远不会被并发调用。
        let (f, made) = counting_factory();
        let pool = InstancePool::new("t", f, PoolPolicy::default())
            .await
            .unwrap();
        assert_eq!(pool.size(), 1);
        assert_eq!(made.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn holding_an_instance_is_what_makes_the_pool_grow() {
        let (f, _) = counting_factory();
        let pool = InstancePool::new("t", f, PoolPolicy::default().with_max(8))
            .await
            .unwrap();

        // 握着不放 = 占用。池因此必须再长一个。
        let held = pool.pick().await;
        assert_eq!(pool.size(), 1);
        let second = pool.pick().await;
        assert_eq!(pool.size(), 2, "a busy pool must grow");

        // 指数：1 → 2 → 4。
        let third = pool.pick().await;
        assert_eq!(pool.size(), 4);

        // 松手之后不再长——空闲的实例会被重新挑到。
        drop(third);
        let _reused = pool.pick().await;
        assert_eq!(pool.size(), 4);
        drop(held);
        drop(second);
    }

    #[tokio::test]
    async fn growth_stops_at_the_ceiling() {
        let (f, _) = counting_factory();
        let pool = InstancePool::new("t", f, PoolPolicy::default().with_max(2))
            .await
            .unwrap();
        let _a = pool.pick().await;
        let _b = pool.pick().await;
        // 到顶了：第三个只能排队，不能再长。
        let _c = pool.pick().await;
        assert_eq!(pool.size(), 2);
    }

    #[tokio::test]
    async fn an_idle_pool_gives_instances_back_one_at_a_time() {
        let (f, _) = counting_factory();
        // idle_secs 会被 with_idle_secs 的 0 特判挡掉，所以这里直接构造。
        let policy = PoolPolicy {
            max: 8,
            idle_secs: 0,
        };
        let pool = InstancePool::new("t", f, policy).await.unwrap();

        {
            let _a = pool.pick().await;
            let _b = pool.pick().await;
            let _c = pool.pick().await;
        }
        assert_eq!(pool.size(), 4);

        // 线性：一次巡检收一个，不是一次收干净。
        assert_eq!(pool.sweep(), 1);
        assert_eq!(pool.size(), 3);
        assert_eq!(pool.sweep(), 1);
        assert_eq!(pool.size(), 2);
        assert_eq!(pool.sweep(), 1);
        assert_eq!(pool.size(), 1);
        // 永远留一个：池空了就没人能回答 list-tools 了。
        assert_eq!(pool.sweep(), 0);
        assert_eq!(pool.size(), 1);
        // 涨到过 4 这件事收回之后仍然看得见。
        assert_eq!(pool.high_water(), 4);
    }

    #[tokio::test]
    async fn a_pool_that_never_hit_contention_is_not_swept_early() {
        let (f, _) = counting_factory();
        let pool = InstancePool::new("t", f, PoolPolicy::default().with_max(4))
            .await
            .unwrap();
        let _a = pool.pick().await;
        let _b = pool.pick().await;
        assert_eq!(pool.size(), 2);
        // 默认 idle 是 600s，刚撞上过，所以这次巡检什么都不该收。
        assert_eq!(pool.sweep(), 0);
        assert_eq!(pool.size(), 2);
    }

    #[tokio::test]
    async fn an_instance_in_use_is_never_reclaimed() {
        let (f, _) = counting_factory();
        let policy = PoolPolicy {
            max: 4,
            idle_secs: 0,
        };
        let pool = InstancePool::new("t", f, policy).await.unwrap();
        let held_a = pool.pick().await;
        let held_b = pool.pick().await;
        assert_eq!(pool.size(), 2);
        // 两个都握在手里，巡检必须一个都不动——收走一个正在被调用的实例
        // 会把它的 wasm 内存连同调用一起拆掉。
        assert_eq!(pool.sweep(), 0);
        assert_eq!(pool.size(), 2);
        drop(held_a);
        assert_eq!(pool.sweep(), 1);
        drop(held_b);
    }

    #[tokio::test]
    async fn a_failing_factory_does_not_fail_the_call() {
        // 长不出来就排队，不能让调用方拿到错误——它要的只是「跑这个工具」。
        let attempts = Arc::new(AtomicUsize::new(0));
        let seen = Arc::clone(&attempts);
        let f: Factory<Fake> = Arc::new(move || {
            let seen = Arc::clone(&seen);
            Box::pin(async move {
                let n = seen.fetch_add(1, Ordering::SeqCst);
                if n == 0 {
                    Ok(Fake(0))
                } else {
                    Err(anyhow::anyhow!("out of memory"))
                }
            })
        });
        let pool = InstancePool::new("t", f, PoolPolicy::default().with_max(4))
            .await
            .unwrap();
        let _held = pool.pick().await;
        let queued = pool.pick().await;
        // 没长出新的，于是排到了同一个实例上。
        assert_eq!(pool.size(), 1);
        assert_eq!(queued.0, 0);
    }

    #[test]
    fn zero_in_the_config_means_follow_the_cpu_count() {
        assert_eq!(PoolPolicy::default().with_max(0).max, cpu_count());
        assert_eq!(PoolPolicy::default().with_max(3).max, 3);
        assert_eq!(
            PoolPolicy::default().with_idle_secs(0).idle_secs,
            DEFAULT_IDLE_SECS
        );
    }

    #[test]
    fn a_stateful_plugin_is_pinned_to_one_instance() {
        assert_eq!(PoolPolicy::single().max, 1);
    }
}
