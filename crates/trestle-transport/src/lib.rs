//! 传输工具箱：host 提供给 connector 插件的那些「wasm 沙箱里跑不了」的能力。
//!
//! 拨号、SSH 握手、加解密、按哈希幂等部署、端口转发——这些都在这里，用 native 代码实现。
//! **但编排逻辑不在这里**：先探端口还是先拉容器、断了重试几次、agent 装在哪、
//! 哪些机器归我管，全部由 connector 插件决定。
//!
//! 这条分工是刻意的。connector 对上层只暴露一个 name 和七个基本操作，
//! 上层永远不知道下面是 SSH——所以 `ssh.wasm` 这样的东西不该存在，SSH 只是
//! 某个 connector 的内部实现细节。之所以这一段留在 host，纯粹是因为 russh 建在
//! tokio 之上而 tokio 的 `net` 在 wasi target 上不支持，wasm 里今天跑不了 SSH。

pub mod agent;
pub mod deploy;
pub mod dial;
pub mod forward;
pub mod session;
pub mod ssh;
pub mod transfer;

pub use agent::{AgentClient, AgentInfo};
pub use deploy::{Bootstrap, DeployReason, Interpreter, agent_sha256};
pub use dial::{DialContext, DialPlan};
pub use forward::Forward;
pub use session::{ConnectOptions, ConnectStats, Session};
pub use ssh::{Credentials, ExecOutput, HostKeyPolicy, SshSession};
