//! SSH 会话：在一条已经拨通的字节流上建立 SSH，然后开 channel。
//!
//! 这一层对上层是不可见的——connector 用它，但基本操作的调用方永远不知道下面是 SSH。

use std::sync::Arc;
use std::time::Duration;

use russh::client::{self, AuthResult, Handle, Handler};
use russh::keys::{PrivateKey, PrivateKeyWithHashAlg};
use russh::{Channel, ChannelMsg, Disconnect};
use tokio::io::{AsyncRead, AsyncWrite};

use trestle_core::{Result, TrestleError};

/// 认证方式。
#[derive(Clone)]
pub enum Credentials {
    Password(String),
    /// 私钥 + 可选的口令。
    PublicKey {
        key: Arc<PrivateKey>,
        path: String,
    },
}

impl Credentials {
    pub fn method_name(&self) -> &'static str {
        match self {
            Credentials::Password(_) => "password",
            Credentials::PublicKey { .. } => "publickey",
        }
    }

    /// 从磁盘读一把私钥。
    pub fn load_key(path: &str, passphrase: Option<&str>) -> Result<Self> {
        let expanded = expand_home(path);
        let key = russh::keys::load_secret_key(&expanded, passphrase).map_err(|e| {
            TrestleError::Config {
                path: format!("key_path = {path}"),
                detail: format!("cannot load private key {expanded}: {e}"),
            }
        })?;
        Ok(Credentials::PublicKey {
            key: Arc::new(key),
            path: path.to_string(),
        })
    }
}

impl std::fmt::Debug for Credentials {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Credentials::Password(_) => f.write_str("Password(<redacted>)"),
            Credentials::PublicKey { path, .. } => write!(f, "PublicKey({path})"),
        }
    }
}

/// 展开路径里的 `~`。
pub fn expand_home(path: &str) -> String {
    if let Some(rest) = path.strip_prefix("~/").or_else(|| path.strip_prefix("~\\"))
        && let Some(home) = home_dir()
    {
        return format!("{home}/{rest}");
    }
    path.to_string()
}

fn home_dir() -> Option<String> {
    std::env::var("USERPROFILE")
        .ok()
        .or_else(|| std::env::var("HOME").ok())
}

/// 主机公钥策略。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HostKeyPolicy {
    /// 查 `~/.ssh/known_hosts`：匹配放行，**不匹配直接拒绝**，没见过的首次接受。
    KnownHostsTofu,
    /// 一律接受。只在明确知道自己在干什么时用。
    AcceptAny,
}

/// 记下这次握手看到的主机公钥指纹，供上层落进事件与状态。
#[derive(Debug, Clone, Default)]
pub struct HostKeyRecord {
    pub fingerprint: Option<String>,
    pub first_seen: bool,
}

struct ClientHandler {
    host: String,
    port: u16,
    policy: HostKeyPolicy,
    record: Arc<std::sync::Mutex<HostKeyRecord>>,
}

impl Handler for ClientHandler {
    type Error = russh::Error;

    async fn check_server_key(
        &mut self,
        server_public_key: &russh::keys::ssh_key::PublicKey,
    ) -> std::result::Result<bool, Self::Error> {
        let fingerprint = server_public_key
            .fingerprint(Default::default())
            .to_string();
        let mut record = self.record.lock().unwrap();
        record.fingerprint = Some(fingerprint);

        match self.policy {
            HostKeyPolicy::AcceptAny => Ok(true),
            HostKeyPolicy::KnownHostsTofu => {
                match russh::keys::check_known_hosts(&self.host, self.port, server_public_key) {
                    // 见过且一致 → 放行。
                    Ok(true) => Ok(true),
                    // 没见过 → 首次接受（TOFU），记下来让上层把指纹报出去。
                    Ok(false) => {
                        record.first_seen = true;
                        Ok(true)
                    }
                    // 见过但**变了** → 唯一必须硬拒的情况。
                    Err(_) => Ok(false),
                }
            }
        }
    }
}

/// 一条活着的 SSH 会话。
pub struct SshSession {
    handle: Handle<ClientHandler>,
    record: Arc<std::sync::Mutex<HostKeyRecord>>,
    target: String,
}

/// 「连的是谁」。
///
/// 把四个字符串打成一个参数，不是为了好看：它们**总是一起出现、一起被错**——
/// 分开传的时候 host 和 name 调换了顺序编译器一句话都不会说。
#[derive(Debug, Clone, Copy)]
pub struct SshTarget<'a> {
    /// 机器的名字（错误消息里用）。
    pub name: &'a str,
    pub host: &'a str,
    pub port: u16,
    pub user: &'a str,
}

