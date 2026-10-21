//! 通用的资源仲裁：**同一份资源谁在用，由同一个点说了算**。
//!
//! 这里没有一个字是关于 GPU 的。GPU 只是第一个用它的资源，实现在 `gpu` 插件里；
//! 下一个是许可证席位、独占的盘、还是一台机器上的某个端口，host 都不需要知道。
//!
//! ## 为什么不是租约
//!
//! 早先设计里有一张「谁占了哪张卡」的租约表，问题是：资源 key 是自由文本
//! （`gpu:gpu-4:0,1` 和 `gpu:gpu-4:1,0` 指不到同一个东西）、TTL 只能靠猜（没人知道
//! 训练要跑多久）、又声明成 advisory 允许绕过——于是它既不保证互斥、也不反映真实
//! 占用，等于发明了一个跟现实平行的锁世界。
//!
//! 换成两条：
//!
//! 1. **占用视图从真实状态推导。** 别人绕过 Trestle 直接 ssh 上去占的卡照样看得见，
//!    因为判据是插件刚查到的快照，不是我们自己的表。
//! 2. **互斥收进单点分配。** 要资源的人向同一个仲裁者排队，daemon 天然把他们排成序。
//!    不需要 CAS、不需要 TTL，也没有「锁悬着」的问题。
//!
//! ## 一条不能破的规矩：仲裁者不查真实世界
//!
//! [`Arbiter::acquire`] **不做任何 I/O**。快照由调用方（插件）查好递进来，
//! 我们只在锁里挑、记账。
//!
//! 这不是洁癖，是上一版的墓志铭：上一版的 `allocate()` 先取 `reservations` 锁，
//! 再在锁里调 `view()` 去问 `nvidia-smi`，而 `view()` 会再取一次同一把锁。
//! tokio 的 `Mutex` 不可重入，所以**第一次真的要卡就会永久挂死**。它没被发现，
//! 是因为从来没有测试真的走过那条路。把 I/O 挪出去之后，这一整类问题不存在了。

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicU64, Ordering};

use serde::{Deserialize, Serialize};
use tokio::sync::Mutex;

use trestle_core::{Result, TrestleError};

/// 一次占用在真实世界里都空了多久之后算作废。
///
/// 用途是收尸：任务起失败了、进程被人 kill 了，卡早就闲着但账还记着。
/// 给足够长的宽限是因为「刚分到卡、还在装环境」的窗口是真实存在的。
pub const STALE_MS: u64 = 5 * 60 * 1000;

/// 池里的一个单位，由插件查真实世界得来。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Unit {
    pub id: String,
    /// 外面有人在用（不是我们分出去的）。
    #[serde(default)]
    pub busy: bool,
    /// 拿不到时错误里会原样带上它。「gpu0 (40000 MiB used)」比「失败」有用得多。
    #[serde(default)]
    pub label: String,
}

/// 一次占用。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Claim {
    pub id: String,
    pub pool: String,
    pub units: Vec<String>,
    pub purpose: String,
    pub agent: String,
    /// 关联的 job。job 结束 → 占用失效。这才是正确的过期方式。
    pub job_id: Option<String>,
    pub at_ms: u64,
}

pub struct Arbiter {
    /// pool → 这个池上的占用。
    claims: Mutex<BTreeMap<String, Vec<Claim>>>,
    next: AtomicU64,
    /// 这个仲裁者的「纪元」，拼进每个 claim 编号里。
    ///
    /// 占用账本只在内存里，daemon 一重启就清空、编号从头数；但 job 插件把 claim
    /// 编号存在**持久化**的 KV 里。没有纪元的话，重启前那个任务结束时去还 `c1`，
    /// 还掉的是重启后别人刚拿到的 `c1`。
    epoch: u64,
}

impl Default for Arbiter {
    fn default() -> Self {
        Self {
            claims: Mutex::default(),
            next: AtomicU64::default(),
            epoch: std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_millis() as u64)
                .unwrap_or_default(),
        }
    }
}

impl Arbiter {
    pub fn new() -> Self {
        Self::default()
    }

