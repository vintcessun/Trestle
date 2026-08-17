//! connector 的「前置条件」状态机：**探不通就按配置把它拉起来**。
//!
//! 两个驱动共用这一段：一个走 SOCKS5、一个直连，但「进去之前得先有点什么」
//! 这件事对谁都一样——实验室那台要先把 VPN 容器叫醒，将来某组机器可能要先
//! `wg-quick up`。差别只在那条命令是什么，而那是**配置**该说的话，不是驱动。
//!
//! ```toml
//! [connectors.gpu-cluster]
//! plugin = "ssh-socks5"
//! socks = "127.0.0.1:11080"
//! allow_exec = ["docker"]              # 准它在本机跑什么
//!
//! [connectors.gpu-cluster.ready]
//! check = ["docker", "ps", "-a", "--filter", "name=^vpn-proxy$",
//!          "--format", "{{.Names}}"]
//! check_expect = "vpn-proxy"
//! start = ["docker", "start", "vpn-proxy"]
//! ```
//!
//! 抽出来还有一个不小的好处：这段状态机在 host 上用普通 `cargo test` 就能测——
//! 「容器不存在时报什么」「start 失败时报什么」不必先编一个 wasm 出来才验得了。
//!
//! **它只把东西叫醒，永远不创建东西。** `check` 说了前置条件不存在，它报一个
//! 带创建命令的错误就停下——自作主张地建一个容器，是在替用户做一个他没同意的决定。

use serde::Deserialize;

/// 一个 connector 的前置条件。整节不写 = 没有前置条件（直连就是这种）。
#[derive(Debug, Clone, Deserialize)]
pub struct ReadyConfig {
    /// 探这个地址通不通。不写 = 用驱动给的默认（`ssh-socks5` 用它的 socks 地址）。
    #[serde(default)]
    pub probe: String,
    #[serde(default = "d_probe_timeout_ms")]
    pub probe_timeout_ms: u32,

    /// 探不通时，先用这条命令确认前置条件**在不在**（可选）。
    #[serde(default)]
    pub check: Vec<String>,
    /// `check` 的标准输出里必须出现这段文字，否则就算「不存在」。
    #[serde(default)]
    pub check_expect: String,
    /// 不存在时报什么。留空则用一句通用的。
    #[serde(default)]
    pub missing: String,
    /// 不存在时**怎么办**——通常是那条创建命令。这句会原样交给 agent。
    #[serde(default)]
    pub missing_remedy: String,

    /// 把它拉起来的命令。不写 = 不自动拉起，探不通就直接报错。
    #[serde(default)]
    pub start: Vec<String>,
    /// 拉起之后等它开始接受连接的上限。
    #[serde(default = "d_timeout_secs")]
    pub timeout_secs: u64,
    /// 等待期间多久探一次。
    #[serde(default = "d_poll_ms")]
    pub poll_ms: u32,
    /// 成功之后多久内不再检查。没有它的话每次拨号都要去问一次 docker。
    #[serde(default = "d_cache_secs")]
    pub cache_secs: u64,
}

impl Default for ReadyConfig {
    fn default() -> Self {
        Self {
            probe: String::new(),
            probe_timeout_ms: d_probe_timeout_ms(),
            check: Vec::new(),
            check_expect: String::new(),
            missing: String::new(),
            missing_remedy: String::new(),
            start: Vec::new(),
            timeout_secs: d_timeout_secs(),
            poll_ms: d_poll_ms(),
            cache_secs: d_cache_secs(),
        }
    }
}

fn d_probe_timeout_ms() -> u32 {
    800
}
fn d_timeout_secs() -> u64 {
    40
}
fn d_poll_ms() -> u32 {
    400
}
fn d_cache_secs() -> u64 {
    30
}

/// 前置条件没准备好。`detail` 说发生了什么，`remedy` 说下一步能做什么——
/// 两个都是给 agent 看的，都不是客套话。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NotReady {
    pub detail: String,
    pub remedy: String,
}

/// 一次本机命令的结果。
#[derive(Debug, Clone, Default)]
pub struct Exec {
    pub exit_code: i32,
    pub stdout: String,
    pub stderr: String,
}

/// 本机命令没跑成。`denied` 区分「host 不准跑」和「跑了但失败了」——
/// 两者的下一步完全不同，糊成一个错误会让人去查 docker 而不是去查配置。
#[derive(Debug, Clone)]
pub struct ExecError {
    pub denied: bool,
    pub detail: String,
}

