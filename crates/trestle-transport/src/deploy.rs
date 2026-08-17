//! 远端 agent 的自举与部署。
//!
//! 快路径是**先试着接管**：直接跑中继，接得上就说明有一个还活着的 agent，
//! 而且它自报的脚本哈希与本地一致——那就什么都不用做。接不上才走部署。
//!
//! 这条顺序是 D20（懒恢复）的落点：daemon 挂了、电脑重启了、网断了，
//! agent 都还在远端跑着，下次连上来是一次接管而不是一次重新安装。

use std::time::Duration;

use sha2::{Digest, Sha256};
use tokio::io::AsyncWriteExt;

use trestle_core::{Result, TrestleError};

use crate::agent::AgentClient;
use crate::ssh::SshSession;

/// 编进二进制里的远端 agent 源码。部署就是把它写过去。
pub const AGENT_SOURCE: &str = include_str!("../../../agent-py/trestle_agent.py");
pub const RELAY_SOURCE: &str = include_str!("../../../agent-py/relay.py");

/// 远端落点里的文件名。
const AGENT_FILE: &str = "trestle_agent.py";
const RELAY_FILE: &str = "relay.py";
const SOCK_FILE: &str = "agent.sock";

pub fn agent_sha256() -> String {
    hex(Sha256::digest(AGENT_SOURCE.as_bytes()).as_slice())
}

/// 把 `~/x` 换成 `$HOME/x`。
///
/// shell 的波浪号展开**只在词首、且不在引号内**时发生，所以 `RUN="$PY ~/.trestle/a.py"`
/// 里的 `~` 会被原样保留，拼出 `/home/user/~/.trestle/a.py` 这种路径。用 `$HOME`
/// 就没有这个陷阱——它在双引号里照样展开。
fn shell_dir(dir: &str) -> String {
    if dir == "~" {
        return "$HOME".to_string();
    }
    match dir.strip_prefix("~/") {
        Some(rest) => format!("$HOME/{rest}"),
        None => dir.to_string(),
    }
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// 一次接管/部署的结果，用于事件与诊断。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Bootstrap {
    /// 接管了一个已经在跑的 agent，什么都没装。
    Reattached { uptime_s: u64 },
    /// 装了（或更新了）脚本并起了一个新 agent。
    Deployed { reason: DeployReason },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DeployReason {
    /// 远端压根没有 agent 在跑。
    NotRunning,
    /// 在跑，但跑的是另一个版本的脚本。
    VersionMismatch { remote: String },
}

/// 远端解释器的选法。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Interpreter {
    /// `uv run --script`：解释器版本由脚本头里的 requires-python 锁定。
    Uv { path: String },
    /// 退路：系统 python3。整组机器都有 3.9+，所以这条路一直是通的。
    SystemPython { path: String },
}

impl Interpreter {
    /// 跑常驻 agent 的命令前缀。
    fn run_agent(&self, script: &str) -> String {
        match self {
            Interpreter::Uv { path } => format!("{path} run --script {script}"),
            Interpreter::SystemPython { path } => format!("{path} {script}"),
        }
    }

    pub fn describe(&self) -> String {
        match self {
            Interpreter::Uv { path } => format!("uv ({path})"),
            Interpreter::SystemPython { path } => format!("system python ({path})"),
        }
    }
}

