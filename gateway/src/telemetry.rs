//! GW-2 zero-blocking telemetry bus: MPSC ring + batched Redis Stream flush.
//!
//! Data-plane workers must never perform synchronous network I/O. Events are
//! pushed via [`TelemetryPublisher::emit`] (`try_send`, dropped when full) and
//! flushed by [`TelemetryWorker`] in batches of 200 / every 100ms into
//! `stream:proxy:telemetry` as `XADD * payload=<json> domain=<d> status=<code>`.

use redis::aio::ConnectionManager;
use serde::{Deserialize, Serialize};
use std::sync::{
    atomic::{AtomicU64, Ordering},
    Arc,
};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use tokio::sync::mpsc;

/// Data-plane channel capacity: full queue drops, never blocks (GW-2a).
pub const TELEMETRY_CHANNEL_CAP: usize = 10_000;
/// Batch flush thresholds (GW-2a).
pub const TELEMETRY_BATCH_SIZE: usize = 200;
pub const TELEMETRY_FLUSH_INTERVAL: Duration = Duration::from_millis(100);
/// OPT-4 落库重试间隔：首次 XADD 失败后睡这么久再重发一次。
pub const TELEMETRY_FLUSH_RETRY_DELAY: Duration = Duration::from_millis(100);

/// Telemetry event. Wire format keeps manual-A2 fields (`payload` JSON +
/// `domain`/`status` index fields); GW-4 columns (`provider/tier/country/…`)
/// ride inside the JSON payload so no signature change is needed later.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct TelemetryEvent {
    pub client_ip: String,
    pub target_domain: String,
    pub out_ip: String,
    pub provider: String,
    pub tier: String,
    pub country: String,
    pub status_code: u16,
    pub latency_ms: u64,
    pub transferred_bytes: u64,
    pub retry_count: u8,
    pub tenant_id: Option<String>,
    pub error_type: Option<String>,
    pub timestamp: u64,
}

impl TelemetryEvent {
    pub fn now_unix_ms() -> u64 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0)
    }
}

/// Data-plane handle: ultra-fast non-blocking push.
pub struct TelemetryPublisher {
    sender: mpsc::Sender<TelemetryEvent>,
}

impl TelemetryPublisher {
    pub fn new(sender: mpsc::Sender<TelemetryEvent>) -> Self {
        Self { sender }
    }

    /// Push an event; drops (with a debug count) when the queue is full.
    /// Never blocks the data plane.
    #[inline]
    pub fn emit(&self, event: TelemetryEvent) {
        if self.sender.try_send(event).is_err() {
            log::debug!("[Telemetry] channel full, event dropped");
        }
    }
}

/// Background batch worker: aggregates events and pipelines XADD to Redis.
pub struct TelemetryWorker {
    receiver: mpsc::Receiver<TelemetryEvent>,
    redis_conn: ConnectionManager,
    stream_key: String,
    batch_size: usize,
    flush_interval: Duration,
    /// OPT-4 丢弃计数：flush 重试一次仍失败时，整批计数累加于此。
    /// 与 `MetricsRegistry` 持有同一个 `Arc`（main 装配），由 metrics 侧渲染，
    /// 网关 `logging` 无需经手——worker 直写，数据面零阻塞语义不变。
    dropped: Arc<AtomicU64>,
}

impl TelemetryWorker {
    /// 新建落库 worker。
    ///
    /// - `dropped` 必须与 metrics 注册表共享同一个 `Arc`（见 `main.rs`），
    ///   以便丢弃数实时进 `telemetry_dropped_total`；
    /// - 签名变更（OPT-4）：调用方（main + live 单测）需同步传入计数器。
    pub fn new(
        receiver: mpsc::Receiver<TelemetryEvent>,
        redis_conn: ConnectionManager,
        stream_key: String,
        dropped: Arc<AtomicU64>,
    ) -> Self {
        Self {
            receiver,
            redis_conn,
            stream_key,
            batch_size: TELEMETRY_BATCH_SIZE,
            flush_interval: TELEMETRY_FLUSH_INTERVAL,
            dropped,
        }
    }

    pub async fn run(mut self) {
        let mut buffer = Vec::with_capacity(self.batch_size);
        let mut last_flush = Instant::now();

        loop {
            tokio::select! {
                Some(event) = self.receiver.recv() => {
                    buffer.push(event);
                    if buffer.len() >= self.batch_size
                        || last_flush.elapsed() >= self.flush_interval
                    {
                        self.flush(&mut buffer).await;
                        last_flush = Instant::now();
                    }
                }
                _ = tokio::time::sleep(self.flush_interval) => {
                    if !buffer.is_empty() {
                        self.flush(&mut buffer).await;
                        last_flush = Instant::now();
                    }
                }
            }
        }
    }

