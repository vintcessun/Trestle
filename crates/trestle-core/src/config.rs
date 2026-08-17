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
/// 样例配置。`trestle.toml` 不在时的兜底——见 [`ConfigStore::load`]。
pub const EXAMPLE_CONFIG_FILE: &str = "trestle.example.toml";

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
    /// 一个插件最多能有几个 wasm 实例 = 它最多能几路并发。**0 = 跟 CPU 核心数走**。
    ///
    /// 池从 1 个实例起，撞上并发才指数长到这个上限。
    #[serde(default)]
    pub pool_max: usize,
    /// 一个池连续这么久没撞上并发，就开始一个一个把实例收回来。0 = 用默认值。
    #[serde(default)]
    pub pool_idle_secs: u64,
}

impl Default for DaemonConfig {
    fn default() -> Self {
        Self {
            idle_timeout_secs: default_idle_timeout(),
            ipc_bind: default_ipc_bind(),
            http_bind: default_http_bind(),
            pool_max: 0,
            pool_idle_secs: 0,
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
    /// 加载哪个 connector **驱动**（`plugins/connectors/<plugin>/<plugin>.wasm`）。
    ///
    /// 驱动是通用的（`ssh-socks5`、`ssh-direct`），配置节的名字才是这一组机器的
    /// 称呼（`gpu-cluster`）。同一个驱动可以被配置成任意多个 connector。
    pub plugin: String,
    #[serde(default = "default_true")]
    pub enabled: bool,
    /// 准这个 connector 在**本机**跑哪些命令（按 argv[0] 的基名精确匹配）。
    ///
    /// 通用驱动没法在自己的 manifest 里预知你要用 `docker` 还是 `wg-quick` 把
    /// 前置条件拉起来，所以这份授权由你在配置里给。host 把它并进 manifest 的
    /// 白名单——**本机跑任意命令等于全部权限**，所以这里写的东西要看清楚。
    #[serde(default)]
    pub allow_exec: Vec<String>,
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
    /// 真配置不在，用的是样例。调用方该说一声——样例里的机器连不上。
    from_example: bool,
    /// 显式指定的插件目录。测试用它，好过去改进程全局的环境变量
    /// （同一个测试二进制里的别的测试会跟着一起变）。
    plugins_override: Option<PathBuf>,
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

    /// 读配置。`trestle.toml` 不在就退到 `trestle.example.toml`。
    ///
    /// 这个兜底不是给用户的便利，是给**新 clone** 的：`trestle.toml` 是你自己的
    /// 机器清单，已 gitignore，没有它的话 clone 下来连 `cargo test` 都跑不了。
    /// 退到样例之后 [`Self::from_example`] 为真，daemon 会在启动时说一声——
    /// 样例里的地址是 RFC 5737 文档保留段，连不上任何东西，**静默**用它才是坑。
    pub fn load(root: impl Into<PathBuf>) -> Result<Self> {
        let root = root.into();
        let mut from_example = false;
        let config = match Self::read_toml(&root.join(CONFIG_FILE))? {
            Some(c) => c,
            None => match Self::read_toml(&root.join(EXAMPLE_CONFIG_FILE))? {
                Some(c) => {
                    from_example = true;
                    c
                }
                None => Config::default(),
            },
        };
        let secrets = Self::read_toml(&root.join(SECRETS_FILE))?.unwrap_or_default();
        Ok(Self {
            root,
            config,
            secrets,
            from_example,
            plugins_override: None,
        })
    }

    /// 换一个插件目录。
    pub fn with_plugins_dir(mut self, dir: impl Into<PathBuf>) -> Self {
        self.plugins_override = Some(dir.into());
        self
    }

    /// 现在跑的是样例配置吗。
    pub fn from_example(&self) -> bool {
        self.from_example
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
        if let Some(dir) = &self.plugins_override {
            return dir.clone();
        }
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
            plugin = "ssh-socks5"

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
            from_example: false,
            plugins_override: None,
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
            plugin = "ssh-socks5"
            socks = "127.0.0.1:11080"
            container = "vpn-proxy"
            "#,
        )
        .unwrap();
        let c = &config.connectors["gpu-cluster"];
        assert_eq!(c.plugin, "ssh-socks5");
        assert!(c.enabled);
        // host 不解释这些字段，原样交给插件。
        assert_eq!(c.settings["socks"].as_str(), Some("127.0.0.1:11080"));
    }

    #[test]
    fn a_fresh_clone_falls_back_to_the_example_and_admits_it() {
        // trestle.toml 是用户自己的机器清单，不入库。所以 clone 下来只有样例——
        // 兜底读它，但必须**说出来**：样例里的地址是 RFC 5737 文档保留段，
        // 静默用它会让人对着一堆连不上的机器查半天网络。
        let dir = std::env::temp_dir().join("trestle-example-fallback");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join(EXAMPLE_CONFIG_FILE),
            "[targets.demo]\nconnector = \"c\"\nhost = \"203.0.113.1\"\nport = 22\n\
             user = \"alice\"\n\n[connectors.c]\nplugin = \"ssh-direct\"\n",
        )
        .unwrap();

        let store = ConfigStore::load(&dir).unwrap();
        assert!(store.from_example());
        assert!(store.config().targets.contains_key("demo"));

        // 真配置一在，样例就完全不参与。
        std::fs::write(dir.join(CONFIG_FILE), "[targets]\n").unwrap();
        let store = ConfigStore::load(&dir).unwrap();
        assert!(!store.from_example());
        assert!(store.config().targets.is_empty());

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn defaults_apply_when_the_file_is_empty() {
        let config: Config = toml::from_str("").unwrap();
        assert_eq!(config.daemon.idle_timeout_secs, 1800);
        assert_eq!(config.defaults.shell_timeout_secs, 60);
        assert!(config.defaults.exclude.contains(&"__pycache__".to_string()));
    }
}
