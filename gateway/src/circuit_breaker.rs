//! GW-2 passive circuit breaker: consumes `stream:proxy:telemetry` and applies
//! domain-scoped quarantine (429→60s, 403→600s, 502/504→30s).
//!
//! Isolation is applied in three layers, fastest first:
//! 1. in-memory [`RouterEngine::set_quarantine`] (<50ms, same process);
//! 2. `SETEX quarantine:{domain}:{ip}` (cross-instance persistence);
//! 3. `PUBLISH proxy:events:delta "QUARANTINE|{domain}|{ip}|{ttl}"` (fan-out).
//!
//! Deviation from manual-A2 (verified against redis 0.26.1 source): stream
//! field values match on `Value::BulkString`, not `Value::Data`.

use crate::router::RouterEngine;
use redis::{aio::ConnectionManager, AsyncCommands};
use std::collections::HashMap;
use std::future::Future;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

/// PubSub fan-out channel for quarantine deltas (GW-2d subscribes).
pub const DELTA_CHANNEL: &str = "proxy:events:delta";

/// Ban-code → quarantine TTL mapping (GW-2 acceptance).
pub fn quarantine_ttl_for_status(status: u16) -> Option<u64> {
    match status {
        // 429: rate limited → soft isolation 60s
        429 => Some(60),
        // 403: WAF/Cloudflare block → deep isolation 600s
        403 => Some(600),
        // 502/504: upstream sick → cooldown 30s
        502 | 504 => Some(30),
        _ => None,
    }
}

/// Parse a `QUARANTINE|domain|ip|ttl` delta message. Strict: exactly 4 parts.
/// 复审钳制：ttl 为 0 或超上限（24h）直接拒收（毒报文不得进隔离表；
/// 上限内由 `set_quarantine` 二次钳制兜底，双保险）。
/// REVIEW-R2 Q2：隔离目标合法性（空 domain／空 ip／网关无节点回落 `"none"` 拒绝）。
/// 网关 `logging` 无节点时 `out_ip` 回落 `"none"`（`gateway.rs`），此类遥测与毒 PubSub
/// 不得写入隔离表（junk 键＋跨实例广播噪音；`"none"` 永不命中真实节点，纯属浪费）。
pub fn valid_quarantine_target(domain: &str, ip: &str) -> bool {
    !domain.is_empty() && !ip.is_empty() && ip != "none"
}

pub fn parse_delta_message(payload: &str) -> Option<(String, String, u64)> {
    use crate::router::{QUARANTINE_DOMAIN_MAX_BYTES, QUARANTINE_MAX_TTL_SECS};
    let mut parts = payload.split('|');
    match (
        parts.next(),
        parts.next(),
        parts.next(),
        parts.next(),
        parts.next(),
    ) {
        (Some("QUARANTINE"), Some(domain), Some(ip), Some(ttl), None) => ttl
            .parse::<u64>()
            .ok()
            .filter(|t| *t > 0 && *t <= QUARANTINE_MAX_TTL_SECS)
            .filter(|_| valid_quarantine_target(domain, ip))
            // OPT-R11 C1：domain 长度上限。此前**只校验非空**——Redis 可达者
            // 发一个 `QUARANTINE|<任意长字符串>|ip|600` 报文即可直接往隔离表
            // 外层灌入超长 key，绕过数据面的一切约束。这里在**解析层**拦
            // （而非只靠 `set_quarantine` 兜底），是为了让毒报文尽早被拒。
            // 上限与 `set_quarantine` 同源单一真源（`QUARANTINE_DOMAIN_MAX_BYTES`）。
            //
            // 另注：此前 sweep 回收空内层 map 也救不了——外层条目在窗口内存活，
            // 而 TTL 窗口上限可达 24h。
            .filter(|_| domain.len() <= QUARANTINE_DOMAIN_MAX_BYTES)
            .map(|t| (domain.to_string(), ip.to_string(), t)),
        _ => None,
    }
}

