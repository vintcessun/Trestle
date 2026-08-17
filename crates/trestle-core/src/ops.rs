//! 七个基本操作的请求与响应类型。
//!
//! 这份定义同时是两处的契约：
//!   * host ←→ connector（WIT 接口由它生成/对齐）
//!   * connector ←→ agent-py（JSON-Lines 线上帧）
//!
//! 所以字段名就是线上的字段名，改名即破协议。

use serde::{Deserialize, Serialize};

/// 让默认为 `false` 的开关不出现在线上帧里。
fn is_false(b: &bool) -> bool {
    !*b
}

// ─────────────────────────────── read ───────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReadRequest {
    pub path: String,
    /// 1-based 起始行，省略即从头。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub start_line: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_lines: Option<u32>,
    /// 返回内容的字节上限，防止一次把 8G 日志拉过来。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_bytes: Option<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReadResponse {
    pub content: String,
    /// 文件总行数（不受 max_lines 影响），让调用方知道自己看到的是多大一块。
    pub total_lines: u32,
    /// 因为 max_lines / max_bytes 被截断过。
    pub truncated: bool,
}

// ─────────────────────────────── write ──────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WriteRequest {
    pub path: String,
    pub content: String,
    #[serde(default, skip_serializing_if = "is_false")]
    pub append: bool,
    /// 父目录不存在时自动创建。
    #[serde(default, skip_serializing_if = "is_false")]
    pub make_dirs: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WriteResponse {
    pub bytes: u64,
    /// 实际写入的路径。**必须等于入参 path**——见 docs/07 第 4 坑。
    pub path: String,
}

// ─────────────────────────────── edit ───────────────────────────────

