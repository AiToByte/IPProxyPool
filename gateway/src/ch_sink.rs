//! GW-R2 ClickHouse sink pump: Redis Stream → warehouse batch landings.
//!
//! Own consumer group (`ch_sink_group`) over `stream:proxy:telemetry`.
//! Each cycle reclaims stale pending (>60s idle, crashed workers) then reads
//! fresh entries, converts to [`TelemetryRow`], and lands one LZ4 block via
//! `insert_batch` (batch 5000 / 1s, from [`AnalyticsEngine`] thresholds).
//!
//! Ack discipline: ack only after a successful insert (redelivery on failure);
//! poison entries (bad JSON) are acked + skipped with a warn so one bad event
//! can never wedge the group.

use crate::analytics::{AnalyticsEngine, TelemetryRow};
use crate::circuit_breaker::field_text;
use crate::telemetry::TelemetryEvent;
use redis::{aio::ConnectionManager, AsyncCommands};
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

/// Sink consumer group (separate from the circuit-breaker group).
pub const SINK_GROUP: &str = "ch_sink_group";
/// Pending entries idle longer than this are reclaimed after a crash.
pub const RECLAIM_IDLE: Duration = Duration::from_secs(60);
/// Backoff when ClickHouse insert fails (weights/acks hold, loop continues).
pub const INSERT_BACKOFF: Duration = Duration::from_secs(1);

/// Parse one stream entry into a warehouse row (`None` = poison, ack+skip).
pub fn parse_entry(fields: &HashMap<String, redis::Value>) -> Option<TelemetryRow> {
    let payload = field_text(fields, "payload")?;
    let event: TelemetryEvent = serde_json::from_str(&payload).ok()?;
    Some(TelemetryRow::from(&event))
}

pub struct ChSinkWorker {
    redis_conn: ConnectionManager,
    analytics: Arc<AnalyticsEngine>,
    stream_key: String,
    group: String,
    consumer: String,
    batch_size: usize,
    flush_interval: Duration,
}

impl ChSinkWorker {
    pub fn new(
        redis_conn: ConnectionManager,
        analytics: Arc<AnalyticsEngine>,
        stream_key: String,
    ) -> Self {
        let batch_size = analytics.batch_size();
        let flush_interval = analytics.flush_interval();
        Self {
            redis_conn,
            analytics,
            stream_key,
            group: SINK_GROUP.to_string(),
            consumer: format!("sink_{}", std::process::id()),
            batch_size,
            flush_interval,
        }
    }

    /// One reclaim + read + land cycle. Returns rows landed (0 on idle/error).
    pub async fn pump_once(&self) -> usize {
        let mut ids: Vec<String> = Vec::new();
        let mut rows: Vec<TelemetryRow> = Vec::new();

        // 1. Reclaim stale pending from crashed consumers.
        let mut conn = self.redis_conn.clone();
        let opts = redis::streams::StreamAutoClaimOptions::default().count(self.batch_size);
        if let Ok(reply) = conn
            .xautoclaim_options(
                &self.stream_key,
                &self.group,
                &self.consumer,
                RECLAIM_IDLE.as_millis() as usize,
                "0-0",
                opts,
            )
            .await
        {
            let reply: redis::streams::StreamAutoClaimReply = reply;
            for entry in reply.claimed {
                match parse_entry(&entry.map) {
                    Some(row) => {
                        rows.push(row);
                        ids.push(entry.id);
                    }
                    None => {
                        log::warn!("[ChSink] poison entry {} acked+skipped", entry.id);
                        ids.push(entry.id);
                    }
                }
            }
        }

        // 2. Read fresh entries (block up to one flush interval).
        let read_opts = redis::streams::StreamReadOptions::default()
            .group(&self.group, &self.consumer)
            .count(self.batch_size)
            .block(self.flush_interval.as_millis() as usize);
        match conn
            .xread_options(&[&self.stream_key], &[">"], &read_opts)
            .await
        {
            Ok(reply) => {
                let reply: redis::streams::StreamReadReply = reply;
                for key in reply.keys {
                    for entry in key.ids {
                        match parse_entry(&entry.map) {
                            Some(row) => {
                                rows.push(row);
                                ids.push(entry.id);
                            }
                            None => {
                                log::warn!("[ChSink] poison entry {} acked+skipped", entry.id);
                                ids.push(entry.id);
                            }
                        }
                    }
                }
            }
            Err(e) => {
                log::debug!("[ChSink] stream read error: {e:?}");
            }
        }

        if rows.is_empty() {
            // Idle cycle: still ack pure-poison batches so the group advances.
            if !ids.is_empty() {
                let _: () = conn
                    .xack(&self.stream_key, &self.group, &ids)
                    .await
                    .unwrap_or(());
            }
            return 0;
        }

        match self.analytics.insert_batch(&rows).await {
            Ok(()) => {
                let landed = rows.len();
                let _: () = conn
                    .xack(&self.stream_key, &self.group, &ids)
                    .await
                    .unwrap_or(());
                log::info!("[ChSink] landed {landed} rows to ClickHouse");
                landed
            }
            Err(e) => {
                log::error!("[ChSink] insert failed ({e:?}), holding acks for redelivery");
                tokio::time::sleep(INSERT_BACKOFF).await;
                0
            }
        }
    }

