//! GW-4 Prometheus exposition: hand-rolled text format, zero new deps.
//!
//! Served on `127.0.0.1:9091/metrics` (see `deploy/prometheus/prometheus.yml`,
//! which already scrapes it). Four manual-A4 signals are covered:
//! success-rate counters, P99 duration histogram, per-provider 403 ratio
//! counters, and total transferred bytes.

use dashmap::DashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

/// Histogram upper bounds in ms (`+Inf` is implicit as `_count`).
pub const DURATION_BUCKETS_MS: [u64; 10] = [1, 5, 10, 25, 50, 100, 250, 500, 1000, 2500];
/// Metrics HTTP endpoint (matches the Prometheus scrape target).
pub const METRICS_ADDR: &str = "127.0.0.1:9091";

pub struct MetricsRegistry {
    req_2xx: AtomicU64,
    req_4xx: AtomicU64,
    req_5xx: AtomicU64,
    req_other: AtomicU64,
    forbidden_by_provider: DashMap<String, AtomicU64>,
    bytes_total: AtomicU64,
    duration_buckets: Vec<AtomicU64>,
    duration_count: AtomicU64,
    duration_sum_ms: AtomicU64,
    /// OPT-4 遥测落库丢弃计数：与 `TelemetryWorker` 共享同一个 `Arc`
    ///（worker 重试一次仍失败时整批累加），此处只读渲染，不参与 `observe`。
    telemetry_dropped: Arc<AtomicU64>,
}

impl MetricsRegistry {
    pub fn new() -> Self {
        Self::new_with_dropped(Arc::new(AtomicU64::new(0)))
    }

    /// OPT-4 装配入口：与落库 worker 共享同一个丢弃计数器。
    /// `new()` 等价于传入全新计数器（存量单测行为不变）；线上 `main` 传入共享实例。
    pub fn new_with_dropped(telemetry_dropped: Arc<AtomicU64>) -> Self {
        Self {
            req_2xx: AtomicU64::new(0),
            req_4xx: AtomicU64::new(0),
            req_5xx: AtomicU64::new(0),
            req_other: AtomicU64::new(0),
            forbidden_by_provider: DashMap::new(),
            bytes_total: AtomicU64::new(0),
            duration_buckets: (0..DURATION_BUCKETS_MS.len())
                .map(|_| AtomicU64::new(0))
                .collect(),
            duration_count: AtomicU64::new(0),
            duration_sum_ms: AtomicU64::new(0),
            telemetry_dropped,
        }
    }

    /// 当前已丢弃的遥测事件总数（单测断言用；线上看 `/metrics` 渲染行）。
    #[cfg(test)]
    pub fn dropped_count(&self) -> u64 {
        self.telemetry_dropped.load(Ordering::Relaxed)
    }

    /// Record one finished proxied response (called from `logging`).
    ///
    /// Buckets follow Prometheus convention: every bucket with `le >= value`
    /// is incremented, so stored counts are already cumulative.
    pub fn observe(&self, status: u16, provider: Option<&str>, bytes: u64, duration: Duration) {
        match status {
            200..=299 => self.req_2xx.fetch_add(1, Ordering::Relaxed),
            400..=499 => self.req_4xx.fetch_add(1, Ordering::Relaxed),
            500..=599 => self.req_5xx.fetch_add(1, Ordering::Relaxed),
            _ => self.req_other.fetch_add(1, Ordering::Relaxed),
        };
        if status == 403 {
            if let Some(p) = provider {
                self.forbidden_by_provider
                    .entry(p.to_string())
                    .or_insert_with(|| AtomicU64::new(0))
                    .fetch_add(1, Ordering::Relaxed);
            }
        }
        self.bytes_total.fetch_add(bytes, Ordering::Relaxed);
        let ms = duration.as_millis().min(u64::MAX as u128) as u64;
        for (i, bound) in DURATION_BUCKETS_MS.iter().enumerate() {
            if ms <= *bound {
                self.duration_buckets[i].fetch_add(1, Ordering::Relaxed);
            }
        }
        self.duration_count.fetch_add(1, Ordering::Relaxed);
        self.duration_sum_ms.fetch_add(ms, Ordering::Relaxed);
    }

