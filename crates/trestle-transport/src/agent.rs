//! 远端常驻 agent 的客户端：一条 SSH channel 上的 JSON-Lines 多路复用。
//!
//! 每个请求带一个 id，响应按 id 派发回等待者——所以慢操作（`sleep 60`）不会挡住
//! 快操作（`ping`）。这是「常驻 agent」相对「每次 ssh exec」的第二个好处，
//! 第一个是省掉每次几百 ms 的握手。
//!
//! ## 重试的诚实边界
//!
//! ```text
//! 请求还没发出去   →  重建连接后自动重放，安全
//! 请求已经发出去   →  绝不自动重放，返回 UnknownState
//! ```
//!
//! 已经发出但没拿到响应时，那条命令**可能已经在远端执行了**。自动重放意味着可能把
//! 一条 `rm -rf` 或一次训练启动跑两遍。这里把不确定性如实交给上层。

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde::de::DeserializeOwned;
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncWrite, AsyncWriteExt, BufReader};
use tokio::sync::oneshot;

use trestle_core::{Op, Result, TrestleError};

/// agent 协议版本。与 `agent-py/trestle_agent.py` 里的 `PROTOCOL_VERSION` 必须一致。
pub const PROTOCOL_VERSION: u64 = 1;

/// agent 报回来的握手信息。
#[derive(Debug, Clone, serde::Deserialize)]
pub struct AgentInfo {
    pub protocol: u64,
    pub version: String,
    pub pid: u32,
    #[serde(default)]
    pub uptime_s: u64,
    #[serde(default)]
    pub python: String,
    /// agent 自报的源码哈希。主机侧靠它判断远端跑的是不是当前这版，
    /// 从而省掉重新 attach 路径上的一次 `sha256sum` 往返。
    #[serde(default)]
    pub script_sha256: String,
}

/// 一个正在跑的远端 agent 的句柄。
pub struct AgentClient {
    target: String,
    next_id: AtomicU64,
    pending: Arc<Mutex<HashMap<u64, oneshot::Sender<Value>>>>,
    writer: tokio::sync::Mutex<Box<dyn AsyncWrite + Unpin + Send>>,
    /// 读循环结束（连接断了）时被置位。
    dead: Arc<std::sync::atomic::AtomicBool>,
    pub info: AgentInfo,
}

impl AgentClient {
    /// 在一条已经开好的双向流上接管 agent。
    ///
    /// 会等 agent 的**就绪帧**（id=0），而不是 sleep 猜时间。
    pub async fn attach<R, W>(target: &str, reader: R, writer: W, timeout: Duration) -> Result<Self>
    where
        R: AsyncRead + Unpin + Send + 'static,
        W: AsyncWrite + Unpin + Send + 'static,
    {
        let pending: Arc<Mutex<HashMap<u64, oneshot::Sender<Value>>>> =
            Arc::new(Mutex::new(HashMap::new()));
        let dead = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let (ready_tx, ready_rx) = oneshot::channel();
        pending.lock().unwrap().insert(0, ready_tx);

        tokio::spawn(read_loop(
            BufReader::new(reader),
            Arc::clone(&pending),
            Arc::clone(&dead),
        ));

        let ready = tokio::time::timeout(timeout, ready_rx).await;
        let ready = match ready {
            Ok(Ok(v)) => v,
            _ => {
                return Err(TrestleError::RemoteEnvironment {
                    target: target.to_string(),
                    detail: format!("the remote agent did not announce itself within {timeout:?}"),
                    remedy: format!("trestle doctor {target}"),
                });
            }
        };

        let info: AgentInfo = serde_json::from_value(ready["result"].clone()).map_err(|e| {
            TrestleError::Protocol {
                target: target.to_string(),
                detail: format!("malformed agent hello: {e}"),
            }
        })?;

        if info.protocol != PROTOCOL_VERSION {
            return Err(TrestleError::Protocol {
                target: target.to_string(),
                detail: format!(
                    "remote agent speaks protocol v{} but this host speaks v{PROTOCOL_VERSION}; \
                     the agent will be redeployed",
                    info.protocol
                ),
            });
        }

        Ok(Self {
            target: target.to_string(),
            next_id: AtomicU64::new(1),
            pending,
            writer: tokio::sync::Mutex::new(Box::new(writer)),
            dead,
            info,
        })
    }

