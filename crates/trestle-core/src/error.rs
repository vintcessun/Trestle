//! 错误类型。
//!
//! 这些错误主要读者是 **coding agent**，不是人。所以每个变体都被设计成
//! 「说清楚发生了什么 + 下一步能做什么」，而不是「失败了」。
//!
//! ```text
//! ✗  "target not found"
//! ✓  "unknown target 'x36'; known: gpu-1, gpu-2, gpu-3, gpu-4"
//!
//! ✗  "timeout"
//! ✓  "base_shell on gpu-4 timed out after 60s; process group killed.
//!     For long jobs use job_start instead."
//! ```
//!
//! 上一代实测下来，第三条那种「在错误里指出正确的工具」收益最大：agent 读到就会
//! 改用 `job_start`，而不是把 timeout 调大再撞一次。

use std::fmt;

use thiserror::Error;

pub type Result<T, E = TrestleError> = std::result::Result<T, E>;

#[derive(Debug, Error)]
pub enum TrestleError {
    /// 目标名解析失败。**必须**带上所有可选名字——这在 agent 手里比「未找到」有用得多。
    #[error("unknown target '{name}'; known: {}", CommaList(known))]
    UnknownTarget { name: String, known: Vec<String> },

    /// connector 名解析失败。
    #[error("unknown connector '{name}'; known: {}", CommaList(known))]
    UnknownConnector { name: String, known: Vec<String> },

    /// 前置条件没能就绪（VPN 容器没起来、凭据刷不出来……）。
    /// `remedy` 是给 agent 的下一步，不是可选的客套话。
    #[error("connector '{connector}' is not ready: {detail}\nTry: {remedy}")]
    ConnectorNotReady {
        connector: String,
        detail: String,
        remedy: String,
    },

    /// 连不上目标。带上实际拨号地址，否则 agent 无从判断是网络问题还是配置问题。
    #[error(
        "cannot reach {target} ({endpoint}) via connector '{connector}': {detail}\nTry: {remedy}"
    )]
    Unreachable {
        target: String,
        endpoint: String,
        connector: String,
        detail: String,
        remedy: String,
    },

    /// 认证失败。
    #[error("authentication failed for {user}@{target} ({method}): {detail}")]
    AuthFailed {
        target: String,
        user: String,
        method: String,
        detail: String,
    },

    /// 短命令超时。**必须**指出该改用 `job_start`——这是上一代收益最大的一条错误消息。
    #[error(
        "shell on {target} timed out after {timeout_secs}s; process group killed.\n\
         For long-running work use job_start instead of shell."
    )]
    ShellTimeout { target: String, timeout_secs: u64 },

    /// 请求**已经发出去**但没拿到响应。
    ///
    /// 这个变体存在的唯一理由是：那条命令**可能已经在远端执行了**。自动重放意味着
    /// 可能把一条 `rm -rf` 或一次训练启动跑两遍。所以这里如实把不确定性交给上层，
    /// 绝不在下面偷偷重试。只有明确幂等的读操作（read/list/stat/hash/probe）才自动重试。
    #[error(
        "unknown state: '{op}' on {target} was sent but no response came back.\n\
         The remote side may have executed it; check state before retrying."
    )]
    UnknownState { target: String, op: String },

    /// 远端 agent 那边报回来的错误（文件不存在、权限不足……）。
    ///
    /// `target` 允许为空（有些失败不属于任何一台机器），这时不要印出
    /// 「failed on :」这种半截话——错误消息是给 agent 读的产物。
    #[error("{}: {detail}", Where(op, target))]
    Remote {
        target: String,
        op: String,
        detail: String,
    },

    /// 远端环境不满足要求（没有 python、uv 装不上……）。
    #[error("{target}: {detail}\nTry: {remedy}")]
    RemoteEnvironment {
        target: String,
        detail: String,
        remedy: String,
    },

    /// 配置问题。`path` 是配置里的位置（如 `connectors.gpu-cluster.socks`）。
    #[error("config error at '{path}': {detail}")]
    Config { path: String, detail: String },

    /// 插件被 capability 挡掉。这个必须能在 Monitor 里看见，所以带全上下文。
    #[error("plugin '{plugin}' denied: {action} is not in its manifest allowlist")]
    CapabilityDenied { plugin: String, action: String },

    /// 协议层面对不上（帧格式、版本不匹配……）。
    #[error("protocol error with {target}: {detail}")]
    Protocol { target: String, detail: String },

    #[error(transparent)]
    Io(#[from] std::io::Error),

    #[error(transparent)]
    Other(#[from] anyhow::Error),
}

impl TrestleError {
    /// 这个错误是不是「重试可能有用」。用于自愈路径决策。
    ///
    /// 注意 [`TrestleError::UnknownState`] **不在**此列——那正是它存在的意义。
    pub fn is_retryable(&self) -> bool {
        matches!(
            self,
            TrestleError::Unreachable { .. } | TrestleError::ConnectorNotReady { .. }
        )
    }

    /// 构造一个带补救建议的目标解析错误。
    pub fn unknown_target(
        name: impl Into<String>,
        known: impl IntoIterator<Item = String>,
    ) -> Self {
        let mut known: Vec<String> = known.into_iter().collect();
        known.sort();
        TrestleError::UnknownTarget {
            name: name.into(),
            known,
        }
    }
}

/// `read on gpu-4` / `read`（机器名为空时）。
struct Where<'a>(&'a str, &'a str);

impl fmt::Display for Where<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.1.is_empty() {
            f.write_str(self.0)
        } else {
            write!(f, "{} on {}", self.0, self.1)
        }
    }
}

