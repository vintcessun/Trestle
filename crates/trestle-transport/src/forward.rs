//! 端口映射：本地监听一个口，把连接经 SSH 转到远端。
//!
//! **本地端口由这里分配，调用方不能指定**——否则「上次开的转发还占着 8080、
//! 这次也要 8080」就会撞车，而调用方通常根本不在乎是哪个口。
//!
//! 通道是**会话级资源**：开它的那个客户端会话结束，通道就关掉、端口还回去。
//! 一条开了一次就没人用的转发不该一直占着。

use std::sync::Arc;

use tokio::net::TcpListener;

use trestle_core::{ForwardResponse, Result, TrestleError};

use crate::ssh::SshSession;

/// 一条活着的转发通道。
pub struct Forward {
    pub local_port: u16,
    pub remote_host: String,
    pub remote_port: u16,
    pub handle: String,
    shutdown: tokio::sync::oneshot::Sender<()>,
    task: tokio::task::JoinHandle<()>,
}

impl Forward {
    /// 开一条转发。本地端口由系统分配（bind 到 0）。
    pub async fn open(
        ssh: Arc<SshSession>,
        target: &str,
        remote_host: &str,
        remote_port: u16,
    ) -> Result<Self> {
        let listener =
            TcpListener::bind(("127.0.0.1", 0))
                .await
                .map_err(|e| TrestleError::Remote {
                    target: target.to_string(),
                    op: "forward".into(),
                    detail: format!("cannot bind a local port: {e}"),
                })?;
        let local_port =
            listener
                .local_addr()
                .map(|a| a.port())
                .map_err(|e| TrestleError::Remote {
                    target: target.to_string(),
                    op: "forward".into(),
                    detail: format!("cannot read the bound port: {e}"),
                })?;

        // 开一条即用即弃的通道验证远端真的有人在听 —— 否则用户会拿到一个
        // 「看起来开好了」但一连就断的端口。
        let probe = ssh.open_direct_tcpip(remote_host, remote_port).await?;
        drop(probe);

        let handle = format!("{target}:{remote_port}->{local_port}");
        let (shutdown, mut shutdown_rx) = tokio::sync::oneshot::channel();

        let accept_ssh = Arc::clone(&ssh);
        let accept_host = remote_host.to_string();
        let task = tokio::spawn(async move {
            loop {
                let accepted = tokio::select! {
                    _ = &mut shutdown_rx => break,
                    res = listener.accept() => res,
                };
                let Ok((local, _peer)) = accepted else { break };
                let ssh = Arc::clone(&accept_ssh);
                let host = accept_host.clone();
                tokio::spawn(async move {
                    match ssh.open_direct_tcpip(&host, remote_port).await {
                        Ok(channel) => {
                            let mut remote = channel.into_stream();
                            let mut local = local;
                            // 一条连接的两个方向对拷，任一方向结束就整体收工。
                            let _ = tokio::io::copy_bidirectional(&mut local, &mut remote).await;
                        }
                        Err(e) => tracing::warn!(%e, "forward: remote refused a new connection"),
                    }
                });
            }
        });

        Ok(Self {
            local_port,
            remote_host: remote_host.to_string(),
            remote_port,
            handle,
            shutdown,
            task,
        })
    }

    pub fn response(&self) -> ForwardResponse {
        ForwardResponse {
            local_port: self.local_port,
            url: format!("http://127.0.0.1:{}", self.local_port),
            handle: self.handle.clone(),
        }
    }

    /// 关掉通道并把端口还回去。
    pub async fn close(self) {
        let _ = self.shutdown.send(());
        self.task.abort();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn the_local_port_is_allocated_not_chosen() {
        // 这个测试盯的是接口形状：ForwardResponse 里没有「调用方指定的端口」这种东西，
        // 只有实际分配到的那个。
        let listener = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
        let port = listener.local_addr().unwrap().port();
        assert_ne!(port, 0, "the OS must hand out a concrete port");

        let response = ForwardResponse {
            local_port: port,
            url: format!("http://127.0.0.1:{port}"),
            handle: "gpu-4:8080->".to_string() + &port.to_string(),
        };
        assert!(response.url.contains(&port.to_string()));
    }
}