    pub fn is_dead(&self) -> bool {
        self.dead.load(Ordering::SeqCst)
    }

    /// 发一个请求并等响应。
    ///
    /// `op` 只用来在出错时说清楚是什么操作没了下文——**它决定错误是不是
    /// [`TrestleError::UnknownState`]**，所以别乱传。
    pub async fn call<T: DeserializeOwned>(&self, op: Op, args: Value) -> Result<T> {
        let value = self.call_raw(op.as_str(), args).await?;
        serde_json::from_value(value).map_err(|e| TrestleError::Protocol {
            target: self.target.clone(),
            detail: format!("cannot decode {op} response: {e}"),
        })
    }

    /// 同上，但接受任意 op 名（部署期的 `stat` / `hash` / `put_chunk` 等辅助操作）。
    pub async fn call_raw(&self, op: &str, args: Value) -> Result<Value> {
        let id = self.next_id.fetch_add(1, Ordering::SeqCst);
        let (tx, rx) = oneshot::channel();
        self.pending.lock().unwrap().insert(id, tx);

        let frame = serde_json::to_string(&json!({"id": id, "op": op, "args": args}))
            .expect("request frames are always serializable");

        // 写失败 = **请求没发出去**，重放是安全的，所以这里不是 UnknownState。
        {
            let mut w = self.writer.lock().await;
            if let Err(e) = w.write_all(frame.as_bytes()).await {
                self.pending.lock().unwrap().remove(&id);
                return Err(self.not_sent(op, e));
            }
            if let Err(e) = w.write_all(b"\n").await {
                self.pending.lock().unwrap().remove(&id);
                return Err(self.not_sent(op, e));
            }
            if let Err(e) = w.flush().await {
                self.pending.lock().unwrap().remove(&id);
                return Err(self.not_sent(op, e));
            }
        }

        // 到这里请求**已经在线上了**。再失败就只能是 UnknownState。
        let response = match rx.await {
            Ok(v) => v,
            Err(_) => {
                self.pending.lock().unwrap().remove(&id);
                return Err(TrestleError::UnknownState {
                    target: self.target.clone(),
                    op: op.to_string(),
                });
            }
        };

        if response["ok"].as_bool() == Some(true) {
            return Ok(response["result"].clone());
        }

        let kind = response["error"]["kind"].as_str().unwrap_or("unknown");
        let detail = response["error"]["detail"]
            .as_str()
            .unwrap_or("no detail provided");
        Err(TrestleError::Remote {
            target: self.target.clone(),
            op: op.to_string(),
            detail: format!("{detail} [{kind}]"),
        })
    }

    fn not_sent(&self, op: &str, e: std::io::Error) -> TrestleError {
        TrestleError::Unreachable {
            target: self.target.clone(),
            endpoint: "agent channel".into(),
            connector: String::new(),
            detail: format!("could not send '{op}' to the remote agent: {e}"),
            remedy: format!("trestle doctor {}", self.target),
        }
    }

    /// 只报可公开的部分——写端与等待表里没有秘密，但也没有诊断价值。
    fn debug_summary(&self) -> String {
        format!(
            "AgentClient({}, pid {}, v{}, {})",
            self.target,
            self.info.pid,
            self.info.version,
            if self.is_dead() { "dead" } else { "alive" }
        )
    }

    pub async fn ping(&self) -> Result<AgentInfo> {
        let v = self.call_raw("ping", json!({})).await?;
        serde_json::from_value(v).map_err(|e| TrestleError::Protocol {
            target: self.target.clone(),
            detail: format!("malformed ping response: {e}"),
        })
    }
}

