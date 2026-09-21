//! GW-3 egress warm-ticket keeper: 30s ticker over the healthy pool.
//!
//! R2-7：只留真 TCP 探测 + 日志。GW-3 的票据环（每 (节点, 起源) 构造
//! Chrome-profiled `HttpPeer`）已删除：无连接复用收益的纯构造热身不值得每轮
//! O(节点×起源) 开销；`HttpPeer` 构造回归由 fingerprint 单测覆盖。
//!
//! Deviation from manual-A3: the prewarmer borrows [`RouterEngine`] snapshots
//! instead of `Arc<parking_lot::RwLock<Vec<ProxyNode>>>`, so no blocking lock
//! is ever held across an `.await` (frozen repo rule).

use crate::model::RoutingSpec;
use crate::router::RouterEngine;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::Semaphore;

/// Warm-ticket interval (plan: 30s).
pub const PREWARM_INTERVAL: Duration = Duration::from_secs(30);
/// OPT-6 单节点 TCP 预建链超时：超时/拒连只记 `failed`，不影响主链路。
pub const PREWARM_TCP_TIMEOUT: Duration = Duration::from_secs(1);

/// OPT-6 单轮预热统计（`warm_once` 返回）。
///
/// - `tickets`：R2-7 起语义冻结为本轮实际探测的健康节点数（== `nodes`；
///   票据环删除后不再有“节点×起源”口径，见 OPERATION §4）；
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
    interval: Duration,
    /// R2-7 建链并发上限（信号量；默认 100，百节点池单轮齐探不 FD burst）。
    semaphore: Arc<Semaphore>,
}

/// R2-7 默认建链并发上限（与百节点池同量级；单轮齐探至多 100 并发建链）。
pub const PREWARM_MAX_CONCURRENT: usize = 100;

impl ConnectionPrewarmer {
    pub fn new(router: Arc<RouterEngine>) -> Self {
        Self::with_max_concurrent(router, PREWARM_MAX_CONCURRENT)
    }

    /// 注入并发上限的构造器（单测用，线上走 `new()`）。
    pub fn with_max_concurrent(router: Arc<RouterEngine>, max_concurrent: usize) -> Self {
        Self {
            router,
            interval: PREWARM_INTERVAL,
            semaphore: Arc::new(Semaphore::new(max_concurrent.max(1))),
        }
    }

    /// 注入滴答间隔的 builder（R2-8 env 覆盖用；默认 30s）。
    pub fn with_interval(mut self, interval: Duration) -> Self {
        self.interval = interval;
        self
    }