/// 状态机要用到的那点外界能力。驱动把自己的 host 导入接到这上面。
///
/// 它存在的唯一理由是让这段逻辑能在 host 上被测——wit-bindgen 生成的导入
/// 只在 wasm 里有意义，直接调它就等于「不编个 wasm 出来就测不了」。
pub trait Sys {
    fn probe_tcp(&self, addr: &str, timeout_ms: u32) -> bool;
    fn local_exec(&self, argv: &[String]) -> Result<Exec, ExecError>;
    fn now_ms(&self) -> u64;
    fn sleep_ms(&self, ms: u32);
    fn emit(&self, level: &str, kind: &str, fields: &str);
}

/// 「刚确认过」的短缓存。驱动自己存着它。
#[derive(Debug, Clone, Copy, Default)]
pub struct Cache {
    until_ms: u64,
}

impl Cache {
    pub fn fresh(&self, now_ms: u64) -> bool {
        self.until_ms > now_ms
    }
    fn remember(&mut self, now_ms: u64, cache_secs: u64) {
        self.until_ms = now_ms + cache_secs * 1000;
    }
    /// 作废。配置变了或者上一次调用发现连接其实是坏的时候用。
    pub fn forget(&mut self) {
        self.until_ms = 0;
    }
}

/// 幂等地把前置条件准备好。
///
/// `fallback_probe` 是驱动的默认探测地址（`ssh-socks5` 传自己的 socks 地址，
/// 直连驱动传空串）。配置里的 `probe` 优先。
pub fn ensure(
    sys: &impl Sys,
    cfg: &ReadyConfig,
    cache: &mut Cache,
    fallback_probe: &str,
) -> Result<(), NotReady> {
    let probe = if cfg.probe.is_empty() {
        fallback_probe
    } else {
        &cfg.probe
    };

    // 没有可探的东西，也没有要跑的命令 —— 那就是「没有前置条件」。
    if probe.is_empty() && cfg.start.is_empty() {
        return Ok(());
    }

    let now = sys.now_ms();
    if cache.fresh(now) {
        return Ok(());
    }

    // 先探。绝大多数调用走的是这条路，所以它必须最便宜。
    if !probe.is_empty() && sys.probe_tcp(probe, cfg.probe_timeout_ms) {
        cache.remember(now, cfg.cache_secs);
        return Ok(());
    }

    if cfg.start.is_empty() {
        return Err(NotReady {
            detail: format!("{probe} is not accepting connections"),
            remedy: "把它拉起来的命令写在这个 connector 的 [.ready] 配置节里\
                     （start = [\"docker\", \"start\", \"...\"]），或者手工先起它"
                .into(),
        });
    }

    // 探不通。先看看要拉的那个东西在不在——**不在就报错，绝不替用户创建**。
    if !cfg.check.is_empty() {
        let out = run(sys, &cfg.check)?;
        let present = out.exit_code == 0
            && (cfg.check_expect.is_empty() || out.stdout.contains(&cfg.check_expect));
        if !present {
            return Err(NotReady {
                detail: if cfg.missing.is_empty() {
                    format!(
                        "the prerequisite for this connector is not there \
                         (`{}` said: {})",
                        cfg.check.join(" "),
                        first_line(&out.stdout, &out.stderr)
                    )
                } else {
                    cfg.missing.clone()
                },
                remedy: if cfg.missing_remedy.is_empty() {
                    "create it once by hand; Trestle will only ever start it".into()
                } else {
                    cfg.missing_remedy.clone()
                },
            });
        }
    }

    let started = run(sys, &cfg.start)?;
    if started.exit_code != 0 {
        return Err(NotReady {
            detail: format!(
                "`{}` failed with exit code {}: {}",
                cfg.start.join(" "),
                started.exit_code,
                first_line(&started.stderr, &started.stdout)
            ),
            remedy: "跑一次这条命令看它到底说了什么".into(),
        });
    }

    // 起来了不等于能连上：容器起来到端口真的在听之间有几秒的窗口。
    if probe.is_empty() {
        cache.remember(sys.now_ms(), cfg.cache_secs);
        return Ok(());
    }
    let deadline = sys.now_ms() + cfg.timeout_secs * 1000;
    while sys.now_ms() < deadline {
        if sys.probe_tcp(probe, cfg.probe_timeout_ms) {
            cache.remember(sys.now_ms(), cfg.cache_secs);
            sys.emit(
                "info",
                "connector_ensure_ready",
                &format!(
                    r#"{{"action":"started","command":"{}"}}"#,
                    escape(&cfg.start.join(" "))
                ),
            );
            return Ok(());
        }
        sys.sleep_ms(cfg.poll_ms);
    }

    Err(NotReady {
        detail: format!(
            "`{}` succeeded but {probe} never started accepting connections within {}s",
            cfg.start.join(" "),
            cfg.timeout_secs
        ),
        remedy: "看它自己的日志；或者把 [.ready] 的 timeout_secs 调大".into(),
    })
}

