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
use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::Arc;
use std::time::Duration;

/// Sink consumer group (separate from the circuit-breaker group).
pub const SINK_GROUP: &str = "ch_sink_group";
/// Pending entries idle longer than this are reclaimed after a crash.
pub const RECLAIM_IDLE: Duration = Duration::from_secs(60);
/// Backoff when ClickHouse insert fails (weights/acks hold, loop continues).
pub const INSERT_BACKOFF: Duration = Duration::from_secs(1);
/// R2-5 已见 event_id 窗口：命中即重复（ack+skip，不进仓）。
/// 10k × ~40B ≈ 400KB 常驻，insert→ack 间崩溃的重放对消于此。
pub const SEEN_IDS_CAP: usize = 10_000;

/// Parse one stream entry into a warehouse row (`None` = poison, ack+skip).
/// R2-5：返回 `(row, event_id)`，空 id 表示滚动升级中的老事件（永不去重）。
pub fn parse_entry(fields: &HashMap<String, redis::Value>) -> Option<(TelemetryRow, String)> {
    let payload = field_text(fields, "payload")?;
    let event: TelemetryEvent = serde_json::from_str(&payload).ok()?;
    let id = event.event_id.clone();
    Some((TelemetryRow::from(&event), id))
}

/// R2-5 已见 ID 环形窗（`check_and_insert` 复合操作，调用方无需二次查找）。
pub struct SeenIds {
    set: HashSet<String>,
    order: VecDeque<String>,
    cap: usize,
}

impl SeenIds {
    pub fn new(cap: usize) -> Self {
        Self {
            set: HashSet::with_capacity(cap.min(1024)),
            order: VecDeque::with_capacity(cap.min(1024)),
            cap: cap.max(1),
        }
    }

    /// 没见过 → 记录并返回 false；见过 → true（重复）。
    /// 空串永不记录、永不命中（老事件全放行）。
    pub fn check_and_insert(&mut self, id: &str) -> bool {
        if id.is_empty() {
            return false;
        }
        if self.set.contains(id) {
            return true;
        }
        if self.order.len() >= self.cap {
            if let Some(old) = self.order.pop_front() {
                self.set.remove(&old);
            }
        }
        self.set.insert(id.to_string());
        self.order.push_back(id.to_string());
        false
    }

    #[cfg(test)]
    pub fn len(&self) -> usize {
        self.set.len()
    }
}

