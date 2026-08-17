//! 一台机器的一条会话：拨号 + SSH + 常驻 agent，七个基本操作的落点。
//!
//! 这一层是**给 connector 用的工具箱**，不是给上层用的接口。connector 决定怎么组合
//! 它们（先拉容器还是先拨号、断了重试几次、agent 装在哪），上层只看到七个操作。

use std::sync::Arc;
use std::time::{Duration, Instant};

use serde_json::json;

use trestle_core::config::Defaults;
use trestle_core::{
    EditOp, EditResponse, ForwardResponse, Op, ReadResponse, Result, ShellRequest, ShellResponse,
    Target, TransferOptions, TransferResponse, WriteResponse,
};

use crate::agent::AgentClient;
use crate::deploy::{self, Bootstrap};
use crate::dial::{self, DialContext, DialPlan};
use crate::forward::Forward;
use crate::ssh::{Credentials, HostKeyPolicy, SshSession};
use crate::transfer;

/// 建立一条会话时的参数。
#[derive(Debug, Clone)]
pub struct ConnectOptions {
    pub plan: DialPlan,
    pub host_key_policy: HostKeyPolicy,
    pub dial_timeout: Duration,
    pub bootstrap_timeout: Duration,
    pub keepalive: Duration,
}

impl Default for ConnectOptions {
    fn default() -> Self {
        Self {
            plan: DialPlan::Direct,
            host_key_policy: HostKeyPolicy::KnownHostsTofu,
            dial_timeout: Duration::from_secs(15),
            // gpu-1 经 VPN 冷启动实测 5s，留一倍余量。
            bootstrap_timeout: Duration::from_secs(30),
            keepalive: Duration::from_secs(20),
        }
    }
}

/// 一次建链的度量，用于验收里的冷热延迟对比。
#[derive(Debug, Clone)]
pub struct ConnectStats {
    pub dial_ms: u64,
    pub ssh_ms: u64,
    pub bootstrap_ms: u64,
    pub total_ms: u64,
    pub bootstrap: Bootstrap,
    pub host_key_first_seen: bool,
}

pub struct Session {
    pub target: Target,
    ssh: Arc<SshSession>,
    agent: AgentClient,
    defaults: Defaults,
    pub stats: ConnectStats,
}

impl Session {
    pub async fn connect(
        target: &Target,
        creds: &Credentials,
        opts: &ConnectOptions,
        defaults: &Defaults,
    ) -> Result<Self> {
        let started = Instant::now();
        let ctx = DialContext::new(&target.name, &target.connector);

        let dial_started = Instant::now();
        let stream: Box<dyn crate::dial::Stream> = match &opts.plan {
            DialPlan::Direct => Box::new(
                dial::dial_direct(&target.host, target.port, opts.dial_timeout, &ctx).await?,
            ),
            DialPlan::Socks5 { proxy } => Box::new(
                dial::dial_socks5(proxy, &target.host, target.port, opts.dial_timeout, &ctx)
                    .await?,
            ),
        };
        let dial_ms = dial_started.elapsed().as_millis() as u64;

        let ssh_started = Instant::now();
        let ssh = SshSession::connect(
            stream,
            &crate::ssh::SshTarget {
                name: &target.name,
                host: &target.host,
                port: target.port,
                user: &target.user,
            },
            creds,
            opts.host_key_policy,
            opts.keepalive,
        )
        .await?;
        let ssh_ms = ssh_started.elapsed().as_millis() as u64;
        let host_key = ssh.host_key();
        let ssh = Arc::new(ssh);

        let boot_started = Instant::now();
        let (agent, bootstrap) = deploy::ensure_agent(
            &ssh,
            &target.name,
            &target.agent_dir,
            opts.bootstrap_timeout,
        )
        .await?;
        let bootstrap_ms = boot_started.elapsed().as_millis() as u64;

        Ok(Self {
            target: target.clone(),
            ssh,
            agent,
            defaults: defaults.clone(),
            stats: ConnectStats {
                dial_ms,
                ssh_ms,
                bootstrap_ms,
                total_ms: started.elapsed().as_millis() as u64,
                bootstrap,
                host_key_first_seen: host_key.first_seen,
            },
        })
    }

    /// 连接还活着吗。后台探活与自愈判断用它。
    pub fn is_alive(&self) -> bool {
        !self.agent.is_dead() && !self.ssh.is_closed()
    }

    pub fn agent_info(&self) -> &crate::agent::AgentInfo {
        &self.agent.info
    }

    // ────────────────────────── 七个基本操作 ──────────────────────────

    pub async fn read(
        &self,
        path: &str,
        start_line: Option<u32>,
        max_lines: Option<u32>,
    ) -> Result<ReadResponse> {
        self.agent
            .call(
                Op::Read,
                json!({"path": path, "start_line": start_line, "max_lines": max_lines}),
            )
            .await
    }