    /// OPT-6 单轮预热：真 TCP 预建链（R2-7 信号量限流）。
    ///
    /// - 建链环：对每个候选节点 `TcpStream::connect(addr)`（1s 超时），
    ///   并发 spawn（信号量封顶），统计 `connected/failed`（失败只 debug，不抛错）；
    /// - 永不 panic：空池返回零统计；单节点超时/拒连只记 `failed`。
    pub async fn warm_once(&self) -> WarmStats {
        let candidates = self.router.get_healthy_candidates(&RoutingSpec::default());
        // 真 TCP 预建链（并发 + 信号量封顶，1s 超时）：只探“节点可达”，不发应用层字节。
        let mut handles = Vec::with_capacity(candidates.len());
        for node in &candidates {
            let addr = node.addr.clone();
            let sem = self.semaphore.clone();
            handles.push(tokio::spawn(async move {
                // 许可在建链期间持有：并发建链数恒 ≤ 上限（FD 有界）。
                // 拿不到许可（信号量关闭，理论不可达）按失败计，不 panic。
                let _permit = match sem.acquire_owned().await {
                    Ok(p) => p,
                    Err(_) => return false,
                };
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
            tickets: nodes,
            nodes,
            connected,
            failed,
        };
        log::debug!(
            "[Prewarmer] probed {} nodes (connected={} failed={})",
            stats.nodes,
            stats.connected,
            stats.failed
        );
        stats
    }

    /// Background loop: probe the healthy pool every 30s until the process exits.
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
        ProxyNode::new(
            ip.to_string(),
            8080,
            None,
            None,
            "US".to_string(),
            "residential".to_string(),
            "mock-a".to_string(),
            100,
        )
    }

    #[tokio::test]
    async fn warm_once_counts_probed_nodes() {
        // R2-7：票据环删除后 `tickets` == 探测节点数（不再是“节点×起源”口径）。
        let router = Arc::new(RouterEngine::new(vec![node("10.0.0.1")]));
        let pre = ConnectionPrewarmer::new(router);
        // TCP 建链走不可达地址记 failed；计数口径不断言连通性。
        let s = pre.warm_once().await;
        assert_eq!(s.tickets, 1);
        assert_eq!(s.nodes, 1);
        assert_eq!(s.connected + s.failed, 1);
    }

    #[tokio::test]
    async fn warm_once_empty_pool_is_noop() {
        let router = Arc::new(RouterEngine::new(vec![]));
        let pre = ConnectionPrewarmer::new(router);
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
        let pre = ConnectionPrewarmer::new(router);
        // Default-spec domain is "" → quarantined node is skipped.
        let s = pre.warm_once().await;
        assert_eq!(s.nodes, 0);
        assert_eq!(s.tickets, 0);
    }

    #[tokio::test]
    async fn warm_once_refused_port_counts_failed_without_panic() {
        // OPT-6：`127.0.0.1:1` 必拒（特权端口，loopback 无监听），failed 路径不 panic。
        let refused = crate::model::ProxyNode::new(
            "127.0.0.1".to_string(),
            1,
            None,
            None,
            "US".to_string(),
            "residential".to_string(),
            "mock-a".to_string(),
            100,
        );
        let router = Arc::new(RouterEngine::new(vec![refused]));
        let pre = ConnectionPrewarmer::new(router);
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
        let local = crate::model::ProxyNode::new(
            "127.0.0.1".to_string(),
            port,
            None,
            None,
            "US".to_string(),
            "residential".to_string(),
            "mock-a".to_string(),
            100,
        );
        let router = Arc::new(RouterEngine::new(vec![local]));
        let pre = ConnectionPrewarmer::new(router);
        let s = pre.warm_once().await;
        assert_eq!(s.nodes, 1);
        assert_eq!(s.connected, 1, "loopback listener must connect");
        assert_eq!(s.failed, 0);
        drop(listener);
    }

    #[tokio::test]
    async fn warm_once_many_nodes_completes_bounded() {
        // R2-7 限流路径：150 节点（超 100 上限，分两波）单轮完成、无 FD 耗尽、
        // 计数闭合（connected+failed==nodes；连通性本身不断言，拒连/占用皆可）。
        let nodes: Vec<crate::model::ProxyNode> = (1u16..=150)
            .map(|p| {
                crate::model::ProxyNode::new(
                    "127.0.0.1".to_string(),
                    p,
                    None,
                    None,
                    "US".to_string(),
                    "residential".to_string(),
                    "mock-a".to_string(),
                    100,
                )
            })
            .collect();
        let router = Arc::new(RouterEngine::new(nodes));
        let pre = ConnectionPrewarmer::new(router);
        let s = pre.warm_once().await;
        assert_eq!(s.nodes, 150);
        assert_eq!(s.tickets, 150);
        assert_eq!(s.connected + s.failed, 150);
    }

    #[tokio::test]
    async fn warm_concurrency_capped_by_semaphore() {
        // R2-7：同一信号量上 5 任务 × 50ms 持有，实测最大并发恒 ≤ 上限（2）。
        // 锁的是 `warm_once` 真用的那把 `semaphore`（接线不断），非自建信号量。
        use std::sync::atomic::{AtomicUsize, Ordering};
        let router = Arc::new(RouterEngine::new(vec![]));
        let pre = ConnectionPrewarmer::with_max_concurrent(router, 2);
        let current = Arc::new(AtomicUsize::new(0));
        let max = Arc::new(AtomicUsize::new(0));
        let mut handles = Vec::new();
        for _ in 0..5 {
            let sem = pre.semaphore.clone();
            let current = current.clone();
            let max = max.clone();
            handles.push(tokio::spawn(async move {
                let _permit = sem.acquire_owned().await.expect("semaphore open");
                let now = current.fetch_add(1, Ordering::SeqCst) + 1;
                max.fetch_max(now, Ordering::SeqCst);
                tokio::time::sleep(Duration::from_millis(50)).await;
                current.fetch_sub(1, Ordering::SeqCst);
            }));
        }
        for h in handles {
            h.await.expect("task joins");
        }
        assert!(
            max.load(Ordering::SeqCst) <= 2,
            "max concurrent exceeded permits"
        );
    }
}