    /// Background loop until process exit.
    pub async fn run(self) {
        let mut conn = self.redis_conn.clone();
        let _: redis::RedisResult<()> = conn
            .xgroup_create_mkstream(&self.stream_key, &self.group, "$")
            .await;
        log::info!(
            "[ChSink] pump online (group={} batch={})",
            self.group,
            self.batch_size
        );
        loop {
            self.pump_once().await;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fields(payload: &str) -> HashMap<String, redis::Value> {
        HashMap::from([
            (
                "payload".to_string(),
                redis::Value::BulkString(payload.as_bytes().to_vec()),
            ),
            (
                "domain".to_string(),
                redis::Value::BulkString(b"d.example".to_vec()),
            ),
            (
                "status".to_string(),
                redis::Value::BulkString(b"200".to_vec()),
            ),
        ])
    }

    fn valid_payload() -> String {
        serde_json::to_string(&TelemetryEvent {
            client_ip: "c".to_string(),
            target_domain: "d.example".to_string(),
            out_ip: "o".to_string(),
            provider: "sinktest".to_string(),
            tier: "datacenter".to_string(),
            country: "US".to_string(),
            status_code: 200,
            latency_ms: 5,
            transferred_bytes: 10,
            retry_count: 0,
            tenant_id: None,
            error_type: None,
            timestamp: 1_700_000_000_000,
        })
        .unwrap()
    }

    #[test]
    fn valid_entry_converts() {
        let row = parse_entry(&fields(&valid_payload())).expect("row");
        assert_eq!(row.provider, "sinktest");
        assert_eq!(row.status_code, 200);
    }

    #[test]
    fn poison_entries_rejected() {
        assert!(parse_entry(&fields("{not json")).is_none());
        assert!(parse_entry(&fields("")).is_none());
        assert!(parse_entry(&HashMap::new()).is_none());
        // Valid JSON but wrong shape.
        assert!(parse_entry(&fields(r#"{"a":1}"#)).is_none());
    }

    /// Live pump: XADD 2 events (1 valid + 1 poison), `pump_once` lands 1 row
    /// in ClickHouse and advances the group. Needs Redis + CH. `-- --ignored`.
    #[tokio::test]
    #[ignore]
    async fn live_pump_once_lands_rows() {
        use redis::AsyncCommands;
        let client = match redis::Client::open("redis://127.0.0.1:6379/") {
            Ok(c) => c,
            Err(e) => {
                eprintln!("SKIP live_pump: bad URL ({e:?})");
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
                eprintln!("SKIP live_pump: Redis unreachable");
                return;
            }
        };
        let analytics = Arc::new(AnalyticsEngine::new(
            "http://127.0.0.1:8123",
            "proxy",
            "123456",
            "proxy",
        ));
        if analytics.ping().await.is_err() {
            eprintln!("SKIP live_pump: ClickHouse unreachable");
            return;
        }
        // Ensure the sink group exists before injecting.
        let mut conn = manager.clone();
        let _: redis::RedisResult<()> = conn
            .xgroup_create_mkstream(STREAM_KEY_TEST, SINK_GROUP, "$")
            .await;
        let id_ok: String = conn
            .xadd(
                STREAM_KEY_TEST,
                "*",
                &[
                    ("payload", valid_payload().as_str()),
                    ("domain", "d.example"),
                    ("status", "200"),
                ],
            )
            .await
            .expect("xadd valid");
        let id_bad: String = conn
            .xadd(
                STREAM_KEY_TEST,
                "*",
                &[
                    ("payload", "{poison"),
                    ("domain", "d.example"),
                    ("status", "200"),
                ],
            )
            .await
            .expect("xadd poison");
        let worker = ChSinkWorker::new(
            manager.clone(),
            analytics.clone(),
            STREAM_KEY_TEST.to_string(),
        );
        // Poll a few cycles: block-read needs the entries to arrive.
        let mut landed = 0;
        for _ in 0..5 {
            landed += worker.pump_once().await;
            if landed >= 1 {
                break;
            }
        }
        assert_eq!(landed, 1, "exactly the valid row lands, poison skipped");
        // Row visible in CH.
        let count: u64 = analytics
            .ch_client()
            .query("SELECT count() FROM proxy.proxy_telemetry_log WHERE provider = 'sinktest'")
            .fetch_one()
            .await
            .expect("count query");
        assert!(count >= 1, "landed row must be queryable");
        // Cleanup both sides.
        let _: () = conn
            .xdel(STREAM_KEY_TEST, &[id_ok, id_bad])
            .await
            .unwrap_or(());
        analytics
            .ch_client()
            .query("ALTER TABLE proxy.proxy_telemetry_log DELETE WHERE provider = 'sinktest'")
            .execute()
            .await
            .expect("ch cleanup");
    }

    const STREAM_KEY_TEST: &str = "stream:proxy:telemetry:sinktest";
}
