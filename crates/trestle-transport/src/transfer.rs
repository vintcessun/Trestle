//! `upload` / `download` 的主机侧编排：分块、校验、目录递归、增量比对。
//!
//! 分块与 sha256 在这里和 agent 之间完成，**不暴露给上层**——上层看到的就是
//! 「给一个路径、产出一个文件」。
//!
//! 这条接口有一个必须守住的性质：**产出路径就是入参路径**。上一代在这里栽过——
//! `shutil.make_archive(base, "gztar")` 自己决定后缀，你传 `x.tgz` 它产出 `x.tar.gz`，
//! 调用方拿自己给的路径去解包就 404。任何「你给一个路径、我产出一个文件」的接口，
//! 产出必须就是你给的那个路径。

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use base64::Engine;
use serde::Deserialize;
use serde_json::json;
use sha2::{Digest, Sha256};

use trestle_core::{Result, TransferOptions, TransferResponse, TrestleError};

use crate::agent::AgentClient;

/// 单块大小。与 agent 侧的 `MAX_CHUNK` 对齐。
const CHUNK: usize = 1 << 19; // 512 KiB

#[derive(Debug, Clone, Deserialize)]
struct RemoteEntry {
    rel: String,
    size: u64,
    mtime: i64,
}

#[derive(Debug, Clone, Deserialize)]
struct RemoteTree {
    entries: Vec<RemoteEntry>,
}

#[derive(Debug, Clone, Deserialize)]
struct RemoteStat {
    size: u64,
    #[serde(default)]
    mtime: i64,
    #[serde(default)]
    is_dir: bool,
}

/// 本地 → 远端。文件与目录自动识别。
pub async fn upload(
    agent: &AgentClient,
    target: &str,
    local_path: &str,
    remote_path: &str,
    opts: &TransferOptions,
    default_exclude: &[String],
) -> Result<TransferResponse> {
    let local = Path::new(local_path);
    let meta = tokio::fs::metadata(local)
        .await
        .map_err(|e| TrestleError::Remote {
            target: target.to_string(),
            op: "upload".into(),
            detail: format!("cannot read local path {local_path}: {e}"),
        })?;

    if meta.is_file() {
        if opts.dry_run {
            return Ok(TransferResponse {
                files: 1,
                bytes: meta.len(),
                sha256: None,
                path: remote_path.to_string(),
                planned: vec![local_name(local)],
            });
        }
        let sha = upload_one(agent, target, local, remote_path).await?;
        return Ok(TransferResponse {
            files: 1,
            bytes: meta.len(),
            sha256: Some(sha),
            path: remote_path.to_string(),
            planned: Vec::new(),
        });
    }

    // ── 目录 ──
    let exclude = effective_exclude(opts, default_exclude);
    let local_files = walk_local(local, &exclude).await?;

    let remote_index = if opts.sync {
        remote_index(agent, remote_path, &exclude).await?
    } else {
        HashMap::new()
    };

    let mut planned = Vec::new();
    let mut bytes = 0u64;
    for entry in &local_files {
        if opts.sync
            && let Some(remote) = remote_index.get(&entry.rel)
        {
            // size + mtime 一致就认为没变。和 rsync 的默认判据一样，
            // 便宜且对训练脚本这类工作负载足够准。
            if remote.size == entry.size && remote.mtime >= entry.mtime {
                continue;
            }
        }
        planned.push(entry.rel.clone());
        bytes += entry.size;
    }

    if opts.dry_run {
        return Ok(TransferResponse {
            files: planned.len() as u64,
            bytes,
            sha256: None,
            path: remote_path.to_string(),
            planned,
        });
    }

    for rel in &planned {
        let src = local.join(rel.replace('/', std::path::MAIN_SEPARATOR_STR));
        let dst = format!("{}/{}", remote_path.trim_end_matches('/'), rel);
        upload_one(agent, target, &src, &dst).await?;
    }

    Ok(TransferResponse {
        files: planned.len() as u64,
        bytes,
        sha256: None,
        path: remote_path.to_string(),
        planned: Vec::new(),
    })
}

