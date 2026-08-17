//! host 导入的实现：传输工具箱与 host 服务。
//!
//! 每一个都是插件唯一能碰到外界的入口——wasm 组件没有 syscall，
//! 所以 capability 检查放在这里就是真的强制，不是约定。

use std::sync::Arc;
use std::time::Duration;

use trestle_core::TrestleError;
use trestle_transport::{
    AgentClient, Credentials, DialContext, Forward, HostKeyPolicy, SshSession, deploy, dial,
    transfer,
};

use crate::bindings::trestle::plugin::types::{Error, ErrorKind, ExecOutput, TargetInfo};
use crate::bindings::trestle::plugin::{host_services, transport};
use crate::state::PluginState;

/// 把内部错误翻译成插件能看到的形状。**保住可操作性**——
/// detail 与 remedy 一路传到 agent 眼前。
pub fn error_to_wit(e: TrestleError) -> Error {
    let kind = match &e {
        TrestleError::UnknownTarget { .. } | TrestleError::UnknownConnector { .. } => {
            ErrorKind::NotFound
        }
        TrestleError::ConnectorNotReady { .. } => ErrorKind::NotReady,
        TrestleError::Unreachable { .. } => ErrorKind::Unreachable,
        TrestleError::AuthFailed { .. } => ErrorKind::AuthFailed,
        TrestleError::ShellTimeout { .. } => ErrorKind::Timeout,
        TrestleError::UnknownState { .. } => ErrorKind::UnknownState,
        TrestleError::Protocol { .. } => ErrorKind::Protocol,
        TrestleError::CapabilityDenied { .. } => ErrorKind::Denied,
        TrestleError::Config { .. } => ErrorKind::InvalidRequest,
        TrestleError::Remote { detail, .. } => {
            if detail.contains("not_found") {
                ErrorKind::NotFound
            } else if detail.contains("permission_denied") {
                ErrorKind::PermissionDenied
            } else {
                ErrorKind::Internal
            }
        }
        TrestleError::RemoteEnvironment { .. } => ErrorKind::NotReady,
        _ => ErrorKind::Internal,
    };
    // 错误的 Display 里已经带了 remedy（`\nTry: ...`），这里把它拆出来，
    // 让插件与上层能分别呈现。
    let text = e.to_string();
    let (detail, remedy) = match text.split_once("\nTry: ") {
        Some((d, r)) => (d.to_string(), r.to_string()),
        None => (text, String::new()),
    };
    Error {
        kind,
        detail,
        remedy,
    }
}

fn denied(state: &PluginState, action: &str) -> Error {
    let e = state.deny(action);
    Error {
        kind: ErrorKind::Denied,
        detail: e.to_string(),
        remedy: format!(
            "add it to the `capabilities` section of plugins/{}/manifest.toml",
            state.plugin
        ),
    }
}

fn gone(what: &str) -> Error {
    Error {
        kind: ErrorKind::InvalidRequest,
        detail: format!("{what} handle is not valid (already closed, or never opened)"),
        remedy: "open it again".into(),
    }
}

impl transport::Host for PluginState {
    async fn dial(&mut self, addr: String, timeout_ms: u32) -> Result<u64, Error> {
        if !self.caps().allows_dial(&addr) {
            return Err(denied(self, &format!("dial {addr}")));
        }
        let ctx = DialContext::new("", &self.plugin);
        let (host, port) = split_addr(&addr)?;
        let stream = dial::dial_direct(&host, port, Duration::from_millis(timeout_ms as u64), &ctx)
            .await
            .map_err(error_to_wit)?;
        Ok(self.shared.handles.lock().await.put_stream(stream))
    }

    async fn dial_socks5(
        &mut self,
        proxy: String,
        addr: String,
        timeout_ms: u32,
    ) -> Result<u64, Error> {
        if !self.caps().allows_dial(&addr) {
            return Err(denied(self, &format!("dial {addr}")));
        }
        let ctx = DialContext::new("", &self.plugin);
        let (host, port) = split_addr(&addr)?;
        let stream = dial::dial_socks5(
            &proxy,
            &host,
            port,
            Duration::from_millis(timeout_ms as u64),
            &ctx,
        )
        .await
        .map_err(error_to_wit)?;
        Ok(self.shared.handles.lock().await.put_stream(stream))
    }

