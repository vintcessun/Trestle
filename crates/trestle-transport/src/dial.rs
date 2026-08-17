//! 拨号：直连 TCP 与 SOCKS5 CONNECT。
//!
//! 这一层的唯一职责是**交出一条已经连到 (host, port) 的字节流**。SSH 层拿到流之后
//! 完全不关心它从哪来——于是「经 VPN」不是一种特例代码，只是换一种拨号方式。

use std::time::Duration;

use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::net::TcpStream;

use trestle_core::{Result, TrestleError};

/// 一条能喂给 SSH 层的双向流。
pub trait Stream: AsyncRead + AsyncWrite + Unpin + Send + 'static {}
impl<T: AsyncRead + AsyncWrite + Unpin + Send + 'static> Stream for T {}

/// 怎么拿到到目标的字节流。由 connector 决定，host 执行。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DialPlan {
    /// 直接 TCP 连过去。
    Direct,
    /// 经一个 SOCKS5 代理（no-auth）。
    Socks5 { proxy: String },
}

/// 直连。
pub async fn dial_direct(
    host: &str,
    port: u16,
    timeout: Duration,
    ctx: &DialContext,
) -> Result<TcpStream> {
    let addr = format!("{host}:{port}");
    let connect = TcpStream::connect(&addr);
    match tokio::time::timeout(timeout, connect).await {
        Ok(Ok(stream)) => {
            stream.set_nodelay(true).ok();
            Ok(stream)
        }
        Ok(Err(e)) => Err(ctx.unreachable(&addr, format!("TCP connect failed: {e}"))),
        Err(_) => Err(ctx.unreachable(
            &addr,
            format!("TCP connect timed out after {}s", timeout.as_secs()),
        )),
    }
}

/// 经 SOCKS5 代理连到目标（no-auth）。
///
/// 握手字节序列直接来自上一代跑通的实现，别凭记忆改。
pub async fn dial_socks5(
    proxy: &str,
    host: &str,
    port: u16,
    timeout: Duration,
    ctx: &DialContext,
) -> Result<TcpStream> {
    let target = format!("{host}:{port}");

    let connect = TcpStream::connect(proxy);
    let mut stream = match tokio::time::timeout(timeout, connect).await {
        Ok(Ok(s)) => s,
        Ok(Err(e)) => {
            return Err(TrestleError::ConnectorNotReady {
                connector: ctx.connector.clone(),
                detail: format!("SOCKS5 proxy {proxy} is not accepting connections: {e}"),
                remedy: format!("trestle doctor {}", ctx.target),
            });
        }
        Err(_) => {
            return Err(TrestleError::ConnectorNotReady {
                connector: ctx.connector.clone(),
                detail: format!(
                    "SOCKS5 proxy {proxy} did not answer within {}s",
                    timeout.as_secs()
                ),
                remedy: format!("trestle doctor {}", ctx.target),
            });
        }
    };
    stream.set_nodelay(true).ok();

    let handshake = socks5_handshake(&mut stream, host, port);
    match tokio::time::timeout(timeout, handshake).await {
        Ok(Ok(())) => Ok(stream),
        Ok(Err(detail)) => Err(ctx.unreachable(&target, format!("via SOCKS5 {proxy}: {detail}"))),
        Err(_) => Err(ctx.unreachable(
            &target,
            format!(
                "via SOCKS5 {proxy}: handshake timed out after {}s",
                timeout.as_secs()
            ),
        )),
    }
}

