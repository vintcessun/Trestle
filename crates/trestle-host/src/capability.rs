//! Capability：插件能做什么，由它自己的 manifest 声明，由 host 强制。
//!
//! 强制发生在 host 导入的入口处——插件绕不过去，因为它**没有别的路**：
//! wasm 组件没有 syscall，唯一能碰到外界的地方就是这些导入。
//!
//! 被拒绝的调用必须能被看见（发 `plugin-call-denied` 事件），
//! 否则权限模型就是个黑盒，出问题时没人知道是被挡了还是根本没调。

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, thiserror::Error)]
#[error("plugin '{plugin}' denied: {action} is not in its manifest allowlist")]
pub struct CapabilityError {
    pub plugin: String,
    pub action: String,
}

/// 一个插件的 manifest。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Manifest {
    pub name: String,
    #[serde(default)]
    pub version: String,
    #[serde(default)]
    pub description: String,
    /// `connector` 或 `tool`。
    #[serde(default)]
    pub kind: String,
    #[serde(default)]
    pub capabilities: Capabilities,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Capabilities {
    /// 允许 `local-exec` 的 argv[0]。空 = 一条本机命令都不许跑。
    #[serde(default)]
    pub local_exec: Vec<String>,
    /// 允许拨号到的地址前缀（`127.0.0.1:`、`10.`…）。空 = 不限制。
    ///
    /// 注意：connector 天然需要连任意目标机器，所以这条通常留空；
    /// 它存在是为了限制**技能插件**——技能插件根本不该自己拨号。
    #[serde(default)]
    pub dial: Vec<String>,
    /// 允许调用的别的插件。
    #[serde(default)]
    pub call_plugins: Vec<String>,
    /// 允许开 ws 端点。
    #[serde(default)]
    pub ws: bool,
    /// 允许注册周期任务。
    #[serde(default)]
    pub tasks: bool,
    /// 允许向 GPU 分配器要卡。
    #[serde(default)]
    pub gpu: bool,
    /// 允许开端口转发。
    #[serde(default)]
    pub forward: bool,
    /// 允许贡献 Web UI 资源。
    #[serde(default)]
    pub ui: bool,

    /// 我不在 wasm 内存里存跨调用的状态，可以起多个实例。
    ///
    /// 一个 wasm 实例在被调用期间是**独占**的，所以默认每个插件只有一个实例，
    /// 两个 agent 同时调同一个工具就得排队。声明了这条之后 host 会起一个实例池，
    /// 它们互不阻塞。
    ///
    /// **只有真的无状态才能声明。** 如果插件把东西存在 wasm 全局变量里
    /// （`static mut`、`thread_local!`），池里的实例会各看各的，而且是**静默**出错——
    /// host 没有办法替你验证这一点。要跨调用记东西，用 `host.state-*`（per-plugin KV）。
    #[serde(default)]
    pub stateless: bool,
}

impl Capabilities {
    pub fn allows_local_exec(&self, program: &str) -> bool {
        // 只比对 argv[0] 的基名：白名单里写 `docker`，`/usr/bin/docker` 也算。
        let base = program
            .rsplit(['/', '\\'])
            .next()
            .unwrap_or(program)
            .trim_end_matches(".exe");
        self.local_exec
            .iter()
            .any(|allowed| allowed.trim_end_matches(".exe") == base)
    }

    pub fn allows_dial(&self, addr: &str) -> bool {
        self.dial.is_empty() || self.dial.iter().any(|p| addr.starts_with(p))
    }

    pub fn allows_calling(&self, plugin: &str) -> bool {
        self.call_plugins.iter().any(|p| p == plugin)
    }
}

/// 一次拒绝的完整上下文，用于发事件。
pub fn deny(plugin: &str, action: impl Into<String>) -> CapabilityError {
    CapabilityError {
        plugin: plugin.to_string(),
        action: action.into(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn caps() -> Capabilities {
        Capabilities {
            local_exec: vec!["docker".into()],
            dial: vec!["127.0.0.1:".into()],
            call_plugins: vec!["nvidia".into()],
            ..Default::default()
        }
    }

    #[test]
    fn local_exec_matches_on_the_program_basename() {
        let c = caps();
        assert!(c.allows_local_exec("docker"));
        assert!(c.allows_local_exec("/usr/bin/docker"));
        assert!(c.allows_local_exec("C:\\Program Files\\Docker\\docker.exe"));
    }

    #[test]
    fn anything_not_listed_is_denied() {
        let c = caps();
        assert!(!c.allows_local_exec("rm"));
        assert!(!c.allows_local_exec("bash"));
        // 名字里含 docker 不等于是 docker。
        assert!(!c.allows_local_exec("docker-compose"));
    }

    #[test]
    fn an_empty_allowlist_denies_everything() {
        let c = Capabilities::default();
        assert!(!c.allows_local_exec("docker"));
        assert!(!c.allows_local_exec("echo"));
    }

    #[test]
    fn an_empty_dial_list_means_unrestricted() {
        // connector 天生要连任意目标机器，所以「不写」是「不限制」。
        let c = Capabilities::default();
        assert!(c.allows_dial("203.0.113.10:2201"));
    }

    #[test]
    fn a_dial_allowlist_is_a_prefix_match() {
        let c = caps();
        assert!(c.allows_dial("127.0.0.1:11080"));
        assert!(!c.allows_dial("203.0.113.10:2201"));
    }

    #[test]
    fn plugin_to_plugin_calls_need_an_explicit_entry() {
        let c = caps();
        assert!(c.allows_calling("nvidia"));
        assert!(!c.allows_calling("job"));
    }

    #[test]
    fn a_manifest_round_trips_through_toml() {
        let raw = r#"
            name = "ssh-socks5"
            kind = "connector"

            [capabilities]
            local_exec = ["docker"]
            forward = true
        "#;
        let m: Manifest = toml::from_str(raw).unwrap();
        assert_eq!(m.name, "ssh-socks5");
        assert!(m.capabilities.allows_local_exec("docker"));
        assert!(m.capabilities.forward);
        assert!(!m.capabilities.ws);
    }
}