    async fn probe_tcp(&mut self, addr: String, timeout_ms: u32) -> bool {
        matches!(
            tokio::time::timeout(
                Duration::from_millis(timeout_ms as u64),
                tokio::net::TcpStream::connect(&addr),
            )
            .await,
            Ok(Ok(_))
        )
    }

    async fn local_exec(&mut self, argv: Vec<String>) -> Result<ExecOutput, Error> {
        let Some(program) = argv.first() else {
            return Err(Error {
                kind: ErrorKind::InvalidRequest,
                detail: "local-exec called with an empty argv".into(),
                remedy: String::new(),
            });
        };
        // 这是整个 capability 模型最要紧的一道闸：本机执行任意命令等于全部权限。
        if !self.caps().allows_local_exec(program) {
            return Err(denied(self, &format!("local-exec {program}")));
        }

        let out = tokio::process::Command::new(program)
            .args(&argv[1..])
            .output()
            .await
            .map_err(|e| Error {
                kind: ErrorKind::NotReady,
                detail: format!("cannot run `{}`: {e}", argv.join(" ")),
                remedy: format!("is {program} installed and on PATH?"),
            })?;

        Ok(ExecOutput {
            exit_code: out.status.code().unwrap_or(-1),
            stdout: String::from_utf8_lossy(&out.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
        })
    }

    async fn ssh_connect(
        &mut self,
        conn: u64,
        target: String,
        host: String,
        port: u16,
        user: String,
        creds_ref: String,
    ) -> Result<u64, Error> {
        let Some(stream) = self.shared.handles.lock().await.take_stream(conn) else {
            return Err(gone("stream"));
        };
        let creds = self.credentials(&creds_ref).map_err(error_to_wit)?;
        let session = SshSession::connect(
            stream,
            &trestle_transport::ssh::SshTarget {
                name: &target,
                host: &host,
                port,
                user: &user,
            },
            &creds,
            HostKeyPolicy::KnownHostsTofu,
            Duration::from_secs(20),
        )
        .await
        .map_err(error_to_wit)?;
        Ok(self
            .shared
            .handles
            .lock()
            .await
            .put_session(Arc::new(session)))
    }

    async fn ssh_exec(&mut self, session: u64, command: String) -> Result<ExecOutput, Error> {
        let Some(s) = self.shared.handles.lock().await.session(session) else {
            return Err(gone("session"));
        };
        let out = s.exec_capture(&command).await.map_err(error_to_wit)?;
        Ok(ExecOutput {
            exit_code: out.exit_code,
            stdout: out.stdout,
            stderr: out.stderr,
        })
    }

    async fn ssh_alive(&mut self, session: u64) -> bool {
        self.shared
            .handles
            .lock()
            .await
            .session(session)
            .map(|s| !s.is_closed())
            .unwrap_or(false)
    }

    async fn ssh_close(&mut self, session: u64) {
        if let Some(s) = self.shared.handles.lock().await.drop_session(session) {
            s.disconnect().await;
        }
    }

    async fn agent_ensure(
        &mut self,
        session: u64,
        target: String,
        agent_dir: String,
    ) -> Result<u64, Error> {
        let Some(s) = self.shared.handles.lock().await.session(session) else {
            return Err(gone("session"));
        };
        let (client, bootstrap) =
            deploy::ensure_agent(&s, &target, &agent_dir, Duration::from_secs(45))
                .await
                .map_err(error_to_wit)?;
        self.events.emit(
            &self.plugin,
            "info",
            "session_connected",
            &serde_json::json!({
                "target": target,
                "bootstrap": format!("{bootstrap:?}"),
            })
            .to_string(),
        );
        Ok(self.shared.handles.lock().await.put_agent(Arc::new(client)))
    }

    async fn agent_call(
        &mut self,
        agent: u64,
        op: String,
        payload: String,
    ) -> Result<String, Error> {
        let Some(a) = self.shared.handles.lock().await.agent(agent) else {
            return Err(gone("agent"));
        };
        let args: serde_json::Value = serde_json::from_str(&payload).map_err(|e| Error {
            kind: ErrorKind::InvalidRequest,
            detail: format!("payload for '{op}' is not valid JSON: {e}"),
            remedy: String::new(),
        })?;
        let value = a.call_raw(&op, args).await.map_err(error_to_wit)?;
        Ok(value.to_string())
    }

    async fn agent_alive(&mut self, agent: u64) -> bool {
        self.shared
            .handles
            .lock()
            .await
            .agent(agent)
            .map(|a| !a.is_dead())
            .unwrap_or(false)
    }

    /// 记住这台机器的连接，供同一个 connector 的其他实例复用。
    async fn session_remember(&mut self, target: String, session: u64, agent: u64) {
        self.shared
            .sessions
            .lock()
            .await
            .insert(target, (session, agent));
    }

    async fn session_lookup(&mut self, target: String) -> Option<(u64, u64)> {
        self.shared.sessions.lock().await.get(&target).copied()
    }

    async fn session_forget(&mut self, target: String) {
        self.shared.sessions.lock().await.remove(&target);
    }

    async fn agent_upload(
        &mut self,
        agent: u64,
        local: String,
        remote: String,
        opts: String,
    ) -> Result<String, Error> {
        let (a, target, exclude) = self.transfer_context(agent).await?;
        let opts = parse_opts(&opts)?;
        let res = transfer::upload(&a, &target, &local, &remote, &opts, &exclude)
            .await
            .map_err(error_to_wit)?;
        Ok(serde_json::to_string(&res).unwrap_or_default())
    }

    async fn agent_download(
        &mut self,
        agent: u64,
        remote: String,
        local: String,
        opts: String,
    ) -> Result<String, Error> {
        let (a, target, exclude) = self.transfer_context(agent).await?;
        let opts = parse_opts(&opts)?;
        let res = transfer::download(&a, &target, &remote, &local, &opts, &exclude)
            .await
            .map_err(error_to_wit)?;
        Ok(serde_json::to_string(&res).unwrap_or_default())
    }

    async fn forward_open(
        &mut self,
        session: u64,
        remote_host: String,
        remote_port: u16,
    ) -> Result<String, Error> {
        if !self.caps().forward {
            return Err(denied(self, "forward"));
        }
        let Some(s) = self.shared.handles.lock().await.session(session) else {
            return Err(gone("session"));
        };
        let fw = Forward::open(s, &self.plugin, &remote_host, remote_port)
            .await
            .map_err(error_to_wit)?;
        let response = fw.response();
        self.shared.handles.lock().await.put_forward(fw);
        Ok(serde_json::to_string(&response).unwrap_or_default())
    }
}

impl host_services::Host for PluginState {
    async fn targets(&mut self) -> Vec<TargetInfo> {
        self.targets
            .iter()
            .map(|t| TargetInfo {
                name: t.name.clone(),
                host: t.host.clone(),
                port: t.port,
                user: t.user.clone(),
                workdir: t.workdir.clone(),
                note: t.note.clone(),
                aliases: t.aliases.clone(),
                agent_dir: t.agent_dir.clone(),
                connector: t.connector.clone(),
            })
            .collect()
    }