/// 远端 → 本地。文件与目录自动识别。
pub async fn download(
    agent: &AgentClient,
    target: &str,
    remote_path: &str,
    local_path: &str,
    opts: &TransferOptions,
    default_exclude: &[String],
) -> Result<TransferResponse> {
    let stat: RemoteStat =
        serde_json::from_value(agent.call_raw("stat", json!({"path": remote_path})).await?)
            .map_err(|e| protocol(target, e))?;

    if !stat.is_dir {
        if opts.dry_run {
            return Ok(TransferResponse {
                files: 1,
                bytes: stat.size,
                sha256: None,
                path: local_path.to_string(),
                planned: vec![
                    remote_path
                        .rsplit('/')
                        .next()
                        .unwrap_or(remote_path)
                        .to_string(),
                ],
            });
        }
        let sha = download_one(
            agent,
            target,
            remote_path,
            Path::new(local_path),
            stat.mtime,
        )
        .await?;
        return Ok(TransferResponse {
            files: 1,
            bytes: stat.size,
            sha256: Some(sha),
            path: local_path.to_string(),
            planned: Vec::new(),
        });
    }

    let exclude = effective_exclude(opts, default_exclude);
    let tree: RemoteTree = serde_json::from_value(
        agent
            .call_raw(
                "list_tree",
                json!({"path": remote_path, "exclude": exclude}),
            )
            .await?,
    )
    .map_err(|e| protocol(target, e))?;

    let local_root = Path::new(local_path);
    let local_index: HashMap<String, LocalEntry> = if opts.sync {
        walk_local(local_root, &exclude)
            .await
            .unwrap_or_default()
            .into_iter()
            .map(|e| (e.rel.clone(), e))
            .collect()
    } else {
        HashMap::new()
    };

    let mut planned = Vec::new();
    let mut bytes = 0u64;
    for entry in &tree.entries {
        if opts.sync
            && let Some(local) = local_index.get(&entry.rel)
            && local.size == entry.size
            && local.mtime >= entry.mtime
        {
            continue;
        }
        planned.push(entry.rel.clone());
        bytes += entry.size;
    }

    if opts.dry_run {
        return Ok(TransferResponse {
            files: planned.len() as u64,
            bytes,
            sha256: None,
            path: local_path.to_string(),
            planned,
        });
    }

    let mtimes: HashMap<&str, i64> = tree
        .entries
        .iter()
        .map(|e| (e.rel.as_str(), e.mtime))
        .collect();
    for rel in &planned {
        let src = format!("{}/{}", remote_path.trim_end_matches('/'), rel);
        let dst = local_root.join(rel.replace('/', std::path::MAIN_SEPARATOR_STR));
        let mtime = mtimes.get(rel.as_str()).copied().unwrap_or(0);
        download_one(agent, target, &src, &dst, mtime).await?;
    }

    Ok(TransferResponse {
        files: planned.len() as u64,
        bytes,
        sha256: None,
        path: local_path.to_string(),
        planned: Vec::new(),
    })
}

// ───────────────────────────── 单个文件 ─────────────────────────────

async fn upload_one(
    agent: &AgentClient,
    target: &str,
    local: &Path,
    remote: &str,
) -> Result<String> {
    let data = tokio::fs::read(local)
        .await
        .map_err(|e| TrestleError::Remote {
            target: target.to_string(),
            op: "upload".into(),
            detail: format!("cannot read {}: {e}", local.display()),
        })?;
    let sha = hex(Sha256::digest(&data).as_slice());
    // 把源文件的 mtime 带过去。增量同步的判据是 size+mtime，如果远端记的是
    // 「写入时刻」，两台机器之间的时钟偏差会让下次同步误判成「变了」。
    let mtime = tokio::fs::metadata(local)
        .await
        .map(|m| mtime_secs(&m))
        .unwrap_or(0);

    let mut offset = 0usize;
    loop {
        let end = (offset + CHUNK).min(data.len());
        let final_chunk = end >= data.len();
        let encoded = base64::engine::general_purpose::STANDARD.encode(&data[offset..end]);
        agent
            .call_raw(
                "put_chunk",
                json!({
                    "path": remote,
                    "offset": offset,
                    "data": encoded,
                    "final": final_chunk,
                    "sha256": if final_chunk { Some(sha.clone()) } else { None },
                    "mtime": if final_chunk { Some(mtime) } else { None },
                }),
            )
            .await?;
        offset = end;
        if final_chunk {
            break;
        }
    }
    Ok(sha)
}

