//! WASM 插件宿主。
//!
//! 这里做三件事：把传输工具箱与 host 服务导出给插件、强制 capability、
//! 把 `target` 路由到它所属的 connector。
//!
//! **编排逻辑一行都不在这里**——什么时候拉容器、走哪条路、断了重试几次，
//! 全在插件里。这个边界是整个架构成立的前提：如果 host 开始替插件做决定，
//! 那插件就只是配置文件了。

pub mod capability;
pub mod fleet;
pub mod gpu;
pub mod handles;
pub mod host;
pub mod imports;
pub mod runtime;
pub mod state;
pub mod tool_state;
pub mod tools;

pub mod bindings {
    //! 由 `wit/trestle.wit` 生成的 host 侧绑定（connector 世界）。
    wasmtime::component::bindgen!({
        path: "../../wit",
        world: "connector",
        // host 导入全部异步：它们真的在做 I/O（拨号、SSH、传文件）。
        // 插件那一侧看到的仍是普通的同步调用——wasmtime 用 fiber 把它挂起，
        // 于是一个插件在等网络时，别的插件照常推进。
        imports: { default: async },
        exports: { default: async },
    });
}

pub mod bindings_tool {
    //! 技能插件世界。
    //!
    //! `types` 与 `host-services` 两个接口和 connector 世界是同一份——用 `with:`
    //! 指过去，免得同一个 WIT 类型在两处各生成一遍、彼此还不兼容。
    wasmtime::component::bindgen!({
        path: "../../wit",
        world: "tool-plugin",
        imports: { default: async },
        exports: { default: async },
        with: {
            "trestle:plugin/types": crate::bindings::trestle::plugin::types,
        },
    });
}

pub use capability::{Capabilities, CapabilityError};
pub use handles::HandleTable;
pub use state::PluginState;

/// 主机侧的中转文件路径。
///
/// 跨机搬运要在本地落一份中转文件，而插件不能自己拼这个路径——它在 wasm 里看到的
/// 文件系统跟 host 的不是一回事，拼出来的 `/tmp/x` 在 Windows 主机上根本不存在。
pub fn staging_path(name: &str) -> String {
    let dir = std::env::temp_dir().join("trestle-staging");
    let _ = std::fs::create_dir_all(&dir);
    let safe: String = name
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '.' || c == '-' || c == '_' {
                c
            } else {
                '-'
            }
        })
        .collect();
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0);
    dir.join(format!("{stamp}-{safe}")).display().to_string()
}
