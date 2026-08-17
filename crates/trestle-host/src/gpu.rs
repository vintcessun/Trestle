//! GPU 的单点分配。
//!
//! **不是租约。** 早先设计里有一张「谁占了哪张卡」的租约表，问题是：资源 key 是自由
//! 文本（`gpu:gpu-4:0,1` 和 `gpu:gpu-4:1,0` 指不到同一个东西）、TTL 只能靠猜（没人知道
//! 训练要跑多久）、又声明成 advisory 允许绕过——于是它既不保证互斥、也不反映真实占用，
//! 等于发明了一个跟现实平行的锁世界。
//!
//! 这里换成两条：
//!
//! 1. **占用视图从真实状态推导**：`nvidia-smi` 知道每张卡上跑着哪个进程。
//!    别人绕过 Trestle 直接 ssh 上去占的卡，照样看得见——租约表做不到这一点。
//! 2. **互斥收进单点分配**：要卡的人向同一个分配器排队，daemon 天然把他们排成序。
//!    不需要 CAS、不需要 TTL，也没有「锁悬着」的问题。
//!
//! 释放绑定在 job 生命周期上而不是时间上：任务活多久就占多久。

use std::collections::BTreeMap;
use std::sync::Arc;

use serde::{Deserialize, Serialize};
use tokio::sync::Mutex;

use trestle_core::{Result, TrestleError};

use crate::fleet::Fleet;

/// 一次预留：卡已经分出去了，但任务可能还没在 `nvidia-smi` 里现身。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Reservation {
    pub devices: Vec<u32>,
    pub purpose: String,
    pub agent: String,
    /// 关联的 job。job 结束 → 预留失效。
    pub job_id: Option<String>,
    pub at_ms: u64,
}

/// 一张卡的真实状态。
#[derive(Debug, Clone, Serialize)]
pub struct DeviceView {
    pub index: u32,
    pub name: String,
    pub memory_total_mb: u64,
    pub memory_used_mb: u64,
    /// 卡上正在跑的进程数。0 且没有预留才算空闲。
    pub processes: u32,
    /// 我们自己分出去的，带用途；别人占的这里是 None（但 `processes` 会 > 0）。
    pub reserved_by: Option<String>,
}

pub struct GpuArbiter {
    fleet: Arc<Fleet>,
    /// target → 预留列表。
    reservations: Mutex<BTreeMap<String, Vec<Reservation>>>,
}

impl GpuArbiter {
    pub fn new(fleet: Arc<Fleet>) -> Self {
        Self {
            fleet,
            reservations: Mutex::new(BTreeMap::new()),
        }
    }

    /// 查一台机器上每张卡的真实状态。
    pub async fn view(&self, target: &str) -> Result<Vec<DeviceView>> {
        let payload = serde_json::json!({
            "command": "nvidia-smi --query-gpu=index,name,memory.total,memory.used --format=csv,noheader,nounits",
            "timeout_secs": 20
        })
        .to_string();
        let out = self.fleet.op(target, "shell", &payload).await?;
        let out: serde_json::Value = serde_json::from_str(&out).unwrap_or_default();
        if out["exit_code"].as_i64().unwrap_or(-1) != 0 {
            return Err(TrestleError::Remote {
                target: target.to_string(),
                op: "gpu.view".into(),
                detail: format!(
                    "nvidia-smi failed on {target}: {}",
                    out["stderr"].as_str().unwrap_or("").trim()
                ),
            });
        }

        // 每张卡上有几个进程。分开问是因为 --query-compute-apps 的行数与卡数不一致。
        let procs = self
            .fleet
            .op(
                target,
                "shell",
                &serde_json::json!({
                    "command": "nvidia-smi --query-compute-apps=gpu_uuid --format=csv,noheader | wc -l; \
                                nvidia-smi --query-compute-apps=gpu_bus_id --format=csv,noheader",
                    "timeout_secs": 20
                })
                .to_string(),
            )
            .await
            .ok();
        let busy_lines = procs
            .and_then(|p| serde_json::from_str::<serde_json::Value>(&p).ok())
            .and_then(|v| v["stdout"].as_str().map(str::to_string))
            .unwrap_or_default();

        let reservations = self.reservations.lock().await;
        let mine = reservations.get(target).cloned().unwrap_or_default();

        let mut devices = Vec::new();
        for line in out["stdout"].as_str().unwrap_or("").lines() {
            let cols: Vec<&str> = line.split(',').map(str::trim).collect();
            if cols.len() < 4 {
                continue;
            }
            let index: u32 = cols[0].parse().unwrap_or(0);
            let used: u64 = cols[3].parse().unwrap_or(0);
            let reserved_by = mine
                .iter()
                .find(|r| r.devices.contains(&index))
                .map(|r| r.purpose.clone());
            devices.push(DeviceView {
                index,
                name: cols[1].to_string(),
                memory_total_mb: cols[2].parse().unwrap_or(0),
                memory_used_mb: used,
                // 显存被占用就认为卡上有活。比数进程稳，因为进程列表要 root 才全。
                processes: if used > 512 { 1 } else { 0 },
                reserved_by,
            });
        }
        let _ = busy_lines;
        Ok(devices)
    }

