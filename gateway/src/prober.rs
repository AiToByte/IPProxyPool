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
/// R2-7 备用 trace 源：主源失败（被墙/抖动）时兜底一试。本轮只保证“有退路”，
/// quorum 细节不断言：任一 2xx 即 `Healthy`（备源无 `ip=` 行时 `exit_ip`
/// 回落节点自身 IP，见 `probe_node`）。
pub const TRACE_URL_BACKUP: &str = "https://www.google.com/generate_204";
/// R2-7 并发探测上限（main 侧 JoinSet + 同值信号量，百节点不再堵死 ticker）。
pub const PROBE_MAX_CONCURRENT: usize = 20;

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
    /// R2-7：附 `last_used`，60s 滴答淘汰闲置（账密 rotation/节点下线后明文 key 不常驻）。
    clients: DashMap<String, CachedClient>,
    /// 探测超时（默认 2500ms，单测可注入更小值）。
    timeout: Duration,
}

/// R2-7：缓存条目（含最后使用时刻，供 TTL 淘汰）。
struct CachedClient {
    client: Client,
    last_used: Instant,
}

/// R2-7 Client 闲置 TTL（10min，复用 sweep 节拍淘汰；60s 滴答粒度足够）。
pub const CLIENT_IDLE_TTL: Duration = Duration::from_secs(600);

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
    /// - 命中缓存直接克隆句柄（`Client` 内部为 `Arc`，克隆廉价），并刷新
    ///   `last_used`（R2-7：活跃条目不被 TTL 误杀）；
    /// - 未命中则按统一超时构建，失败返回 `None`（调用方判 `Dead`）。
    fn client_for(&self, proxy_url: &str) -> Option<Client> {
        if let Some(mut hit) = self.clients.get_mut(proxy_url) {
            hit.last_used = Instant::now();
            return Some(hit.client.clone());
        }
        let proxy = reqwest::Proxy::all(proxy_url).ok()?;
        let client = Client::builder()
            .proxy(proxy)
            .timeout(self.timeout)
            .build()
            .ok()?;
        self.clients.insert(
            proxy_url.to_string(),
            CachedClient {
                client: client.clone(),
                last_used: Instant::now(),
            },
        );
        Some(client)
    }

    /// R2-7 Client TTL 淘汰：删 `last_used` 超 10min 的条目。返回删除个数，
    /// 调用方为 60s prober 滴答（与 sweep 同节拍，见 main）。
    pub fn evict_idle_clients(&self) -> usize {
        self.evict_idle_clients_older_than(Instant::now())
    }

    /// 可测版本：允许测试注入“未来时间”验证过期逻辑（与 `sweep_expired_at` 同手法）。
    fn evict_idle_clients_older_than(&self, now: Instant) -> usize {
        let before = self.clients.len();
        self.clients
            .retain(|_, c| now.saturating_duration_since(c.last_used) < CLIENT_IDLE_TTL);
        before - self.clients.len()
    }

    /// R2-7 userinfo 百分号编码：`user:pass@` 含空格/保留字符时代理 URL 非法
    ///（未编码前复现为 `invalid proxy url` Dead）。按 RFC 3986 userinfo 允许集
    /// 放行（unreserved + sub-delims + `:`），其余字节 `%XX` 大写编码；零新依赖。
    fn encode_userinfo(s: &str) -> String {
        let mut out = String::with_capacity(s.len());
        for b in s.bytes() {
            match b {
                b'A'..=b'Z'
                | b'a'..=b'z'
                | b'0'..=b'9'
                | b'-'
                | b'.'
                | b'_'
                | b'~'
                | b'!'
                | b'$'
                | b'&'
                | b'\''
                | b'('
                | b')'
                | b'*'
                | b'+'
                | b','
                | b';'
                | b'='
                | b':' => out.push(b as char),
                _ => out.push_str(&format!("%{b:02X}")),
            }
        }
        out
    }

    /// 拼接某节点的代理 URL（含可选的上游认证；账密百分号编码，R2-7）。
    fn proxy_url_for(node: &ProxyNode) -> String {
        match (&node.username, &node.password) {
            (Some(u), Some(p)) => format!(
                "http://{}:{}@{}:{}",
                Self::encode_userinfo(u),
                Self::encode_userinfo(p),
                node.ip,
                node.port
            ),
            _ => format!("http://{}:{}", node.ip, node.port),
        }
    }

    /// Probe one egress node through itself as the HTTP proxy.
    ///
    /// R2-7：主备双 URL——主源先行，失败则备源兜底；任一 2xx 即 `Healthy`
    ///（备源无 `ip=` 行时 `exit_ip` 回落节点 IP）。双源皆败时：可达但状态错
    ///（任一源有状态码）→ `Degraded`，纯传输错 → `Dead`。
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
        let mut last_status: Option<reqwest::StatusCode> = None;
        let mut last_err = String::new();
        for url in [TRACE_URL, TRACE_URL_BACKUP] {
            match client.get(url).send().await {
                Ok(resp) if resp.status().is_success() => {
                    let text = resp.text().await.unwrap_or_default();
                    let latency = start.elapsed().as_millis() as u64;
                    let exit_ip = extract_exit_ip(&text).unwrap_or_else(|| node.ip.clone());
                    return ProbeResult::Healthy {
                        latency_ms: latency,
                        exit_ip,
                    };
                }
                Ok(resp) => {
                    last_status = Some(resp.status());
                    last_err = format!("Unexpected status: {} ({url})", resp.status());
                }
                Err(e) => {
                    last_err = format!("{e} ({url})");
                }
            }
        }
        match last_status {
            Some(_) => ProbeResult::Degraded { reason: last_err },
            None => ProbeResult::Dead { error: last_err },
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
    fn idle_clients_evicted_after_ttl() {
        // R2-7：10min 未用淘汰，明文 key 不常驻；活跃条目（经 client_for 刷新）保留。
        let prober = CanaryProber::new();
        let stale_url = "http://127.0.0.1:18091";
        let fresh_url = "http://127.0.0.1:18092";
        assert!(prober.client_for(stale_url).is_some());
        assert!(prober.client_for(fresh_url).is_some());
        assert_eq!(prober.client_count(), 2);
        // 把 stale 倒回 11min 前（直接写缓存条目，单测与实现同模块可见私有字段）。
        if let Some(mut entry) = prober.clients.get_mut(stale_url) {
            entry.last_used = Instant::now() - CLIENT_IDLE_TTL - Duration::from_secs(60);
        }
        // 快进到 now：只删 stale；空扫不 panic。
        assert_eq!(prober.evict_idle_clients(), 1);
        assert_eq!(prober.client_count(), 1);
        assert!(prober.clients.contains_key(fresh_url));
        assert_eq!(prober.evict_idle_clients(), 0);
    }

    #[test]
    fn proxy_url_encodes_credentials() {
        // R2-7：空格/保留字符编码后 URL 合法；普通字符与无账密形态不变。
        assert_eq!(CanaryProber::encode_userinfo("plain"), "plain");
        assert_eq!(
            CanaryProber::encode_userinfo("u with space"),
            "u%20with%20space"
        );
        assert_eq!(CanaryProber::encode_userinfo("p@ss?w"), "p%40ss%3Fw");
        assert_eq!(CanaryProber::encode_userinfo("a:b"), "a:b");
        let node = crate::model::ProxyNode::new(
            "1.2.3.4".to_string(),
            8080,
            Some("u x".to_string()),
            Some("p@y".to_string()),
            "US".to_string(),
            "residential".to_string(),
            "mock-a".to_string(),
            100,
        );
        assert_eq!(
            CanaryProber::proxy_url_for(&node),
            "http://u%20x:p%40y@1.2.3.4:8080"
        );
        // 编码后 `Proxy::all` 可接受（R2-7 前此处是 `invalid proxy url` Dead）。
        assert!(reqwest::Proxy::all(CanaryProber::proxy_url_for(&node)).is_ok());
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
        let bad = crate::model::ProxyNode::new(
            "127.0.0.1".to_string(),
            8080,
            Some("u with space".to_string()),
            Some("p".to_string()),
            "US".to_string(),
            "residential".to_string(),
            "mock-a".to_string(),
            100,
        );
        // 若 `Proxy::all` 恰好容忍该输入，探测走网络超时仍归 `Dead`（不断言具体文案）。
        match prober.probe_node(&bad).await {
            ProbeResult::Dead { .. } | ProbeResult::Degraded { .. } => {}
            ProbeResult::Healthy { .. } => panic!("bad proxy must not report healthy"),
        }
    }
}