/// 编辑操作。
///
/// `edit` 是基本操作而不是 read+write 的组合，因为组合意味着每改一行都要把整个文件
/// 传两遍。远端 agent 在本地做这件事，传输量只有 diff 大小。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum EditOp {
    /// 字面替换。`count = 0` 表示全部替换。
    Literal {
        old: String,
        new: String,
        #[serde(default)]
        count: u32,
    },
    /// 正则替换。`count = 0` 表示全部替换。
    Regex {
        pattern: String,
        replacement: String,
        #[serde(default)]
        count: u32,
        /// 正则标志，如 "im"。
        #[serde(default)]
        flags: String,
    },
    /// 行范围替换（1-based，含两端）。
    Lines {
        start: u32,
        end: u32,
        replacement: String,
    },
    /// 在某行**之前**插入（1-based）。
    Insert { before_line: u32, content: String },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EditRequest {
    pub path: String,
    pub op: EditOp,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EditResponse {
    /// 实际发生了改动。`false` 表示没匹配到——这不是错误，但调用方通常需要知道。
    pub changed: bool,
    /// 替换/插入发生的次数。
    pub occurrences: u32,
    pub path: String,
}

// ─────────────────────────────── shell ──────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ShellRequest {
    pub command: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cwd: Option<String>,
    /// 秒。仅 `detach = false` 时有意义；超时会杀掉**整个进程组**。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timeout_secs: Option<u64>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub env: Vec<(String, String)>,
    /// `true` = 脱离会话在后台跑（SSH 断了照跑），立即返回 pid 与日志路径。
    #[serde(default, skip_serializing_if = "is_false")]
    pub detach: bool,
    /// 仅 detach：给这次运行起个名字，决定日志目录名。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum ShellResponse {
    /// `detach = false`
    Exec(ExecResult),
    /// `detach = true`
    Detached(DetachedResult),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExecResult {
    pub exit_code: i32,
    pub stdout: String,
    pub stderr: String,
    /// 超时被杀。调用方据此区分「命令返回非零」与「根本没跑完」。
    #[serde(default)]
    pub timed_out: bool,
    pub duration_ms: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DetachedResult {
    /// **真正的**任务 pid，不是 setsid 自己的 pid。
    ///
    /// 上一代在这里踩过坑：`setsid` 会 fork，`$!` 拿到的是 setsid 的 pid，它随即退出，
    /// 于是 pid 立刻「死了」。正确做法是让最终进程自己把 pid 落盘。
    pub pid: u32,
    /// 进程组 id。停止任务时要 kill 整个组，否则孙进程残留。
    pub pgid: u32,
    pub log_path: String,
    pub meta_path: String,
    /// 退出码落盘的位置；任务还在跑时该文件不存在。
    pub rc_path: String,
}

// ──────────────────────── upload / download ─────────────────────────

/// 传输选项。`upload` 与 `download` 共用。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct TransferOptions {
    /// glob 排除模式。空则用配置里的默认排除表。
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub exclude: Vec<String>,
    /// 增量：按 size + mtime 比对，只传有变化的文件。
    #[serde(default, skip_serializing_if = "is_false")]
    pub sync: bool,
    /// 只报告会传什么，不真传。
    #[serde(default, skip_serializing_if = "is_false")]
    pub dry_run: bool,
    /// 同步时删除目标端多出来的文件。仅 `sync = true` 有意义。
    #[serde(default, skip_serializing_if = "is_false")]
    pub delete: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TransferResponse {
    /// 实际传输（或 dry_run 下将要传输）的文件数。目录传输时 > 1。
    pub files: u64,
    pub bytes: u64,
    /// 单文件传输时给出校验和；目录传输时为 None。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sha256: Option<String>,
    /// 落地路径。**必须等于入参给的那个路径**。
    ///
    /// docs/07 第 4 坑：`shutil.make_archive(base, "gztar")` 自己决定后缀，你传 `x.tgz`
    /// 它产出 `x.tar.gz`，调用方拿自己给的路径去解包就 404。
    /// 通用教训：任何「你给一个路径、我产出一个文件」的接口，产出必须就是你给的那个路径。
    pub path: String,
    /// dry_run 下将要传输的文件清单（相对路径）。
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub planned: Vec<String>,
}

// ────────────────────────────── forward ─────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ForwardRequest {
    /// 远端要暴露出来的端口。
    pub remote_port: u16,
    /// 远端侧绑定地址，默认 `127.0.0.1`（即「远端机器自己看到的 localhost」）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub remote_host: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ForwardResponse {
    /// **由 host 分配**的本地端口。调用方不能指定——否则新旧转发会抢同一个端口。
    pub local_port: u16,
    /// 直接可访问的 URL，省得调用方自己拼。
    pub url: String,
    /// 关闭这条通道时用的句柄。
    pub handle: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn edit_op_round_trips_through_json() {
        let op = EditOp::Lines {
            start: 3,
            end: 5,
            replacement: "hello\n".into(),
        };
        let json = serde_json::to_string(&op).unwrap();
        assert!(json.contains(r#""kind":"lines""#));
        let back: EditOp = serde_json::from_str(&json).unwrap();
        assert!(matches!(
            back,
            EditOp::Lines {
                start: 3,
                end: 5,
                ..
            }
        ));
    }

    #[test]
    fn shell_response_discriminates_by_shape() {
        let exec = r#"{"exit_code":0,"stdout":"hi","stderr":"","duration_ms":12}"#;
        assert!(matches!(
            serde_json::from_str::<ShellResponse>(exec).unwrap(),
            ShellResponse::Exec(_)
        ));

        let detached = r#"{"pid":42,"pgid":42,"log_path":"/l","meta_path":"/m","rc_path":"/r"}"#;
        assert!(matches!(
            serde_json::from_str::<ShellResponse>(detached).unwrap(),
            ShellResponse::Detached(_)
        ));
    }

    #[test]
    fn transfer_options_default_to_a_plain_copy() {
        let opts = TransferOptions::default();
        assert!(!opts.sync && !opts.dry_run && !opts.delete && opts.exclude.is_empty());
        // 默认序列化应该是空对象，线上帧不带一堆 false。
        assert_eq!(serde_json::to_string(&opts).unwrap(), "{}");
    }
}
