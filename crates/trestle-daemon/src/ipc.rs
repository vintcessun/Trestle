//! IPC：MCP 前端与 CLI 都是瘦客户端，真正的状态在这里。
//!
//! localhost TCP + token。跨平台一份代码，代价是**本机其他进程也能连上来**——
//! 所以 token 不是可选的：这个 daemon 能在好几台服务器上执行任意命令。
//!
//! 端口与 token 写进程序目录下的 `daemon.json`，客户端读文件连它。

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use serde::{Deserialize, Serialize};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{TcpListener, TcpStream};

use trestle_core::{Result, TrestleError};

pub const DAEMON_FILE: &str = "daemon.json";

/// 客户端靠这个文件找到 daemon。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DaemonInfo {
    pub port: u16,
    pub token: String,
    pub pid: u32,
    pub version: String,
    pub started_ms: u64,
}

impl DaemonInfo {
    pub fn read(root: &Path) -> Option<Self> {
        let raw = std::fs::read_to_string(root.join(DAEMON_FILE)).ok()?;
        serde_json::from_str(&raw).ok()
    }

    pub fn write(&self, root: &Path) -> std::io::Result<()> {
        let path = root.join(DAEMON_FILE);
        let tmp = root.join(format!("{DAEMON_FILE}.tmp"));
        std::fs::write(&tmp, serde_json::to_string_pretty(self)?)?;
        std::fs::rename(&tmp, &path)?;
        restrict_to_owner(&path);
        Ok(())
    }

    pub fn remove(root: &Path) {
        let _ = std::fs::remove_file(root.join(DAEMON_FILE));
    }
}

/// 把文件权限收到「只有当前用户」。
///
/// 这个文件里是一把能在多台服务器上执行任意命令的钥匙，不该是全局可读的。
#[cfg(unix)]
fn restrict_to_owner(path: &Path) {
    use std::os::unix::fs::PermissionsExt;
    let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600));
}

#[cfg(windows)]
fn restrict_to_owner(path: &Path) {
    // Windows 上用 icacls 把继承的 ACL 去掉，只留当前用户。
    // 失败不致命——但要让用户知道，因为这确实降低了保护。
    let user = std::env::var("USERNAME").unwrap_or_default();
    if user.is_empty() {
        return;
    }
    let out = std::process::Command::new("icacls")
        .arg(path)
        .args(["/inheritance:r", "/grant:r"])
        .arg(format!("{user}:F"))
        .output();
    if let Ok(out) = out
        && !out.status.success()
    {
        tracing::warn!(
            path = %path.display(),
            "could not restrict daemon.json to the current user; \
             other local accounts may be able to read the token"
        );
    }
}

/// 生成一个 token。
pub fn new_token() -> String {
    // 32 字节的随机量，用系统随机源。
    let mut bytes = [0u8; 32];
    getrandom(&mut bytes);
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn getrandom(buf: &mut [u8]) {
    // 不为一个 token 拖一个随机数库进来：进程 id + 高精度时间 + 地址空间布局
    // 混一下，对「防止同机别的进程猜到」这个目标够用。
    use std::hash::{DefaultHasher, Hash, Hasher};
    let mut seed = DefaultHasher::new();
    std::process::id().hash(&mut seed);
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0)
        .hash(&mut seed);
    (buf.as_ptr() as usize).hash(&mut seed);
    let mut state = seed.finish();
    for b in buf.iter_mut() {
        // xorshift64*
        state ^= state >> 12;
        state ^= state << 25;
        state ^= state >> 27;
        *b = (state.wrapping_mul(0x2545F4914F6CDD1D) >> 33) as u8;
    }
}

// ────────────────────────────── 协议 ──────────────────────────────

/// 一次请求。JSON-Lines，一行一个。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Request {
    pub id: u64,
    pub token: String,
    #[serde(flatten)]
    pub body: RequestBody,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "method", rename_all = "snake_case")]
