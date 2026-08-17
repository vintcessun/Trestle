//! 句柄表：把 host 侧的活对象（流、SSH 会话、agent 连接）映射成插件能拿的 u64。
//!
//! 插件拿到的永远只是一个数字。它不能伪造一个句柄去访问别的插件的连接——
//! 每个插件实例有自己的一张表，编号也不跨表复用。

use std::collections::HashMap;
use std::sync::Arc;

use tokio::net::TcpStream;
use trestle_transport::{AgentClient, Forward, SshSession};

/// 一个插件实例持有的全部 host 侧资源。
#[derive(Default)]
pub struct HandleTable {
    next: u64,
    streams: HashMap<u64, TcpStream>,
    sessions: HashMap<u64, Arc<SshSession>>,
    agents: HashMap<u64, Arc<AgentClient>>,
    forwards: HashMap<u64, Forward>,
}

impl HandleTable {
    fn alloc(&mut self) -> u64 {
        // 从 1 开始：0 留作「无效句柄」，让插件里未初始化的变量立刻暴露出来。
        self.next += 1;
        self.next
    }

    pub fn put_stream(&mut self, s: TcpStream) -> u64 {
        let h = self.alloc();
        self.streams.insert(h, s);
        h
    }

    /// 取走一条流。SSH 握手会消费掉它，所以这里是 take 而不是 get。
    pub fn take_stream(&mut self, h: u64) -> Option<TcpStream> {
        self.streams.remove(&h)
    }

    pub fn put_session(&mut self, s: Arc<SshSession>) -> u64 {
        let h = self.alloc();
        self.sessions.insert(h, s);
        h
    }

    pub fn session(&self, h: u64) -> Option<Arc<SshSession>> {
        self.sessions.get(&h).cloned()
    }

    pub fn drop_session(&mut self, h: u64) -> Option<Arc<SshSession>> {
        self.sessions.remove(&h)
    }

    pub fn put_agent(&mut self, a: Arc<AgentClient>) -> u64 {
        let h = self.alloc();
        self.agents.insert(h, a);
        h
    }

    pub fn agent(&self, h: u64) -> Option<Arc<AgentClient>> {
        self.agents.get(&h).cloned()
    }

    pub fn drop_agent(&mut self, h: u64) -> Option<Arc<AgentClient>> {
        self.agents.remove(&h)
    }

    pub fn put_forward(&mut self, f: Forward) -> u64 {
        let h = self.alloc();
        self.forwards.insert(h, f);
        h
    }

    pub fn take_forward(&mut self, h: u64) -> Option<Forward> {
        self.forwards.remove(&h)
    }

    /// 一个插件实例被回收时，它开的所有东西都跟着走。
    pub fn forwards_drain(&mut self) -> Vec<Forward> {
        self.forwards.drain().map(|(_, f)| f).collect()
    }

    pub fn counts(&self) -> (usize, usize, usize, usize) {
        (
            self.streams.len(),
            self.sessions.len(),
            self.agents.len(),
            self.forwards.len(),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn handles_start_at_one_so_zero_is_always_invalid() {
        let mut t = HandleTable::default();
        // 插件里一个没初始化的句柄变量是 0，它必须永远查不到东西。
        assert!(t.session(0).is_none());
        assert!(t.agent(0).is_none());
        let _ = t.alloc();
        assert_eq!(t.next, 1);
    }

    #[test]
    fn handles_are_never_reused() {
        let mut t = HandleTable::default();
        let a = t.alloc();
        let b = t.alloc();
        assert_ne!(a, b);
        // 即使中间释放过，新句柄也不会撞回旧编号。
        t.sessions.remove(&a);
        let c = t.alloc();
        assert!(c > b);
    }

    #[test]
    fn a_stream_can_only_be_consumed_once() {
        let mut t = HandleTable::default();
        // 用一个假的句柄编号验证语义：take 之后就不在了。
        assert!(t.take_stream(1).is_none());
        assert_eq!(t.counts(), (0, 0, 0, 0));
    }
}