/// 确保远端有一个可用的 agent，并接上它。
pub async fn ensure_agent(
    ssh: &SshSession,
    target: &str,
    agent_dir: &str,
    timeout: Duration,
) -> Result<(AgentClient, Bootstrap)> {
    let dir = agent_dir.trim_end_matches('/');
    let sock = format!("{dir}/{SOCK_FILE}");
    let expected = agent_sha256();

    // ── 快路径：直接试着接管 ──
    if let Some(client) = try_attach(ssh, target, dir, &sock, timeout).await? {
        let remote_hash = client.info.script_sha256.clone();
        if remote_hash == expected {
            let uptime = client.info.uptime_s;
            return Ok((client, Bootstrap::Reattached { uptime_s: uptime }));
        }
        // 版本对不上：让老 agent 体面退场，再装新的。
        let _ = client.call_raw("shutdown", serde_json::json!({})).await;
        drop(client);
        tokio::time::sleep(Duration::from_millis(300)).await;
        // 版本不匹配，所以远端不会自己起——脚本传完之后必须由我们启动。
        let probe = probe_remote(ssh, target, dir, &sock).await?;
        let interpreter = probe.interpreter(target)?;
        deploy_files(ssh, target, dir, &probe).await?;
        if !probe.started {
            start_agent(ssh, target, dir, &sock, &interpreter).await?;
        }
        let client = await_attach(ssh, target, dir, &sock, timeout).await?;
        return Ok((
            client,
            Bootstrap::Deployed {
                reason: DeployReason::VersionMismatch {
                    remote: short(&remote_hash),
                },
            },
        ));
    }

    // ── 慢路径：装 ──
    // 分阶段计时。部署慢的时候必须能一眼看出慢在哪一步，否则只能靠猜。
    let t0 = std::time::Instant::now();
    let probe = probe_remote(ssh, target, dir, &sock).await?;
    let t_probe = t0.elapsed();
    let interpreter = probe.interpreter(target)?;
    deploy_files(ssh, target, dir, &probe).await?;
    let t_files = t0.elapsed() - t_probe;
    if !probe.started {
        start_agent(ssh, target, dir, &sock, &interpreter).await?;
    }
    let t_start = t0.elapsed() - t_probe - t_files;
    let client = await_attach(ssh, target, dir, &sock, timeout).await?;
    tracing::info!(
        target = %target,
        probe_ms = t_probe.as_millis(),
        files_ms = t_files.as_millis(),
        start_ms = t_start.as_millis(),
        attach_ms = (t0.elapsed() - t_probe - t_files - t_start).as_millis(),
        interpreter = %interpreter.describe(),
        "agent deployed"
    );
    Ok((
        client,
        Bootstrap::Deployed {
            reason: DeployReason::NotRunning,
        },
    ))
}

/// 一次问清楚部署要知道的全部事情。
///
/// 每开一个 exec channel 都是一次往返，而 gpu-1 经 VPN 时一次往返接近 200ms——
/// 分成四条命令问就是四倍。这里一条命令拿回所有答案。
#[derive(Debug, Default)]
struct Probe {
    uv: Option<String>,
    python: Option<String>,
    agent_sha: String,
    relay_sha: String,
    /// 脚本本来就是最新的，远端已经顺手把 agent 起起来了，不用再开一个 channel。
    started: bool,
}

impl Probe {
    fn interpreter(&self, target: &str) -> Result<Interpreter> {
        if let Some(path) = &self.uv {
            return Ok(Interpreter::Uv { path: path.clone() });
        }
        if let Some(path) = &self.python {
            return Ok(Interpreter::SystemPython { path: path.clone() });
        }
        Err(TrestleError::RemoteEnvironment {
            target: target.to_string(),
            detail: "neither uv nor python3 is available on this machine".into(),
            remedy: format!(
                "install one on {target}: `curl -LsSf https://astral.sh/uv/install.sh | sh`, \
                 or any system python3 (the agent only uses the standard library)"
            ),
        })
    }
}

/// 一次往返完成「探测 + 在脚本已是最新时顺手把 agent 起起来」。
///
/// 为什么要合并：gpu-1 经 VPN 时**新建一个 exec channel 就要约 1.7 秒**（gpu-4 上同一段
/// 代码是 125ms，所以这是那条链路的性质，不是实现问题）。把探测和启动拆成两条命令，
/// 在 gpu-1 上就是白白多花一倍。绝大多数重新部署都发生在脚本没变的时候（agent 被杀、
/// 机器重启），这条路径因此只花一个往返。
async fn probe_remote(ssh: &SshSession, target: &str, dir: &str, sock: &str) -> Result<Probe> {
    let want_agent = hex(Sha256::digest(AGENT_SOURCE.as_bytes()).as_slice());
    let want_relay = hex(Sha256::digest(RELAY_SOURCE.as_bytes()).as_slice());
    let d = shell_dir(dir);
    let s = shell_dir(sock);

    let script = format!(
        "D=\"{d}\"; mkdir -p \"$D\" && chmod 700 \"$D\"; \
         UV=\"$(command -v uv 2>/dev/null || \
             ([ -x \"$HOME/.local/bin/uv\" ] && echo \"$HOME/.local/bin/uv\") || true)\"; \
         PY=\"$(command -v python3 2>/dev/null || command -v python 2>/dev/null || true)\"; \
         A=\"$(sha256sum \"$D/{AGENT_FILE}\" 2>/dev/null | cut -d' ' -f1)\"; \
         R=\"$(sha256sum \"$D/{RELAY_FILE}\" 2>/dev/null | cut -d' ' -f1)\"; \
         printf 'UV=%s\\nPY=%s\\nAGENT=%s\\nRELAY=%s\\n' \"$UV\" \"$PY\" \"$A\" \"$R\"; \
         if [ \"$A\" = \"{want_agent}\" ] && [ \"$R\" = \"{want_relay}\" ] && [ -n \"$UV$PY\" ]; then \
             if [ -n \"$UV\" ]; then RUN=\"$UV run --script $D/{AGENT_FILE}\"; \
             else RUN=\"$PY $D/{AGENT_FILE}\"; fi; \
             TRESTLE_AGENT_DIR=\"$D\" setsid $RUN --serve \"{s}\" >> \"$D/agent.log\" 2>&1 < /dev/null & \
             printf 'STARTED=1\\n'; \
         else printf 'STARTED=0\\n'; fi"
    );
    let out = ssh.exec_capture(&script).await?;

    let mut probe = Probe::default();
    for line in out.stdout.lines() {
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        let value = value.trim();
        match key.trim() {
            "UV" if !value.is_empty() => probe.uv = Some(value.to_string()),
            "PY" if !value.is_empty() => probe.python = Some(value.to_string()),
            "AGENT" => probe.agent_sha = value.to_string(),
            "RELAY" => probe.relay_sha = value.to_string(),
            "STARTED" => probe.started = value == "1",
            _ => {}
        }
    }

    if probe.uv.is_none() && probe.python.is_none() {
        // 两个都没有才值得花十几秒去装 uv —— 而不是每次部署都试一遍。
        let install = ssh
            .exec_capture(
                "curl -LsSf --max-time 30 https://astral.sh/uv/install.sh 2>/dev/null | sh >/dev/null 2>&1; \
                 ([ -x \"$HOME/.local/bin/uv\" ] && echo \"$HOME/.local/bin/uv\") || true",
            )
            .await?;
        if !install.trimmed().is_empty() {
            probe.uv = Some(install.trimmed().to_string());
        }
    }

    let _ = target;
    Ok(probe)
}