impl SshSession {
    /// 在一条已经拨通的流上建立 SSH 并认证。
    pub async fn connect<S>(
        stream: S,
        who: &SshTarget<'_>,
        creds: &Credentials,
        policy: HostKeyPolicy,
        keepalive: Duration,
    ) -> Result<Self>
    where
        S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
    {
        let config = Arc::new(client::Config {
            // 主动探活：睡眠唤醒 / VPN 抖动之后要能主动发现连接死了，
            // 而不是等下一次调用才发现。
            keepalive_interval: Some(keepalive),
            keepalive_max: 3,
            ..Default::default()
        });

        let SshTarget {
            name: target,
            host,
            port,
            user,
        } = *who;

        let record = Arc::new(std::sync::Mutex::new(HostKeyRecord::default()));
        let handler = ClientHandler {
            host: host.to_string(),
            port,
            policy,
            record: Arc::clone(&record),
        };

        let mut handle = client::connect_stream(config, stream, handler)
            .await
            .map_err(|e| host_key_or_protocol_error(e, target, host, port))?;

        let result = match creds {
            Credentials::Password(pw) => handle
                .authenticate_password(user, pw.clone())
                .await
                .map_err(|e| TrestleError::Protocol {
                    target: target.to_string(),
                    detail: format!("password authentication errored: {e}"),
                })?,
            Credentials::PublicKey { key, .. } => {
                let hash_alg = handle
                    .best_supported_rsa_hash()
                    .await
                    .ok()
                    .flatten()
                    .flatten();
                handle
                    .authenticate_publickey(
                        user,
                        PrivateKeyWithHashAlg::new(Arc::clone(key), hash_alg),
                    )
                    .await
                    .map_err(|e| TrestleError::Protocol {
                        target: target.to_string(),
                        detail: format!("publickey authentication errored: {e}"),
                    })?
            }
        };

        match result {
            AuthResult::Success => Ok(Self {
                handle,
                record,
                target: target.to_string(),
            }),
            AuthResult::Failure {
                remaining_methods, ..
            } => Err(TrestleError::AuthFailed {
                target: target.to_string(),
                user: user.to_string(),
                method: creds.method_name().to_string(),
                detail: format!("server accepts: {remaining_methods:?}"),
            }),
        }
    }

    pub fn host_key(&self) -> HostKeyRecord {
        self.record.lock().unwrap().clone()
    }

    pub fn is_closed(&self) -> bool {
        self.handle.is_closed()
    }

    /// 开一个 session channel。
    pub async fn open_session(&self) -> Result<Channel<client::Msg>> {
        self.handle
            .channel_open_session()
            .await
            .map_err(|e| TrestleError::Protocol {
                target: self.target.clone(),
                detail: format!("cannot open session channel: {e}"),
            })
    }

    /// 开一条到远端 `(host, port)` 的转发通道。端口映射靠它。
    pub async fn open_direct_tcpip(&self, host: &str, port: u16) -> Result<Channel<client::Msg>> {
        self.handle
            .channel_open_direct_tcpip(host, port as u32, "127.0.0.1", 0)
            .await
            .map_err(|e| TrestleError::Remote {
                target: self.target.clone(),
                op: "forward".into(),
                detail: format!(
                    "the remote side refused to open a tunnel to {host}:{port}: {e}. \
                     Is something actually listening there?"
                ),
            })
    }