/// NEXT-A0：增量报文应用纯函数（parse＋合法性守卫＋内存隔离）。
/// 返回是否生效；PubSub 循环体调它（网络重连由 supervise 托管，见 main）。
pub fn apply_delta(router: &RouterEngine, payload: &str) -> bool {
    match parse_delta_message(payload) {
        Some((domain, ip, ttl)) => {
            router.set_quarantine(&domain, &ip, ttl);
            true
        }
        None => false,
    }
}

pub(crate) fn field_text(fields: &HashMap<String, redis::Value>, key: &str) -> Option<String> {
    match fields.get(key) {
        Some(redis::Value::BulkString(bytes)) => String::from_utf8(bytes.clone()).ok(),
        Some(redis::Value::SimpleString(s)) => Some(s.clone()),
        _ => None,
    }
}

/// A6（S）：远端同步失败计数（SETEX 持久化／PUBLISH 扇出各一）。
/// 说明：计数暂为同进程本地值＋warn 日志（失败可观测，零新增生产依赖）；
/// 桥接进 MetricsRegistry 待后续项（届时加回注入入口，共享同一个 `Arc` 即可）。
#[derive(Default)]
pub struct RemoteSyncStats {
    persist_failures: AtomicU64,
    publish_failures: AtomicU64,
}

impl RemoteSyncStats {
    pub fn note_persist_failure(&self) {
        self.persist_failures.fetch_add(1, Ordering::Relaxed);
    }

    pub fn note_publish_failure(&self) {
        self.publish_failures.fetch_add(1, Ordering::Relaxed);
    }

    #[cfg(test)]
    pub fn persist_failures(&self) -> u64 {
        self.persist_failures.load(Ordering::Relaxed)
    }

    #[cfg(test)]
    pub fn publish_failures(&self) -> u64 {
        self.publish_failures.load(Ordering::Relaxed)
    }
}

/// 隔离键格式（抽取为纯函数以便单测锁定线上格式）。
pub(crate) fn quarantine_redis_key(domain: &str, out_ip: &str) -> String {
    format!("quarantine:{domain}:{out_ip}")
}

/// 扇出报文格式（抽取为纯函数以便单测锁定线上格式）。
pub(crate) fn quarantine_delta_message(domain: &str, out_ip: &str, ttl_secs: u64) -> String {
    format!("QUARANTINE|{domain}|{out_ip}|{ttl_secs}")
}

/// 单次重试：首次失败即时再试一次，两次都失败才返回 Err（调用方记数＋warn）。
async fn run_with_single_retry<F, Fut>(mut op: F) -> redis::RedisResult<()>
where
    F: FnMut() -> Fut,
    Fut: Future<Output = redis::RedisResult<()>>,
{
    match op().await {
        Ok(()) => Ok(()),
        Err(_) => op().await,
    }
}

pub struct PassiveCircuitBreaker {
    redis_conn: ConnectionManager,
    stream_key: String,
    consumer_group: String,
    consumer_name: String,
    router: Arc<RouterEngine>,
    remote_stats: Arc<RemoteSyncStats>,
}

impl PassiveCircuitBreaker {
    pub fn new(
        redis_conn: ConnectionManager,
        stream_key: String,
        consumer_group: String,
        consumer_name: String,
        router: Arc<RouterEngine>,
    ) -> Self {
        Self {
            redis_conn,
            stream_key,
            consumer_group,
            consumer_name,
            router,
            remote_stats: Arc::new(RemoteSyncStats::default()),
        }
    }

    pub async fn run(self) {
        // Create the consumer group once (MKSTREAM if the stream is missing).
        let mut conn = self.redis_conn.clone();
        let _: redis::RedisResult<()> = conn
            .xgroup_create_mkstream(&self.stream_key, &self.consumer_group, "$")
            .await;

        loop {
            let mut conn = self.redis_conn.clone();
            let opts = redis::streams::StreamReadOptions::default()
                .group(&self.consumer_group, &self.consumer_name)
                .count(100)
                .block(2000);

            let result: redis::RedisResult<redis::streams::StreamReadReply> =
                conn.xread_options(&[&self.stream_key], &[">"], &opts).await;

            match result {
                Ok(reply) => {
                    for stream_key in reply.keys {
                        for entry in stream_key.ids {
                            self.process_entry(&entry.map).await;
                            let _: () = conn
                                .xack(&self.stream_key, &self.consumer_group, &[&entry.id])
                                .await
                                .unwrap_or(());
                        }
                    }
                }
                Err(e) => {
                    log::debug!("[CircuitBreaker] stream read error: {e:?}");
                    tokio::time::sleep(Duration::from_millis(500)).await;
                }
            }
        }
    }