impl std::fmt::Debug for AgentClient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.debug_summary())
    }
}

async fn read_loop<R: AsyncRead + Unpin>(
    reader: BufReader<R>,
    pending: Arc<Mutex<HashMap<u64, oneshot::Sender<Value>>>>,
    dead: Arc<std::sync::atomic::AtomicBool>,
) {
    let mut lines = reader.lines();
    // 读到 None 或出错都表示连接没了 —— 两种情况处理方式一样，所以一个 while let 就够。
    while let Ok(Some(line)) = lines.next_line().await {
        if line.trim().is_empty() {
            continue;
        }
        let Ok(value) = serde_json::from_str::<Value>(&line) else {
            tracing::warn!(frame = %truncate(&line), "agent sent an unparseable frame");
            continue;
        };
        let Some(id) = value["id"].as_u64() else {
            continue;
        };
        if let Some(tx) = pending.lock().unwrap().remove(&id) {
            let _ = tx.send(value);
        }
    }
    dead.store(true, Ordering::SeqCst);
    // 叫醒所有还在等的调用方——它们会得到 UnknownState，因为请求确实已经发出去了。
    pending.lock().unwrap().clear();
}

fn truncate(s: &str) -> String {
    if s.len() <= 200 {
        s.to_string()
    } else {
        format!("{}…", &s[..200])
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::duplex;

    /// 一个按脚本回话的假 agent。
    fn spawn_fake_agent(
        mut server: tokio::io::DuplexStream,
        script: Vec<String>,
    ) -> tokio::task::JoinHandle<Vec<String>> {
        tokio::spawn(async move {
            let hello = json!({
                "id": 0, "ok": true,
                "result": {"protocol": 1, "version": "0.1.0", "pid": 1234, "uptime_s": 0, "python": "3.12.0"}
            });
            server
                .write_all(format!("{hello}\n").as_bytes())
                .await
                .unwrap();

            let (r, mut w) = tokio::io::split(server);
            let mut lines = BufReader::new(r).lines();
            let mut seen = Vec::new();
            for reply in script {
                match lines.next_line().await {
                    Ok(Some(line)) => {
                        seen.push(line);
                        w.write_all(format!("{reply}\n").as_bytes()).await.unwrap();
                    }
                    _ => break,
                }
            }
            seen
        })
    }

    async fn connect(script: Vec<String>) -> (AgentClient, tokio::task::JoinHandle<Vec<String>>) {
        let (client_side, server_side) = duplex(64 * 1024);
        let server = spawn_fake_agent(server_side, script);
        let (r, w) = tokio::io::split(client_side);
        let client = AgentClient::attach("gpu-4", r, w, Duration::from_secs(5))
            .await
            .unwrap();
        (client, server)
    }

    #[tokio::test]
    async fn handshake_reads_the_ready_frame() {
        let (client, _) = connect(vec![]).await;
        assert_eq!(client.info.protocol, 1);
        assert_eq!(client.info.pid, 1234);
    }

    #[tokio::test]
    async fn a_successful_call_returns_the_result() {
        let reply = json!({"id": 1, "ok": true, "result": {"content": "hi", "total_lines": 1, "truncated": false}});
        let (client, server) = connect(vec![reply.to_string()]).await;

        let res: trestle_core::ReadResponse = client
            .call(Op::Read, json!({"path": "/tmp/x"}))
            .await
            .unwrap();
        assert_eq!(res.content, "hi");

        let seen = server.await.unwrap();
        let sent: Value = serde_json::from_str(&seen[0]).unwrap();
        assert_eq!(sent["op"], "read");
        assert_eq!(sent["args"]["path"], "/tmp/x");
    }

    #[tokio::test]
    async fn a_remote_error_keeps_its_detail_and_kind() {
        let reply = json!({"id": 1, "ok": false, "error": {"kind": "not_found", "detail": "no such file: /nope"}});
        let (client, _) = connect(vec![reply.to_string()]).await;

        let err = client
            .call_raw("read", json!({"path": "/nope"}))
            .await
            .unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("/nope") && msg.contains("not_found"), "{msg}");
    }

    #[tokio::test]
    async fn responses_are_dispatched_by_id_not_by_arrival_order() {
        // 先回 id=2 再回 id=1：多路复用下这是正常情况，不能串台。
        let (client_side, mut server_side) = duplex(64 * 1024);
        tokio::spawn(async move {
            let hello = json!({"id": 0, "ok": true, "result": {"protocol": 1, "version": "0.1.0", "pid": 1, "uptime_s": 0, "python": "3.12"}});
            server_side
                .write_all(format!("{hello}\n").as_bytes())
                .await
                .unwrap();
            let (r, mut w) = tokio::io::split(server_side);
            let mut lines = BufReader::new(r).lines();
            lines.next_line().await.unwrap();
            lines.next_line().await.unwrap();
            let second = json!({"id": 2, "ok": true, "result": {"marker": "second"}});
            let first = json!({"id": 1, "ok": true, "result": {"marker": "first"}});
            w.write_all(format!("{second}\n{first}\n").as_bytes())
                .await
                .unwrap();
        });

        let (r, w) = tokio::io::split(client_side);
        let client = Arc::new(
            AgentClient::attach("gpu-4", r, w, Duration::from_secs(5))
                .await
                .unwrap(),
        );

        let c1 = Arc::clone(&client);
        let one = tokio::spawn(async move { c1.call_raw("ping", json!({})).await });
        // 保证 id=1 先于 id=2 发出。
        tokio::time::sleep(Duration::from_millis(50)).await;
        let c2 = Arc::clone(&client);
        let two = tokio::spawn(async move { c2.call_raw("ping", json!({})).await });

        assert_eq!(one.await.unwrap().unwrap()["marker"], "first");
        assert_eq!(two.await.unwrap().unwrap()["marker"], "second");
    }

    #[tokio::test]
    async fn a_dropped_connection_yields_unknown_state_not_a_silent_retry() {
        let (client_side, server_side) = duplex(64 * 1024);
        tokio::spawn(async move {
            let mut server = server_side;
            let hello = json!({"id": 0, "ok": true, "result": {"protocol": 1, "version": "0.1.0", "pid": 1, "uptime_s": 0, "python": "3.12"}});
            server
                .write_all(format!("{hello}\n").as_bytes())
                .await
                .unwrap();
            let (r, _w) = tokio::io::split(server);
            let mut lines = BufReader::new(r).lines();
            // 收下请求然后直接把连接扔掉——模拟 SSH transport 被掐断。
            let _ = lines.next_line().await;
        });

        let (r, w) = tokio::io::split(client_side);
        let client = AgentClient::attach("gpu-4", r, w, Duration::from_secs(5))
            .await
            .unwrap();

        let err = client
            .call_raw("shell", json!({"command": "rm -rf /tmp/x"}))
            .await
            .unwrap_err();

        // 这条断言是整个模块存在的理由：命令可能已经在远端跑了，绝不能悄悄重放。
        assert!(
            matches!(err, TrestleError::UnknownState { .. }),
            "expected UnknownState, got: {err}"
        );
        assert!(!err.is_retryable());
    }

    #[tokio::test]
    async fn a_version_mismatch_is_rejected_at_handshake() {
        let (client_side, mut server_side) = duplex(4096);
        tokio::spawn(async move {
            let hello = json!({"id": 0, "ok": true, "result": {"protocol": 99, "version": "9.9.9", "pid": 1, "uptime_s": 0, "python": "3.12"}});
            server_side
                .write_all(format!("{hello}\n").as_bytes())
                .await
                .unwrap();
            tokio::time::sleep(Duration::from_secs(1)).await;
        });
        let (r, w) = tokio::io::split(client_side);
        let err = AgentClient::attach("gpu-4", r, w, Duration::from_secs(5))
            .await
            .unwrap_err();
        assert!(err.to_string().contains("protocol v99"), "{err}");
    }
}