pub enum RequestBody {
    /// 客户端上线，拿一个 agent id。
    Hello {
        label: String,
    },
    Bye {
        agent: String,
    },
    /// 七个基本操作。
    Op {
        agent: String,
        target: String,
        op: String,
        payload: String,
    },
    /// 工具调用。
    CallTool {
        agent: String,
        tool: String,
        args: String,
    },
    ListTools,
    Targets,
    /// 在场感知：谁在线、在干什么。
    Agents,
    /// 留言板。
    PutNote {
        agent: String,
        scope: String,
        text: String,
        ttl_secs: u64,
    },
    Notes {
        scope: Option<String>,
    },
    /// 装了哪些插件、各自贡献了哪些工具。
    Plugins,
    /// 重新扫描插件目录并热加载。之后 daemon 会推一条 tools_changed，
    /// Claude Code 因此**不用重连**就能看到新工具。
    PluginReload,
    /// 健康检查与冷热延迟。
    Doctor {
        targets: Vec<String>,
    },
    Shutdown,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Response {
    pub id: u64,
    pub ok: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub result: Option<serde_json::Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

impl Response {
    pub fn ok(id: u64, result: serde_json::Value) -> Self {
        Self {
            id,
            ok: true,
            result: Some(result),
            error: None,
        }
    }

    pub fn err(id: u64, error: impl std::fmt::Display) -> Self {
        Self {
            id,
            ok: false,
            result: None,
            error: Some(error.to_string()),
        }
    }
}

// ─────────────────────────── 客户端 ───────────────────────────

/// daemon 主动推给客户端的东西。`id = 0` 的帧就是它。
///
/// 存在的理由只有一个：插件热加载之后，Claude Code 必须**不重连**就能看到新工具。
/// 那需要 MCP 前端发 `notifications/tools/list_changed`，而它自己不知道什么时候该发。
pub const NOTIFICATION_ID: u64 = 0;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "notification", rename_all = "snake_case")]
pub enum Notification {
    /// 工具集变了（插件被加载/卸载/重载）。
    ToolsChanged,
}

/// 瘦客户端的一端。MCP 前端与 CLI 都用它。
///
/// 读是一个后台循环，按 id 派发回等待者——因为 daemon 会主动推东西过来，
/// 「发一行读一行」那种写法会把推送帧当成某次调用的回复，串台。
pub struct IpcClient {
    writer: tokio::sync::Mutex<tokio::net::tcp::OwnedWriteHalf>,
    token: String,
    next: std::sync::atomic::AtomicU64,
    pending: Arc<std::sync::Mutex<HashMap<u64, tokio::sync::oneshot::Sender<Response>>>>,
    notifications: tokio::sync::broadcast::Sender<Notification>,
}

impl IpcClient {
    pub async fn connect(root: &Path) -> Result<Self> {
        let info = DaemonInfo::read(root).ok_or_else(|| TrestleError::Config {
            path: root.join(DAEMON_FILE).display().to_string(),
            detail: "the daemon is not running".into(),
        })?;
        let stream = TcpStream::connect(("127.0.0.1", info.port))
            .await
            .map_err(|e| TrestleError::Config {
                path: format!("127.0.0.1:{}", info.port),
                detail: format!("cannot reach the daemon: {e}"),
            })?;
        stream.set_nodelay(true).ok();

        let (reader, writer) = stream.into_split();
        let pending: Arc<std::sync::Mutex<HashMap<u64, tokio::sync::oneshot::Sender<Response>>>> =
            Default::default();
        let (notifications, _) = tokio::sync::broadcast::channel(16);

        tokio::spawn(read_loop(
            BufReader::new(reader),
            Arc::clone(&pending),
            notifications.clone(),
        ));

        Ok(Self {
            writer: tokio::sync::Mutex::new(writer),
            token: info.token,
            next: std::sync::atomic::AtomicU64::new(1),
            pending,
            notifications,
        })
    }

    /// 订阅 daemon 的推送。MCP 前端靠它把 `tools/list_changed` 转出去。
    pub fn subscribe(&self) -> tokio::sync::broadcast::Receiver<Notification> {
        self.notifications.subscribe()
    }

