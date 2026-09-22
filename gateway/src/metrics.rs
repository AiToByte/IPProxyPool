//! GW-4 Prometheus exposition: hand-rolled text format, zero new deps.
//!
//! Served on `127.0.0.1:9091/metrics` (see `deploy/prometheus/prometheus.yml`,
//! which already scrapes it). Four manual-A4 signals are covered:
//! success-rate counters, P99 duration histogram, per-provider 403 ratio
//! counters, and total transferred bytes.

use dashmap::DashMap;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

/// Histogram upper bounds in ms (`+Inf` is implicit as `_count`).
pub const DURATION_BUCKETS_MS: [u64; 10] = [1, 5, 10, 25, 50, 100, 250, 500, 1000, 2500];
/// Metrics HTTP endpoint (matches the Prometheus scrape target).
pub const METRICS_ADDR: &str = "127.0.0.1:9091";
/// R2-8 `/metrics` 并发上限（零新依赖：超限即关连接；Prom 抓取低频，64 绰绰有余）。
pub const METRICS_MAX_CONCURRENT: usize = 64;
/// R2-8 `/metrics` 读超时（慢连接不占 worker）。
pub const METRICS_READ_TIMEOUT: Duration = Duration::from_secs(5);
/// R2-8 数据面日志采样分母（非 5xx 每 1000 条全量一条，其余降 `debug!`）。
pub const LOG_SAMPLE_EVERY: u64 = 1000;

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
    /// R2-4 口径冻结：只计落库失败（通道满另计 `channel_dropped`）。
    telemetry_dropped: Arc<AtomicU64>,
    /// R2-4 通道丢弃计数：与 `TelemetryPublisher` 共享同一个 `Arc`
    ///（emit 满队列/已关闭时累加），此处只读渲染，不参与 `observe`。
    channel_dropped: Arc<AtomicU64>,
    /// R2-8 后台重启计数（supervisor 直写，`supervisor_restarts_total{worker}` 渲染）。
    supervisor_restarts: DashMap<String, AtomicU64>,
    /// R2-8 日志采样序号（`sample_full_log` 发号，单调递增；5xx 不经过此处）。
    log_sample_seq: AtomicU64,
    /// R2-8 被采样掉的非 5xx 日志数（`gateway_logs_sampled_total` 渲染，可观测）。
    logs_sampled: AtomicU64,
    /// FreePool 免费池在池节点数（worker 每次合并后设置，只写不参与 observe）。
    free_pool_nodes: AtomicU64,
    /// FreePool per-source 抓取产出（`free_pool_source_yield_total{source}` 渲染）。
    free_source_yield: DashMap<String, AtomicU64>,
    /// FreePool 质检结果计数（`free_pool_verify_total{result}` 渲染；
    /// result∈pass/tcp_fail/full_fail/backoff_skip，调用方保证集合）。
    free_verify: DashMap<String, AtomicU64>,
    /// FreePool 匿名度分级计数（`free_pool_anonymity_total{level}` 渲染；
    /// level∈elite/anonymous/transparent/unknown，调用方保证集合）。
    free_anonymity: DashMap<String, AtomicU64>,
    /// FreePool 源站熔断 gauge（`free_pool_source_suspended{source}` 0/1 渲染）。
    free_suspended: DashMap<String, AtomicU64>,
    /// P2 FreePool 按出站协议水位（`free_pool_nodes_by_proto{proto}` gauge 渲染；
    /// proto∈http/socks5/socks4，merge 后三档恒设，仪表盘行稳定）。
    free_nodes_by_proto: DashMap<String, AtomicU64>,
    /// P3 GeoIP 观察计数（`geoip_lookups_total{result}` 渲染；result∈hit/miss/disabled/error）。
    geo_lookups: DashMap<String, AtomicU64>,
    /// P3 exit-国家分歧计数（`geoip_mismatch_total` 渲染；只收实锤分歧，见 `exit_matches_source`）。
    geo_mismatch: AtomicU64,
}

impl MetricsRegistry {
    pub fn new() -> Self {
        Self::new_with_dropped(Arc::new(AtomicU64::new(0)), Arc::new(AtomicU64::new(0)))
    }