    /// 挑 `want` 个既没人用、也没被别人预留的单位。
    ///
    /// **不做 I/O。** `snapshot` 是调用方刚查到的真实世界。整个过程在一把锁里，
    /// 所以两个 agent 同时要资源时天然被排成序——不需要谁去抢一把锁。
    #[allow(clippy::too_many_arguments)]
    pub async fn acquire(
        &self,
        pool: &str,
        snapshot: &[Unit],
        want: u32,
        purpose: &str,
        agent: &str,
        now_ms: u64,
    ) -> Result<Claim> {
        let mut all = self.claims.lock().await;
        let mine = all.entry(pool.to_string()).or_default();

        // 收尸：账上记着、真实世界里却全空、而且已经过了宽限期——那次启动多半失败了。
        // 放在这里做是因为**恰好此刻有一份新鲜快照**，而且恰好此刻有人在等资源。
        // 单独一个 reconcile 入口反而要自己去查真实世界，那正是不该做的事。
        let idle: Vec<&str> = snapshot
            .iter()
            .filter(|u| !u.busy)
            .map(|u| u.id.as_str())
            .collect();
        //
        // **绑了 job 的不收。** 它的生命周期由 job 管（结束/被停就还），而「卡上暂时
        // 没活」对一个活着的任务再正常不过——装环境、下数据、tokenize 都可能一跑
        // 半小时不碰卡。按空闲收它，就是把一个活任务的卡分给下一个人。
        mine.retain(|c| {
            let all_idle = c.units.iter().all(|u| idle.contains(&u.as_str()));
            c.job_id.is_some() || !(all_idle && now_ms.saturating_sub(c.at_ms) > STALE_MS)
        });

        let taken: Vec<&str> = mine
            .iter()
            .flat_map(|c| c.units.iter().map(String::as_str))
            .collect();
        let free: Vec<&Unit> = snapshot
            .iter()
            .filter(|u| !u.busy && !taken.contains(&u.id.as_str()))
            .collect();

        if (free.len() as u32) < want {
            // 拿不到时说清楚谁占着、干什么——「失败」这两个字对 agent 没有用。
            let busy: Vec<String> = snapshot
                .iter()
                .filter(|u| u.busy || taken.contains(&u.id.as_str()))
                .map(|u| {
                    let who = mine
                        .iter()
                        .find(|c| c.units.contains(&u.id))
                        .map(|c| c.purpose.clone())
                        .unwrap_or_else(|| "something outside Trestle".into());
                    if u.label.is_empty() {
                        format!("{} ({who})", u.id)
                    } else {
                        format!("{} ({who}; {})", u.id, u.label)
                    }
                })
                .collect();
            let (where_, what) = split_pool(pool);
            return Err(TrestleError::Remote {
                target: where_.to_string(),
                op: format!("arbiter.acquire {what}"),
                detail: format!(
                    "asked for {want} free {what} on {where_} but only {} are free.\nBusy: {}",
                    free.len(),
                    if busy.is_empty() {
                        "(none)".into()
                    } else {
                        busy.join(", ")
                    }
                ),
            });
        }

        let claim = Claim {
            id: format!(
                "c{}-{}",
                self.epoch,
                self.next.fetch_add(1, Ordering::Relaxed) + 1
            ),
            pool: pool.to_string(),
            units: free
                .into_iter()
                .take(want as usize)
                .map(|u| u.id.clone())
                .collect(),
            purpose: purpose.to_string(),
            agent: agent.to_string(),
            job_id: None,
            at_ms: now_ms,
        };
        mine.push(claim.clone());
        Ok(claim)
    }

    /// 还回去。
    pub async fn release(&self, claim_id: &str) {
        let mut all = self.claims.lock().await;
        for claims in all.values_mut() {
            claims.retain(|c| c.id != claim_id);
        }
        all.retain(|_, v| !v.is_empty());
    }

    /// 把一次占用绑到某个 job 上。
    pub async fn bind_job(&self, claim_id: &str, job_id: &str) {
        let mut all = self.claims.lock().await;
        for claims in all.values_mut() {
            for c in claims.iter_mut().filter(|c| c.id == claim_id) {
                c.job_id = Some(job_id.to_string());
            }
        }
    }

    /// 某个 job 的全部占用一次还清——只还 `allowed(池名)` 为真的那些池里的。
    ///
    /// 过滤条件由调用方给，是因为「谁能还哪种资源」是 capability 的事，
    /// 不该让一个只能仲裁 gpu 的插件顺手把同一个 job 的别的占用也还掉。
    pub async fn release_job(&self, job_id: &str, allowed: impl Fn(&str) -> bool) {
        let mut all = self.claims.lock().await;
        for (pool, claims) in all.iter_mut() {
            if allowed(pool) {
                claims.retain(|c| c.job_id.as_deref() != Some(job_id));
            }
        }
        all.retain(|_, v| !v.is_empty());
    }

    /// 一次占用在哪个池里。权限检查要按池的资源种类比对。
    pub async fn pool_of_claim(&self, claim_id: &str) -> Option<String> {
        self.claims
            .lock()
            .await
            .iter()
            .find(|(_, cs)| cs.iter().any(|c| c.id == claim_id))
            .map(|(pool, _)| pool.clone())
    }

    pub async fn claims_of(&self, pool: &str) -> Vec<Claim> {
        self.claims
            .lock()
            .await
            .get(pool)
            .cloned()
            .unwrap_or_default()
    }

