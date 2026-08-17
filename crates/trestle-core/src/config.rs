//! 统一配置：一个类、一个入口。
//!
//! 分节：`[daemon]` `[connectors.<name>]` `[plugins.<name>]` `[targets.<name>]`。
//! connector 与插件各自声明 schema，通过 `config.get/set` 只读写属于自己的那一节——
//! 没有「每个插件自己一个配置文件」这种散装形态，Web UI 因此能渲染出一个统一的配置页。
//!
//! 凭据放在同目录的 `secrets.toml`（已 gitignore），值支持三种写法：
//!
//! ```toml
//! password = "明文"
//! password = "env:TRESTLE_GPU1_PW"      # 从环境变量读，值不落盘
//! password = "file:C:/path/to/secret"   # 从文件读
//! ```

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::error::{Result, TrestleError};
use crate::target::{Target, TargetRegistry};

pub const CONFIG_FILE: &str = "trestle.toml";
pub const SECRETS_FILE: &str = "secrets.toml";

// ────────────────────────────── 配置结构 ─────────────────────────────

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Config {
    #[serde(default)]
    pub daemon: DaemonConfig,
    #[serde(default)]
    pub defaults: Defaults,
    /// connector 名 → 它自己的那一节。内容对 host 是不透明的，由 connector 插件解释。
    #[serde(default)]
    pub connectors: BTreeMap<String, ConnectorConfig>,
    /// 插件名 → 它自己的那一节，同样不透明。
    #[serde(default)]
    pub plugins: BTreeMap<String, toml::Value>,
    /// 机器名 → 拓扑与称呼。名字由用户定，不由 connector 定。
    #[serde(default)]
    pub targets: BTreeMap<String, TargetConfig>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DaemonConfig {
    /// 无客户端连接且无活跃 job 超过这个时间就自行退出；0 = 永不退出。
    #[serde(default = "default_idle_timeout")]
    pub idle_timeout_secs: u64,
    /// IPC 监听。端口 0 = 随机；实际端口与 token 写进同目录的 `daemon.json`。
    #[serde(default = "default_ipc_bind")]
    pub ipc_bind: String,
    /// Monitor / Web UI 的 HTTP 服务绑定。
    #[serde(default = "default_http_bind")]
    pub http_bind: String,
}

impl Default for DaemonConfig {
    fn default() -> Self {
        Self {
            idle_timeout_secs: default_idle_timeout(),
            ipc_bind: default_ipc_bind(),
            http_bind: default_http_bind(),
        }
    }
}

fn default_idle_timeout() -> u64 {
    1800
}
fn default_ipc_bind() -> String {
    "127.0.0.1:0".into()
}
fn default_http_bind() -> String {
    "127.0.0.1:0".into()
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Defaults {
    /// `shell` 的默认超时（秒）。超过这个时长的活儿应该用 job_start。
    #[serde(default = "default_shell_timeout")]
    pub shell_timeout_secs: u64,
    /// `shell` 允许的超时上限。
    #[serde(default = "default_shell_max_timeout")]
    pub shell_max_timeout_secs: u64,
    /// 传输分块大小。
    #[serde(default = "default_chunk_bytes")]
    pub chunk_bytes: u64,
    /// 目录传输的默认排除表。
    #[serde(default = "default_exclude")]
    pub exclude: Vec<String>,
}

impl Default for Defaults {
    fn default() -> Self {
        Self {
            shell_timeout_secs: default_shell_timeout(),
            shell_max_timeout_secs: default_shell_max_timeout(),
            chunk_bytes: default_chunk_bytes(),
            exclude: default_exclude(),
        }
    }
}

fn default_shell_timeout() -> u64 {
    60
}
fn default_shell_max_timeout() -> u64 {
    300
}
fn default_chunk_bytes() -> u64 {
    524_288
}
fn default_exclude() -> Vec<String> {
    [
        "__pycache__",
        "*.pyc",
        ".git",
        ".venv",
        "venv",
        "node_modules",
        "*.egg-info",
        ".ipynb_checkpoints",
        ".DS_Store",
    ]
    .into_iter()
    .map(String::from)
    .collect()
}

/// 一个 connector 的配置节。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConnectorConfig {
    /// 加载哪个 connector 插件（`plugins/connectors/<plugin>.wasm`）。
    pub plugin: String,
    #[serde(default = "default_true")]
    pub enabled: bool,
    /// 插件自己的设置，host 不解释。
    #[serde(flatten)]
    pub settings: toml::Table,
}

fn default_true() -> bool {
    true
}

/// 一台机器的拓扑与称呼。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TargetConfig {
    pub connector: String,
    pub host: String,
    pub port: u16,
    pub user: String,
    #[serde(default)]
    pub workdir: String,
    #[serde(default)]
    pub aliases: Vec<String>,
    #[serde(default)]
    pub note: String,
    #[serde(default = "default_agent_dir")]
    pub agent_dir: String,
}

fn default_agent_dir() -> String {
    "~/.trestle".into()
}

// ─────────────────────────────── 凭据 ────────────────────────────────

