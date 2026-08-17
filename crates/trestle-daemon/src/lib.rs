//! `trestled` 的内部件，同时暴露给瘦客户端（MCP 前端与 CLI）。
//!
//! IPC 协议要三边共用，所以它在 lib 里而不是 bin 里——否则协议会在三个地方
//! 各有一份定义，然后慢慢长歪。

pub mod events;
pub mod http;
pub mod ipc;
pub mod registry;
pub mod tasks;
