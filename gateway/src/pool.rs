//! GW-3 egress warm-ticket keeper: 30s ticker over the healthy pool.
//!
//! The loop constructs a Chrome-profiled [`HttpPeer`] per (node, origin) so
//! option-building stays hot and any construction regression fails fast in
//! logs; actual socket reuse stays with Pingora's connection pool.
//!
//! Deviation from manual-A3: the prewarmer borrows [`RouterEngine`] snapshots
//! instead of `Arc<parking_lot::RwLock<Vec<ProxyNode>>>`, so no blocking lock
//! is ever held across an `.await` (frozen repo rule).

use crate::fingerprint::FingerprintHardener;
use crate::model::RoutingSpec;
use crate::router::RouterEngine;
use pingora_core::upstreams::peer::HttpPeer;
use std::sync::Arc;
use std::time::Duration;

/// Warm-ticket interval (plan: 30s).
pub const PREWARM_INTERVAL: Duration = Duration::from_secs(30);
/// OPT-6 单节点 TCP 预建链超时：超时/拒连只记 `failed`，不影响主链路。
pub const PREWARM_TCP_TIMEOUT: Duration = Duration::from_secs(1);

/// OPT-6 单轮预热统计（`warm_once` 返回）。
///
/// - `tickets`：(节点 × 起源) 的 Chrome-profile 票据数（沿用 GW-3 口径）；
/// - `nodes`：本轮实际探测的健康节点数；
/// - `connected` / `failed`：TCP 预建链成功/失败数（失败仅 debug，不告警，
///   Mock 为 plain-HTTP，能连上；连不上也不影响 Pingora 主链路）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WarmStats {
    pub tickets: usize,
    pub nodes: usize,
    pub connected: usize,
    pub failed: usize,
}

pub struct ConnectionPrewarmer {
    router: Arc<RouterEngine>,
    target_origins: Vec<String>,
    interval: Duration,
}

impl ConnectionPrewarmer {
    pub fn new(router: Arc<RouterEngine>, target_origins: Vec<String>) -> Self {
        Self {
            router,
            target_origins,
            interval: PREWARM_INTERVAL,
        }
    }

    /// OPT-6 单轮预热：Chrome 票据保活 + 真 TCP 预建链。
    ///
    /// - 票据环：沿用 GW-3 口径，为每 (节点, 起源) 构造 Chrome-profiled
    ///   `HttpPeer`（option 构建保持 hot，构造回归由 fingerprint 单测覆盖）；
    /// - 建链环：对每个候选节点 `TcpStream::connect(addr)`（1s 超时），
    ///   并发 spawn，统计 `connected/failed`（失败只 debug，不抛错）；
    /// - 永不 panic：空池返回零统计；单节点超时/拒连只记 `failed`。
    pub async fn warm_once(&self) -> WarmStats {
        let candidates = self.router.get_healthy_candidates(&RoutingSpec::default());
        // 票据保活（无网络 I/O）：option 构建热路径。
        let mut tickets = 0;
        for node in &candidates {
            for origin in &self.target_origins {
                let mut peer = HttpPeer::new(node.addr(), false, origin.clone());
                peer.options.connection_timeout = Some(Duration::from_millis(1000));
                FingerprintHardener::apply_chrome_profile(&mut peer);
                tickets += 1;
            }
        }
        // 真 TCP 预建链（并发，1s 超时）：只探“节点可达”，不发应用层字节。
        let mut handles = Vec::with_capacity(candidates.len());
        for node in &candidates {
            let addr = node.addr();
            handles.push(tokio::spawn(async move {
                tokio::time::timeout(PREWARM_TCP_TIMEOUT, tokio::net::TcpStream::connect(addr))
                    .await
                    .is_ok_and(|r| r.is_ok())
            }));
        }
        let mut connected = 0usize;
        for h in handles {
            // Join 失败（任务 panic/取消）按 `failed` 计，不向上传播。
            if let Ok(true) = h.await {
                connected += 1;
            }
        }
        let nodes = candidates.len();
        let failed = nodes.saturating_sub(connected);
        let stats = WarmStats {
            tickets,
            nodes,
            connected,
            failed,
        };
        log::debug!(
            "[Prewarmer] refreshed {} tickets over {} nodes (connected={} failed={})",
            stats.tickets,
            stats.nodes,
            stats.connected,
            stats.failed
        );
        stats
    }

