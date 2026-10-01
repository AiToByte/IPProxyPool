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
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tokio::sync::mpsc;

use crate::metrics::MetricsRegistry;

/// OPT-R15：`telemetry_sink_up{backend="redis"}` 的 backend 标签。
/// Redis 挂了 ⇒ 数据进不了 stream，`ChSinkWorker` 也就无源可读，整条遥测链断在第一级。
pub const SINK_BACKEND_REDIS: &str = "redis";

/// Data-plane channel capacity: full queue drops, never blocks (GW-2a).
pub const TELEMETRY_CHANNEL_CAP: usize = 10_000;
/// Batch flush thresholds (GW-2a).
pub const TELEMETRY_BATCH_SIZE: usize = 200;
pub const TELEMETRY_FLUSH_INTERVAL: Duration = Duration::from_millis(100);
/// OPT-4 落库重试间隔：首次 XADD 失败后睡这么久再重发一次。
pub const TELEMETRY_FLUSH_RETRY_DELAY: Duration = Duration::from_millis(100);
/// R2-5 Stream 上限（约数）：CB/sink 双双挂掉时 Redis 不爆内存。
/// 10 万条按当前量级是数小时缓冲；追不上的消费组以新数据为准（文档写明）。
pub const STREAM_MAXLEN: usize = 100_000;

/// Telemetry event. Wire format keeps manual-A2 fields (`payload` JSON +
/// `domain`/`status` index fields); GW-4 columns (`provider/tier/country/…`)
/// ride inside the JSON payload so no signature change is needed later.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct TelemetryEvent {
    /// R2-4 幂等 ID：`{unix_ms}-{pid}-{seq}`，随 payload 进 Stream，sink 侧
    /// `SeenIds` 窗按它去重（R2-5）。注意它不是 Redis Stream ID（R2-9 更正：
    /// Stream ID 只允许数字型 `<ms>-<seq>`，XADD 一律用自动 `*`）。
    /// 滚动升级中的老 JSON（无此字段）按 `""` 反序列化，worker 侧回填兜底；
    /// 跨实例同毫秒同 seq 的理论碰撞由 pid 隔离。
    #[serde(default)]
    pub event_id: String,
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
    /// R2-4 发号器：`emit` 为空 `event_id` 配 `{ms}-{pid}-{seq}`（本进程唯一）。
    seq: AtomicU64,
    /// R2-4 通道丢弃计数（满队列/已关闭）：与 metrics 共享（publisher 直写，
    /// metrics 只读渲染 `telemetry_channel_dropped_total`）。
    channel_dropped: Arc<AtomicU64>,
}

impl TelemetryPublisher {
    pub fn new(sender: mpsc::Sender<TelemetryEvent>, channel_dropped: Arc<AtomicU64>) -> Self {
        Self {
            sender,
            seq: AtomicU64::new(0),
            channel_dropped,
        }
    }

    /// Push an event; drops (counted) when the queue is full or closed.
    /// Never blocks the data plane.
    #[inline]
    pub fn emit(&self, mut event: TelemetryEvent) {
        if event.event_id.is_empty() {
            let n = self.seq.fetch_add(1, Ordering::Relaxed);
            event.event_id = format!(
                "{}-{}-{n}",
                TelemetryEvent::now_unix_ms(),
                std::process::id()
            );
        }
        if self.sender.try_send(event).is_err() {
            self.channel_dropped.fetch_add(1, Ordering::Relaxed);
            log::debug!("[Telemetry] channel full/closed, event dropped");
        }
    }
}