    async fn config_get(&mut self) -> String {
        self.config_json.clone()
    }

    async fn secret_get(&mut self, reference: String) -> Result<String, Error> {
        // 秘密的值绝不进事件、不进日志——只有取用的动作被记下来。
        self.secret(&reference).map_err(error_to_wit)
    }

    async fn state_get(&mut self, key: String) -> Option<String> {
        self.kv.get(&key).await
    }

    async fn state_set(&mut self, key: String, value: String) {
        self.kv.set(key, value).await;
    }

    async fn state_delete(&mut self, key: String) {
        self.kv.delete(&key).await;
    }

    async fn state_list(&mut self, prefix: String) -> Vec<String> {
        self.kv.list(&prefix).await
    }

    async fn emit(&mut self, level: String, kind: String, fields: String) {
        self.events.emit(&self.plugin, &level, &kind, &fields);
    }

    async fn now_ms(&mut self) -> u64 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0)
    }

    async fn sleep_ms(&mut self, ms: u32) {
        tokio::time::sleep(Duration::from_millis(ms as u64)).await;
    }

    async fn staging_path(&mut self, name: String) -> String {
        crate::staging_path(&name)
    }
}

impl PluginState {
    /// 取 `&mut self` 而不是 `&self`：`PluginState` 里有一个 `WasiCtx`，它不是 `Sync`，
    /// 于是 `&PluginState` 不是 `Send`，跨 await 持有它会让整个 future 不能跨线程。
    /// `&mut` 没有这个问题。
    async fn transfer_context(
        &mut self,
        agent: u64,
    ) -> Result<(Arc<AgentClient>, String, Vec<String>), Error> {
        let Some(a) = self.shared.handles.lock().await.agent(agent) else {
            return Err(gone("agent"));
        };
        let exclude = self.store.config().defaults.exclude.clone();
        Ok((a, self.plugin.clone(), exclude))
    }