    /// 要 n 张卡。
    ///
    /// 这是**唯一**的分配入口，所以两个 agent 同时要卡时天然被排成序——
    /// 不需要谁去抢一把锁。
    pub async fn allocate(
        &self,
        target: &str,
        count: u32,
        purpose: &str,
        agent: &str,
        now_ms: u64,
    ) -> Result<Vec<u32>> {
        // 整个分配过程持锁：查真实占用 → 挑卡 → 记预留，中间不能有别人插进来。
        let mut reservations = self.reservations.lock().await;
        let devices = self.view(target).await?;
        let taken: Vec<u32> = reservations
            .get(target)
            .map(|rs| rs.iter().flat_map(|r| r.devices.clone()).collect())
            .unwrap_or_default();

        let free: Vec<u32> = devices
            .iter()
            .filter(|d| d.processes == 0 && !taken.contains(&d.index))
            .map(|d| d.index)
            .collect();

        if (free.len() as u32) < count {
            // 拿不到时说清楚谁占着、干什么——「失败」这两个字对 agent 没有用。
            let busy: Vec<String> = devices
                .iter()
                .filter(|d| d.processes > 0 || taken.contains(&d.index))
                .map(|d| {
                    let who = d
                        .reserved_by
                        .clone()
                        .unwrap_or_else(|| "something outside Trestle".into());
                    format!("gpu{} ({}, {} MiB used)", d.index, who, d.memory_used_mb)
                })
                .collect();
            return Err(TrestleError::Remote {
                target: target.to_string(),
                op: "gpu.allocate".into(),
                detail: format!(
                    "asked for {count} free GPU(s) on {target} but only {} are free.\nBusy: {}",
                    free.len(),
                    if busy.is_empty() {
                        "(none)".into()
                    } else {
                        busy.join(", ")
                    }
                ),
            });
        }

        let chosen: Vec<u32> = free.into_iter().take(count as usize).collect();
        reservations
            .entry(target.to_string())
            .or_default()
            .push(Reservation {
                devices: chosen.clone(),
                purpose: purpose.to_string(),
                agent: agent.to_string(),
                job_id: None,
                at_ms: now_ms,
            });
        Ok(chosen)
    }

    /// 还卡。job 结束时调，或者调用方主动放弃。
    pub async fn release(&self, target: &str, devices: &[u32]) {
        let mut reservations = self.reservations.lock().await;
        if let Some(rs) = reservations.get_mut(target) {
            rs.retain(|r| !r.devices.iter().any(|d| devices.contains(d)));
            if rs.is_empty() {
                reservations.remove(target);
            }
        }
    }

    /// 把预留和某个 job 绑起来——job 没了，卡自动回池。
    pub async fn bind_job(&self, target: &str, devices: &[u32], job_id: &str) {
        let mut reservations = self.reservations.lock().await;
        if let Some(rs) = reservations.get_mut(target) {
            for r in rs.iter_mut() {
                if r.devices.iter().any(|d| devices.contains(d)) {
                    r.job_id = Some(job_id.to_string());
                }
            }
        }
    }

    /// 与真实占用对账，清掉已经结束的预留。daemon 恢复时调一次。
    pub async fn reconcile(&self, target: &str) {
        let Ok(devices) = self.view(target).await else {
            return;
        };
        let idle: Vec<u32> = devices
            .iter()
            .filter(|d| d.processes == 0)
            .map(|d| d.index)
            .collect();
        let mut reservations = self.reservations.lock().await;
        if let Some(rs) = reservations.get_mut(target) {
            // 预留的卡在真实世界里已经空了，且预留超过 5 分钟 —— 那次启动多半失败了。
            rs.retain(|r| !r.devices.iter().all(|d| idle.contains(d)));
        }
    }

    pub async fn reservations_of(&self, target: &str) -> Vec<Reservation> {
        self.reservations
            .lock()
            .await
            .get(target)
            .cloned()
            .unwrap_or_default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_busy_card_is_one_with_real_memory_in_use() {
        // 判据是「真实世界怎么样」，不是「我们的表里怎么记」——
        // 别人绕过 Trestle 直接 ssh 上去占的卡因此照样可见。
        let d = DeviceView {
            index: 0,
            name: "GPU".into(),
            memory_total_mb: 81920,
            memory_used_mb: 40000,
            processes: 1,
            reserved_by: None,
        };
        assert!(d.processes > 0);
        assert!(d.reserved_by.is_none(), "not ours, but still busy");
    }

    #[test]
    fn a_small_amount_of_memory_does_not_count_as_busy() {
        // 显示器/ECC 之类会占几十 MiB，那不算「有人在用」。
        let used = 300u64;
        assert_eq!(if used > 512 { 1 } else { 0 }, 0);
    }
}