    async fn process_entry(&self, fields: &HashMap<String, redis::Value>) {
        let status: u16 = match field_text(fields, "status") {
            Some(s) => s.parse().unwrap_or(0),
            None => return,
        };
        let domain = match field_text(fields, "domain") {
            Some(d) => d,
            None => return,
        };
        let payload = match field_text(fields, "payload") {
            Some(p) => p,
            None => return,
        };
        let event: crate::telemetry::TelemetryEvent = match serde_json::from_str(&payload) {
            Ok(e) => e,
            Err(_) => return,
        };

        if let Some(ttl) = quarantine_ttl_for_status(status) {
            if !valid_quarantine_target(&domain, &event.out_ip) {
                log::debug!(
                    "[CircuitBreaker] skip quarantine for empty/none target domain={domain:?} ip={:?}",
                    event.out_ip
                );
                return;
            }
            self.apply_quarantine(&domain, &event.out_ip, ttl).await;
        }
    }

    /// Apply domain isolation: memory first, then Redis persist + broadcast.
    pub async fn apply_quarantine(&self, domain: &str, out_ip: &str, ttl_secs: u64) {
        // REVIEW-R2 Q2：pub 方法调用方不可信，首行防御（空/none 直接丢弃）。
        if !valid_quarantine_target(domain, out_ip) {
            log::debug!(
                "[CircuitBreaker] refuse quarantine for empty/none target {domain:?}:{out_ip:?}"
            );
            return;
        }
        // 1. Same-process memory isolation (fast path, <50ms).
        self.router.set_quarantine(domain, out_ip, ttl_secs);

        // 2. Cross-instance persistence（A6：失败单次重试，仍失败记数＋warn，
        // 不再 unwrap_or(()) 静默；内存隔离已生效，重试耗尽仅丢跨实例同步）。
        let key = quarantine_redis_key(domain, out_ip);
        let base_conn = self.redis_conn.clone();
        let op_key = key.clone();
        let persist = run_with_single_retry(move || {
            let mut conn = base_conn.clone();
            let key = op_key.clone();
            async move { conn.set_ex::<_, _, ()>(&key, "BANNED", ttl_secs).await }
        })
        .await;
        if let Err(e) = persist {
            self.remote_stats.note_persist_failure();
            log::warn!(
                "[CircuitBreaker] quarantine persist failed after 1 retry (memory isolation still applied): {key} ttl={ttl_secs}s err={e:?}"
            );
        }

        // 3. Fan-out to every gateway instance（同上：重试一次＋记数＋warn）。
        let message = quarantine_delta_message(domain, out_ip, ttl_secs);
        let base_conn = self.redis_conn.clone();
        let op_message = message.clone();
        let fanout = run_with_single_retry(move || {
            let mut conn = base_conn.clone();
            let message = op_message.clone();
            async move { conn.publish::<_, _, ()>(DELTA_CHANNEL, message).await }
        })
        .await;
        if let Err(e) = fanout {
            self.remote_stats.note_publish_failure();
            log::warn!(
                "[CircuitBreaker] quarantine fan-out failed after 1 retry (memory isolation still applied): {message} err={e:?}"
            );
        }

        log::warn!("[CircuitBreaker] Quarantined IP {out_ip} on domain {domain} for {ttl_secs}s");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{ProxyNode, RoutingSpec};

    #[test]
    fn ttl_mapping_matches_plan() {
        assert_eq!(quarantine_ttl_for_status(429), Some(60));
        assert_eq!(quarantine_ttl_for_status(403), Some(600));
        assert_eq!(quarantine_ttl_for_status(502), Some(30));
        assert_eq!(quarantine_ttl_for_status(504), Some(30));
        assert_eq!(quarantine_ttl_for_status(200), None);
        assert_eq!(quarantine_ttl_for_status(500), None);
    }

    #[test]
    fn delta_parse_strict() {
        assert_eq!(
            parse_delta_message("QUARANTINE|a.com|10.0.0.1|600"),
            Some(("a.com".to_string(), "10.0.0.1".to_string(), 600))
        );
        assert_eq!(parse_delta_message("QUARANTINE|a.com|10.0.0.1"), None);
        assert_eq!(
            parse_delta_message("QUARANTINE|a.com|10.0.0.1|600|extra"),
            None
        );
        assert_eq!(parse_delta_message("OTHER|a.com|10.0.0.1|600"), None);
        assert_eq!(parse_delta_message("QUARANTINE|a.com|10.0.0.1|abc"), None);
    }

    #[test]
    fn delta_parse_rejects_absurd_ttl() {
        // 复审 FLAG：PubSub 毒报文（超大 ttl）不得进入隔离表（set_quarantine 的
        // `Instant + Duration` 会 panic；上限以内由 set 侧钳制兜底）。
        assert_eq!(parse_delta_message("QUARANTINE|a.com|10.0.0.1|0"), None);
        assert_eq!(parse_delta_message("QUARANTINE|a.com|10.0.0.1|86401"), None);
        assert_eq!(
            parse_delta_message("QUARANTINE|a.com|10.0.0.1|18446744073709551615"),
            None
        );
        assert_eq!(
            parse_delta_message("QUARANTINE|a.com|10.0.0.1|86400"),
            Some(("a.com".to_string(), "10.0.0.1".to_string(), 86400))
        );
    }

    #[test]
    fn quarantine_target_rejects_empty_and_none() {
        // REVIEW-R2 Q2：空 domain／空 ip／网关无节点回落 "none"（gateway.rs logging）
        // 不得进隔离表（junk 键＋广播噪音）。
        assert!(valid_quarantine_target("a.com", "10.0.0.9"));
        assert!(!valid_quarantine_target("", "10.0.0.9"));
        assert!(!valid_quarantine_target("a.com", ""));
        assert!(!valid_quarantine_target("a.com", "none"));
        assert!(!valid_quarantine_target("", "none"));
    }

    #[test]
    fn parse_delta_rejects_empty_and_none() {
        assert_eq!(parse_delta_message("QUARANTINE||10.0.0.9|60"), None);
        assert_eq!(parse_delta_message("QUARANTINE|a.com||60"), None);
        assert_eq!(parse_delta_message("QUARANTINE|a.com|none|60"), None);
    }

    #[test]
    fn apply_delta_applies_valid_and_rejects_junk() {
        // NEXT-A0：增量应用纯函数（PubSub 循环体抽取，可单测；网络循环由 supervise 托管）。
        use crate::router::RouterEngine;
        let router = RouterEngine::new(vec![]);
        assert!(apply_delta(&router, "QUARANTINE|a.com|10.0.0.9|60"));
        assert!(!apply_delta(&router, "GARBAGE"));
        assert!(!apply_delta(&router, "QUARANTINE||10.0.0.9|60"));
        assert!(!apply_delta(&router, "QUARANTINE|a.com|none|60"));
        // 生效验证：被隔离域选路摘空（空池本就 None，换有节点断言）。
        let node = ProxyNode::new(
            "10.0.0.9".to_string(),
            8080,
            None,
            None,
            "US".to_string(),
            "residential".to_string(),
            "mock-a".to_string(),
            100,
        );
        let router2 = RouterEngine::new(vec![node]);
        let spec = RoutingSpec {
            country: None,
            session_id: None,
            tier: None,
            target_domain: "a.com".to_string(),
            proto: None,
        };
        assert!(router2.select_node(&spec).is_some());
        assert!(apply_delta(&router2, "QUARANTINE|a.com|10.0.0.9|60"));
        assert!(router2.select_node(&spec).is_none());
    }

    #[test]
    fn memory_isolation_applies_within_50ms() {
        let node = ProxyNode::new(
            "10.0.0.9".to_string(),
            8080,
            None,
            None,
            "US".to_string(),
            "residential".to_string(),
            "mock-a".to_string(),
            100,
        );
        let router = RouterEngine::new(vec![node]);
        let spec = |d: &str| RoutingSpec {
            country: None,
            session_id: None,
            tier: None,
            target_domain: d.to_string(),
            proto: None,
        };
        assert!(router.select_node(&spec("a.com")).is_some());
        let start = std::time::Instant::now();
        // Same call the CB worker makes on a 403 (ttl from mapping).
        router.set_quarantine("a.com", "10.0.0.9", quarantine_ttl_for_status(403).unwrap());
        assert!(start.elapsed() < Duration::from_millis(50));
        assert!(router.select_node(&spec("a.com")).is_none());
        // Other domains stay routable (domain-scoped isolation).
        assert!(router.select_node(&spec("b.com")).is_some());
    }

    /// Live Redis integration: `apply_quarantine` persists SETEX, broadcasts
    /// the delta, and isolates the domain in memory. Skips (passes) when Redis
    /// is unreachable. Run with `cargo test -- --ignored`.
    #[tokio::test]
    #[ignore]
    async fn live_quarantine_persists_and_broadcasts() {
        use futures::StreamExt;
        use redis::AsyncCommands;
        let domain = "itest.example";
        let out_ip = "10.9.9.9";
        // OPT-R4 C6：live 测试读 REDIS_URL（带密 compose），缺省沿用无密本机。
        let url =
            std::env::var("REDIS_URL").unwrap_or_else(|_| "redis://127.0.0.1:6379/".to_string());
        let client = match redis::Client::open(url) {
            Ok(c) => c,
            Err(e) => {
                eprintln!("SKIP live_quarantine: bad URL ({e:?})");
                return;
            }
        };
        let manager = match tokio::time::timeout(
            Duration::from_secs(2),
            ConnectionManager::new(client.clone()),
        )
        .await
        {
            Ok(Ok(m)) => m,
            _ => {
                eprintln!("SKIP live_quarantine: Redis unreachable");
                return;
            }
        };
        let mut pubsub = match client.get_async_pubsub().await {
            Ok(p) => p,
            Err(e) => {
                eprintln!("SKIP live_quarantine: pubsub failed ({e:?})");
                return;
            }
        };
        if pubsub.subscribe(DELTA_CHANNEL).await.is_err() {
            eprintln!("SKIP live_quarantine: subscribe failed");
            return;
        }
        let mut stream = pubsub.on_message();
        let node = ProxyNode::new(
            out_ip.to_string(),
            8080,
            None,
            None,
            "US".to_string(),
            "residential".to_string(),
            "mock-a".to_string(),
            100,
        );
        let router = Arc::new(RouterEngine::new(vec![node]));
        let cb = PassiveCircuitBreaker::new(
            manager.clone(),
            "stream:proxy:telemetry".to_string(),
            "circuit_breaker_group".to_string(),
            "itest".to_string(),
            router.clone(),
        );
        cb.apply_quarantine(domain, out_ip, 5).await;
        // 1. Memory isolation: target domain blocked, others routable.
        let spec = |d: &str| RoutingSpec {
            country: None,
            session_id: None,
            tier: None,
            target_domain: d.to_string(),
            proto: None,
        };
        assert!(router.select_node(&spec(domain)).is_none());
        assert!(router.select_node(&spec("other.example")).is_some());
        // 2. Redis persistence.
        let mut conn = manager.clone();
        let persisted: Option<String> = conn
            .get(format!("quarantine:{domain}:{out_ip}"))
            .await
            .unwrap_or(None);
        assert_eq!(persisted.as_deref(), Some("BANNED"));
        // 3. Delta broadcast received.
        let payload: String =
            match tokio::time::timeout(Duration::from_secs(3), stream.next()).await {
                Ok(Some(msg)) => msg.get_payload().unwrap_or_default(),
                _ => panic!("delta broadcast not received"),
            };
        assert_eq!(
            parse_delta_message(&payload),
            Some((domain.to_string(), out_ip.to_string(), 5))
        );
        let _: () = conn
            .del(format!("quarantine:{domain}:{out_ip}"))
            .await
            .unwrap_or(());
    }

    /// A6（S）：远端同步失败必须可观测＋只重试一次（错误注入，无需 Redis）。
    /// TDD 红→绿：先断言新 API 存在（缺实现时编译即红），再补最小实现。
    #[test]
    fn remote_stats_counts_failures() {
        // 熔断远端吞错：SETEX/PUBLISH 失败要计数＋warn，不能 unwrap_or(()) 静默。
        let stats = RemoteSyncStats::default();
        assert_eq!(stats.persist_failures(), 0);
        assert_eq!(stats.publish_failures(), 0);
        stats.note_persist_failure();
        stats.note_persist_failure();
        stats.note_publish_failure();
        assert_eq!(stats.persist_failures(), 2);
        assert_eq!(stats.publish_failures(), 1);
    }

    #[test]
    fn quarantine_key_and_delta_format_locked() {
        // 键/报文格式锁定：重构重试逻辑时不得改动线上格式。
        assert_eq!(
            quarantine_redis_key("a.com", "10.0.0.9"),
            "quarantine:a.com:10.0.0.9"
        );
        assert_eq!(
            quarantine_delta_message("a.com", "10.0.0.9", 60),
            "QUARANTINE|a.com|10.0.0.9|60"
        );
    }

    #[tokio::test]
    async fn remote_retry_runs_single_retry_on_failure() {
        // 错误注入：闭包每次都失败 → 恰好尝试 2 次（首次＋单次重试），仍返回 Err。
        use std::sync::atomic::{AtomicUsize, Ordering};
        use std::sync::Arc;
        let attempts = Arc::new(AtomicUsize::new(0));
        let a = attempts.clone();
        let r = run_with_single_retry(move || {
            let a = a.clone();
            async move {
                a.fetch_add(1, Ordering::SeqCst);
                Err::<(), redis::RedisError>(redis::RedisError::from((
                    redis::ErrorKind::IoError,
                    "injected persist failure",
                )))
            }
        })
        .await;
        assert!(r.is_err());
        assert_eq!(attempts.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn remote_retry_succeeds_on_second_attempt() {
        // 错误注入：首次失败、重试成功 → 返回 Ok，且只尝试 2 次。
        use std::sync::atomic::{AtomicUsize, Ordering};
        use std::sync::Arc;
        let attempts = Arc::new(AtomicUsize::new(0));
        let a = attempts.clone();
        let r = run_with_single_retry(move || {
            let a = a.clone();
            async move {
                let n = a.fetch_add(1, Ordering::SeqCst);
                if n == 0 {
                    Err::<(), redis::RedisError>(redis::RedisError::from((
                        redis::ErrorKind::IoError,
                        "injected transient failure",
                    )))
                } else {
                    Ok(())
                }
            }
        })
        .await;
        assert!(r.is_ok());
        assert_eq!(attempts.load(Ordering::SeqCst), 2);
    }

    // ---- OPT-R11 C1：PubSub 入口的 domain 长度闸门 ----

    /// 超长 domain 的增量报文必须在**解析层**就被拒（不得依赖
    /// 下游 `set_quarantine` 兜底）——Redis 可达者可绕过数据面一刀注入任意长度。
    #[test]
    fn opt_r11_c1_parse_delta_rejects_overlong_domain() {
        use crate::router::QUARANTINE_DOMAIN_MAX_BYTES;
        let long = "a".repeat(QUARANTINE_DOMAIN_MAX_BYTES + 1);
        let payload = format!("QUARANTINE|{long}|10.0.0.1|600");
        assert!(
            parse_delta_message(&payload).is_none(),
            "超长 domain 的 PubSub 报文必须在解析层被拒"
        );
        // 恰好上限仍应通过（不得多拒）。
        let exact = "b".repeat(QUARANTINE_DOMAIN_MAX_BYTES);
        let ok = format!("QUARANTINE|{exact}|10.0.0.1|600");
        assert!(
            parse_delta_message(&ok).is_some(),
            "恰好上限的 domain 必须仍然可用"
        );
    }
}