/// 一台机器的凭据。密码与公钥两种认证都支持。
#[derive(Debug, Clone, Default, Deserialize)]
pub struct TargetSecrets {
    #[serde(default)]
    pub password: Option<SecretRef>,
    #[serde(default)]
    pub key_path: Option<String>,
    #[serde(default)]
    pub key_passphrase: Option<SecretRef>,
}

/// 一个可能需要去别处取值的凭据引用。
///
/// 反序列化时**不解析**——解析发生在 [`SecretRef::resolve`]，这样配置能安全地
/// 序列化回去（Web UI 改配置时不会把明文值写回文件）。
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(transparent)]
pub struct SecretRef(String);

impl SecretRef {
    pub fn new(raw: impl Into<String>) -> Self {
        Self(raw.into())
    }

    /// 原始写法（`env:FOO` / `file:/x` / 明文）。用于回写配置。
    pub fn raw(&self) -> &str {
        &self.0
    }

    /// 是否是间接引用——间接的值不该出现在任何日志或配置导出里。
    pub fn is_indirect(&self) -> bool {
        self.0.starts_with("env:") || self.0.starts_with("file:")
    }

    /// 取出真实值。
    pub fn resolve(&self) -> Result<String> {
        if let Some(var) = self.0.strip_prefix("env:") {
            return std::env::var(var).map_err(|_| TrestleError::Config {
                path: format!("secret env:{var}"),
                detail: format!("environment variable {var} is not set"),
            });
        }
        if let Some(path) = self.0.strip_prefix("file:") {
            let raw = std::fs::read_to_string(path).map_err(|e| TrestleError::Config {
                path: format!("secret file:{path}"),
                detail: format!("cannot read {path}: {e}"),
            })?;
            // 文件里的密码几乎总是带个换行，去掉它——否则认证会以一个极难查的方式失败。
            return Ok(raw.trim_end_matches(['\r', '\n']).to_string());
        }
        Ok(self.0.clone())
    }
}

/// `Debug` 不能泄密：明文值也可能被人直接写在配置里。
impl std::fmt::Display for SecretRef {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if self.is_indirect() {
            f.write_str(&self.0)
        } else {
            f.write_str("<redacted>")
        }
    }
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct Secrets {
    #[serde(default)]
    pub targets: BTreeMap<String, TargetSecrets>,
}

// ───────────────────────────── ConfigStore ──────────────────────────

/// 配置的唯一入口。
///
/// 所有运行期文件都落在**程序所在目录**（portable），不写 `%LOCALAPPDATA%`——
/// 避免「配置在这、状态在那」的管理混乱。
#[derive(Debug, Clone)]
pub struct ConfigStore {
    root: PathBuf,
    config: Config,
    secrets: Secrets,
}

impl ConfigStore {
    /// 程序所在目录——所有运行期文件都落在这里（portable）。
    ///
    /// `TRESTLE_HOME` 可以覆盖它。这不是 D23 的例外，而是开发期的必需：
    /// `cargo run` 产出的可执行文件在 `target/debug/` 下，配置不该跟着跑到那里去。
    pub fn default_root() -> PathBuf {
        if let Ok(home) = std::env::var("TRESTLE_HOME")
            && !home.trim().is_empty()
        {
            return PathBuf::from(home);
        }
        std::env::current_exe()
            .ok()
            .and_then(|exe| exe.parent().map(Path::to_path_buf))
            .unwrap_or_else(|| PathBuf::from("."))
    }

    pub fn load(root: impl Into<PathBuf>) -> Result<Self> {
        let root = root.into();
        let config = Self::read_toml(&root.join(CONFIG_FILE))?.unwrap_or_default();
        let secrets = Self::read_toml(&root.join(SECRETS_FILE))?.unwrap_or_default();
        Ok(Self {
            root,
            config,
            secrets,
        })
    }