    /// OPT-4 装配入口：与落库 worker 共享同一个丢弃计数器。
    /// `new()` 等价于传入全新计数器（存量单测行为不变）；线上 `main` 传入共享实例。
    /// R2-4：第二个参数为通道丢弃计数器（与 publisher 共享）。
    pub fn new_with_dropped(
        telemetry_dropped: Arc<AtomicU64>,
        channel_dropped: Arc<AtomicU64>,
    ) -> Self {
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
            channel_dropped,
            supervisor_restarts: DashMap::new(),
            log_sample_seq: AtomicU64::new(0),
            logs_sampled: AtomicU64::new(0),
            free_pool_nodes: AtomicU64::new(0),
            free_source_yield: DashMap::new(),
            free_verify: DashMap::new(),
            free_anonymity: DashMap::new(),
            free_suspended: DashMap::new(),
            free_nodes_by_proto: DashMap::new(),
            geo_lookups: DashMap::new(),
            geo_mismatch: AtomicU64::new(0),
        }
    }

    /// 当前落库丢弃总数（单测断言用；线上看 `/metrics` 渲染行）。
    #[cfg(test)]
    pub fn dropped_count(&self) -> u64 {
        self.telemetry_dropped.load(Ordering::Relaxed)
    }

    /// 当前通道丢弃总数（单测断言用；线上看 `/metrics` 渲染行）。
    #[cfg(test)]
    pub fn channel_dropped_count(&self) -> u64 {
        self.channel_dropped.load(Ordering::Relaxed)
    }

    /// R2-8：记录一次后台 worker 重启（supervisor 在 panic/意外返回后调用）。
    pub fn note_supervisor_restart(&self, worker: &str) {
        self.supervisor_restarts
            .entry(worker.to_string())
            .or_insert_with(|| AtomicU64::new(0))
            .fetch_add(1, Ordering::Relaxed);
    }

    /// R2-8 数据面日志采样：`status>=500` 全量；其余每 `LOG_SAMPLE_EVERY`
    /// 条全量一条（序号 0 起首条全量），被采样掉的计 `logs_sampled`。
    /// 决定性（计数器取模，无随机），单测锁定形态。
    pub fn sample_full_log(&self, status: u16) -> bool {
        if status >= 500 {
            return true;
        }
        if self
            .log_sample_seq
            .fetch_add(1, Ordering::Relaxed)
            .is_multiple_of(LOG_SAMPLE_EVERY)
        {
            true
        } else {
            self.logs_sampled.fetch_add(1, Ordering::Relaxed);
            false
        }
    }

    /// 免费池在池节点数（FreePool worker 每次合并后设置，只写不参与 observe）。
    pub fn set_free_pool_nodes(&self, n: u64) {
        self.free_pool_nodes.store(n, Ordering::Relaxed);
    }

    /// FreePool 源站抓取产出累加（worker 每轮按 source 聚合计数；零产出源不记，
    /// 其熔断由 suspend gauge 可见）。
    pub fn note_free_source_yield(&self, source: &str, n: u64) {
        self.free_source_yield
            .entry(source.to_string())
            .or_insert_with(|| AtomicU64::new(0))
            .fetch_add(n, Ordering::Relaxed);
    }

    /// FreePool 质检结果计数（result 调用方保证∈pass/tcp_fail/full_fail/backoff_skip/geo_fail）。
    pub fn note_free_verify(&self, result: &str) {
        self.free_verify
            .entry(result.to_string())
            .or_insert_with(|| AtomicU64::new(0))
            .fetch_add(1, Ordering::Relaxed);
    }

    /// FreePool 匿名度分级计数（level 调用方保证∈elite/anonymous/transparent/unknown）。
    pub fn note_free_anonymity(&self, level: &str) {
        self.free_anonymity
            .entry(level.to_string())
            .or_insert_with(|| AtomicU64::new(0))
            .fetch_add(1, Ordering::Relaxed);
    }

    /// FreePool 源站熔断 gauge（worker 每轮同步各源 suspend 状态）。
    pub fn set_free_source_suspended(&self, source: &str, suspended: bool) {
        self.free_suspended
            .entry(source.to_string())
            .or_insert_with(|| AtomicU64::new(0))
            .store(u64::from(suspended), Ordering::Relaxed);
    }

    /// P2 FreePool 按出站协议水位（worker 每次合并后按快照聚合设置三档；只写 gauge）。
    pub fn set_free_pool_nodes_proto(&self, proto: &str, n: u64) {
        self.free_nodes_by_proto
            .entry(proto.to_string())
            .or_insert_with(|| AtomicU64::new(0))
            .store(n, Ordering::Relaxed);
    }

    /// P3 GeoIP 观察计数（result 调用方保证∈hit/miss/disabled/error）。
    pub fn note_geo_lookup(&self, result: &str) {
        self.geo_lookups
            .entry(result.to_string())
            .or_insert_with(|| AtomicU64::new(0))
            .fetch_add(1, Ordering::Relaxed);
    }

    /// P3 exit-国家实锤分歧计数（只观察不执法；执法留 Phase 4）。
    pub fn note_geo_mismatch(&self) {
        self.geo_mismatch.fetch_add(1, Ordering::Relaxed);
    }
    /// Record one finished proxied response (called from `logging`).
    ///
    /// R2-6 写侧单原子：只给首个 `le >= value` 的桶 +1（每次 `observe` 恰一次
    /// 原子写，而非 10 次），`render` 侧前缀累加成 Prometheus 累计桶。
    /// 根治旧累计存储的撕裂：旧写法按桶序逐个 +1，`render` 若在两次 +1 之间
    /// 读到相邻两桶，会渲染出 `le 小 > le 大` 的非单调行；单原子写下渲染值
    /// 恒为非负前缀和，天然单调。
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
        if let Some(i) = DURATION_BUCKETS_MS.iter().position(|bound| ms <= *bound) {
            self.duration_buckets[i].fetch_add(1, Ordering::Relaxed);
        } else {
            // 超最大桶：只进 `_count`/`_sum`（`+Inf` 行），各 `le` 桶不动。
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
        // R2-6：写侧只存单桶原始值，此处前缀累加成 Prometheus 累计桶。
        let mut cumulative = 0u64;
        for (i, bound) in DURATION_BUCKETS_MS.iter().enumerate() {
            cumulative += self.duration_buckets[i].load(Ordering::Relaxed);
            out.push_str(&format!(
                "gateway_processing_duration_ms_bucket{{le=\"{bound}\"}} {cumulative}\n"
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
        // R2-4：通道满/关闭丢弃总数（常驻 0 行；与落库口径分离，归因不混）。
        out.push_str("# HELP telemetry_channel_dropped_total Telemetry events dropped on a full/closed channel.\n");
        out.push_str("# TYPE telemetry_channel_dropped_total counter\n");
        out.push_str(&format!(
            "telemetry_channel_dropped_total {}\n",
            self.channel_dropped.load(Ordering::Relaxed)
        ));
        // R2-8：后台 worker 重启数（supervisor 计数；无重启时无线，保持 exposition 干净）。
        out.push_str("# HELP supervisor_restarts_total Background worker restarts by worker.\n");
        out.push_str("# TYPE supervisor_restarts_total counter\n");
        let mut workers: Vec<(String, u64)> = self
            .supervisor_restarts
            .iter()
            .map(|e| (e.key().clone(), e.value().load(Ordering::Relaxed)))
            .collect();
        workers.sort();
        for (w, n) in workers {
            out.push_str(&format!(
                "supervisor_restarts_total{{worker=\"{w}\"}} {n}\n"
            ));
        }
        // R2-8：被采样掉的非 5xx 数据面日志数（常驻行，采样本身可观测）。
        out.push_str("# HELP gateway_logs_sampled_total Data-plane log lines sampled to debug.\n");
        out.push_str("# TYPE gateway_logs_sampled_total counter\n");
        out.push_str(&format!(
            "gateway_logs_sampled_total {}\n",
            self.logs_sampled.load(Ordering::Relaxed)
        ));
        out.push_str(
            "# HELP free_pool_nodes_total Free-tier nodes currently merged into the pool.\n",
        );
        out.push_str("# TYPE free_pool_nodes_total gauge\n");
        out.push_str(&format!(
            "free_pool_nodes_total {}\n",
            self.free_pool_nodes.load(Ordering::Relaxed)
        ));
        // FreePool per-source 抓取产出（label 源名内部生成，无引号；调用方保证集合）。
        out.push_str("# HELP free_pool_source_yield_total FreePool fetched nodes by source.\n");
        out.push_str("# TYPE free_pool_source_yield_total counter\n");
        let mut yields: Vec<(String, u64)> = self
            .free_source_yield
            .iter()
            .map(|e| (e.key().clone(), e.value().load(Ordering::Relaxed)))
            .collect();
        yields.sort();
        for (s, n) in yields {
            out.push_str(&format!(
                "free_pool_source_yield_total{{source=\"{s}\"}} {n}\n"
            ));
        }
        // FreePool 质检结果分布。
        out.push_str("# HELP free_pool_verify_total FreePool verify outcomes by result.\n");
        out.push_str("# TYPE free_pool_verify_total counter\n");
        let mut verifies: Vec<(String, u64)> = self
            .free_verify
            .iter()
            .map(|e| (e.key().clone(), e.value().load(Ordering::Relaxed)))
            .collect();
        verifies.sort();
        for (r, n) in verifies {
            out.push_str(&format!("free_pool_verify_total{{result=\"{r}\"}} {n}\n"));
        }
        // FreePool 匿名度分级分布。
        out.push_str("# HELP free_pool_anonymity_total FreePool anonymity levels by level.\n");
        out.push_str("# TYPE free_pool_anonymity_total counter\n");
        let mut anons: Vec<(String, u64)> = self
            .free_anonymity
            .iter()
            .map(|e| (e.key().clone(), e.value().load(Ordering::Relaxed)))
            .collect();
        anons.sort();
        for (l, n) in anons {
            out.push_str(&format!("free_pool_anonymity_total{{level=\"{l}\"}} {n}\n"));
        }
        // FreePool 源站熔断状态（0/1 gauge；无数据时无线，保持 exposition 干净）。
        out.push_str(
            "# HELP free_pool_source_suspended FreePool source suspended flag by source.\n",
        );
        out.push_str("# TYPE free_pool_source_suspended gauge\n");
        let mut susps: Vec<(String, u64)> = self
            .free_suspended
            .iter()
            .map(|e| (e.key().clone(), e.value().load(Ordering::Relaxed)))
            .collect();
        susps.sort();
        for (s, n) in susps {
            out.push_str(&format!(
                "free_pool_source_suspended{{source=\"{s}\"}} {n}\n"
            ));
        }
        // P2 FreePool 按出站协议水位（merge 后三档恒设；proto 白名单 http/socks5/socks4）。
        out.push_str("# HELP free_pool_nodes_by_proto FreePool merged nodes by egress proto.\n");
        out.push_str("# TYPE free_pool_nodes_by_proto gauge\n");
        let mut protos: Vec<(String, u64)> = self
            .free_nodes_by_proto
            .iter()
            .map(|e| (e.key().clone(), e.value().load(Ordering::Relaxed)))
            .collect();
        protos.sort();
        for (p, n) in protos {
            out.push_str(&format!("free_pool_nodes_by_proto{{proto=\"{p}\"}} {n}\n"));
        }
        // P3 GeoIP 观察分布＋实锤分歧（无数据时只 HELP/TYPE，沿 suspend 惯例保持干净）。
        out.push_str("# HELP geoip_lookups_total GeoIP exit-country lookups by result.\n");
        out.push_str("# TYPE geoip_lookups_total counter\n");
        let mut geos: Vec<(String, u64)> = self
            .geo_lookups
            .iter()
            .map(|e| (e.key().clone(), e.value().load(Ordering::Relaxed)))
            .collect();
        geos.sort();
        for (r, n) in geos {
            out.push_str(&format!("geoip_lookups_total{{result=\"{r}\"}} {n}\n"));
        }
        out.push_str("# HELP geoip_mismatch_total GeoIP exit-country hard mismatches.\n");
        out.push_str("# TYPE geoip_mismatch_total counter\n");
        out.push_str(&format!(
            "geoip_mismatch_total {}\n",
            self.geo_mismatch.load(Ordering::Relaxed)
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
///
/// R2-8 加固：读超时 5s（慢连接不占 worker）+ 在途连接超 64 即关（零新依赖
/// 的并发上限；Prom 抓取低频）+ 单连接 body 上限沿用 1024。
pub async fn serve_metrics(registry: Arc<MetricsRegistry>, addr: &str) {
    let listener = match tokio::net::TcpListener::bind(addr).await {
        Ok(l) => l,
        Err(e) => {
            log::error!("[Metrics] bind {addr} failed: {e:?}");
            return;
        }
    };
    log::info!("[Metrics] exposition on http://{addr}/metrics");
    let inflight = Arc::new(AtomicUsize::new(0));
    loop {
        let (mut stream, _) = match listener.accept().await {
            Ok(pair) => pair,
            Err(e) => {
                log::debug!("[Metrics] accept failed: {e:?}");
                continue;
            }
        };
        if inflight.fetch_add(1, Ordering::SeqCst) >= METRICS_MAX_CONCURRENT {
            inflight.fetch_sub(1, Ordering::SeqCst);
            log::debug!("[Metrics] over connection cap, closing");
            continue;
        }
        let registry = registry.clone();
        let inflight = inflight.clone();
        tokio::spawn(async move {
            use tokio::io::{AsyncReadExt, AsyncWriteExt};
            let mut buf = [0u8; 1024];
            let n = match tokio::time::timeout(METRICS_READ_TIMEOUT, stream.read(&mut buf)).await {
                Ok(Ok(n)) => n,
                _ => {
                    inflight.fetch_sub(1, Ordering::SeqCst);
                    return;
                }
            };
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
            inflight.fetch_sub(1, Ordering::SeqCst);
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
    fn histogram_buckets_render_monotonic() {
        // R2-6：混合延迟 + 超界值下渲染桶恒单调（单原子写 + 前缀累加的直接收益）；
        // 超最大桶只进 +Inf/count，不污染各 le 行。
        let m = MetricsRegistry::new();
        for ms in [0, 1, 3, 30, 1200, 2500, 9999, 30, 3] {
            m.observe(200, None, 0, Duration::from_millis(ms));
        }
        let text = m.render();
        let mut last = 0u64;
        let mut seen = 0usize;
        for line in text.lines() {
            if let Some(rest) = line.strip_prefix("gateway_processing_duration_ms_bucket{le=\"") {
                let value: u64 = rest.rsplit(' ').next().unwrap().parse().unwrap();
                assert!(
                    value >= last,
                    "buckets must be monotonic: {value} after {last} ({line})"
                );
                last = value;
                seen += 1;
            }
        }
        assert_eq!(seen, DURATION_BUCKETS_MS.len() + 1, "10 le + Inf");
        assert!(text.contains("gateway_processing_duration_ms_bucket{le=\"+Inf\"} 9"));
        assert!(text.contains("gateway_processing_duration_ms_count 9"));
    }

    #[test]
    fn dropped_counter_shared_and_rendered() {
        // OPT-4：worker 与注册表共享同一个 Arc；worker 侧累加后 render 可见。
        let shared = Arc::new(AtomicU64::new(0));
        // R2-4：双计数器各自共享、各自渲染、互不干扰。
        let channel = Arc::new(AtomicU64::new(0));
        let m = MetricsRegistry::new_with_dropped(shared.clone(), channel.clone());
        // 零值也必须渲染（PromQL 告警依赖该行常驻）。
        assert!(m.render().contains("telemetry_dropped_total 0"));
        assert!(m.render().contains("telemetry_channel_dropped_total 0"));
        shared.fetch_add(7, Ordering::Relaxed);
        channel.fetch_add(3, Ordering::Relaxed);
        assert_eq!(m.dropped_count(), 7);
        assert_eq!(m.channel_dropped_count(), 3);
        assert!(m.render().contains("telemetry_dropped_total 7"));
        assert!(m.render().contains("telemetry_channel_dropped_total 3"));
    }

    #[test]
    fn supervisor_restarts_counted_and_rendered() {
        // R2-8：各 worker 重启数分别计数、排序渲染；无重启时无线。
        let m = MetricsRegistry::new();
        assert!(!m.render().contains("supervisor_restarts_total{"));
        m.note_supervisor_restart("sink");
        m.note_supervisor_restart("sink");
        m.note_supervisor_restart("circuit_breaker");
        let text = m.render();
        assert!(text.contains("supervisor_restarts_total{worker=\"circuit_breaker\"} 1"));
        assert!(text.contains("supervisor_restarts_total{worker=\"sink\"} 2"));
    }

    #[test]
    fn log_sampling_shape() {
        // R2-8：5xx 全量；非 5xx 首条全量、随后 999 条采样、千条一循环；
        // 采样数可观测（`gateway_logs_sampled_total` 常驻行）。
        let m = MetricsRegistry::new();
        assert!(m.sample_full_log(500));
        assert!(m.sample_full_log(503));
        assert!(m.sample_full_log(200), "seq 0 must be full");
        for _ in 0..999 {
            assert!(!m.sample_full_log(200));
        }
        assert!(m.sample_full_log(200), "seq 1000 must be full again");
        assert!(m.render().contains("gateway_logs_sampled_total 999"));
    }

    #[test]
    fn free_pool_nodes_rendered() {
        // 免费池水位：默认 0 常驻行；set 后渲染新值。
        let m = MetricsRegistry::new();
        assert!(m.render().contains("free_pool_nodes_total 0"));
        m.set_free_pool_nodes(37);
        assert!(m.render().contains("free_pool_nodes_total 37"));
    }

    #[test]
    fn free_pool_extended_rendered() {
        // Task 3 水位计存量断言保留；新增四组：
        let m = MetricsRegistry::new();
        m.note_free_source_yield("api0", 12);
        m.note_free_verify("pass");
        m.note_free_verify("tcp_fail");
        m.note_free_anonymity("elite");
        m.set_free_source_suspended("gh0", true);
        let r = m.render();
        assert!(r.contains("free_pool_source_yield_total{source=\"api0\"} 12"));
        assert!(r.contains("free_pool_verify_total{result=\"pass\"} 1"));
        assert!(r.contains("free_pool_anonymity_total{level=\"elite\"} 1"));
        assert!(r.contains("free_pool_source_suspended{source=\"gh0\"} 1"));
        m.set_free_source_suspended("gh0", false);
        assert!(m
            .render()
            .contains("free_pool_source_suspended{source=\"gh0\"} 0"));
    }

    #[test]
    fn metrics_nodes_by_proto_rendered() {
        // P2-5：按出站协议水位（merge 后按快照聚合设置；need 行存在且值正确）。
        let m = MetricsRegistry::new();
        m.set_free_pool_nodes_proto("socks5", 2);
        m.set_free_pool_nodes_proto("http", 5);
        let r = m.render();
        assert!(r.contains("free_pool_nodes_by_proto{proto=\"socks5\"} 2"));
        assert!(r.contains("free_pool_nodes_by_proto{proto=\"http\"} 5"));
    }

    #[test]
    fn metrics_geo_rendered() {
        // P3-4：lookup 分布＋mismatch 行存在；mismatch 常驻 0 行（counter 语义）。
        let m = MetricsRegistry::new();
        m.note_geo_lookup("disabled");
        m.note_geo_lookup("hit");
        m.note_geo_mismatch();
        let r = m.render();
        assert!(r.contains("geoip_lookups_total{result=\"disabled\"} 1"));
        assert!(r.contains("geoip_lookups_total{result=\"hit\"} 1"));
        assert!(r.contains("geoip_mismatch_total 1"));
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