pub struct ChSinkWorker {
    redis_conn: ConnectionManager,
    analytics: Arc<AnalyticsEngine>,
    stream_key: String,
    group: String,
    consumer: String,
    batch_size: usize,
    flush_interval: Duration,
    /// R2-5 去重窗：insert→ack 间崩溃的重放对消于此，不进仓。
    seen: SeenIds,
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
            seen: SeenIds::new(SEEN_IDS_CAP),
        }
    }

    /// R2-5 单条目分类（纯内存操作，无网络 I/O，可单测）：
    /// - 毒丸（坏 JSON）→ `skip_ids`（即时 ack，永不卡组）；
    /// - 重复 event_id → `skip_ids`（debug，不进仓）；
    /// - 有效 → `rows` + `valid_ids`（超 `rows_cap` 则两边都不进，
    ///   不 ack 不丢，留待下轮，内存有界）；
    /// - 空 id 老事件 → 按有效处理（永不去重）。
    /// - P5 重放直通：`is_redelivery` 为 true（autoclaim 取回的 hold 条目，
    ///   同 stream id 重现）时跳过去重检查，直进 rows——hold 条目首读已进过去重窗，
    ///   若再检查必被误判重复而吞掉（P5-2 CH 中断演练抓获的静默丢数，见 EXEC_LOG）。
    ///   重试新条目恒走 fresh 路径（不同 stream id），去重语义不受影响。
    ///   自由函数（不借 `self`）：单测无需构造 worker（免 Redis/CH）。
    // 8 参数沿 ProxyNode::new 惯例放行（调用点皆为固定容器＋标量，打包反增分配）。
    #[allow(clippy::too_many_arguments)]
    fn classify_entry(
        seen: &mut SeenIds,
        id: String,
        fields: &HashMap<String, redis::Value>,
        rows: &mut Vec<TelemetryRow>,
        valid_ids: &mut Vec<String>,
        skip_ids: &mut Vec<String>,
        rows_cap: usize,
        is_redelivery: bool,
    ) {
        match parse_entry(fields) {
            Some((row, event_id)) => {
                if !is_redelivery && !event_id.is_empty() && seen.check_and_insert(&event_id) {
                    log::debug!("[ChSink] duplicate {event_id} ({id}) acked+skipped");
                    skip_ids.push(id);
                } else if rows.len() < rows_cap {
                    rows.push(row);
                    valid_ids.push(id);
                } else {
                    log::warn!("[ChSink] rows cap {rows_cap} hit, holding {id} for next cycle");
                }
            }
            None => {
                log::warn!("[ChSink] poison entry {id} acked+skipped");
                skip_ids.push(id);
            }
        }
    }

    /// One reclaim + read + land cycle. Returns rows landed (0 on idle/error).
    pub async fn pump_once(&mut self) -> usize {
        let mut valid_ids: Vec<String> = Vec::new();
        let mut skip_ids: Vec<String> = Vec::new();
        let mut rows: Vec<TelemetryRow> = Vec::new();
        // R2-5 背压上限：单轮 reclaim(batch) + fresh(batch) 至多 2×batch，
        // 显式 cap 截断护内存（CH 宕机时不积压）。
        let rows_cap = self.batch_size.saturating_mul(2).max(1);

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
                Self::classify_entry(
                    &mut self.seen,
                    entry.id,
                    &entry.map,
                    &mut rows,
                    &mut valid_ids,
                    &mut skip_ids,
                    rows_cap,
                    true, // P5：重放直通（hold 条目首读已进窗，再检查必误判重复而吞数）
                );
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
                        Self::classify_entry(
                            &mut self.seen,
                            entry.id,
                            &entry.map,
                            &mut rows,
                            &mut valid_ids,
                            &mut skip_ids,
                            rows_cap,
                            false, // 新条目走去重检查（重试 XADD 对消于此）
                        );
                    }
                }
            }
            Err(e) => {
                log::debug!("[ChSink] stream read error: {e:?}");
            }
        }

        // 3. R2-5：毒丸/重复即时 ack（不随 insert 成败），有效按 insert 成败 ack。
        // 失败只 hold 有效（毒丸不再重放 warn，下轮只剩有效 + 新货）。
        if !skip_ids.is_empty() {
            let _: () = conn
                .xack(&self.stream_key, &self.group, &skip_ids)
                .await
                .unwrap_or(());
        }
        if rows.is_empty() {
            return 0;
        }

        match self.analytics.insert_batch(&rows).await {
            Ok(()) => {
                let landed = rows.len();
                let _: () = conn
                    .xack(&self.stream_key, &self.group, &valid_ids)
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

    /// R2-5 启动时清一次幽灵消费者（idle 超过 reclaim horizon 且非本实例）。
    /// 被删者的未 ack 条目由 autoclaim 接管重放（去重窗对消重复），故删除安全。
    async fn cleanup_stale_consumers(&self) {
        let mut conn = self.redis_conn.clone();
        let reply: redis::RedisResult<redis::streams::StreamInfoConsumersReply> =
            conn.xinfo_consumers(&self.stream_key, &self.group).await;
        match reply {
            Ok(info) => {
                for c in info.consumers {
                    if c.name != self.consumer && c.idle as u64 > RECLAIM_IDLE.as_millis() as u64 {
                        let _: () = conn
                            .xgroup_delconsumer(&self.stream_key, &self.group, &c.name)
                            .await
                            .unwrap_or(());
                        log::info!(
                            "[ChSink] delconsumer {} (idle {}ms, pending {})",
                            c.name,
                            c.idle,
                            c.pending
                        );
                    }
                }
            }
            Err(e) => {
                log::debug!("[ChSink] xinfo consumers failed: {e:?}");
            }
        }
    }

    /// Background loop until process exit.
    pub async fn run(mut self) {
        let mut conn = self.redis_conn.clone();
        let _: redis::RedisResult<()> = conn
            .xgroup_create_mkstream(&self.stream_key, &self.group, "$")
            .await;
        log::info!(
            "[ChSink] pump online (group={} batch={})",
            self.group,
            self.batch_size
        );
        self.cleanup_stale_consumers().await;
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
            event_id: "sink-1".to_string(),
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
        let (row, id) = parse_entry(&fields(&valid_payload())).expect("row");
        assert_eq!(row.provider, "sinktest");
        assert_eq!(row.status_code, 200);
        assert_eq!(id, "sink-1");
    }

    #[test]
    fn poison_entries_rejected() {
        assert!(parse_entry(&fields("{not json")).is_none());
        assert!(parse_entry(&fields("")).is_none());
        assert!(parse_entry(&HashMap::new()).is_none());
        // Valid JSON but wrong shape.
        assert!(parse_entry(&fields(r#"{"a":1}"#)).is_none());
    }

    #[test]
    fn seen_ids_dedupes_and_ignores_empty() {
        // R2-5：二次命中为重复；空 id 永不命中；窗口有界。
        let mut seen = SeenIds::new(3);
        assert!(!seen.check_and_insert("a"));
        assert!(seen.check_and_insert("a"));
        assert!(!seen.check_and_insert(""));
        assert!(!seen.check_and_insert(""));
        assert_eq!(seen.len(), 1);
        assert!(!seen.check_and_insert("b"));
        assert!(!seen.check_and_insert("c"));
        assert_eq!(seen.len(), 3);
        // 驱逐最老（a），a 重新变为“没见过”，窗口长度恒定。
        assert!(!seen.check_and_insert("d"));
        assert_eq!(seen.len(), 3);
        assert!(!seen.check_and_insert("a"));
    }

    #[test]
    fn classify_splits_valid_poison_dup_and_caps() {
        // R2-5：有效/毒丸/重复三分流；超 cap 的有效不 ack 不丢。
        let mut seen = SeenIds::new(SEEN_IDS_CAP);
        let (mut rows, mut valid, mut skip) = (Vec::new(), Vec::new(), Vec::new());
        let f = fields(&valid_payload());
        // 有效 → rows。
        ChSinkWorker::classify_entry(
            &mut seen,
            "1-0".into(),
            &f,
            &mut rows,
            &mut valid,
            &mut skip,
            10,
            false,
        );
        assert_eq!(rows.len(), 1);
        assert_eq!(valid, vec!["1-0".to_string()]);
        assert!(skip.is_empty());
        // 同 event_id 重放 → skip（重复）。
        ChSinkWorker::classify_entry(
            &mut seen,
            "1-1".into(),
            &f,
            &mut rows,
            &mut valid,
            &mut skip,
            10,
            false,
        );
        assert_eq!(rows.len(), 1);
        assert_eq!(skip, vec!["1-1".to_string()]);
        // 毒丸 → skip。
        ChSinkWorker::classify_entry(
            &mut seen,
            "bad-0".into(),
            &fields("{poison"),
            &mut rows,
            &mut valid,
            &mut skip,
            10,
            false,
        );
        assert_eq!(skip.len(), 2);
        // cap=1 且已有 1 行 → 新有效（新 id，非重复）两边都不进（留待下轮）。
        ChSinkWorker::classify_entry(
            &mut seen,
            "held-0".into(),
            &fields(&payload_with_id("fresh-cap-1")),
            &mut rows,
            &mut valid,
            &mut skip,
            1,
            false,
        );
        assert_eq!(rows.len(), 1);
        assert!(!valid.contains(&"held-0".to_string()));
        assert!(!skip.contains(&"held-0".to_string()));
    }

    #[test]
    fn redelivered_hold_is_not_a_duplicate() {
        // P5 真 bug 回归：hold-and-redeliver（同 stream id＋同 event_id，insert 失败
        // hold 后被 autoclaim 取回）必须再次进 rows，不能被去重窗吞掉。
        // 去重窗只对消“重试产生的新 stream 条目”（不同 stream id，同 event_id）。
        let mut seen = SeenIds::new(SEEN_IDS_CAP);
        let (mut rows, mut valid, mut skip) = (Vec::new(), Vec::new(), Vec::new());
        let f = fields(&valid_payload());
        // 首读：有效。
        ChSinkWorker::classify_entry(
            &mut seen,
            "7-0".into(),
            &f,
            &mut rows,
            &mut valid,
            &mut skip,
            10,
            false,
        );
        assert_eq!(rows.len(), 1);
        // hold 后重放（redelivery=true）：仍有效，不是重复。
        ChSinkWorker::classify_entry(
            &mut seen,
            "7-0".into(),
            &f,
            &mut rows,
            &mut valid,
            &mut skip,
            10,
            true,
        );
        assert_eq!(rows.len(), 2, "redelivered hold must re-enter rows");
        assert!(skip.is_empty());
        // 而重试新条目（不同 stream id，同 event_id）仍被去重。
        ChSinkWorker::classify_entry(
            &mut seen,
            "7-1".into(),
            &f,
            &mut rows,
            &mut valid,
            &mut skip,
            10,
            false,
        );
        assert_eq!(rows.len(), 2);
        assert_eq!(skip, vec!["7-1".to_string()]);
    }

    /// 同一有效载荷换新 event_id（cap/去重单测用，不碰线上 valid_payload）。
    fn payload_with_id(id: &str) -> String {
        let mut v: serde_json::Value = serde_json::from_str(&valid_payload()).unwrap();
        v["event_id"] = serde_json::Value::String(id.to_string());
        serde_json::to_string(&v).unwrap()
    }

    #[test]
    fn pure_poison_batch_advances_without_rows() {
        // R2-5：纯毒丸批 → 无行、有 skip（pump 侧即时 ack，组永不卡住）。
        let mut seen = SeenIds::new(SEEN_IDS_CAP);
        let (mut rows, mut valid, mut skip) = (Vec::new(), Vec::new(), Vec::new());
        for i in 0..3 {
            ChSinkWorker::classify_entry(
                &mut seen,
                format!("poison-{i}"),
                &fields("{poison"),
                &mut rows,
                &mut valid,
                &mut skip,
                10,
                false,
            );
        }
        assert!(rows.is_empty());
        assert!(valid.is_empty());
        assert_eq!(skip.len(), 3);
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
        // R2-5：pump 需 &mut（去重窗写入）。
        let mut worker = worker;
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