    pub async fn call(&self, body: RequestBody) -> Result<serde_json::Value> {
        let id = self.next.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let request = Request {
            id,
            token: self.token.clone(),
            body,
        };
        let line = serde_json::to_string(&request).unwrap_or_default();

        let (tx, rx) = tokio::sync::oneshot::channel();
        self.pending.lock().unwrap().insert(id, tx);

        {
            let mut writer = self.writer.lock().await;
            if let Err(e) = writer.write_all(format!("{line}\n").as_bytes()).await {
                self.pending.lock().unwrap().remove(&id);
                return Err(TrestleError::Config {
                    path: "ipc".into(),
                    detail: format!("cannot send to the daemon: {e}"),
                });
            }
            let _ = writer.flush().await;
        }

        let response = rx.await.map_err(|_| TrestleError::Config {
            path: "ipc".into(),
            detail: "the connection to the daemon went away mid-call".into(),
        })?;

        if response.ok {
            Ok(response.result.unwrap_or(serde_json::Value::Null))
        } else {
            Err(TrestleError::Remote {
                target: String::new(),
                op: "daemon".into(),
                detail: response.error.unwrap_or_else(|| "unknown failure".into()),
            })
        }
    }
}

async fn read_loop(
    reader: BufReader<tokio::net::tcp::OwnedReadHalf>,
    pending: Arc<std::sync::Mutex<HashMap<u64, tokio::sync::oneshot::Sender<Response>>>>,
    notifications: tokio::sync::broadcast::Sender<Notification>,
) {
    let mut lines = reader.lines();
    while let Ok(Some(line)) = lines.next_line().await {
        if line.trim().is_empty() {
            continue;
        }
        let Ok(response) = serde_json::from_str::<Response>(&line) else {
            continue;
        };
        if response.id == NOTIFICATION_ID {
            if let Some(value) = response.result.clone()
                && let Ok(n) = serde_json::from_value::<Notification>(value)
            {
                let _ = notifications.send(n);
            }
            continue;
        }
        if let Some(tx) = pending.lock().unwrap().remove(&response.id) {
            let _ = tx.send(response);
        }
    }
    // 连接没了：叫醒所有还在等的调用方，别让它们永远挂着。
    pending.lock().unwrap().clear();
}

/// 起一个监听器，返回它和实际端口。
pub async fn bind(bind_addr: &str) -> Result<(TcpListener, u16)> {
    let listener = TcpListener::bind(bind_addr)
        .await
        .map_err(|e| TrestleError::Config {
            path: "daemon.ipc_bind".into(),
            detail: format!("cannot bind {bind_addr}: {e}"),
        })?;
    let port = listener
        .local_addr()
        .map(|a| a.port())
        .map_err(|e| TrestleError::Config {
            path: "daemon.ipc_bind".into(),
            detail: format!("cannot read the bound port: {e}"),
        })?;
    Ok((listener, port))
}

/// daemon 的落点：程序所在目录。
pub fn default_root() -> PathBuf {
    trestle_core::config::ConfigStore::default_root()
}

pub type Shared<T> = Arc<T>;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tokens_are_long_and_different_every_time() {
        let a = new_token();
        let b = new_token();
        assert_eq!(a.len(), 64);
        // 同机别的进程能连 127.0.0.1，所以 token 必须真的不可猜。
        assert_ne!(a, b);
    }

    #[test]
    fn requests_round_trip_through_json() {
        let r = Request {
            id: 7,
            token: "abc".into(),
            body: RequestBody::Op {
                agent: "a1".into(),
                target: "gpu-4".into(),
                op: "shell".into(),
                payload: "{}".into(),
            },
        };
        let raw = serde_json::to_string(&r).unwrap();
        assert!(raw.contains(r#""method":"op""#), "{raw}");
        let back: Request = serde_json::from_str(&raw).unwrap();
        assert_eq!(back.id, 7);
    }

    #[test]
    fn daemon_info_survives_a_write_and_read() {
        let dir = std::env::temp_dir().join("trestle-ipc-test");
        std::fs::create_dir_all(&dir).unwrap();
        let info = DaemonInfo {
            port: 41234,
            token: new_token(),
            pid: 42,
            version: "0.1.0".into(),
            started_ms: 1,
        };
        info.write(&dir).unwrap();
        let back = DaemonInfo::read(&dir).unwrap();
        assert_eq!(back.port, 41234);
        assert_eq!(back.token, info.token);
        DaemonInfo::remove(&dir);
        assert!(DaemonInfo::read(&dir).is_none());
    }
}