    pub async fn write(
        &self,
        path: &str,
        content: &str,
        append: bool,
        make_dirs: bool,
    ) -> Result<WriteResponse> {
        self.agent
            .call(
                Op::Write,
                json!({"path": path, "content": content, "append": append, "make_dirs": make_dirs}),
            )
            .await
    }

    pub async fn edit(&self, path: &str, op: &EditOp) -> Result<EditResponse> {
        self.agent
            .call(Op::Edit, json!({"path": path, "op": op}))
            .await
    }

    pub async fn shell(&self, mut req: ShellRequest) -> Result<ShellResponse> {
        if !req.detach && req.timeout_secs.is_none() {
            req.timeout_secs = Some(self.defaults.shell_timeout_secs);
        }
        if let Some(t) = req.timeout_secs {
            // 超时上限是配置里定的：更长的活儿该用 job_start，不是把超时调大再撞一次。
            req.timeout_secs = Some(t.min(self.defaults.shell_max_timeout_secs));
        }
        if req.cwd.is_none() && !self.target.workdir.is_empty() {
            req.cwd = Some(self.target.workdir.clone());
        }
        let args = serde_json::to_value(&req).expect("shell requests are serializable");
        self.agent.call(Op::Shell, args).await
    }

    pub async fn upload(
        &self,
        local_path: &str,
        remote_path: &str,
        opts: &TransferOptions,
    ) -> Result<TransferResponse> {
        transfer::upload(
            &self.agent,
            &self.target.name,
            local_path,
            remote_path,
            opts,
            &self.defaults.exclude,
        )
        .await
    }

    pub async fn download(
        &self,
        remote_path: &str,
        local_path: &str,
        opts: &TransferOptions,
    ) -> Result<TransferResponse> {
        transfer::download(
            &self.agent,
            &self.target.name,
            remote_path,
            local_path,
            opts,
            &self.defaults.exclude,
        )
        .await
    }

    /// 把远端的一个端口映射到本地。本地端口由这里分配，调用方不能指定。
    pub async fn forward(&self, remote_port: u16) -> Result<(ForwardResponse, Forward)> {
        let fw = Forward::open(
            Arc::clone(&self.ssh),
            &self.target.name,
            "127.0.0.1",
            remote_port,
        )
        .await?;
        Ok((fw.response(), fw))
    }

    // ─────────────────────────── 辅助能力 ───────────────────────────

    /// 给一个进程组发信号。job 插件的 stop 建在它上面。
    pub async fn signal(&self, pid: u32, sig: &str) -> Result<()> {
        self.agent
            .call_raw("signal", json!({"pid": pid, "signal": sig}))
            .await?;
        Ok(())
    }

    /// 量一次热调用延迟。
    pub async fn ping_ms(&self) -> Result<u64> {
        let started = Instant::now();
        self.agent.ping().await?;
        Ok(started.elapsed().as_millis() as u64)
    }

    pub async fn disconnect(self) {
        self.ssh.disconnect().await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn target() -> Target {
        Target {
            name: "gpu-4".into(),
            host: "203.0.113.31".into(),
            port: 2204,
            user: "alice".into(),
            connector: "gpu-cluster".into(),
            workdir: "/home/alice/data".into(),
            aliases: vec![],
            note: String::new(),
            agent_dir: "~/.trestle".into(),
        }
    }

    #[test]
    fn shell_requests_default_to_the_configured_timeout() {
        let defaults = Defaults::default();
        let mut req = ShellRequest {
            command: "echo hi".into(),
            cwd: None,
            timeout_secs: None,
            env: vec![],
            detach: false,
            name: None,
        };
        // 复刻 Session::shell 里的归一化逻辑（不建真连接也能测到它）。
        if !req.detach && req.timeout_secs.is_none() {
            req.timeout_secs = Some(defaults.shell_timeout_secs);
        }
        assert_eq!(req.timeout_secs, Some(60));
    }

    #[test]
    fn an_oversized_timeout_is_clamped_not_honoured() {
        let defaults = Defaults::default();
        let asked = 100_000u64;
        let effective = asked.min(defaults.shell_max_timeout_secs);
        // 想跑更久就该用 job_start，而不是把 shell 的超时调到天上去。
        assert_eq!(effective, 300);
    }

    #[test]
    fn a_target_workdir_becomes_the_default_cwd() {
        let t = target();
        let cwd: Option<String> = None;
        let effective = cwd.or_else(|| {
            if t.workdir.is_empty() {
                None
            } else {
                Some(t.workdir.clone())
            }
        });
        // 好几台机器根分区都吃紧，默认落在大盘而不是 ~。
        assert_eq!(effective.as_deref(), Some("/home/alice/data"));
    }
}