    /// 全部占用，按池分组。Web UI 与 `trestle agents` 用它。
    pub async fn all(&self) -> BTreeMap<String, Vec<Claim>> {
        self.claims.lock().await.clone()
    }
}

/// `gpu-4/gpu` → `("gpu-4", "gpu")`。没有斜杠就整串当资源名。
fn split_pool(pool: &str) -> (&str, &str) {
    match pool.split_once('/') {
        Some((where_, what)) => (where_, what),
        None => ("", pool),
    }
}

/// 池名里的资源种类，capability 按它比对。
pub fn pool_kind(pool: &str) -> &str {
    split_pool(pool).1
}

#[cfg(test)]
mod tests {
    use super::*;

    fn units(spec: &[(&str, bool)]) -> Vec<Unit> {
        spec.iter()
            .map(|(id, busy)| Unit {
                id: (*id).into(),
                busy: *busy,
                label: format!("{} MiB used", if *busy { 40000 } else { 12 }),
            })
            .collect()
    }

    #[tokio::test]
    async fn two_callers_never_get_the_same_unit() {
        // 这是整个模块存在的理由。两个 agent 拿着**同一份**快照来要卡——
        // 快照里四张全空，但第二个人必须看见第一个人已经拿走的两张。
        let a = Arbiter::new();
        let snap = units(&[("0", false), ("1", false), ("2", false), ("3", false)]);

        let first = a
            .acquire("gpu-4/gpu", &snap, 2, "job one", "agent-a", 0)
            .await
            .unwrap();
        let second = a
            .acquire("gpu-4/gpu", &snap, 2, "job two", "agent-b", 0)
            .await
            .unwrap();

        assert_eq!(first.units.len(), 2);
        assert_eq!(second.units.len(), 2);
        for u in &first.units {
            assert!(!second.units.contains(u), "both got {u}");
        }
    }

    #[tokio::test]
    async fn a_unit_someone_else_is_using_is_not_available() {
        // 别人绕过 Trestle 直接 ssh 上去占的卡——判据是真实世界，不是我们的表。
        let a = Arbiter::new();
        let snap = units(&[("0", true), ("1", true), ("2", false)]);
        let claim = a
            .acquire("gpu-4/gpu", &snap, 1, "job", "agent", 0)
            .await
            .unwrap();
        assert_eq!(claim.units, ["2"]);
    }

