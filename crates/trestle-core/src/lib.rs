//! Trestle 的核心抽象：类型、错误、配置、事件。
//!
//! 这个 crate **不依赖任何具体实现**——transport、host、daemon 都依赖它，反向不成立。
//!
//! 整个系统立在两句话上：
//!
//! 1. **基本操作只有七个**：read / write / edit / shell / upload / download / forward。
//! 2. **connector 是一整块自包含的接入能力**——向上只暴露一个 name 和这七个操作，
//!    向下自己管连哪些机器、怎么连、长连接怎么维持、断了怎么重试、远端 agent 怎么部署。
//!    上层永远不知道下面是 SSH 还是别的。
//!
//! 其余一切（job / fs / xfer / fleet / monitor / Web UI）都是建在这七个操作之上的插件。

pub mod config;
pub mod error;
pub mod event;
pub mod ops;
pub mod target;

pub use error::{Result, TrestleError};
pub use ops::{
    DetachedResult, EditOp, EditRequest, EditResponse, ExecResult, ForwardRequest, ForwardResponse,
    ReadRequest, ReadResponse, ShellRequest, ShellResponse, TransferOptions, TransferResponse,
    WriteRequest, WriteResponse,
};
pub use target::{Health, Target, TargetRegistry};

/// 七个基本操作的名字。用于路由、capability 检查、错误消息。
///
/// 顺序即文档里的顺序，别改。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Op {
    Read,
    Write,
    Edit,
    Shell,
    Upload,
    Download,
    Forward,
}

impl Op {
    pub const ALL: [Op; 7] = [
        Op::Read,
        Op::Write,
        Op::Edit,
        Op::Shell,
        Op::Upload,
        Op::Download,
        Op::Forward,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            Op::Read => "read",
            Op::Write => "write",
            Op::Edit => "edit",
            Op::Shell => "shell",
            Op::Upload => "upload",
            Op::Download => "download",
            Op::Forward => "forward",
        }
    }

    /// 明确幂等的读操作才允许在连接重建后自动重放。
    ///
    /// 其余的一律返回 [`TrestleError::UnknownState`]——已经发出去的 `shell`
    /// 可能已经在远端跑过了，重放意味着可能把一条 `rm -rf` 或一次训练启动跑两遍。
    pub fn is_idempotent_read(self) -> bool {
        matches!(self, Op::Read | Op::Download)
    }
}

impl std::fmt::Display for Op {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

impl std::str::FromStr for Op {
    type Err = TrestleError;

    fn from_str(s: &str) -> Result<Self> {
        Op::ALL
            .into_iter()
            .find(|op| op.as_str() == s)
            .ok_or_else(|| TrestleError::Protocol {
                target: String::new(),
                detail: format!(
                    "unknown operation '{s}'; known: {}",
                    Op::ALL.map(Op::as_str).join(", ")
                ),
            })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn op_names_round_trip() {
        for op in Op::ALL {
            assert_eq!(op.as_str().parse::<Op>().unwrap(), op);
        }
    }

    #[test]
    fn only_reads_may_be_replayed_automatically() {
        assert!(Op::Read.is_idempotent_read());
        assert!(Op::Download.is_idempotent_read());
        // 这四条如果被自动重放，可能把一条 rm -rf 或一次训练启动跑两遍。
        for op in [Op::Write, Op::Edit, Op::Shell, Op::Upload] {
            assert!(!op.is_idempotent_read(), "{op} must not be auto-replayed");
        }
    }

    #[test]
    fn unknown_op_error_lists_the_real_ones() {
        let err = "reed".parse::<Op>().unwrap_err();
        assert!(err.to_string().contains("read"), "{err}");
    }
}