/// A2：把 batch 逐条序列化后装进 pipe，返回实际进 pipe 的条数 `queued`。
///
/// 序列化失败的事件直接跳过，由调用方按 `batch.len() - queued` 单独计数
///（落库失败只按 `queued` 计，不再按整批累加）。
/// `serialize` 参数供单测注入失败：线上 `serde_json` 对本 struct 几乎不失败，
/// 不可注入则 `queued==0` 全失败分支与部分失败都不可测。
fn encode_batch(
    batch: &[TelemetryEvent],
    stream_key: &str,
    pipe: &mut redis::Pipeline,
    serialize: impl Fn(&TelemetryEvent) -> Option<String>,
) -> u64 {
    let mut queued = 0u64;
    for event in batch {
        if let Some(json_data) = serialize(event) {
            queued += 1;
            // R2-9：XADD 必须用自动 ID `*`。R2-4 的显式 ID（`{ms}-{pid}-{seq}`
            // 三段式）不是合法 Redis Stream ID（只允许数字型 `<ms>-<seq>`），
            // 对真 Redis 全部落库失败（R2-9 curl 回归抓获；此前 live 全为
            // Docker 宕机下的自跳过，见 EXEC_LOG 更正）。
            // 幂等不靠 Stream ID：重试产生的新条目由 payload 内 `event_id`
            // 经 sink 侧 `SeenIds` 窗去重（R2-5），CB 消费组按 payload 语义
            // 处理，两边都不依赖 Stream ID 相等。
            // R2-5：MAXLEN ~ 上限，消费组全挂时 Redis 不爆（老数据先丢）。
            pipe.xadd_maxlen(
                stream_key,
                redis::streams::StreamMaxlen::Approx(STREAM_MAXLEN),
                "*",
                &[
                    ("payload", json_data),
                    ("domain", event.target_domain.clone()),
                    ("status", event.status_code.to_string()),
                ],
            );
        }
    }
    queued
}

/// Background batch worker: aggregates events and pipelines XADD to Redis.
pub struct TelemetryWorker {
    receiver: mpsc::Receiver<TelemetryEvent>,
    redis_conn: ConnectionManager,
    stream_key: String,
    batch_size: usize,
    flush_interval: Duration,
    /// OPT-4 丢弃计数：flush 重试一次仍失败时累加于此。
    /// 与 `MetricsRegistry` 持有同一个 `Arc`（main 装配），由 metrics 侧渲染，
    /// 网关 `logging` 无需经手——worker 直写，数据面零阻塞语义不变。
    /// A2 口径：落库失败按实际进 pipe 的 `queued` 计数；序列化失败
    ///（`batch_len - queued`，含 `queued==0` 全失败分支）单独累加于此，
    /// 不再按整批 `batch_len` 累加。通道满另计 `channel_dropped`。
    flush_dropped: Arc<AtomicU64>,
    /// OPT-R15：落地端健康上报口（与 `flush_dropped` 同一个 `MetricsRegistry`）。
    /// 只在真正与 Redis 交互成功/失败时调用——序列化失败不算落地端问题。
    metrics: Arc<MetricsRegistry>,
    /// R2-4 空 id 回填序号（正常只走 emit 配号；直调 worker 的单测/旧代码走这里）。
    fallback_seq: u64,
}

impl TelemetryWorker {
    /// 新建落库 worker。
    ///
    /// - `flush_dropped` 必须与 metrics 注册表共享同一个 `Arc`（见 `main.rs`），
    ///   以便丢弃数实时进 `telemetry_dropped_total`；
    /// - 签名变更（OPT-4/R2-4）：调用方（main + live 单测）需同步传入计数器。
    pub fn new(
        receiver: mpsc::Receiver<TelemetryEvent>,
        redis_conn: ConnectionManager,
        stream_key: String,
        flush_dropped: Arc<AtomicU64>,
        metrics: Arc<MetricsRegistry>,
    ) -> Self {
        Self {
            receiver,
            redis_conn,
            stream_key,
            batch_size: TELEMETRY_BATCH_SIZE,
            flush_interval: TELEMETRY_FLUSH_INTERVAL,
            flush_dropped,
            metrics,
            fallback_seq: 0,
        }
    }

    pub async fn run(mut self) {
        let mut buffer = Vec::with_capacity(self.batch_size);
        // R2-4：interval 节拍替代每轮新建 sleep，低 QPS 下 flush 延迟稳定，
        // 高 QPS 下满批即刷不等待节拍。
        let mut ticker = tokio::time::interval(self.flush_interval);
        ticker.tick().await; // 首 tick 立即就绪：跳过，避免启动空转一次。

        loop {
            tokio::select! {
                Some(event) = self.receiver.recv() => {
                    buffer.push(event);
                    if buffer.len() >= self.batch_size {
                        self.flush(&mut buffer).await;
                    }
                }
                _ = ticker.tick() => {
                    if !buffer.is_empty() {
                        self.flush(&mut buffer).await;
                    }
                }
            }
        }
    }