fn run(sys: &impl Sys, argv: &[String]) -> Result<Exec, NotReady> {
    sys.local_exec(argv).map_err(|e| {
        if e.denied {
            // 被 host 挡住和命令本身失败是两回事：这里要把人指向配置，
            // 而不是指向 docker。
            let program = argv.first().cloned().unwrap_or_default();
            NotReady {
                detail: format!("this connector is not allowed to run `{program}` on this machine"),
                remedy: format!(
                    "在它的配置节里加上 allow_exec = [\"{}\"]",
                    basename(&program)
                ),
            }
        } else {
            NotReady {
                detail: format!("cannot run `{}`: {}", argv.join(" "), e.detail),
                remedy: String::new(),
            }
        }
    })
}

fn basename(program: &str) -> &str {
    program
        .rsplit(['/', '\\'])
        .next()
        .unwrap_or(program)
        .trim_end_matches(".exe")
}

/// 命令输出的第一行有用的内容。整段塞进错误消息里没人读得下去。
fn first_line<'a>(primary: &'a str, fallback: &'a str) -> &'a str {
    let pick = if primary.trim().is_empty() {
        fallback
    } else {
        primary
    };
    pick.lines()
        .find(|l| !l.trim().is_empty())
        .unwrap_or("")
        .trim()
}