    /// 解析一个凭据引用。格式是 `target:<name>`——插件说「我要连 gpu-4」，
    /// 而不是「把密码给我」。明文因此从不进入 wasm。
    fn credentials(&self, reference: &str) -> trestle_core::Result<Credentials> {
        let name = reference.strip_prefix("target:").unwrap_or(reference);
        let secrets = self.store.secrets_for(name);
        if let Some(key_path) = &secrets.key_path {
            let passphrase = match &secrets.key_passphrase {
                Some(r) => Some(r.resolve()?),
                None => None,
            };
            return Credentials::load_key(key_path, passphrase.as_deref());
        }
        if let Some(password) = &secrets.password {
            return Ok(Credentials::Password(password.resolve()?));
        }
        Err(TrestleError::Config {
            path: format!("secrets.toml [targets.{name}]"),
            detail: "no password and no key_path configured for this target".into(),
        })
    }

    fn secret(&self, reference: &str) -> trestle_core::Result<String> {
        trestle_core::config::SecretRef::new(reference).resolve()
    }
}

fn split_addr(addr: &str) -> Result<(String, u16), Error> {
    match addr.rsplit_once(':') {
        Some((host, port)) => match port.parse::<u16>() {
            Ok(p) => Ok((host.to_string(), p)),
            Err(_) => Err(Error {
                kind: ErrorKind::InvalidRequest,
                detail: format!("'{addr}' does not end in a port number"),
                remedy: "use host:port".into(),
            }),
        },
        None => Err(Error {
            kind: ErrorKind::InvalidRequest,
            detail: format!("'{addr}' has no port"),
            remedy: "use host:port".into(),
        }),
    }
}

fn parse_opts(raw: &str) -> Result<trestle_core::TransferOptions, Error> {
    if raw.trim().is_empty() {
        return Ok(Default::default());
    }
    serde_json::from_str(raw).map_err(|e| Error {
        kind: ErrorKind::InvalidRequest,
        detail: format!("transfer options are not valid JSON: {e}"),
        remedy: String::new(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn addresses_split_on_the_last_colon() {
        assert_eq!(
            split_addr("127.0.0.1:11080").unwrap(),
            ("127.0.0.1".into(), 11080)
        );
        // IPv6 也不能把端口丢了。
        assert_eq!(split_addr("::1:22").unwrap(), ("::1".into(), 22));
    }

    #[test]
    fn an_address_without_a_port_is_rejected_clearly() {
        let err = split_addr("127.0.0.1").unwrap_err();
        assert!(err.detail.contains("no port"), "{}", err.detail);
        assert_eq!(err.remedy, "use host:port");
    }

    #[test]
    fn empty_transfer_options_mean_defaults() {
        let o = parse_opts("").unwrap();
        assert!(!o.sync && !o.dry_run);
    }

    #[test]
    fn errors_keep_their_remedy_separate_from_their_detail() {
        let e = error_to_wit(TrestleError::ConnectorNotReady {
            connector: "gpu-cluster".into(),
            detail: "container is not running".into(),
            remedy: "docker start vpn-proxy".into(),
        });
        assert!(matches!(e.kind, ErrorKind::NotReady));
        assert!(
            e.detail.contains("container is not running"),
            "{}",
            e.detail
        );
        // remedy 必须单独拿得到，否则上层没法把「下一步」突出显示。
        assert_eq!(e.remedy, "docker start vpn-proxy");
    }

    #[test]
    fn unknown_state_keeps_its_identity_across_the_boundary() {
        let e = error_to_wit(TrestleError::UnknownState {
            target: "gpu-4".into(),
            op: "shell".into(),
        });
        // 这个 kind 一旦在边界上丢了，插件就可能去重放一条可能已经执行过的命令。
        assert!(matches!(e.kind, ErrorKind::UnknownState));
        assert!(e.detail.contains("may have executed"), "{}", e.detail);
    }
}
