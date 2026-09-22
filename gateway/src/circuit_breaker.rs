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
pub fn parse_delta_message(payload: &str) -> Option<(String, String, u64)> {
    use crate::router::QUARANTINE_MAX_TTL_SECS;
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
            .map(|t| (domain.to_string(), ip.to_string(), t)),
        _ => None,
    }
}

pub(crate) fn field_text(fields: &HashMap<String, redis::Value>, key: &str) -> Option<String> {
    match fields.get(key) {
        Some(redis::Value::BulkString(bytes)) => String::from_utf8(bytes.clone()).ok(),
        Some(redis::Value::SimpleString(s)) => Some(s.clone()),
        _ => None,
    }
}

pub struct PassiveCircuitBreaker {
    redis_conn: ConnectionManager,
    stream_key: String,
    consumer_group: String,
    consumer_name: String,
    router: Arc<RouterEngine>,
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
            self.apply_quarantine(&domain, &event.out_ip, ttl).await;
        }
    }

    /// Apply domain isolation: memory first, then Redis persist + broadcast.
    pub async fn apply_quarantine(&self, domain: &str, out_ip: &str, ttl_secs: u64) {
        // 1. Same-process memory isolation (fast path, <50ms).
        self.router.set_quarantine(domain, out_ip, ttl_secs);

        // 2. Cross-instance persistence.
        let mut conn = self.redis_conn.clone();
        let key = format!("quarantine:{domain}:{out_ip}");
        let _: () = conn.set_ex(&key, "BANNED", ttl_secs).await.unwrap_or(());

        // 3. Fan-out to every gateway instance.
        let message = format!("QUARANTINE|{domain}|{out_ip}|{ttl_secs}");
        let _: () = conn.publish(DELTA_CHANNEL, message).await.unwrap_or(());

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
        let client = match redis::Client::open("redis://127.0.0.1:6379/") {
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
}