fn short(hash: &str) -> String {
    hash.chars().take(12).collect()
}

/// 跑一次中继，接得上就返回客户端；接不上返回 `None`（**不是错误**——
/// 「远端还没装 agent」是完全正常的第一次）。
async fn try_attach(
    ssh: &SshSession,
    target: &str,
    dir: &str,
    sock: &str,
    timeout: Duration,
) -> Result<Option<AgentClient>> {
    let relay = format!("{dir}/{RELAY_FILE}");
    let channel = ssh.open_session().await?;
    // 中继只用标准库、不经 uv：每条新连接都要跑一次，越快越好。
    let cmd = format!("python3 {relay} {sock} 2>/dev/null");
    if channel.exec(true, cmd.as_bytes()).await.is_err() {
        return Ok(None);
    }
    let stream = channel.into_stream();
    let (reader, writer) = tokio::io::split(stream);

    match AgentClient::attach(target, reader, writer, timeout).await {
        Ok(client) => Ok(Some(client)),
        // 接不上有很多原因（没有 relay.py、没有 socket、agent 没起来），
        // 它们的处理方式都一样：去部署。
        Err(_) => Ok(None),
    }
}

/// 部署之后等 agent 就绪。socket 的出现有几百毫秒的延迟，所以要重试而不是一次定生死。
async fn await_attach(
    ssh: &SshSession,
    target: &str,
    dir: &str,
    sock: &str,
    timeout: Duration,
) -> Result<AgentClient> {
    let deadline = tokio::time::Instant::now() + timeout;
    let mut last_wait = Duration::from_millis(80);
    loop {
        if let Some(client) = try_attach(ssh, target, dir, sock, Duration::from_secs(5)).await? {
            return Ok(client);
        }
        if tokio::time::Instant::now() >= deadline {
            // 把远端的日志捞回来——否则这里只能说「起不来」，对排查毫无帮助。
            let log = ssh
                .exec_capture(&format!("tail -n 20 {dir}/agent.log 2>/dev/null"))
                .await
                .map(|o| o.stdout)
                .unwrap_or_default();
            return Err(TrestleError::RemoteEnvironment {
                target: target.to_string(),
                detail: format!(
                    "deployed the agent but it never started listening on {sock}.\nRemote log:\n{}",
                    if log.trim().is_empty() {
                        "(empty)"
                    } else {
                        log.trim()
                    }
                ),
                remedy: format!("trestle doctor {target}"),
            });
        }
        tokio::time::sleep(last_wait).await;
        last_wait = (last_wait * 2).min(Duration::from_millis(500));
    }
}

/// 按内容哈希幂等地把两个脚本推过去。哈希已经在 [`probe_remote`] 里一次问清楚了，
/// 所以一致的时候这里一个往返都不用花。
async fn deploy_files(ssh: &SshSession, target: &str, dir: &str, probe: &Probe) -> Result<()> {
    for (name, source, have) in [
        (AGENT_FILE, AGENT_SOURCE, probe.agent_sha.as_str()),
        (RELAY_FILE, RELAY_SOURCE, probe.relay_sha.as_str()),
    ] {
        let want = hex(Sha256::digest(source.as_bytes()).as_slice());
        if have == want {
            continue;
        }
        write_remote_file(ssh, target, &format!("{dir}/{name}"), source).await?;
    }
    Ok(())
}