/// `a, b, c` —— 让 `#[error(...)]` 里能直接排版名字列表。
struct CommaList<'a>(&'a [String]);

impl fmt::Display for CommaList<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.0.is_empty() {
            return f.write_str("(none configured)");
        }
        for (i, name) in self.0.iter().enumerate() {
            if i > 0 {
                f.write_str(", ")?;
            }
            f.write_str(name)?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unknown_target_lists_all_known_names_sorted() {
        let err = TrestleError::unknown_target(
            "x36",
            [
                "gpu-4".to_string(),
                "gpu-1".to_string(),
                "gpu-2".to_string(),
            ],
        );
        assert_eq!(
            err.to_string(),
            "unknown target 'x36'; known: gpu-1, gpu-2, gpu-4"
        );
    }

    #[test]
    fn unknown_target_says_so_when_nothing_is_configured() {
        let err = TrestleError::unknown_target("gpu-4", []);
        assert_eq!(
            err.to_string(),
            "unknown target 'gpu-4'; known: (none configured)"
        );
    }

    #[test]
    fn shell_timeout_points_at_the_right_tool() {
        let err = TrestleError::ShellTimeout {
            target: "gpu-4".into(),
            timeout_secs: 60,
        };
        // 这条断言是刻意的：错误消息里必须出现 job_start，否则 agent 会去调大 timeout 再撞一次。
        assert!(err.to_string().contains("job_start"));
    }

    #[test]
    fn a_failure_that_belongs_to_no_machine_still_reads_as_a_sentence() {
        let err = TrestleError::Remote {
            target: String::new(),
            op: "monitor".into(),
            detail: "timeout_secs is required".into(),
        };
        // 不能是「monitor failed on : ...」这种半截话。
        assert_eq!(err.to_string(), "monitor: timeout_secs is required");

        let err = TrestleError::Remote {
            target: "gpu-4".into(),
            op: "read".into(),
            detail: "no such file".into(),
        };
        assert_eq!(err.to_string(), "read on gpu-4: no such file");
    }

    #[test]
    fn unknown_state_is_never_retryable() {
        let err = TrestleError::UnknownState {
            target: "gpu-4".into(),
            op: "shell".into(),
        };
        assert!(!err.is_retryable());
    }
}