    /// Render Prometheus text exposition (cumulative buckets).
    pub fn render(&self) -> String {
        let mut out = String::with_capacity(1024);
        out.push_str("# HELP proxy_requests_total Proxied responses by status class.\n");
        out.push_str("# TYPE proxy_requests_total counter\n");
        out.push_str(&format!(
            "proxy_requests_total{{status=\"2xx\"}} {}\n",
            self.req_2xx.load(Ordering::Relaxed)
        ));
        out.push_str(&format!(
            "proxy_requests_total{{status=\"4xx\"}} {}\n",
            self.req_4xx.load(Ordering::Relaxed)
        ));
        out.push_str(&format!(
            "proxy_requests_total{{status=\"5xx\"}} {}\n",
            self.req_5xx.load(Ordering::Relaxed)
        ));
        out.push_str(&format!(
            "proxy_requests_total{{status=\"other\"}} {}\n",
            self.req_other.load(Ordering::Relaxed)
        ));
        out.push_str("# HELP proxy_requests_forbidden_total 403 responses by provider.\n");
        out.push_str("# TYPE proxy_requests_forbidden_total counter\n");
        let mut providers: Vec<(String, u64)> = self
            .forbidden_by_provider
            .iter()
            .map(|e| (e.key().clone(), e.value().load(Ordering::Relaxed)))
            .collect();
        providers.sort();
        for (p, n) in providers {
            out.push_str(&format!(
                "proxy_requests_forbidden_total{{provider=\"{p}\"}} {n}\n"
            ));
        }
        out.push_str("# HELP gateway_transferred_bytes_total Egress bytes metered.\n");
        out.push_str("# TYPE gateway_transferred_bytes_total counter\n");
        out.push_str(&format!(
            "gateway_transferred_bytes_total {}\n",
            self.bytes_total.load(Ordering::Relaxed)
        ));
        out.push_str("# HELP gateway_processing_duration_ms Gateway overhead histogram.\n");
        out.push_str("# TYPE gateway_processing_duration_ms histogram\n");
        // Buckets are stored cumulative (see `observe`); render raw.
        for (i, bound) in DURATION_BUCKETS_MS.iter().enumerate() {
            let n = self.duration_buckets[i].load(Ordering::Relaxed);
            out.push_str(&format!(
                "gateway_processing_duration_ms_bucket{{le=\"{bound}\"}} {n}\n"
            ));
        }
        let count = self.duration_count.load(Ordering::Relaxed);
        out.push_str(&format!(
            "gateway_processing_duration_ms_bucket{{le=\"+Inf\"}} {count}\n"
        ));
        out.push_str(&format!("gateway_processing_duration_ms_count {count}\n"));
        out.push_str(&format!(
            "gateway_processing_duration_ms_sum {}\n",
            self.duration_sum_ms.load(Ordering::Relaxed)
        ));
        // OPT-4：落库重试一次仍失败的丢弃事件总数（常驻 0 行，便于 PromQL 告警）。
        out.push_str("# HELP telemetry_dropped_total Telemetry events dropped after one retry.\n");
        out.push_str("# TYPE telemetry_dropped_total counter\n");
        out.push_str(&format!(
            "telemetry_dropped_total {}\n",
            self.telemetry_dropped.load(Ordering::Relaxed)
        ));
        out
    }
}

impl Default for MetricsRegistry {
    fn default() -> Self {
        Self::new()
    }
}