async fn download_one(
    agent: &AgentClient,
    target: &str,
    remote: &str,
    local: &Path,
    remote_mtime: i64,
) -> Result<String> {
    #[derive(Deserialize)]
    struct Chunk {
        data: String,
        eof: bool,
    }

    if let Some(parent) = local.parent() {
        tokio::fs::create_dir_all(parent).await.ok();
    }

    let mut buf = Vec::new();
    let mut offset = 0usize;
    loop {
        let chunk: Chunk = serde_json::from_value(
            agent
                .call_raw(
                    "get_chunk",
                    json!({"path": remote, "offset": offset, "length": CHUNK}),
                )
                .await?,
        )
        .map_err(|e| protocol(target, e))?;

        let bytes = base64::engine::general_purpose::STANDARD
            .decode(chunk.data.as_bytes())
            .map_err(|e| TrestleError::Protocol {
                target: target.to_string(),
                detail: format!("malformed chunk while downloading {remote}: {e}"),
            })?;
        offset += bytes.len();
        buf.extend_from_slice(&bytes);
        if chunk.eof {
            break;
        }
        if bytes.is_empty() {
            return Err(TrestleError::Protocol {
                target: target.to_string(),
                detail: format!("download of {remote} stalled at offset {offset}"),
            });
        }
    }

    let local_sha = hex(Sha256::digest(&buf).as_slice());

    // 先落临时文件再改名：半路失败不会在目标位置留下一个看起来完整的半截文件。
    let tmp = local.with_extension(format!(
        "{}trestle-part",
        local
            .extension()
            .map(|e| format!("{}.", e.to_string_lossy()))
            .unwrap_or_default()
    ));
    tokio::fs::write(&tmp, &buf)
        .await
        .map_err(|e| TrestleError::Remote {
            target: target.to_string(),
            op: "download".into(),
            detail: format!("cannot write {}: {e}", tmp.display()),
        })?;
    tokio::fs::rename(&tmp, local)
        .await
        .map_err(|e| TrestleError::Remote {
            target: target.to_string(),
            op: "download".into(),
            detail: format!("cannot finalise {}: {e}", local.display()),
        })?;

    // 和上传方向对称：保留源端 mtime，让下一次增量同步的判据与时钟无关。
    if remote_mtime > 0
        && let Ok(file) = std::fs::OpenOptions::new().write(true).open(local)
    {
        let when = std::time::UNIX_EPOCH + std::time::Duration::from_secs(remote_mtime as u64);
        let _ = file.set_modified(when);
    }

    Ok(local_sha)
}

// ────────────────────────────── 辅助 ──────────────────────────────

#[derive(Debug, Clone, Default)]
struct LocalEntry {
    rel: String,
    size: u64,
    mtime: i64,
}

async fn walk_local(root: &Path, exclude: &[String]) -> Result<Vec<LocalEntry>> {
    let mut out = Vec::new();
    let mut stack = vec![(root.to_path_buf(), String::new())];

    while let Some((dir, prefix)) = stack.pop() {
        let mut entries = match tokio::fs::read_dir(&dir).await {
            Ok(e) => e,
            Err(_) => continue,
        };
        while let Ok(Some(entry)) = entries.next_entry().await {
            let name = entry.file_name().to_string_lossy().into_owned();
            let rel = if prefix.is_empty() {
                name.clone()
            } else {
                format!("{prefix}/{name}")
            };
            if is_excluded(&rel, exclude) {
                continue;
            }
            let Ok(meta) = entry.metadata().await else {
                continue;
            };
            if meta.is_dir() {
                stack.push((entry.path(), rel));
            } else if meta.is_file() {
                out.push(LocalEntry {
                    rel,
                    size: meta.len(),
                    mtime: mtime_secs(&meta),
                });
            }
        }
    }
    out.sort_by(|a, b| a.rel.cmp(&b.rel));
    Ok(out)
}

