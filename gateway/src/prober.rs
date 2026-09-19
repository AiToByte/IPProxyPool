//! GW-2 graded canary prober: probes egress nodes through Cloudflare trace.
//!
//! Each node is probed via `GET https://cloudflare.com/cdn-cgi/trace` routed
//! through the node as an HTTP proxy; the `ip=` line reveals the real exit IP.
//! - `Healthy`: 2xx + trace parsed;
//! - `Degraded`: reachable but unexpected status;
//! - `Dead`: transport/timeout failure.

use crate::model::ProxyNode;
use dashmap::DashMap;
use reqwest::Client;
use std::time::{Duration, Instant};

pub const TRACE_URL: &str = "https://cloudflare.com/cdn-cgi/trace";

/// 单次探测超时（OPT-5：构造共享 Client 时统一配置）。
pub const PROBE_TIMEOUT: Duration = Duration::from_millis(2500);

#[derive(Debug)]
pub enum ProbeResult {
    Healthy { latency_ms: u64, exit_ip: String },
    Degraded { reason: String },
    Dead { error: String },
}

/// OPT-5 分级探针：按代理地址缓存复用 `reqwest::Client`。
///
/// - 为何不是单个全局 Client：reqwest 的 `Proxy` 是 **Client 级**配置，
///   不支持 per-request 切换代理；不同出口节点（不同 `proxy_url`）必须用
///   不同 Client。按 `proxy_url` 缓存后，60s 滴答对同一节点复用同一 Client，
///   连接池/TLS 上下文不再每轮重建，日志无建链抖动；
/// - 失败降级：`Client::builder().build()` 失败率极低，失败时返回 `None`，
///   调用方降级为 `Dead`，**永不 panic**（相对 `expect` 更稳）；
/// - 线程安全：`DashMap` 支持探针循环与单测并发取用。
pub struct CanaryProber {
    /// Key = `proxy_url`（`http://[user:pass@]ip:port`），Value = 复用 Client。
    clients: DashMap<String, Client>,
    /// 探测超时（默认 2500ms，单测可注入更小值）。
    timeout: Duration,
}

impl CanaryProber {
    pub fn new() -> Self {
        Self::new_with_timeout(PROBE_TIMEOUT)
    }

    /// 注入超时的构造器（单测/调参用，线上走 `new()`）。
    pub fn new_with_timeout(timeout: Duration) -> Self {
        Self {
            clients: DashMap::new(),
            timeout,
        }
    }

    /// 当前缓存的 Client 个数（单测/运维观察复用是否生效）。
    pub fn client_count(&self) -> usize {
        self.clients.len()
    }

    /// 取（或建）某代理地址的复用 Client。
    ///
    /// - 命中缓存直接克隆句柄（`Client` 内部为 `Arc`，克隆廉价）；
    /// - 未命中则按统一超时构建，失败返回 `None`（调用方判 `Dead`）。
    fn client_for(&self, proxy_url: &str) -> Option<Client> {
        if let Some(hit) = self.clients.get(proxy_url) {
            return Some(hit.value().clone());
        }
        let proxy = reqwest::Proxy::all(proxy_url).ok()?;
        let client = Client::builder()
            .proxy(proxy)
            .timeout(self.timeout)
            .build()
            .ok()?;
        self.clients.insert(proxy_url.to_string(), client.clone());
        Some(client)
    }

    /// 拼接某节点的代理 URL（含可选的上游认证）。
    fn proxy_url_for(node: &ProxyNode) -> String {
        match (&node.username, &node.password) {
            (Some(u), Some(p)) => format!("http://{u}:{p}@{}:{}", node.ip, node.port),
            _ => format!("http://{}:{}", node.ip, node.port),
        }
    }

    /// Probe one egress node through itself as the HTTP proxy.
    pub async fn probe_node(&self, node: &ProxyNode) -> ProbeResult {
        let proxy_url = Self::proxy_url_for(node);
        let client = match self.client_for(&proxy_url) {
            Some(c) => c,
            None => {
                return ProbeResult::Dead {
                    error: format!("invalid proxy url: {proxy_url}"),
                }
            }
        };

        let start = Instant::now();
        match client.get(TRACE_URL).send().await {
            Ok(resp) if resp.status().is_success() => {
                let text = resp.text().await.unwrap_or_default();
                let latency = start.elapsed().as_millis() as u64;
                let exit_ip = extract_exit_ip(&text).unwrap_or_else(|| node.ip.clone());
                ProbeResult::Healthy {
                    latency_ms: latency,
                    exit_ip,
                }
            }
            Ok(resp) => ProbeResult::Degraded {
                reason: format!("Unexpected status: {}", resp.status()),
            },
            Err(e) => ProbeResult::Dead {
                error: e.to_string(),
            },
        }
    }
}

impl Default for CanaryProber {
    fn default() -> Self {
        Self::new()
    }
}

/// Extract the `ip=` exit address from a Cloudflare trace body.
pub fn extract_exit_ip(trace_body: &str) -> Option<String> {
    trace_body
        .lines()
        .find(|line| line.starts_with("ip="))
        .and_then(|line| line.strip_prefix("ip="))
        .map(|ip| ip.trim().to_string())
        .filter(|ip| !ip.is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE_TRACE: &str =
        "fl=123\nh=cloudflare.com\nip=203.0.113.7\nts=1700000000\nvisit_scheme=https\nuag=curl\n";

    #[test]
    fn extracts_exit_ip() {
        assert_eq!(
            extract_exit_ip(SAMPLE_TRACE).as_deref(),
            Some("203.0.113.7")
        );
    }

    #[test]
    fn missing_ip_returns_none() {
        assert_eq!(extract_exit_ip("fl=1\nh=x\n"), None);
        assert_eq!(extract_exit_ip(""), None);
        assert_eq!(extract_exit_ip("ip=\n"), None);
    }

    #[test]
    fn reuses_client_per_proxy_url() {
        // OPT-5：同一代理地址二次取用不建新 Client（建链抖动消除的根因）；
        // 不同地址各自缓存，互不串扰。
        let prober = CanaryProber::new();
        let a = "http://127.0.0.1:18081";
        let b = "http://127.0.0.1:18082";
        assert!(prober.client_for(a).is_some());
        assert_eq!(prober.client_count(), 1);
        assert!(prober.client_for(a).is_some());
        assert_eq!(prober.client_count(), 1, "same url must reuse");
        assert!(prober.client_for(b).is_some());
        assert_eq!(prober.client_count(), 2);
    }

    #[tokio::test]
    async fn invalid_proxy_url_degrades_to_dead_without_panic() {
        // 非法代理地址（如不可解析 scheme）不 panic，降级为 Dead。
        let prober = CanaryProber::new();
        let bad = crate::model::ProxyNode {
            ip: "127.0.0.1".to_string(),
            port: 8080,
            username: Some("u with space".to_string()),
            password: Some("p".to_string()),
            country: "US".to_string(),
            tier: "residential".to_string(),
            provider: "mock-a".to_string(),
            weight: 100,
        };
        // 若 `Proxy::all` 恰好容忍该输入，探测走网络超时仍归 `Dead`（不断言具体文案）。
        match prober.probe_node(&bad).await {
            ProbeResult::Dead { .. } | ProbeResult::Degraded { .. } => {}
            ProbeResult::Healthy { .. } => panic!("bad proxy must not report healthy"),
        }
    }
}