    async fn flush(&mut self, buffer: &mut Vec<TelemetryEvent>) {
        if buffer.is_empty() {
            return;
        }

        // 先整批移出：重发/丢弃都按实际口径计数，避免移出后长度丢失。
        let mut batch: Vec<TelemetryEvent> = std::mem::take(buffer);
        // R2-4/R2-9：空 id 兜底回填（payload 级幂等键完整，sink 去重窗消费它）。
        for event in &mut batch {
            if event.event_id.is_empty() {
                self.fallback_seq += 1;
                event.event_id = format!("{}-fallback-{}", event.timestamp, self.fallback_seq);
            }
        }
        let batch_len = batch.len() as u64;
        let mut pipe = redis::pipe();
        // A2：只统计实际进 pipe 的 queued；序列化失败单独计数，不按整批累加。
        let queued = encode_batch(&batch, &self.stream_key, &mut pipe, |e| {
            serde_json::to_string(e).ok()
        });
        let ser_failed = batch_len.saturating_sub(queued);
        if ser_failed > 0 {
            self.flush_dropped.fetch_add(ser_failed, Ordering::Relaxed);
        }

        // OPT-4：失败 → 睡 100ms 重发一次 → 仍失败则丢弃并计数（原来静默丢）。
        // R2-9：重试是同一 pipe 重放（自动 ID 下产生新条目，重复由 sink 侧
        // payload `event_id` 去重窗对消）。
        // A2：queued==0 说明整批序列化失败（buffer 已被 take，不能静默 return
        // 漏计）：上面已单独计数，这里打 warn 后返回；空 pipe 不 query。
        if queued == 0 {
            log::warn!("[TelemetryWorker] serialize failed, dropped {batch_len} events");
            // OPT-R15：序列化失败**不**标记落地端不健康——那是网关自己的问题，
            // 标到 Redis/ClickHouse 上会把责任指错方向，也无法靠改依赖解决。
            return;
        }
        let mut conn = self.redis_conn.clone();
        if let Err(first) = pipe.query_async::<()>(&mut conn).await {
            log::warn!("[TelemetryWorker] flush failed ({first:?}), retrying once");
            // OPT-R15：首次失败即置 `up=0`（语义是"最近一次尝试"，告警侧用持续时间过滤毛刺）。
            self.metrics.mark_sink_failed(SINK_BACKEND_REDIS);
            tokio::time::sleep(TELEMETRY_FLUSH_RETRY_DELAY).await;
            let mut retry_conn = self.redis_conn.clone();
            if let Err(second) = pipe.query_async::<()>(&mut retry_conn).await {
                // A2：按实际进 pipe 的 queued 计数（原来按整批 batch_len，
                // 含未进 pipe 的序列化失败，虚高），供 SLA 审计。
                self.flush_dropped.fetch_add(queued, Ordering::Relaxed);
                log::error!("[TelemetryWorker] retry failed, dropped {queued} events: {second:?}");
            }
        } else {
            // OPT-R15：XADD 成功——Redis 这一级健康。
            self.metrics.mark_sink_ok(SINK_BACKEND_REDIS);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> TelemetryEvent {
        TelemetryEvent {
            event_id: "7-123-0".to_string(),
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
    fn legacy_json_without_id_deserializes() {
        // R2-4：滚动升级中老 JSON 无 event_id，反序列化得 ""（worker 回填兜底）。
        let back: TelemetryEvent =
            serde_json::from_str(r#"{"client_ip":"c","target_domain":"d","out_ip":"o","provider":"p","tier":"t","country":"c","status_code":200,"latency_ms":1,"transferred_bytes":0,"retry_count":0,"tenant_id":null,"error_type":null,"timestamp":1}"#)
                .unwrap();
        assert_eq!(back.event_id, "");
    }

    #[test]
    fn emit_assigns_unique_ids_and_keeps_preset() {
        // R2-4：空 id 配号唯一；预置 id 不覆盖。
        let (tx, mut rx) = mpsc::channel::<TelemetryEvent>(10);
        let publisher = TelemetryPublisher::new(tx, Arc::new(AtomicU64::new(0)));
        let mut a = sample();
        a.event_id.clear();
        let mut b = sample();
        b.event_id.clear();
        let mut c = sample();
        c.event_id = "keep-me".to_string();
        publisher.emit(a);
        publisher.emit(b);
        publisher.emit(c);
        let ra = rx.try_recv().unwrap();
        let rb = rx.try_recv().unwrap();
        let rc = rx.try_recv().unwrap();
        assert!(!ra.event_id.is_empty());
        assert!(!rb.event_id.is_empty());
        assert_ne!(ra.event_id, rb.event_id);
        assert_eq!(rc.event_id, "keep-me");
    }

    #[test]
    fn emit_never_blocks_when_full() {
        let (tx, _rx) = mpsc::channel::<TelemetryEvent>(1);
        // R2-4：满队列丢弃计数（首个占槽，后两个丢弃）。
        let dropped = Arc::new(AtomicU64::new(0));
        let publisher = TelemetryPublisher::new(tx, dropped.clone());
        // Fill the single slot, then overflow must still return immediately.
        publisher.emit(sample());
        publisher.emit(sample());
        publisher.emit(sample());
        assert_eq!(dropped.load(Ordering::Relaxed), 2);
    }

    #[test]
    fn a2_flush_counts_queued_not_batch_len() {
        // A2：5 条中 2 条序列化失败 → 实际进 pipe 只有 3 条；
        // 落库失败只能按 queued=3 计数（原来按整批 5 累加，虚高）。
        let mut batch = vec![sample(), sample(), sample(), sample(), sample()];
        batch[1].client_ip = "ser-fail".to_string();
        batch[3].client_ip = "ser-fail".to_string();
        let mut pipe = redis::pipe();
        let queued = encode_batch(&batch, "stream:test", &mut pipe, |e| {
            if e.client_ip == "ser-fail" {
                None
            } else {
                serde_json::to_string(e).ok()
            }
        });
        let batch_len = batch.len() as u64;
        assert_eq!(queued, 3);
        assert_eq!(batch_len.saturating_sub(queued), 2);
        assert_ne!(queued, batch_len);
    }

    #[test]
    fn a2_zero_queued_counts_batch_instead_of_missing() {
        // A2：整批序列化失败（queued==0）时丢数 = 整批 batch_len，
        // 不能像原来那样直接 return 记 0（漏计）。
        let batch = vec![sample(), sample()];
        let mut pipe = redis::pipe();
        let queued = encode_batch(&batch, "stream:test", &mut pipe, |_| None);
        assert_eq!(queued, 0);
        let batch_len = batch.len() as u64;
        let ser_failed = batch_len.saturating_sub(queued);
        assert_eq!(ser_failed, batch_len);
        assert_ne!(ser_failed, 0);
    }

    /// Live Redis integration: worker batch-flushes one event into a test
    /// stream. Skips (passes) when Redis is unreachable, e.g. Docker daemon
    /// down on Windows dev boxes. Run with `cargo test -- --ignored`.
    #[tokio::test]
    #[ignore]
    async fn live_flush_writes_stream() {
        use redis::AsyncCommands;
        // OPT-R4 C6：live 测试读 REDIS_URL（带密 compose），缺省沿用无密本机。
        let url =
            std::env::var("REDIS_URL").unwrap_or_else(|_| "redis://127.0.0.1:6379/".to_string());
        let client = match redis::Client::open(url) {
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
        let worker = TelemetryWorker::new(
            rx,
            manager.clone(),
            stream_key.clone(),
            dropped,
            Arc::new(MetricsRegistry::new()),
        );
        tokio::spawn(worker.run());
        // R2-4：publisher 持独立通道计数器（live 只断言落库，不污染 flush 口径）。
        TelemetryPublisher::new(tx, Arc::new(AtomicU64::new(0))).emit(sample());
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