fn escape(s: &str) -> String {
    s.replace('\\', "\\\\").replace('"', "\\\"")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;

    /// 一个可编排的外界。probe 按「第 N 次调用」返回，命令按 argv[0] 编排。
    #[derive(Default)]
    struct Mock {
        probes: RefCell<Vec<bool>>,
        exec: RefCell<Vec<Result<Exec, ExecError>>>,
        ran: RefCell<Vec<String>>,
        now: RefCell<u64>,
        slept: RefCell<u64>,
        events: RefCell<Vec<String>>,
    }

    impl Sys for Mock {
        fn probe_tcp(&self, _addr: &str, _timeout_ms: u32) -> bool {
            let mut p = self.probes.borrow_mut();
            if p.is_empty() {
                false
            } else {
                p.remove(0)
            }
        }
        fn local_exec(&self, argv: &[String]) -> Result<Exec, ExecError> {
            self.ran.borrow_mut().push(argv.join(" "));
            let mut e = self.exec.borrow_mut();
            if e.is_empty() {
                Ok(Exec::default())
            } else {
                e.remove(0)
            }
        }
        fn now_ms(&self) -> u64 {
            *self.now.borrow()
        }
        fn sleep_ms(&self, ms: u32) {
            *self.slept.borrow_mut() += ms as u64;
            *self.now.borrow_mut() += ms as u64;
        }
        fn emit(&self, _level: &str, kind: &str, _fields: &str) {
            self.events.borrow_mut().push(kind.into());
        }
    }

    fn ok(stdout: &str) -> Result<Exec, ExecError> {
        Ok(Exec {
            exit_code: 0,
            stdout: stdout.into(),
            stderr: String::new(),
        })
    }

    fn lab_config() -> ReadyConfig {
        ReadyConfig {
            check: ["docker", "ps", "-a", "--filter", "name=^box$"]
                .iter()
                .map(|s| s.to_string())
                .collect(),
            check_expect: "box".into(),
            missing: "the VPN container 'box' does not exist".into(),
            missing_remedy: "docker run -d --name box ...".into(),
            start: ["docker", "start", "box"]
                .iter()
                .map(|s| s.to_string())
                .collect(),
            timeout_secs: 2,
            poll_ms: 400,
            ..Default::default()
        }
    }

    #[test]
    fn a_reachable_prerequisite_costs_exactly_one_probe() {
        // 绝大多数调用走这条路，所以它必须不碰 docker。
        let sys = Mock {
            probes: RefCell::new(vec![true]),
            ..Default::default()
        };
        let mut cache = Cache::default();
        assert!(ensure(&sys, &lab_config(), &mut cache, "127.0.0.1:11080").is_ok());
        assert!(
            sys.ran.borrow().is_empty(),
            "it should not have run anything"
        );
        assert!(cache.fresh(sys.now_ms()));
    }

    #[test]
    fn the_cache_keeps_it_from_asking_again() {
        let sys = Mock {
            probes: RefCell::new(vec![true]),
            ..Default::default()
        };
        let mut cache = Cache::default();
        let cfg = lab_config();
        ensure(&sys, &cfg, &mut cache, "127.0.0.1:11080").unwrap();
        // 第二次：探测队列已经空了（空 = false），但缓存还新鲜，所以仍然 Ok。
        assert!(ensure(&sys, &cfg, &mut cache, "127.0.0.1:11080").is_ok());
        assert!(sys.ran.borrow().is_empty());
    }

    #[test]
    fn an_unreachable_port_starts_the_thing_and_waits_for_it() {
        // 第一次探不通 → check 说在 → start → 再探就通了。
        let sys = Mock {
            probes: RefCell::new(vec![false, true]),
            exec: RefCell::new(vec![ok("box"), ok("")]),
            ..Default::default()
        };
        let mut cache = Cache::default();
        ensure(&sys, &lab_config(), &mut cache, "127.0.0.1:11080").unwrap();
        assert_eq!(
            *sys.ran.borrow(),
            vec!["docker ps -a --filter name=^box$", "docker start box"]
        );
        assert_eq!(*sys.events.borrow(), vec!["connector_ensure_ready"]);
    }

    #[test]
    fn a_missing_prerequisite_is_reported_never_created() {
        // check 的输出里没有那个名字 = 它不存在。这时候**不能**去创建它——
        // 报一个带创建命令的错误，让用户自己决定。
        let sys = Mock {
            probes: RefCell::new(vec![false]),
            exec: RefCell::new(vec![ok("")]),
            ..Default::default()
        };
        let mut cache = Cache::default();
        let err = ensure(&sys, &lab_config(), &mut cache, "127.0.0.1:11080").unwrap_err();
        assert!(err.detail.contains("does not exist"), "{err:?}");
        assert!(err.remedy.contains("docker run"), "{err:?}");
        // 只跑了 check，没跑 start。
        assert_eq!(sys.ran.borrow().len(), 1);
    }

    #[test]
    fn being_denied_points_at_the_config_not_at_docker() {
        // host 挡下来和命令失败是两件事。前者的下一步是改配置。
        let sys = Mock {
            probes: RefCell::new(vec![false]),
            exec: RefCell::new(vec![Err(ExecError {
                denied: true,
                detail: "local-exec docker is not in its manifest allowlist".into(),
            })]),
            ..Default::default()
        };
        let mut cache = Cache::default();
        let err = ensure(&sys, &lab_config(), &mut cache, "127.0.0.1:11080").unwrap_err();
        assert!(err.remedy.contains("allow_exec"), "{err:?}");
        assert!(err.remedy.contains("docker"), "{err:?}");
    }

    #[test]
    fn a_start_command_that_fails_says_what_it_said() {
        let sys = Mock {
            probes: RefCell::new(vec![false]),
            exec: RefCell::new(vec![
                ok("box"),
                Ok(Exec {
                    exit_code: 1,
                    stdout: String::new(),
                    stderr: "Cannot connect to the Docker daemon\n".into(),
                }),
            ]),
            ..Default::default()
        };
        let mut cache = Cache::default();
        let err = ensure(&sys, &lab_config(), &mut cache, "127.0.0.1:11080").unwrap_err();
        assert!(
            err.detail.contains("Cannot connect to the Docker daemon"),
            "{err:?}"
        );
    }

    #[test]
    fn it_gives_up_after_the_configured_timeout() {
        // 起来了但端口一直不通：报超时，并说清楚是等了多久。
        let sys = Mock {
            probes: RefCell::new(vec![false]),
            exec: RefCell::new(vec![ok("box"), ok("")]),
            ..Default::default()
        };
        let mut cache = Cache::default();
        let err = ensure(&sys, &lab_config(), &mut cache, "127.0.0.1:11080").unwrap_err();
        assert!(err.detail.contains("never started accepting"), "{err:?}");
        assert!(err.detail.contains("2s"), "{err:?}");
        // 真的等满了，没有立刻放弃。
        assert!(*sys.slept.borrow() >= 2000, "slept {}", sys.slept.borrow());
    }

    #[test]
    fn no_start_command_means_it_only_reports() {
        let cfg = ReadyConfig {
            start: Vec::new(),
            ..Default::default()
        };
        let sys = Mock::default();
        let mut cache = Cache::default();
        let err = ensure(&sys, &cfg, &mut cache, "127.0.0.1:11080").unwrap_err();
        assert!(err.detail.contains("127.0.0.1:11080"), "{err:?}");
        assert!(err.remedy.contains("start"), "{err:?}");
    }

    #[test]
    fn a_connector_with_no_prerequisites_does_nothing_at_all() {
        // 直连驱动的默认形态：不写 [.ready]，也不给探测地址。
        let sys = Mock::default();
        let mut cache = Cache::default();
        assert!(ensure(&sys, &ReadyConfig::default(), &mut cache, "").is_ok());
        assert!(sys.ran.borrow().is_empty());
    }

    #[test]
    fn the_probe_in_the_config_wins_over_the_drivers_default() {
        let cfg = ReadyConfig {
            probe: "10.0.0.1:9999".into(),
            start: Vec::new(),
            ..Default::default()
        };
        let sys = Mock::default();
        let mut cache = Cache::default();
        let err = ensure(&sys, &cfg, &mut cache, "127.0.0.1:11080").unwrap_err();
        assert!(err.detail.contains("10.0.0.1:9999"), "{err:?}");
    }
}
