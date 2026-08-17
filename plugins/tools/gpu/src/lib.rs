//! `gpu`：GPU 仲裁。
//!
//! host 那边的 `arbiter` 里**没有一个字是关于 GPU 的**——它只会「在一把锁里挑几个
//! 单位出来并记账」。所有关于显卡的知识都在这个插件里：
//!
//!   * 怎么问一台机器有几张卡、每张卡上有没有活（`nvidia-smi`）
//!   * 多少显存算「有人在用」
//!   * 怎么把一堆机器按空闲卡数排出来
//!
//! 分工的理由是分工的位置：**互斥必须在一个点上**，而**真实世界只有插件知道怎么问**。
//! 所以插件查、host 挑。插件把刚查到的快照连同申请一起递进去，host 在锁里把它和
//! 自己的账对一遍——两个 agent 同时要卡，谁也拿不到同一张。
//!
//! 上一版把这两件事混在 host 里，结果是：分配时先取锁、再在锁里去查 `nvidia-smi`，
//! 而那条查询路径会再取一次同一把锁，于是第一次真的要卡就永久挂死。
//! 现在 host 侧那个函数一行 I/O 都没有，这类问题不可能再发生。

#[allow(warnings)]
mod bindings {
    wit_bindgen::generate!({
        path: "../../../wit",
        world: "tool-plugin",
    });
}

use bindings::Guest;
use bindings::trestle::plugin::arbiter;
use bindings::trestle::plugin::base;
use bindings::trestle::plugin::host_services as host;
use bindings::trestle::plugin::types::{Error, ErrorKind};

/// 显存占用超过这个数就认为卡上有活。
///
/// 判据用显存而不是进程列表：`--query-compute-apps` 要 root 才看得全，
/// 而显示器/ECC 之类本来就会占几十 MiB。
const BUSY_MIB: u64 = 512;

fn err(kind: ErrorKind, detail: impl Into<String>, remedy: impl Into<String>) -> Error {
    Error {
        kind,
        detail: detail.into(),
        remedy: remedy.into(),
    }
}

fn bad(detail: impl Into<String>) -> Error {
    err(ErrorKind::InvalidRequest, detail, "")
}

fn need_target(v: &serde_json::Value) -> Result<String, Error> {
    // 没有默认机。默认机会制造「你以为在 gpu-4 上抢卡、其实在 gpu-1」这类静默事故。
    v.get("target")
        .and_then(|t| t.as_str())
        .filter(|t| !t.is_empty())
        .map(str::to_string)
        .ok_or_else(|| bad("this tool needs a `target`; there is no default machine"))
}

fn pool_of(target: &str) -> String {
    format!("{target}/gpu")
}

/// 一张卡查出来的样子。
struct Card {
    index: u32,
    name: String,
    total_mib: u64,
    used_mib: u64,
}

impl Card {
    fn busy(&self) -> bool {
        self.used_mib > BUSY_MIB
    }
}

/// 问显卡状态的那条命令。**这是这个插件存在的理由**——host 不该知道 nvidia-smi。
fn scan_payload() -> String {
    serde_json::json!({
        "command": "nvidia-smi --query-gpu=index,name,memory.total,memory.used \
                    --format=csv,noheader,nounits",
        "timeout_secs": 20
    })
    .to_string()
}

/// 问一台机器的显卡状态。
fn scan(target: &str) -> Result<Vec<Card>, Error> {
    parse_cards(target, &base::call(target, "shell", &scan_payload())?)
}

/// 把 `shell` 的返回翻成卡的清单。
fn parse_cards(target: &str, raw: &str) -> Result<Vec<Card>, Error> {
    let out: serde_json::Value = serde_json::from_str(raw).unwrap_or_default();
    if out["exit_code"].as_i64().unwrap_or(-1) != 0 {
        return Err(err(
            ErrorKind::Unreachable,
            format!(
                "nvidia-smi failed on {target}: {}",
                out["stderr"].as_str().unwrap_or("").trim()
            ),
            "这台机器上有 NVIDIA 驱动吗？`base_shell` 跑一次 nvidia-smi 看看",
        ));
    }

    let mut cards = Vec::new();
    for line in out["stdout"].as_str().unwrap_or("").lines() {
        let cols: Vec<&str> = line.split(',').map(str::trim).collect();
        if cols.len() < 4 {
            continue;
        }
        cards.push(Card {
            index: cols[0].parse().unwrap_or(0),
            name: cols[1].to_string(),
            total_mib: cols[2].parse().unwrap_or(0),
            used_mib: cols[3].parse().unwrap_or(0),
        });
    }
    Ok(cards)
}