async fn socks5_handshake<S>(
    stream: &mut S,
    host: &str,
    port: u16,
) -> std::result::Result<(), String>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    // 问候：VER=5, NMETHODS=1, METHOD=0（无认证）
    stream
        .write_all(&[0x05, 0x01, 0x00])
        .await
        .map_err(|e| format!("greeting write failed: {e}"))?;

    let mut reply = [0u8; 2];
    stream
        .read_exact(&mut reply)
        .await
        .map_err(|e| format!("greeting read failed: {e}"))?;
    if reply[0] != 0x05 {
        return Err(format!(
            "not a SOCKS5 proxy (version byte {:#04x})",
            reply[0]
        ));
    }
    if reply[1] != 0x00 {
        return Err(format!(
            "proxy demands authentication method {:#04x}; only no-auth is supported",
            reply[1]
        ));
    }

    // 请求：VER=5, CMD=CONNECT, RSV=0, ATYP+ADDR+PORT
    let mut req = vec![0x05, 0x01, 0x00];
    match host.parse::<std::net::IpAddr>() {
        Ok(std::net::IpAddr::V4(v4)) => {
            req.push(0x01);
            req.extend_from_slice(&v4.octets());
        }
        Ok(std::net::IpAddr::V6(v6)) => {
            req.push(0x04);
            req.extend_from_slice(&v6.octets());
        }
        Err(_) => {
            let bytes = host.as_bytes();
            if bytes.len() > 255 {
                return Err(format!(
                    "hostname too long for SOCKS5 ({} bytes)",
                    bytes.len()
                ));
            }
            req.push(0x03);
            req.push(bytes.len() as u8);
            req.extend_from_slice(bytes);
        }
    }
    req.extend_from_slice(&port.to_be_bytes());
    stream
        .write_all(&req)
        .await
        .map_err(|e| format!("connect request failed: {e}"))?;

    // 响应：VER, REP, RSV, ATYP, BND.ADDR, BND.PORT —— 必须把地址读干净，
    // 否则残留字节会污染后面的 SSH 协议流。
    let mut head = [0u8; 4];
    stream
        .read_exact(&mut head)
        .await
        .map_err(|e| format!("connect reply read failed: {e}"))?;
    if head[1] != 0x00 {
        return Err(socks5_reply_message(head[1]).to_string());
    }
    let addr_len = match head[3] {
        0x01 => 4,
        0x04 => 16,
        0x03 => {
            let mut n = [0u8; 1];
            stream
                .read_exact(&mut n)
                .await
                .map_err(|e| format!("reply address length read failed: {e}"))?;
            n[0] as usize
        }
        other => return Err(format!("unexpected address type {other:#04x} in reply")),
    };
    let mut rest = vec![0u8; addr_len + 2]; // 地址 + 端口
    stream
        .read_exact(&mut rest)
        .await
        .map_err(|e| format!("reply address read failed: {e}"))?;
    Ok(())
}

/// SOCKS5 的 REP 码翻译。原始数字对 agent 毫无意义，翻成人话才可操作。
fn socks5_reply_message(code: u8) -> &'static str {
    match code {
        0x01 => "proxy reported a general SOCKS server failure",
        0x02 => "proxy refused the connection (not allowed by ruleset)",
        0x03 => "network unreachable from the proxy",
        0x04 => "host unreachable from the proxy",
        0x05 => "connection refused by the destination",
        0x06 => "TTL expired",
        0x07 => "command not supported by the proxy",
        0x08 => "address type not supported by the proxy",
        _ => "proxy returned an unknown failure code",
    }
}

/// 拨号时的上下文，只为了让错误消息说得清是谁连不上谁。
#[derive(Debug, Clone)]
pub struct DialContext {
    pub target: String,
    pub connector: String,
}

impl DialContext {
    pub fn new(target: impl Into<String>, connector: impl Into<String>) -> Self {
        Self {
            target: target.into(),
            connector: connector.into(),
        }
    }