    /// Background loop: refresh tickets every 30s until the process exits.
    pub async fn run(self) {
        let mut ticker = tokio::time::interval(self.interval);
        loop {
            ticker.tick().await;
            let s = self.warm_once().await;
            // 如实日志：成功/失败数都打 info，失败是常态（远端抖动），不告警。
            log::info!(
                "[Prewarmer] tick nodes={} connected={} failed={} tickets={}",
                s.nodes,
                s.connected,
                s.failed,
                s.tickets
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::ProxyNode;

    fn node(ip: &str) -> ProxyNode {
        ProxyNode {
            ip: ip.to_string(),
            port: 8080,
            username: None,
            password: None,
            country: "US".to_string(),
            tier: "residential".to_string(),
            provider: "mock-a".to_string(),
            weight: 100,
        }
    }

    #[tokio::test]
    async fn warm_once_counts_node_origin_tickets() {
        let router = Arc::new(RouterEngine::new(vec![node("10.0.0.1")]));
        let pre = ConnectionPrewarmer::new(
            router,
            vec!["a.example".to_string(), "b.example".to_string()],
        );
        // OPT-6：票据口径不变（1 节点 × 2 起源 = 2）；TCP 建链走不可达地址记 failed。
        let s = pre.warm_once().await;
        assert_eq!(s.tickets, 2);
        assert_eq!(s.nodes, 1);
        assert_eq!(s.connected + s.failed, 1);
    }

    #[tokio::test]
    async fn warm_once_empty_pool_is_noop() {
        let router = Arc::new(RouterEngine::new(vec![]));
        let pre = ConnectionPrewarmer::new(router, vec!["a.example".to_string()]);
        assert_eq!(
            pre.warm_once().await,
            WarmStats {
                tickets: 0,
                nodes: 0,
                connected: 0,
                failed: 0,
            }
        );
    }

    #[tokio::test]
    async fn warm_once_skips_quarantined_nodes() {
        let router = Arc::new(RouterEngine::new(vec![node("10.0.0.9")]));
        router.set_quarantine("", "10.0.0.9", 600);
        let pre = ConnectionPrewarmer::new(router, vec!["a.example".to_string()]);
        // Default-spec domain is "" → quarantined node is skipped.
        let s = pre.warm_once().await;
        assert_eq!(s.nodes, 0);
        assert_eq!(s.tickets, 0);
    }

    #[tokio::test]
    async fn warm_once_refused_port_counts_failed_without_panic() {
        // OPT-6：`127.0.0.1:1` 必拒（特权端口，loopback 无监听），failed 路径不 panic。
        let refused = crate::model::ProxyNode {
            ip: "127.0.0.1".to_string(),
            port: 1,
            username: None,
            password: None,
            country: "US".to_string(),
            tier: "residential".to_string(),
            provider: "mock-a".to_string(),
            weight: 100,
        };
        let router = Arc::new(RouterEngine::new(vec![refused]));
        let pre = ConnectionPrewarmer::new(router, vec!["a.example".to_string()]);
        let s = pre.warm_once().await;
        assert_eq!(s.nodes, 1);
        assert_eq!(s.connected, 0);
        assert_eq!(s.failed, 1);
        assert_eq!(s.tickets, 1);
    }

    #[tokio::test]
    async fn warm_once_local_listener_counts_connected() {
        // OPT-6：本地临时 listener 必连上，connected 计数；零外部依赖。
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind temp listener");
        let port = listener.local_addr().expect("local addr").port();
        // 保持 listener 存活至 warm 结束（connect 靠 backlog 即成功，无需 accept）。
        let local = crate::model::ProxyNode {
            ip: "127.0.0.1".to_string(),
            port,
            username: None,
            password: None,
            country: "US".to_string(),
            tier: "residential".to_string(),
            provider: "mock-a".to_string(),
            weight: 100,
        };
        let router = Arc::new(RouterEngine::new(vec![local]));
        let pre = ConnectionPrewarmer::new(router, vec!["a.example".to_string()]);
        let s = pre.warm_once().await;
        assert_eq!(s.nodes, 1);
        assert_eq!(s.connected, 1, "loopback listener must connect");
        assert_eq!(s.failed, 0);
        drop(listener);
    }
}
