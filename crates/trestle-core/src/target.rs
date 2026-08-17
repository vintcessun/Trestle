//! Target 与解析规则。
//!
//! **没有默认机。** 每个针对单机的操作，`target` 都是必填参数。这条是上一代实测后
//! 由用户拍板的：默认机会制造「打错机器」这类静默事故——你以为在 gpu-4 上删文件，
//! 其实在 gpu-1。多写一个词，换掉一整类事故。
//!
//! 面向全队的操作用可选的 `targets`，留空表示全部——那是「全队」语义，不是「默认机」。

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::error::{Result, TrestleError};

/// 一台可达的机器。
///
/// 注意 `name` / `aliases` / `workdir` / `note` 都来自**配置**而不是 connector：
/// connector 报告它管辖哪些机器，怎么称呼它们是用户的事。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Target {
    /// 全局唯一，操作里用它指名。
    pub name: String,
    pub host: String,
    pub port: u16,
    pub user: String,
    /// 归属的 connector 名。
    pub connector: String,
    /// 默认工作目录。写东西默认往这里写，不要往 `~` 写——好几台机器根分区都吃紧。
    pub workdir: String,
    /// IP / hostname / 别名，解析时也认。
    #[serde(default)]
    pub aliases: Vec<String>,
    /// 给 agent 看的用途说明，会进 targets_list 概览。
    #[serde(default)]
    pub note: String,
    /// 远端 agent 落点。
    #[serde(default = "default_agent_dir")]
    pub agent_dir: String,
}

fn default_agent_dir() -> String {
    "~/.trestle".to_string()
}

impl Target {
    /// 这个名字是否指向本机器（主名或别名，大小写不敏感）。
    pub fn matches(&self, query: &str) -> bool {
        self.name.eq_ignore_ascii_case(query)
            || self.host.eq_ignore_ascii_case(query)
            || self.aliases.iter().any(|a| a.eq_ignore_ascii_case(query))
    }
}

/// 全部 target 的注册表，按 connector 分组呈现。
#[derive(Debug, Clone, Default)]
pub struct TargetRegistry {
    /// 保持 name 有序，让 `targets_list` 和错误消息里的名字列表是稳定的。
    targets: BTreeMap<String, Target>,
}

impl TargetRegistry {
    pub fn new(targets: impl IntoIterator<Item = Target>) -> Self {
        Self {
            targets: targets.into_iter().map(|t| (t.name.clone(), t)).collect(),
        }
    }

    pub fn insert(&mut self, target: Target) {
        self.targets.insert(target.name.clone(), target);
    }

    pub fn is_empty(&self) -> bool {
        self.targets.is_empty()
    }

    pub fn len(&self) -> usize {
        self.targets.len()
    }

    pub fn iter(&self) -> impl Iterator<Item = &Target> {
        self.targets.values()
    }

    pub fn names(&self) -> Vec<String> {
        self.targets.keys().cloned().collect()
    }

    /// 按 connector 分组。用于 `targets_list` 的集约呈现。
    pub fn by_connector(&self) -> BTreeMap<&str, Vec<&Target>> {
        let mut grouped: BTreeMap<&str, Vec<&Target>> = BTreeMap::new();
        for t in self.targets.values() {
            grouped.entry(t.connector.as_str()).or_default().push(t);
        }
        grouped
    }

    /// 解析规则：**名字 → 别名 → host 精确匹配**。
    ///
    /// 失败时错误里带上所有可选名字（见 [`TrestleError::UnknownTarget`]）。
    pub fn resolve(&self, query: &str) -> Result<&Target> {
        // 主名优先：别名不该盖过一个真实存在的主名。
        if let Some(t) = self.targets.get(query) {
            return Ok(t);
        }
        if let Some(t) = self
            .targets
            .values()
            .find(|t| t.name.eq_ignore_ascii_case(query))
        {
            return Ok(t);
        }
        if let Some(t) = self
            .targets
            .values()
            .find(|t| t.aliases.iter().any(|a| a.eq_ignore_ascii_case(query)))
        {
            return Ok(t);
        }
        if let Some(t) = self
            .targets
            .values()
            .find(|t| t.host.eq_ignore_ascii_case(query))
        {
            return Ok(t);
        }
        Err(TrestleError::unknown_target(query, self.names()))
    }

    /// 解析一组名字；留空表示**全部**（「全队」语义）。
    pub fn resolve_many(&self, queries: &[String]) -> Result<Vec<&Target>> {
        if queries.is_empty() {
            return Ok(self.targets.values().collect());
        }
        queries.iter().map(|q| self.resolve(q)).collect()
    }
}

/// connector 自检结果，给 `trestle doctor` 用。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Health {
    pub ok: bool,
    /// 人和 agent 都能读懂的一句话。
    pub detail: String,
    /// 不 ok 时的下一步。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub remedy: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub latency_ms: Option<u64>,
}

impl Health {
    pub fn ok(detail: impl Into<String>) -> Self {
        Self {
            ok: true,
            detail: detail.into(),
            remedy: None,
            latency_ms: None,
        }
    }

    pub fn failed(detail: impl Into<String>, remedy: impl Into<String>) -> Self {
        Self {
            ok: false,
            detail: detail.into(),
            remedy: Some(remedy.into()),
            latency_ms: None,
        }
    }

    pub fn with_latency(mut self, ms: u64) -> Self {
        self.latency_ms = Some(ms);
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn registry() -> TargetRegistry {
        TargetRegistry::new([
            Target {
                name: "gpu-4".into(),
                host: "203.0.113.31".into(),
                port: 2204,
                user: "alice".into(),
                connector: "gpu-cluster".into(),
                workdir: "/home/alice/data".into(),
                aliases: vec!["node-16".into()],
                note: String::new(),
                agent_dir: default_agent_dir(),
            },
            Target {
                name: "gpu-1".into(),
                host: "203.0.113.10".into(),
                port: 2201,
                user: "alice".into(),
                connector: "gpu-cluster".into(),
                workdir: "/mnt/data/alice".into(),
                aliases: vec!["node-a".into(), "lab".into()],
                note: String::new(),
                agent_dir: default_agent_dir(),
            },
        ])
    }

    #[test]
    fn resolves_by_name_alias_and_host() {
        let r = registry();
        assert_eq!(r.resolve("gpu-4").unwrap().name, "gpu-4");
        assert_eq!(r.resolve("lab").unwrap().name, "gpu-1");
        assert_eq!(r.resolve("203.0.113.31").unwrap().name, "gpu-4");
        assert_eq!(r.resolve("node-a").unwrap().name, "gpu-1");
    }

    #[test]
    fn resolution_is_case_insensitive() {
        assert_eq!(registry().resolve("X63").unwrap().name, "gpu-4");
    }

    #[test]
    fn unknown_name_lists_the_alternatives() {
        let err = registry().resolve("x36").unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("x36"), "{msg}");
        assert!(msg.contains("gpu-1") && msg.contains("gpu-4"), "{msg}");
    }

    #[test]
    fn empty_target_list_means_the_whole_fleet() {
        let r = registry();
        assert_eq!(r.resolve_many(&[]).unwrap().len(), 2);
    }

    #[test]
    fn resolve_many_fails_loudly_on_one_bad_name() {
        let r = registry();
        assert!(r.resolve_many(&["gpu-4".into(), "nope".into()]).is_err());
    }

    #[test]
    fn grouping_is_by_connector() {
        let registry = registry();
        let grouped = registry.by_connector();
        assert_eq!(grouped.len(), 1);
        assert_eq!(grouped["gpu-cluster"].len(), 2);
    }
}