/// Serve `GET /metrics` until process exit (minimal HTTP, no new deps).
pub async fn serve_metrics(registry: Arc<MetricsRegistry>, addr: &str) {
    let listener = match tokio::net::TcpListener::bind(addr).await {
        Ok(l) => l,
        Err(e) => {
            log::error!("[Metrics] bind {addr} failed: {e:?}");
            return;
        }
    };
    log::info!("[Metrics] exposition on http://{addr}/metrics");
    loop {
        let (mut stream, _) = match listener.accept().await {
            Ok(pair) => pair,
            Err(e) => {
                log::debug!("[Metrics] accept failed: {e:?}");
                continue;
            }
        };
        let registry = registry.clone();
        tokio::spawn(async move {
            use tokio::io::{AsyncReadExt, AsyncWriteExt};
            let mut buf = [0u8; 1024];
            let n = stream.read(&mut buf).await.unwrap_or(0);
            let head = String::from_utf8_lossy(&buf[..n]);
            let (code, reason, body) = if head.starts_with("GET /metrics") {
                ("200", "OK", registry.render())
            } else {
                ("404", "Not Found", "not found\n".to_string())
            };
            let _ = stream
                .write_all(
                    format!(
                        "HTTP/1.1 {code} {reason}\r\ncontent-type: text/plain; version=0.0.4\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                        body.len()
                    )
                    .as_bytes(),
                )
                .await;
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn counters_and_histogram_render() {
        let m = MetricsRegistry::new();
        m.observe(200, Some("mock-a"), 100, Duration::from_millis(3));
        m.observe(403, Some("mock-a"), 0, Duration::from_millis(1200));
        m.observe(502, Some("mock-b"), 50, Duration::from_millis(30));
        let text = m.render();
        assert!(text.contains("proxy_requests_total{status=\"2xx\"} 1"));
        assert!(text.contains("proxy_requests_total{status=\"4xx\"} 1"));
        assert!(text.contains("proxy_requests_total{status=\"5xx\"} 1"));
        assert!(text.contains("proxy_requests_forbidden_total{provider=\"mock-a\"} 1"));
        assert!(
            !text.contains("provider=\"mock-b\""),
            "only 403s counted per provider"
        );
        assert!(text.contains("gateway_transferred_bytes_total 150"));
        // Cumulative buckets: 3ms lands in le>=5; 30ms in le>=50; 1200ms in le>=2500.
        assert!(text.contains("gateway_processing_duration_ms_bucket{le=\"1\"} 0"));
        assert!(text.contains("gateway_processing_duration_ms_bucket{le=\"5\"} 1"));
        assert!(text.contains("gateway_processing_duration_ms_bucket{le=\"50\"} 2"));
        assert!(text.contains("gateway_processing_duration_ms_bucket{le=\"+Inf\"} 3"));
        assert!(text.contains("gateway_processing_duration_ms_count 3"));
    }

    #[test]
    fn dropped_counter_shared_and_rendered() {
        // OPT-4：worker 与注册表共享同一个 Arc；worker 侧累加后 render 可见。
        let shared = Arc::new(AtomicU64::new(0));
        let m = MetricsRegistry::new_with_dropped(shared.clone());
        // 零值也必须渲染（PromQL 告警依赖该行常驻）。
        assert!(m.render().contains("telemetry_dropped_total 0"));
        shared.fetch_add(7, Ordering::Relaxed);
        assert_eq!(m.dropped_count(), 7);
        assert!(m.render().contains("telemetry_dropped_total 7"));
    }

    #[tokio::test]
    async fn metrics_endpoint_serves_render() {
        let m = Arc::new(MetricsRegistry::new());
        m.observe(200, None, 10, Duration::from_millis(1));
        tokio::spawn(serve_metrics(m, "127.0.0.1:19091"));
        tokio::time::sleep(Duration::from_millis(200)).await;
        let body = reqwest::get("http://127.0.0.1:19091/metrics")
            .await
            .expect("GET /metrics")
            .text()
            .await
            .expect("body");
        assert!(body.contains("proxy_requests_total"), "{body}");
    }
}