    async fn flush(&mut self, buffer: &mut Vec<TelemetryEvent>) {
        if buffer.is_empty() {
            return;
        }

        // 先整批移出：重发/丢弃都按整批口径计数，避免移出后长度丢失。
        let batch: Vec<TelemetryEvent> = std::mem::take(buffer);
        let batch_len = batch.len() as u64;
        let mut pipe = redis::pipe();
        for event in &batch {
            if let Ok(json_data) = serde_json::to_string(&event) {
                pipe.xadd(
                    &self.stream_key,
                    "*",
                    &[
                        ("payload", json_data),
                        ("domain", event.target_domain.clone()),
                        ("status", event.status_code.to_string()),
                    ],
                );
            }
        }

        // OPT-4：失败 → 睡 100ms 重发一次 → 仍失败则丢弃并计数（原来静默丢）。
        // 重试仍用同一 pipe（命令表不变）；计数按整批事件数累加，供 SLA 审计。
        let mut conn = self.redis_conn.clone();
        if let Err(first) = pipe.query_async::<()>(&mut conn).await {
            log::warn!("[TelemetryWorker] flush failed ({first:?}), retrying once");
            tokio::time::sleep(TELEMETRY_FLUSH_RETRY_DELAY).await;
            let mut retry_conn = self.redis_conn.clone();
            if let Err(second) = pipe.query_async::<()>(&mut retry_conn).await {
                self.dropped.fetch_add(batch_len, Ordering::Relaxed);
                log::error!(
                    "[TelemetryWorker] retry failed, dropped {batch_len} events: {second:?}"
                );
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> TelemetryEvent {
        TelemetryEvent {
            client_ip: "10.0.1.2".to_string(),
            target_domain: "api.target.com".to_string(),
            out_ip: "185.220.101.5".to_string(),
            provider: "mock-a".to_string(),
            tier: "residential".to_string(),
            country: "US".to_string(),
            status_code: 403,
            latency_ms: 42,
            transferred_bytes: 0,
            retry_count: 0,
            tenant_id: None,
            error_type: None,
            timestamp: 1_700_000_000,
        }
    }

    #[test]
    fn event_json_roundtrip() {
        let e = sample();
        let json = serde_json::to_string(&e).unwrap();
        let back: TelemetryEvent = serde_json::from_str(&json).unwrap();
        assert_eq!(e, back);
    }

    #[test]
    fn emit_never_blocks_when_full() {
        let (tx, _rx) = mpsc::channel::<TelemetryEvent>(1);
        let publisher = TelemetryPublisher::new(tx);
        // Fill the single slot, then overflow must still return immediately.
        publisher.emit(sample());
        publisher.emit(sample());
        publisher.emit(sample());
    }

    /// Live Redis integration: worker batch-flushes one event into a test
    /// stream. Skips (passes) when Redis is unreachable, e.g. Docker daemon
    /// down on Windows dev boxes. Run with `cargo test -- --ignored`.
    #[tokio::test]
    #[ignore]
    async fn live_flush_writes_stream() {
        use redis::AsyncCommands;
        let client = match redis::Client::open("redis://127.0.0.1:6379/") {
            Ok(c) => c,
            Err(e) => {
                eprintln!("SKIP live_flush_writes_stream: bad URL ({e:?})");
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
                eprintln!("SKIP live_flush_writes_stream: Redis unreachable");
                return;
            }
        };
        let stream_key = format!("stream:proxy:telemetry:test:{}", std::process::id());
        let (tx, rx) = mpsc::channel::<TelemetryEvent>(TELEMETRY_CHANNEL_CAP);
        // OPT-4：live 工人持独立计数器（与线上 main 的共享装配语义一致，单测不污染全局）。
        let dropped = Arc::new(AtomicU64::new(0));
        let worker = TelemetryWorker::new(rx, manager.clone(), stream_key.clone(), dropped);
        tokio::spawn(worker.run());
        TelemetryPublisher::new(tx).emit(sample());
        tokio::time::sleep(Duration::from_millis(600)).await;
        let mut conn = manager.clone();
        let reply: redis::RedisResult<redis::streams::StreamRangeReply> =
            conn.xrange_all(&stream_key).await;
        match reply {
            Ok(entries) => assert!(!entries.ids.is_empty(), "stream should hold flushed event"),
            Err(e) => panic!("xrange failed on reachable Redis: {e:?}"),
        }
        let _: () = conn.del(&stream_key).await.unwrap_or(());
    }
}