/// 把扫描结果翻成仲裁者认识的样子。`label` 会原样出现在「卡不够」的错误里。
fn snapshot(cards: &[Card]) -> String {
    let units: Vec<serde_json::Value> = cards
        .iter()
        .map(|c| {
            serde_json::json!({
                "id": c.index.to_string(),
                "busy": c.busy(),
                "label": format!("{}, {} MiB used", c.name, c.used_mib),
            })
        })
        .collect();
    serde_json::to_string(&units).unwrap_or_else(|_| "[]".into())
}

/// 这台机器上哪些卡是**我们自己**分出去的。
///
/// 它们在 `nvidia-smi` 里可能还没现身（任务刚起、还在装环境），但已经名花有主，
/// 所以「空闲」必须把它们排除掉——否则 gpu_find 会推荐一张马上就要被占的卡。
fn held_units(target: &str) -> Vec<String> {
    let claims: Vec<serde_json::Value> =
        serde_json::from_str(&arbiter::claims(&pool_of(target))).unwrap_or_default();
    claims
        .iter()
        .filter_map(|c| c["units"].as_array())
        .flatten()
        .filter_map(|u| u.as_str().map(str::to_string))
        .collect()
}

/// 一台机器的完整占用视图：真实状态 + 我们自己发出去的占用。
fn status_of(target: &str) -> Result<serde_json::Value, Error> {
    let cards = scan(target)?;
    let claims: Vec<serde_json::Value> =
        serde_json::from_str(&arbiter::claims(&pool_of(target))).unwrap_or_default();

    let devices: Vec<serde_json::Value> = cards
        .iter()
        .map(|c| {
            let id = c.index.to_string();
            let held = claims.iter().find(|cl| {
                cl["units"]
                    .as_array()
                    .map(|u| u.iter().any(|x| x.as_str() == Some(id.as_str())))
                    .unwrap_or(false)
            });
            serde_json::json!({
                "index": c.index,
                "name": c.name,
                "memory_total_mb": c.total_mib,
                "memory_used_mb": c.used_mib,
                "busy": c.busy(),
                // 我们自己分出去的说得出用途；别人占的这里是 null，但 busy 仍是 true。
                "held_by": held.map(|h| h["purpose"].clone()),
                "claim": held.map(|h| h["id"].clone()),
            })
        })
        .collect();

    let free = devices.iter().filter(|d| d["busy"] == false).count();
    Ok(serde_json::json!({
        "target": target,
        "gpus": devices,
        "free_count": free,
        "total": cards.len(),
    }))
}

struct Component;

impl Guest for Component {
    fn list_tools() -> String {
        serde_json::json!([
            {
                "name": "gpu_status",
                "description": "一台机器上每张卡的真实状态，以及哪几张是 Trestle 分出去的。",
                "input_schema": {
                    "type": "object",
                    "required": ["target"],
                    "properties": {"target": {"type": "string", "description": "机器名。必填——没有默认机。"}}
                }
            },
            {
                "name": "gpu_find",
                "description": "按空闲卡数把机器排出来，空得多的在前。挑机器跑训练之前先问它。",
                "input_schema": {
                    "type": "object",
                    "properties": {
                        "need": {"type": "integer", "description": "要几张，默认 1"},
                        "only_targets": {
                            "type": "array", "items": {"type": "string"},
                            "description": "只看这几台。不给就看全部。这是过滤条件，不是操作对象。"
                        }
                    }
                }
            },
            {
                "name": "gpu_acquire",
                "description": "要 N 张空闲卡，拿到就是独占的。两个 agent 同时要卡会被排成序，\
                                拿不到时错误里说清楚谁占着、干什么。用完请 gpu_release；\
                                跑 job 的话用 job_start 的 gpus=\"auto:N\"，它会替你绑好生命周期。",
                "input_schema": {
                    "type": "object",
                    "required": ["target", "count"],
                    "properties": {
                        "target": {"type": "string", "description": "机器名。必填——没有默认机。"},
                        "count": {"type": "integer"},
                        "purpose": {"type": "string", "description": "拿来干什么。会出现在别人的「卡不够」错误里。"}
                    }
                }
            },
            {
                "name": "gpu_release",
                "description": "把 gpu_acquire 拿到的卡还回去。",
                "input_schema": {
                    "type": "object",
                    "required": ["claim"],
                    "properties": {"claim": {"type": "string"}}
                }
            }
        ])
        .to_string()
    }