    fn read_toml<T: serde::de::DeserializeOwned>(path: &Path) -> Result<Option<T>> {
        match std::fs::read_to_string(path) {
            Ok(raw) => toml::from_str(&raw)
                .map(Some)
                .map_err(|e| TrestleError::Config {
                    path: path.display().to_string(),
                    detail: e.to_string(),
                }),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(TrestleError::Config {
                path: path.display().to_string(),
                detail: e.to_string(),
            }),
        }
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn config(&self) -> &Config {
        &self.config
    }

    /// 状态目录（job 表 / GPU 分配 / 留言板 / forward 声明 / agent 指纹）。
    pub fn state_dir(&self) -> PathBuf {
        self.root.join("state")
    }

    /// 插件目录。
    ///
    /// `TRESTLE_PLUGINS` 可以覆盖它。这不是给用户用的开关，是给**测试与开发**用的：
    /// 每个测试用自己的 home（否则并行跑的时候会互相踩 daemon.json），
    /// 但插件仍然指向仓库里那一份，不必复制一遍。
    pub fn plugins_dir(&self) -> PathBuf {
        if let Ok(dir) = std::env::var("TRESTLE_PLUGINS")
            && !dir.trim().is_empty()
        {
            return PathBuf::from(dir);
        }
        self.root.join("plugins")
    }

    /// 把配置里的 targets 组装成注册表。
    ///
    /// 每台机器的 `connector` 必须指向一个真实存在的 connector 节，否则报可操作的错误。
    pub fn targets(&self) -> Result<TargetRegistry> {
        let known: Vec<String> = self.config.connectors.keys().cloned().collect();
        let mut registry = TargetRegistry::default();
        for (name, tc) in &self.config.targets {
            if !self.config.connectors.contains_key(&tc.connector) {
                return Err(TrestleError::UnknownConnector {
                    name: tc.connector.clone(),
                    known: known.clone(),
                });
            }
            registry.insert(Target {
                name: name.clone(),
                host: tc.host.clone(),
                port: tc.port,
                user: tc.user.clone(),
                connector: tc.connector.clone(),
                workdir: tc.workdir.clone(),
                aliases: tc.aliases.clone(),
                note: tc.note.clone(),
                agent_dir: tc.agent_dir.clone(),
            });
        }
        Ok(registry)
    }

    /// 某台机器的凭据。没配就是空——由 connector 决定这算不算错误
    /// （公钥认证可以完全不配密码）。
    pub fn secrets_for(&self, target: &str) -> TargetSecrets {
        self.secrets
            .targets
            .get(target)
            .cloned()
            .unwrap_or_default()
    }

    /// 属于某个插件的配置节。
    pub fn plugin_section(&self, plugin: &str) -> Option<&toml::Value> {
        self.config.plugins.get(plugin)
    }

    /// 属于某个 connector 的配置节。
    pub fn connector_section(&self, connector: &str) -> Result<&ConnectorConfig> {
        self.config
            .connectors
            .get(connector)
            .ok_or_else(|| TrestleError::UnknownConnector {
                name: connector.to_string(),
                known: self.config.connectors.keys().cloned().collect(),
            })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn secret_ref_passes_plaintext_through() {
        assert_eq!(SecretRef::new("hunter2").resolve().unwrap(), "hunter2");
    }

    #[test]
    fn secret_ref_reads_env() {
        // SAFETY: 单线程测试，设置后立即读取。
        unsafe { std::env::set_var("TRESTLE_TEST_SECRET", "from-env") };
        assert_eq!(
            SecretRef::new("env:TRESTLE_TEST_SECRET").resolve().unwrap(),
            "from-env"
        );
    }

    #[test]
    fn missing_env_var_says_which_one() {
        let err = SecretRef::new("env:TRESTLE_DEFINITELY_NOT_SET")
            .resolve()
            .unwrap_err();
        assert!(err.to_string().contains("TRESTLE_DEFINITELY_NOT_SET"));
    }

    #[test]
    fn file_secret_loses_its_trailing_newline() {
        let dir = std::env::temp_dir().join("trestle-secret-test");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("pw");
        std::fs::write(&path, "s3cret\r\n").unwrap();

        let r = SecretRef::new(format!("file:{}", path.display()));
        // 带着换行去认证会以一种极难排查的方式失败，所以这条断言是刻意的。
        assert_eq!(r.resolve().unwrap(), "s3cret");
    }

    #[test]
    fn plaintext_secret_is_redacted_when_displayed() {
        assert_eq!(SecretRef::new("hunter2").to_string(), "<redacted>");
        // 间接引用本身不是秘密，显示出来有助排查。
        assert_eq!(SecretRef::new("env:FOO").to_string(), "env:FOO");
    }

    #[test]
    fn target_pointing_at_a_missing_connector_is_rejected() {
        let config: Config = toml::from_str(
            r#"
            [connectors.gpu-cluster]
            plugin = "gpu-cluster"

            [targets.gpu-4]
            connector = "typo-here"
            host = "1.2.3.4"
            port = 22
            user = "alice"
            "#,
        )
        .unwrap();
        let store = ConfigStore {
            root: PathBuf::from("."),
            config,
            secrets: Secrets::default(),
        };
        let err = store.targets().unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("typo-here") && msg.contains("gpu-cluster"),
            "{msg}"
        );
    }

    #[test]
    fn connector_settings_stay_opaque_to_the_host() {
        let config: Config = toml::from_str(
            r#"
            [connectors.gpu-cluster]
            plugin = "gpu-cluster"
            socks = "127.0.0.1:11080"
            container = "vpn-proxy"
            "#,
        )
        .unwrap();
        let c = &config.connectors["gpu-cluster"];
        assert_eq!(c.plugin, "gpu-cluster");
        assert!(c.enabled);
        // host 不解释这些字段，原样交给插件。
        assert_eq!(c.settings["socks"].as_str(), Some("127.0.0.1:11080"));
    }

    #[test]
    fn defaults_apply_when_the_file_is_empty() {
        let config: Config = toml::from_str("").unwrap();
        assert_eq!(config.daemon.idle_timeout_secs, 1800);
        assert_eq!(config.defaults.shell_timeout_secs, 60);
        assert!(config.defaults.exclude.contains(&"__pycache__".to_string()));
    }
}