    /// 跑一条命令并收全部输出。用于部署期的探测（`uv --version` 之类），
    /// **不是**基本操作里的 `shell`——那个走常驻 agent。
    pub async fn exec_capture(&self, command: &str) -> Result<ExecOutput> {
        let started = std::time::Instant::now();
        let mut channel = self.open_session().await?;
        let opened = started.elapsed();

        // 显式走**登录** shell，而不是 SSH 默认的 `$SHELL -c`。
        //
        // 两个理由，第二个是实测出来的：
        //   * 登录 shell 才有用户平时的 PATH（`~/.local/bin` 里的 uv 就靠它）。
        //   * gpu-1 上 `bash -c true` 要 1.51s 而 `bash -lc true` 是 0.00s ——
        //     非交互非登录的 bash 会去读 BASH_ENV 指向的重初始化脚本。部署一次要开
        //     五六个 channel，这一项就吃掉 8 秒。
        let wrapped = format!("/bin/bash -lc {}", shell_quote(command));
        channel
            .exec(true, wrapped.as_bytes())
            .await
            .map_err(|e| TrestleError::Protocol {
                target: self.target.clone(),
                detail: format!("exec failed: {e}"),
            })?;

        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        let mut exit_code = None;
        let mut saw_eof = false;
        while let Some(msg) = channel.wait().await {
            match msg {
                ChannelMsg::Data { data } => stdout.extend_from_slice(&data),
                ChannelMsg::ExtendedData { data, ext: 1 } => stderr.extend_from_slice(&data),
                ChannelMsg::ExitStatus { exit_status } => exit_code = Some(exit_status as i32),
                ChannelMsg::Eof => saw_eof = true,
                ChannelMsg::Close => break,
                _ => {}
            }
            // 拿到退出码和 EOF 就已经有全部信息了，不必再等服务端发 Close。
            //
            // 这不是微优化：等 Close 时对端往往在等我们先关，实测每条命令要多花
            // ~1.8s（gpu-1 经 VPN），而部署一次要开七八个 channel —— 13s 的部署里
            // 有 11s 是白等的。反过来，只等 Eof 就收工又会漏掉退出码，因为
            // ExitStatus 常常在 Eof 之后才到。两个都拿到才是正确的收工条件。
            if saw_eof && exit_code.is_some() {
                break;
            }
        }
        // 我们先关，让对端立刻释放 channel。
        let _ = channel.close().await;

        tracing::debug!(
            target = %self.target,
            open_ms = opened.as_millis(),
            total_ms = started.elapsed().as_millis(),
            command = %command.chars().take(60).collect::<String>(),
            "exec"
        );

        Ok(ExecOutput {
            exit_code: exit_code.unwrap_or(-1),
            stdout: String::from_utf8_lossy(&stdout).into_owned(),
            stderr: String::from_utf8_lossy(&stderr).into_owned(),
        })
    }

    pub async fn disconnect(&self) {
        let _ = self
            .handle
            .disconnect(Disconnect::ByApplication, "bye", "en")
            .await;
    }
}

#[derive(Debug, Clone)]
pub struct ExecOutput {
    pub exit_code: i32,
    pub stdout: String,
    pub stderr: String,
}

impl ExecOutput {
    pub fn ok(&self) -> bool {
        self.exit_code == 0
    }

    /// stdout 去掉首尾空白。探测类命令几乎都要这个。
    pub fn trimmed(&self) -> &str {
        self.stdout.trim()
    }
}

/// 把一段命令包成一个 POSIX shell 的单引号参数。
pub fn shell_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', r"'\''"))
}

/// 主机公钥变了要说得非常清楚——这可能是中间人，也可能只是服务器重装了。
fn host_key_or_protocol_error(
    err: russh::Error,
    target: &str,
    host: &str,
    port: u16,
) -> TrestleError {
    let detail = err.to_string();
    if detail.contains("key") && detail.contains("chang") {
        return TrestleError::Protocol {
            target: target.to_string(),
            detail: format!(
                "host key for {host}:{port} does not match ~/.ssh/known_hosts. \
                 This is either a man-in-the-middle or the server was rebuilt. \
                 Verify out-of-band, then remove the stale line from known_hosts."
            ),
        };
    }
    TrestleError::Protocol {
        target: target.to_string(),
        detail: format!("SSH handshake with {host}:{port} failed: {detail}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn credentials_never_print_the_password() {
        let creds = Credentials::Password("hunter2".into());
        assert_eq!(format!("{creds:?}"), "Password(<redacted>)");
        assert!(!format!("{creds:?}").contains("hunter2"));
    }

    #[test]
    fn tilde_expands_to_the_home_directory() {
        let expanded = expand_home("~/.ssh/id_ed25519");
        assert!(!expanded.starts_with('~'), "{expanded}");
        assert!(expanded.ends_with(".ssh/id_ed25519"), "{expanded}");
    }

    #[test]
    fn absolute_paths_pass_through_untouched() {
        assert_eq!(expand_home("C:/keys/id"), "C:/keys/id");
        assert_eq!(expand_home("/home/x/id"), "/home/x/id");
    }

    #[test]
    fn a_missing_key_file_names_the_path_it_tried() {
        let err = Credentials::load_key("/definitely/not/here/id_ed25519", None).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("/definitely/not/here/id_ed25519"), "{msg}");
    }
}