    fn call(tool: String, args: String) -> Result<String, Error> {
        let v: serde_json::Value = serde_json::from_str(&args).unwrap_or_default();
        match tool.as_str() {
            "gpu_status" => {
                let target = need_target(&v)?;
                Ok(status_of(&target)?.to_string())
            }

            "gpu_find" => {
                let need = v.get("need").and_then(|n| n.as_u64()).unwrap_or(1) as usize;
                let only: Vec<String> = v
                    .get("only_targets")
                    .and_then(|t| t.as_array())
                    .map(|a| {
                        a.iter()
                            .filter_map(|x| x.as_str().map(str::to_string))
                            .collect()
                    })
                    .unwrap_or_default();

                let all = host::targets();
                let names: Vec<String> = all
                    .iter()
                    .map(|t| t.name.clone())
                    .filter(|n| only.is_empty() || only.contains(n))
                    .collect();

                // **一次全发出去，host 并发。** 顺序问整支机队就是六倍延迟，
                // 而这里每一台都要一次 SSH 往返——gpu-1 经代理冷启动就是好几秒。
                let results = base::call_many(&names, "shell", &scan_payload());

                let mut rows = Vec::new();
                for (name, r) in names.iter().zip(results.iter()) {
                    // 一台没有卡（或者根本连不上）不该让整个查询失败——
                    // 「哪台有空卡」这个问题，答不上来的那台跳过就是了。
                    let Ok(raw) = r else { continue };
                    let Ok(cards) = parse_cards(name, raw) else { continue };
                    let held = held_units(name);
                    let free: Vec<u64> = cards
                        .iter()
                        .filter(|c| !c.busy() && !held.contains(&c.index.to_string()))
                        .map(|c| c.index as u64)
                        .collect();
                    rows.push(serde_json::json!({
                        "target": name,
                        "free": free,
                        "free_count": free.len(),
                        "enough": free.len() >= need,
                        "total": cards.len(),
                    }));
                }
                // 空卡多的排前面，agent 一眼就知道该去哪台。
                rows.sort_by_key(|r| std::cmp::Reverse(r["free_count"].as_u64().unwrap_or(0)));
                Ok(serde_json::to_string(&rows).unwrap_or_default())
            }

            "gpu_acquire" => {
                let target = need_target(&v)?;
                let count = v.get("count").and_then(|c| c.as_u64()).unwrap_or(0) as u32;
                if count == 0 {
                    return Err(bad("gpu_acquire needs a `count` of at least 1"));
                }
                let purpose = v
                    .get("purpose")
                    .and_then(|p| p.as_str())
                    .unwrap_or("unspecified")
                    .to_string();

                // 查真实世界 → 递给仲裁者。挑哪几张、以及互斥，都在 host 那一把锁里。
                let cards = scan(&target)?;
                let got = arbiter::acquire(&pool_of(&target), &snapshot(&cards), count, &purpose)?;
                let got: serde_json::Value = serde_json::from_str(&got).unwrap_or_default();
                let devices: Vec<u64> = got["units"]
                    .as_array()
                    .map(|u| {
                        u.iter()
                            .filter_map(|x| x.as_str())
                            .filter_map(|s| s.parse().ok())
                            .collect()
                    })
                    .unwrap_or_default();
                Ok(serde_json::json!({
                    "claim": got["claim"],
                    "target": target,
                    "devices": devices,
                    // 直接给出能用的那一行，省得调用方自己拼。
                    "cuda_visible_devices": devices
                        .iter().map(u64::to_string).collect::<Vec<_>>().join(","),
                })
                .to_string())
            }

            "gpu_release" => {
                let claim = v
                    .get("claim")
                    .and_then(|c| c.as_str())
                    .ok_or_else(|| bad("gpu_release needs the `claim` that gpu_acquire returned"))?;
                arbiter::release(claim);
                Ok(serde_json::json!({"released": claim}).to_string())
            }

            other => Err(err(
                ErrorKind::NotFound,
                format!("unknown tool '{other}'"),
                "gpu_status, gpu_find, gpu_acquire, gpu_release",
            )),
        }
    }

    fn on_tick(_name: String, _payload: String) {}

    fn config_schema() -> String {
        serde_json::json!({
            "type": "object",
            "properties": {},
            "description": "没有需要配置的东西。哪些机器有卡由 targets 决定。"
        })
        .to_string()
    }

    fn ui_panel() -> String {
        // 这块面板从 fleet 搬过来：显卡是谁的资源，它的界面就该跟着谁走。
        r#"
<section class="panel">
  <h2>GPU</h2>
  <div id="gpu-rows"><p class="empty">读取中…</p></div>
</section>
<script>
(async () => {
  const box = document.getElementById("gpu-rows");
  const draw = async () => {
    try {
      const rows = await (await fetch("/api/tool/gpu_find", {
        method: "POST", headers: {"content-type": "application/json"}, body: "{}"
      })).json();
      box.innerHTML = rows.length ? rows.map(r =>
        `<div class="row"><b>${r.target}</b>
         <span>${r.free_count}/${r.total} 空闲</span>
         <span class="dim">${r.free.join(", ") || "—"}</span></div>`).join("")
        : '<p class="empty">没有报告显卡的机器</p>';
    } catch (e) {
      box.innerHTML = '<p class="empty">读不到：' + e + '</p>';
    }
  };
  await draw();
  setInterval(draw, 15000);
})();
</script>
"#
        .into()
    }
}

bindings::export!(Component with_types_in bindings);