/// 把一段内容写到远端文件。先写临时文件再改名，避免半截文件被当成完整的。
async fn write_remote_file(
    ssh: &SshSession,
    target: &str,
    path: &str,
    content: &str,
) -> Result<()> {
    let tmp = format!("{path}.part");
    let channel = ssh.open_session().await?;
    channel
        .exec(true, format!("cat > {tmp}").as_bytes())
        .await
        .map_err(|e| TrestleError::Protocol {
            target: target.to_string(),
            detail: format!("cannot start remote write of {path}: {e}"),
        })?;

    let mut stream = channel.into_stream();
    stream
        .write_all(content.as_bytes())
        .await
        .map_err(|e| TrestleError::Protocol {
            target: target.to_string(),
            detail: format!("writing {path} failed: {e}"),
        })?;
    stream.flush().await.ok();
    stream.shutdown().await.ok();
    drop(stream);

    let finish = ssh
        .exec_capture(&format!("mv {tmp} {path} && chmod 600 {path}"))
        .await?;
    if !finish.ok() {
        return Err(TrestleError::RemoteEnvironment {
            target: target.to_string(),
            detail: format!("could not install {path}: {}", finish.stderr.trim()),
            remedy: format!("check free space and permissions on {target}"),
        });
    }
    Ok(())
}

/// 起常驻 agent。
async fn start_agent(
    ssh: &SshSession,
    target: &str,
    dir: &str,
    sock: &str,
    interpreter: &Interpreter,
) -> Result<()> {
    let d = shell_dir(dir);
    let s = shell_dir(sock);
    let script = format!("{d}/{AGENT_FILE}");
    let run = interpreter.run_agent(&script);

    // 三个细节都不能省：
    //   * 不用 `cd X && cmd &` —— `&` 作用于整个 `&&` 列表，bash 会 fork 一个子 shell
    //     去 wait 它，而那个子 shell 一直攥着调用方的 stdout 管道，于是这条 exec
    //     要等到 agent 退出才返回（实测能把一次启动变成一次永久阻塞）。
    //   * 三个 fd 全部重定向掉，channel 才能立刻拿到 EOF。
    //   * setsid 脱离会话，SSH 断了 agent 照跑 —— 这正是 D20 的前提。
    let cmd = format!(
        "TRESTLE_AGENT_DIR=\"{d}\" setsid {run} --serve \"{s}\" >> \"{d}/agent.log\" 2>&1 < /dev/null &"
    );
    let out = ssh.exec_capture(&cmd).await?;
    if !out.ok() {
        return Err(TrestleError::RemoteEnvironment {
            target: target.to_string(),
            detail: format!(
                "could not start the agent with {}: {}",
                interpreter.describe(),
                out.stderr.trim()
            ),
            remedy: format!("trestle doctor {target}"),
        });
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_agent_source_is_actually_embedded() {
        assert!(AGENT_SOURCE.contains("PROTOCOL_VERSION"));
        assert!(AGENT_SOURCE.contains("def serve_socket"));
        assert!(RELAY_SOURCE.contains("AF_UNIX"));
    }

    #[test]
    fn the_embedded_agent_speaks_the_protocol_version_we_expect() {
        // 两边任何一处改了版本号而另一处忘了改，这条就会响。
        let needle = format!("PROTOCOL_VERSION = {}", crate::agent::PROTOCOL_VERSION);
        assert!(AGENT_SOURCE.contains(&needle), "missing: {needle}");
    }

    #[test]
    fn the_source_hash_is_stable_and_hex() {
        let h = agent_sha256();
        assert_eq!(h.len(), 64);
        assert!(h.chars().all(|c| c.is_ascii_hexdigit()));
        assert_eq!(h, agent_sha256());
    }

    #[test]
    fn uv_and_python_build_different_run_commands() {
        let uv = Interpreter::Uv {
            path: "/home/x/.local/bin/uv".into(),
        };
        assert_eq!(
            uv.run_agent("/home/x/.trestle/trestle_agent.py"),
            "/home/x/.local/bin/uv run --script /home/x/.trestle/trestle_agent.py"
        );
        let py = Interpreter::SystemPython {
            path: "/usr/bin/python3".into(),
        };
        assert_eq!(
            py.run_agent("/home/x/.trestle/trestle_agent.py"),
            "/usr/bin/python3 /home/x/.trestle/trestle_agent.py"
        );
    }
}