fn mtime_secs(meta: &std::fs::Metadata) -> i64 {
    meta.modified()
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// 任一路径分量匹配任一 glob 就算被排除。
///
/// 这样 `__pycache__` 能挡掉 `a/b/__pycache__/c.pyc`，而不只是顶层那个——
/// 和 agent 侧 `excluded()` 的语义必须一致，否则两端算出的清单会对不上。
fn is_excluded(rel: &str, patterns: &[String]) -> bool {
    if patterns.is_empty() {
        return false;
    }
    let normalized = rel.replace('\\', "/");
    for pat in patterns {
        if glob_match(pat, &normalized) {
            return true;
        }
        if normalized.split('/').any(|part| glob_match(pat, part)) {
            return true;
        }
    }
    false
}

/// 只支持 `*` 与 `?` 的极简 glob —— 排除表里用到的就这两个。
fn glob_match(pattern: &str, text: &str) -> bool {
    fn helper(p: &[u8], t: &[u8]) -> bool {
        match (p.first(), t.first()) {
            (None, None) => true,
            (Some(b'*'), _) => helper(&p[1..], t) || (!t.is_empty() && helper(p, &t[1..])),
            (Some(b'?'), Some(_)) => helper(&p[1..], &t[1..]),
            (Some(a), Some(b)) if a == b => helper(&p[1..], &t[1..]),
            _ => false,
        }
    }
    helper(pattern.as_bytes(), text.as_bytes())
}

fn effective_exclude(opts: &TransferOptions, default_exclude: &[String]) -> Vec<String> {
    if opts.exclude.is_empty() {
        default_exclude.to_vec()
    } else {
        opts.exclude.clone()
    }
}

async fn remote_index(
    agent: &AgentClient,
    path: &str,
    exclude: &[String],
) -> Result<HashMap<String, RemoteEntry>> {
    let value = agent
        .call_raw("list_tree", json!({"path": path, "exclude": exclude}))
        .await;
    // 远端还没有这个目录是完全正常的第一次同步，不该是错误。
    let Ok(value) = value else {
        return Ok(HashMap::new());
    };
    let tree: RemoteTree = serde_json::from_value(value).unwrap_or(RemoteTree {
        entries: Vec::new(),
    });
    Ok(tree
        .entries
        .into_iter()
        .map(|e| (e.rel.clone(), e))
        .collect())
}

fn local_name(p: &Path) -> String {
    p.file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default()
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn protocol(target: &str, e: serde_json::Error) -> TrestleError {
    TrestleError::Protocol {
        target: target.to_string(),
        detail: format!("malformed transfer response: {e}"),
    }
}

/// 让上层能重用 `PathBuf`，避免各处再拼一遍。
pub fn join_remote(base: &str, rel: &str) -> String {
    format!("{}/{}", base.trim_end_matches('/'), rel)
}

pub fn local_join(base: &Path, rel: &str) -> PathBuf {
    base.join(rel.replace('/', std::path::MAIN_SEPARATOR_STR))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn glob_handles_star_and_question() {
        assert!(glob_match("*.pyc", "b.pyc"));
        assert!(glob_match("*.pyc", ".pyc"));
        assert!(!glob_match("*.pyc", "b.py"));
        assert!(glob_match("a?c", "abc"));
        assert!(!glob_match("a?c", "ac"));
        assert!(glob_match("*", "anything"));
        assert!(glob_match("__pycache__", "__pycache__"));
    }

    #[test]
    fn exclusion_matches_any_path_component() {
        let pats = vec![
            "__pycache__".to_string(),
            "*.pyc".to_string(),
            ".git".to_string(),
        ];
        // 深处的 __pycache__ 也要挡住，不只是顶层那个。
        assert!(is_excluded("pkg/__pycache__/b.pyc", &pats));
        assert!(is_excluded("a/b/c.pyc", &pats));
        assert!(is_excluded(".git/config", &pats));
        assert!(!is_excluded("pkg/b.py", &pats));
        assert!(!is_excluded("src/main.rs", &pats));
    }

    #[test]
    fn backslashes_are_normalised_before_matching() {
        // 本地是 Windows，远端是 Linux —— 两端算出的清单必须能对上。
        let pats = vec!["__pycache__".to_string()];
        assert!(is_excluded("pkg\\__pycache__\\b.pyc", &pats));
    }

    #[test]
    fn an_empty_exclude_list_excludes_nothing() {
        assert!(!is_excluded("anything/at/all", &[]));
    }

    #[test]
    fn options_fall_back_to_the_configured_defaults() {
        let defaults = vec!["__pycache__".to_string()];
        let opts = TransferOptions::default();
        assert_eq!(effective_exclude(&opts, &defaults), defaults);

        // 显式给了就完全覆盖，而不是叠加 —— 否则调用方无法关掉某条默认排除。
        let opts = TransferOptions {
            exclude: vec!["*.log".to_string()],
            ..Default::default()
        };
        assert_eq!(
            effective_exclude(&opts, &defaults),
            vec!["*.log".to_string()]
        );
    }

    #[test]
    fn remote_paths_always_use_forward_slashes() {
        assert_eq!(join_remote("/home/x/", "a/b.txt"), "/home/x/a/b.txt");
        assert_eq!(join_remote("/home/x", "a/b.txt"), "/home/x/a/b.txt");
    }
}