    #[tokio::test]
    async fn not_enough_free_says_who_has_them_and_why() {
        // 「失败」这两个字对 agent 没有用：它要知道下一步该等谁。
        let a = Arbiter::new();
        let snap = units(&[("0", false), ("1", true), ("2", false)]);
        a.acquire("gpu-4/gpu", &snap, 2, "training run", "agent-a", 0)
            .await
            .unwrap();

        let err = a
            .acquire("gpu-4/gpu", &snap, 2, "another run", "agent-b", 0)
            .await
            .unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("gpu-4"), "{msg}");
        // 我们自己占的，说得出用途
        assert!(msg.contains("training run"), "{msg}");
        // 别人占的，说得出是外面的
        assert!(msg.contains("outside Trestle"), "{msg}");
        // label 原样带出来
        assert!(msg.contains("MiB used"), "{msg}");
    }

    #[tokio::test]
    async fn releasing_puts_the_units_back() {
        let a = Arbiter::new();
        let snap = units(&[("0", false), ("1", false)]);
        let claim = a
            .acquire("gpu-4/gpu", &snap, 2, "job", "agent", 0)
            .await
            .unwrap();
        assert!(
            a.acquire("gpu-4/gpu", &snap, 1, "next", "agent", 0)
                .await
                .is_err()
        );

        a.release(&claim.id).await;
        assert!(
            a.acquire("gpu-4/gpu", &snap, 2, "next", "agent", 0)
                .await
                .is_ok()
        );
    }

    #[tokio::test]
    async fn a_jobs_units_come_back_when_the_job_does() {
        // 释放绑在 job 生命周期上，不绑在时间上。
        let a = Arbiter::new();
        let snap = units(&[("0", false), ("1", false)]);
        let claim = a
            .acquire("gpu-4/gpu", &snap, 2, "job", "agent", 0)
            .await
            .unwrap();
        a.bind_job(&claim.id, "train-1").await;

        a.release_job("train-2", |_| true).await;
        assert_eq!(
            a.claims_of("gpu-4/gpu").await.len(),
            1,
            "wrong job released"
        );
        // 不准碰的池不还。
        a.release_job("train-1", |pool| pool_kind(pool) != "gpu")
            .await;
        assert_eq!(a.claims_of("gpu-4/gpu").await.len(), 1);
        a.release_job("train-1", |_| true).await;
        assert!(a.claims_of("gpu-4/gpu").await.is_empty());
    }

    #[tokio::test]
    async fn a_job_bound_claim_is_never_collected_while_idle() {
        // 任务还在装环境/下数据，卡上没活——那也是它的卡。只有 job 结束才还。
        let a = Arbiter::new();
        let snap = units(&[("0", false), ("1", false)]);
        let bound = a
            .acquire("gpu-4/gpu", &snap, 1, "job preparing data", "agent", 0)
            .await
            .unwrap();
        a.bind_job(&bound.id, "train-1").await;
        a.acquire("gpu-4/gpu", &snap, 1, "unbound, start failed", "agent", 0)
            .await
            .unwrap();

        let err = a
            .acquire("gpu-4/gpu", &snap, 2, "next", "agent", STALE_MS * 100)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("job preparing data"), "{err}");
        // 没绑 job 的那次照样被收掉，所以要一张还是拿得到。
        let ok = a
            .acquire("gpu-4/gpu", &snap, 1, "next", "agent", STALE_MS * 100)
            .await;
        assert!(ok.is_ok(), "{:?}", ok.err().map(|e| e.to_string()));
    }

    #[tokio::test]
    async fn the_pool_of_a_claim_can_be_looked_up() {
        let a = Arbiter::new();
        let snap = units(&[("0", false)]);
        let c = a
            .acquire("gpu-4/gpu", &snap, 1, "job", "agent", 0)
            .await
            .unwrap();
        assert_eq!(a.pool_of_claim(&c.id).await.as_deref(), Some("gpu-4/gpu"));
        assert_eq!(a.pool_of_claim("c999").await, None);
    }

    #[tokio::test]
    async fn claim_ids_do_not_repeat_across_restarts() {
        // 重启前存下的 claim 编号，不能撞上重启后发出去的那个。
        let before = Arbiter::new();
        let snap = units(&[("0", false)]);
        let old = before
            .acquire("gpu-4/gpu", &snap, 1, "job", "agent", 0)
            .await
            .unwrap();
        std::thread::sleep(std::time::Duration::from_millis(2));
        let after = Arbiter::new();
        let new = after
            .acquire("gpu-4/gpu", &snap, 1, "job", "agent", 0)
            .await
            .unwrap();
        assert_ne!(old.id, new.id);
    }

    #[tokio::test]
    async fn a_claim_whose_units_went_idle_long_ago_is_collected() {
        // 收尸：任务起失败了，卡早就闲着但账还记着。下一个人来要卡时顺手清掉——
        // 那一刻恰好有一份新鲜快照，也恰好有人在等。
        let a = Arbiter::new();
        let snap = units(&[("0", false), ("1", false)]);
        a.acquire("gpu-4/gpu", &snap, 2, "job that died", "agent", 0)
            .await
            .unwrap();

        // 还在宽限期内：不动它。刚分到卡、还在装环境的窗口是真实存在的。
        assert!(
            a.acquire("gpu-4/gpu", &snap, 1, "next", "agent", STALE_MS)
                .await
                .is_err()
        );
        // 过了宽限期：收掉。
        let ok = a
            .acquire("gpu-4/gpu", &snap, 2, "next", "agent", STALE_MS + 1)
            .await;
        assert!(ok.is_ok(), "{:?}", ok.err().map(|e| e.to_string()));
        assert_eq!(
            a.claims_of("gpu-4/gpu").await.len(),
            1,
            "the dead claim should be gone"
        );
    }

    #[tokio::test]
    async fn a_busy_claim_is_never_collected_however_old() {
        // 卡上真的有活 = 任务还在跑。跑三天也不该被收。
        let a = Arbiter::new();
        let claim_snap = units(&[("0", false), ("1", false)]);
        a.acquire("gpu-4/gpu", &claim_snap, 2, "long training", "agent", 0)
            .await
            .unwrap();

        let now_busy = units(&[("0", true), ("1", true)]);
        let err = a
            .acquire("gpu-4/gpu", &now_busy, 1, "next", "agent", STALE_MS * 100)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("long training"), "{err}");
    }

    #[tokio::test]
    async fn pools_do_not_see_each_other() {
        let a = Arbiter::new();
        let snap = units(&[("0", false)]);
        a.acquire("gpu-4/gpu", &snap, 1, "job", "agent", 0)
            .await
            .unwrap();
        // 另一台机器的同名单位是另一个池。
        assert!(
            a.acquire("gpu-1/gpu", &snap, 1, "job", "agent", 0)
                .await
                .is_ok()
        );
    }

    #[test]
    fn a_pool_name_says_where_and_what() {
        assert_eq!(pool_kind("gpu-4/gpu"), "gpu");
        assert_eq!(pool_kind("gpu"), "gpu");
        assert_eq!(split_pool("gpu-4/gpu"), ("gpu-4", "gpu"));
    }
}