    fn unreachable(&self, endpoint: &str, detail: String) -> TrestleError {
        TrestleError::Unreachable {
            target: self.target.clone(),
            endpoint: endpoint.to_string(),
            connector: self.connector.clone(),
            detail,
            remedy: format!("trestle doctor {}", self.target),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::duplex;

    /// 一个只按脚本回话的假 SOCKS5 服务端。
    async fn fake_proxy(mut server: tokio::io::DuplexStream, reply: Vec<u8>) -> Vec<u8> {
        let mut greeting = [0u8; 3];
        server.read_exact(&mut greeting).await.unwrap();
        server.write_all(&[0x05, 0x00]).await.unwrap();

        let mut head = [0u8; 4];
        server.read_exact(&mut head).await.unwrap();
        let addr_len = match head[3] {
            0x01 => 4,
            0x04 => 16,
            0x03 => {
                let mut n = [0u8; 1];
                server.read_exact(&mut n).await.unwrap();
                n[0] as usize
            }
            _ => unreachable!(),
        };
        let mut rest = vec![0u8; addr_len + 2];
        server.read_exact(&mut rest).await.unwrap();

        let mut request = head.to_vec();
        if head[3] == 0x03 {
            request.push(addr_len as u8);
        }
        request.extend_from_slice(&rest);
        server.write_all(&reply).await.unwrap();
        request
    }

    #[tokio::test]
    async fn connect_request_encodes_an_ipv4_target() {
        let (mut client, server) = duplex(4096);
        let reply = vec![0x05, 0x00, 0x00, 0x01, 0, 0, 0, 0, 0, 0];
        let proxy = tokio::spawn(fake_proxy(server, reply));

        socks5_handshake(&mut client, "203.0.113.10", 2201)
            .await
            .unwrap();

        let request = proxy.await.unwrap();
        assert_eq!(&request[..4], &[0x05, 0x01, 0x00, 0x01]);
        assert_eq!(&request[4..8], &[59, 77, 5, 59]);
        assert_eq!(&request[8..10], &2222u16.to_be_bytes());
    }

    #[tokio::test]
    async fn connect_request_encodes_a_hostname_target() {
        let (mut client, server) = duplex(4096);
        let reply = vec![0x05, 0x00, 0x00, 0x01, 0, 0, 0, 0, 0, 0];
        let proxy = tokio::spawn(fake_proxy(server, reply));

        socks5_handshake(&mut client, "example.internal", 22)
            .await
            .unwrap();

        let request = proxy.await.unwrap();
        assert_eq!(request[3], 0x03);
        assert_eq!(request[4] as usize, "example.internal".len());
    }

    #[tokio::test]
    async fn refusal_is_translated_into_something_actionable() {
        let (mut client, server) = duplex(4096);
        // REP=0x04 host unreachable
        let reply = vec![0x05, 0x04, 0x00, 0x01, 0, 0, 0, 0, 0, 0];
        tokio::spawn(fake_proxy(server, reply));

        let err = socks5_handshake(&mut client, "10.0.0.1", 22)
            .await
            .unwrap_err();
        assert!(err.contains("host unreachable"), "{err}");
    }

    #[tokio::test]
    async fn domain_reply_address_is_fully_drained() {
        // 关键回归：回复里的变长地址如果没读干净，残留字节会污染后面的 SSH 协议流。
        let (mut client, server) = duplex(4096);
        let mut reply = vec![0x05, 0x00, 0x00, 0x03, 4];
        reply.extend_from_slice(b"abcd");
        reply.extend_from_slice(&[0x1F, 0x90]); // port
        let trailing = b"SSH-2.0-OpenSSH";
        let mut full = reply.clone();
        full.extend_from_slice(trailing);

        let proxy = tokio::spawn(fake_proxy(server, full));
        socks5_handshake(&mut client, "1.2.3.4", 22).await.unwrap();
        proxy.await.unwrap();

        // 握手结束后流上剩下的第一批字节必须正好是 SSH 横幅。
        let mut buf = vec![0u8; trailing.len()];
        client.read_exact(&mut buf).await.unwrap();
        assert_eq!(&buf, trailing);
    }

    #[tokio::test]
    async fn a_non_socks_server_is_reported_as_such() {
        let (mut client, mut server) = duplex(4096);
        tokio::spawn(async move {
            let mut greeting = [0u8; 3];
            server.read_exact(&mut greeting).await.unwrap();
            server.write_all(b"HT").await.unwrap(); // 这是个 HTTP 代理
        });
        let err = socks5_handshake(&mut client, "1.2.3.4", 22)
            .await
            .unwrap_err();
        assert!(err.contains("not a SOCKS5 proxy"), "{err}");
    }
}
