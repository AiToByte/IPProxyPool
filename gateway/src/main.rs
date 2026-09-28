//! GW-4 entrypoint: enterprise gateway (self-healing + LinUCB + tenants).
//!
//! Assembly: telemetry batch worker → circuit-breaker consumer → PubSub delta
//! sync → canary prober → warm-ticket keeper → vendor SLA arbitrage →
//! Prometheus exposition (:9091) → Pingora data plane on :8916 (tenant gate +
//! LinUCB select + Chrome profile + byte metering per request).
//!
//! Redis is optional at boot (degraded: log-only telemetry). ClickHouse is
//! consumed lazily by the arbitrage worker (query errors hold weights).

mod analytics;
mod bandit;
mod ch_sink;
mod circuit_breaker;
mod fingerprint;
mod free_pool;
mod gateway;
mod geo;
mod janitor;
mod metrics;
mod model;
mod pool;
mod prober;
mod router;
mod socks_bridge;
mod socks_handshake;
mod telemetry;
mod tenant;
mod vendor_arbitrage;

use analytics::AnalyticsEngine;
use bandit::{LinUCBEngine, DEFAULT_ALPHA};
use ch_sink::ChSinkWorker;
use circuit_breaker::{PassiveCircuitBreaker, DELTA_CHANNEL};
use dashmap::DashMap;
use free_pool::{
    FreePoolConfig, FreePoolWorker, API_MAX_PAGES, DEFAULT_API_URL, DEFAULT_FULL_CHECK_BASE,
    DEFAULT_GITHUB_URL, DEFAULT_HTML_URL,
};
use gateway::{SmartProxyGateway, DEFAULT_API_KEY};
use metrics::{serve_metrics, MetricsRegistry, METRICS_ADDR};
use model::{ProxyNode, RoutingSpec};
use pingora_core::server::configuration::Opt;
use pingora_core::server::Server;
use pool::ConnectionPrewarmer;
use prober::CanaryProber;
use rand::Rng;
use redis::aio::ConnectionManager;
use router::RouterEngine;
use socks_bridge::SocksBridge;
use std::sync::{atomic::AtomicU64, Arc};
use std::time::Duration;
use std::time::Instant;
use telemetry::{TelemetryEvent, TelemetryPublisher, TelemetryWorker, TELEMETRY_CHANNEL_CAP};
use tenant::TenantManager;
use tokio::sync::Semaphore;
use tokio::task::JoinSet;
use vendor_arbitrage::VendorArbitrageWorker;

const REDIS_URL: &str = "redis://127.0.0.1:6379/";
const STREAM_KEY: &str = "stream:proxy:telemetry";
const CONSUMER_GROUP: &str = "circuit_breaker_group";
/// R2-7 三 60s ticker 启动错峰窗口（prober/sweep/arbitrage 各睡 0..5s 再进首轮，
/// 稳态节拍不变；日志以 `staggered start` 行对齐验证）。
const STARTUP_JITTER_WINDOW: Duration = Duration::from_secs(5);
const CLICKHOUSE_URL: &str = "http://127.0.0.1:8123";
const CLICKHOUSE_USER: &str = "proxy";
const CLICKHOUSE_PASSWORD: &str = "123456";
const CLICKHOUSE_DB: &str = "proxy";

/// R2-7 启动错峰：返回 0..`STARTUP_JITTER_WINDOW` 的随机延迟，后台 ticker
/// 进首轮前各睡一次（prober/sweep/arbitrage 三 60s 同相位打散；稳态节拍不变）。
fn startup_jitter() -> Duration {
    Duration::from_millis(rand::thread_rng().gen_range(0..STARTUP_JITTER_WINDOW.as_millis() as u64))
}

/// R2-8 环境覆盖读值（空串视为未设，沿用 code 默认；compose 示例同步）。
fn env_str(key: &str, default: &str) -> String {
    std::env::var(key)
        .ok()
        .filter(|v| !v.is_empty())
        .unwrap_or_else(|| default.to_string())
}

/// R2-8 间隔秒数读值（非法/0 回落默认；单位秒）。
fn env_secs(key: &str, default_secs: u64) -> Duration {
    std::env::var(key)
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
        .filter(|v| *v > 0)
        .map(Duration::from_secs)
        .unwrap_or_else(|| Duration::from_secs(default_secs))
}

/// 复审钳制：FREE_TTL 上限（`now + ttl` 在 upsert/reverify，非法大值即 panic；
/// 后台任务 panic 在 abort 下带走进程）。30 天远超合理 TTL（默认 30min），钳制无行为影响。
pub fn clamp_free_ttl(ttl: Duration) -> Duration {
    ttl.min(Duration::from_secs(30 * 86400))
}

/// FreePool 逗号分隔 URL 列表读值（去空白＋去空项＋仅 http/https，file/dict/gopher
/// 一律过滤防 SSRF；缺省走 default）。
fn split_env_list(key: &str, default: &str) -> Vec<String> {
    env_str(key, default)
        .split(',')
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .filter(|s| s.starts_with("http://") || s.starts_with("https://"))
        .collect()
}

/// A3：主探针信号量关闭时的跳过结果（对齐 pool.rs:92 口径：`acquire_owned`
/// 失败按失败计，不执行探测；调用方记 `Dead` 走既有 warn 日志，不裸奔全并发）。
fn skipped_probe_result() -> prober::ProbeResult {
    prober::ProbeResult::Dead {
        error: "probe semaphore closed, skipped".to_string(),
    }
}

// OPT-R4 S5 stop-aware supervisor: stop broadcast aborts current run without respawn.
// Boundary: in-flight ticks are not drained (abort lands on await points;
// telemetry loss stays counted via channel_dropped/flush_dropped). Data-plane
// drain remains governed by pingora grace; this only stops background revival.
async fn supervise_until<Make, Fut>(
    worker: &'static str,
    metrics: Arc<MetricsRegistry>,
    make: Make,
    mut stop: tokio::sync::watch::Receiver<bool>,
) where
    Make: Fn() -> Fut,
    Fut: std::future::Future<Output = ()> + Send + 'static,
{
    let mut backoff = Duration::from_secs(1);
    loop {
        // S5: stop broadcasts are honored at iteration boundaries and during
        // backoff sleeps. In-flight runs are NOT aborted (bounded by worker
        // tick timeouts); respawns stop, so shutdown windows see no churn.
        if *stop.borrow() {
            log::warn!("[Supervisor] {worker} stop set, exiting loop");
            return;
        }
        let started = Instant::now();
        match tokio::spawn(make()).await {
            Ok(()) => log::error!("[Supervisor] {worker} exited unexpectedly, restarting"),
            Err(e) => log::error!("[Supervisor] {worker} panicked ({e:?}), restarting"),
        }
        metrics.note_supervisor_restart(worker);
        if started.elapsed() > Duration::from_secs(60) {
            backoff = Duration::from_secs(1);
        }
        tokio::select! {
            biased;
            _ = stop.wait_for(|s| *s) => {
                log::warn!("[Supervisor] {worker} stop during backoff, exiting loop");
                return;
            }
            _ = tokio::time::sleep(backoff) => {}
        }
        backoff = (backoff * 2).min(Duration::from_secs(60));
    }
}

/// OPT-R4 S1/S7 同步入口.
/// 后台 workers 跑在本 runtime 上（binding 活到进程结束，run_forever 永不返回）。
fn main() {
    // S7：panic 首行落盘（stderr 进 log/gw.err）＋缺省开 backtrace。
    std::panic::set_hook(Box::new(|info| {
        eprintln!("[panic] {info}");
    }));
    if std::env::var("RUST_BACKTRACE").is_err() {
        std::env::set_var("RUST_BACKTRACE", "1");
    }
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("tokio runtime");
    let server = runtime.block_on(setup_gateway());
    // runtime 不 drop（workers 在上面跑；run_forever 发散，drop 走不到）。
    let _keep_alive = runtime;
    server.run_forever();
}

/// 原 async main 本体：装配全部依赖＋后台 workers，返回配好的 Server。
async fn setup_gateway() -> Server {
    // Must run before any rustls use (ring + aws-lc-rs both compiled in).
    let _ = rustls::crypto::ring::default_provider().install_default();

    env_logger::init_from_env(env_logger::Env::default().default_filter_or("info"));

    // 1. Redis connection (degraded mode when unavailable, e.g. no Docker).
    // R2-8：`REDIS_URL` env 化（缺省沿用 code 常量）。
    let redis_client =
        redis::Client::open(env_str("REDIS_URL", REDIS_URL)).expect("Invalid Redis URL");
    let redis_conn: Option<ConnectionManager> = match tokio::time::timeout(
        Duration::from_secs(3),
        ConnectionManager::new(redis_client.clone()),
    )
    .await
    {
        Ok(Ok(manager)) => {
            log::info!("[GW-2] Redis connected, self-healing workers online");
            Some(manager)
        }
        Ok(Err(e)) => {
            log::warn!(
                "[GW-2] Redis unreachable ({e:?}), running degraded without telemetry/CB workers"
            );
            None
        }
        Err(_) => {
            log::warn!(
                "[GW-2] Redis connect timed out, running degraded without telemetry/CB workers"
            );
            None
        }
    };

    // 2a. OPT-2 API Key 环境门：D3 起默认开启（`REQUIRE_API_KEY=0` 显式关闭）。
    // 开启后，未带 `X-API-Key` 头的请求在网关入口直接 403（见 gateway.rs）；
    // 本机开发用默认 Key `default_key`（启动即注册宽限额，见 OPERATION/USAGE）。
    let require_api_key = std::env::var("REQUIRE_API_KEY").as_deref() != Ok("0");
    if require_api_key {
        log::info!(
            "[OPT-2] API key gate ON (default since D3), missing X-API-Key requests get 403"
        );
    }

    // 2. Initial mock egress pool (GW-4 arbitrage derates/restores via reload).
    // Third vendor (mock-c, GB/mobile) joins so the arbitrage has 3 to poll.
    let initial_nodes = vec![
        ProxyNode::new(
            "127.0.0.1".to_string(),
            8888,
            None,
            None,
            "US".to_string(),
            "residential".to_string(),
            "mock-a".to_string(),
            100,
        ),
        ProxyNode::new(
            "127.0.0.1".to_string(),
            8889,
            None,
            None,
            "JP".to_string(),
            "datacenter".to_string(),
            "mock-b".to_string(),
            80,
        ),
        ProxyNode::new(
            "127.0.0.1".to_string(),
            8890,
            None,
            None,
            "GB".to_string(),
            "mobile".to_string(),
            "mock-c".to_string(),
            60,
        ),
    ];
    let router = Arc::new(RouterEngine::new(initial_nodes));

    // 2b. GW-4 tenants: default key with wide limits keeps GW-1~3 curls green.
    // R2-3 burst=qps/10（单源口径，见 `TenantManager::default_burst_for_qps`）。
    let tenant_mgr = Arc::new(TenantManager::new());
    tenant_mgr.register_tenant(
        "default",
        DEFAULT_API_KEY,
        10_000,
        10_000,
        TenantManager::default_burst_for_qps(10_000),
    );

    // 2c. GW-4 Prometheus registry + exposition endpoint.
    // OPT-4：落库丢弃计数由 worker 与 metrics 共享（worker 直写、metrics 只读渲染）。
    // R2-4：再加通道丢弃计数（publisher 直写）；两行各自渲染、口径分离。
    // R2-8：`METRICS_ADDR` env 化。
    // R3-4：GeoIP 库状态启动即打（移出 FREE 块：不开 FREE 也可见，避免静默 misconfig）。
    let telemetry_dropped = Arc::new(AtomicU64::new(0));
    let channel_dropped = Arc::new(AtomicU64::new(0));
    let metrics = Arc::new(MetricsRegistry::new_with_dropped(
        telemetry_dropped.clone(),
        channel_dropped.clone(),
    ));
    // OPT-R4 S5 shutdown broadcast (signal task holds the only Sender).
    let (shutdown_tx, _shutdown_rx0) = tokio::sync::watch::channel(false);
    // OPT-R4 S3/S5: metrics serve under supervise_until.
    let serve_metrics_clone = metrics.clone();
    let serve_addr = env_str("METRICS_ADDR", METRICS_ADDR);
    tokio::spawn(supervise_until(
        "metrics",
        metrics.clone(),
        move || {
            let m = serve_metrics_clone.clone();
            let addr = serve_addr.clone();
            async move {
                serve_metrics(m, &addr).await;
            }
        },
        shutdown_tx.clone().subscribe(),
    ));

    // 3. Zero-blocking telemetry pipe (capacity 10,000).
    // R2-4 降级零构造：无 Redis 时 publisher 不创建，网关 `logging` 走 None 分支，
    // 每请求省掉 Event 的 6+ String 构造；metrics 双计数器常驻渲染不受影响。
    let (tx, rx) = tokio::sync::mpsc::channel::<TelemetryEvent>(TELEMETRY_CHANNEL_CAP);
    let mut rx_opt = Some(rx);
    let telemetry: Option<Arc<TelemetryPublisher>> = redis_conn
        .as_ref()
        .map(|_| Arc::new(TelemetryPublisher::new(tx, channel_dropped.clone())));

    // 3b. GW-3 LinUCB engine + arm table + warm-ticket keeper.
    // R2-8：预热间隔 env 化（`PREWARM_INTERVAL_SECS`，默认 30s）。
    let bandit_engine = Arc::new(LinUCBEngine::new(DEFAULT_ALPHA));
    let bandit_arms = Arc::new(DashMap::new());
    // OPT-R6 V1：prewarmer 此前是裸 spawn——panic 会让预热循环静默死亡，
    // 节点首包延迟回升（无即时指标暴露）。包进 supervise。
    // `ConnectionPrewarmer::run(self)` 消耗 self，故每次重启需重建实例——
    // router 是 `Arc` clone 零成本，预热器本身无状态，重建无副作用。
    let prewarm_router = router.clone();
    let prewarm_interval = env_secs("PREWARM_INTERVAL_SECS", 30);
    let prewarm_metrics = metrics.clone();
    tokio::spawn(supervise_until(
        "prewarmer",
        prewarm_metrics,
        move || {
            let prewarmer = ConnectionPrewarmer::new(Arc::clone(&prewarm_router))
                .with_interval(prewarm_interval);
            prewarmer.run()
        },
        shutdown_tx.subscribe(),
    ));

    // 3c. P3 exit-IP 画像库（R3-4：装配＋状态日志移出 FREE 块，不开 FREE 也可见库状态，
    // 避免静默 misconfig；`with_geo` 仍只在 FREE 分支内 attach，行为不变）。
    // `GEOIP_MMDB_PATH` 空/不可读→Disabled 降级，只观察不执法。
    let geo_db = Arc::new(geo::GeoDb::open(env_str("GEOIP_MMDB_PATH", "").as_str()));
    if !geo_db.enabled() {
        log::info!(
            "[GeoIP] disabled ({}), free exit-country checks observe-skip",
            geo_db.reason()
        );
    }

    // 4e. FreePool 第二供应线（默认关闭；FREE_ENABLED=1 开启；全 env 见计划 §2）。
    if env_str("FREE_ENABLED", "0") == "1" {
        let mut full_base = env_str("FREE_FULL_CHECK_URL", DEFAULT_FULL_CHECK_BASE);
        if !full_base.starts_with("https://") {
            log::warn!("[FreePool] FREE_FULL_CHECK_URL must be https, falling back to default");
            full_base = DEFAULT_FULL_CHECK_BASE.to_string();
        }
        let free_geo = geo_db.clone();
        let free_config = FreePoolConfig {
            api_urls: split_env_list("FREE_API_URLS", DEFAULT_API_URL),
            // NEXT-B2：API 分页数（默认 1＝单页礼貌轮询；生产开 3 即 ~300 raw）。
            api_pages: env_str("FREE_API_PAGES", "1")
                .parse::<usize>()
                .unwrap_or(1)
                .clamp(1, API_MAX_PAGES),
            html_urls: split_env_list("FREE_HTML_URLS", DEFAULT_HTML_URL),
            github_urls: split_env_list("FREE_GITHUB_URLS", DEFAULT_GITHUB_URL),
            fetch_interval: env_secs("FREE_FETCH_INTERVAL_SECS", 600),
            ttl: clamp_free_ttl(env_secs("FREE_TTL_SECS", 1800)),
            verify_timeout: env_secs("FREE_VERIFY_TIMEOUT_SECS", 3),
            // OPT-R4 B3：单源抓取超时＋intake 上限因子（干旱/洪峰运维可调）。
            fetch_timeout: env_secs("FREE_FETCH_TIMEOUT_SECS", 15),
            intake_factor: env_str("FREE_INTAKE_FACTOR", "2")
                .parse::<usize>()
                .unwrap_or(2)
                .max(1),
            max_latency_ms: env_str("FREE_MAX_LATENCY_MS", "3000")
                .parse::<u64>()
                .unwrap_or(3000),
            max_concurrent: env_str("FREE_MAX_CONCURRENT", "50")
                .parse::<usize>()
                .unwrap_or(50),
            full_concurrent: env_str("FREE_FULL_CONCURRENT", "20")
                .parse::<usize>()
                .unwrap_or(20),
            max_nodes: env_str("FREE_MAX_NODES", "2000")
                .parse::<usize>()
                .unwrap_or(2000),
            full_check_base: full_base,
            // NEXT-B4：备用复检基址列表（逗号分隔；仅 https 入选，默认空＝单基址）。
            full_check_bases: split_env_list("FREE_FULL_CHECK_URLS", "")
                .into_iter()
                .filter(|u| u.starts_with("https://"))
                .collect(),
            require_elite: env_str("FREE_REQUIRE_ELITE", "0") == "1",
            max_zero_cycles: env_str("FREE_SOURCE_MAX_ZERO_CYCLES", "3")
                .parse::<u32>()
                .unwrap_or(3),
            suspend_retry_every: env_str("FREE_SUSPEND_RETRY_EVERY", "3")
                .parse::<u64>()
                .unwrap_or(3),
            geo_enforce: env_str("GEOIP_ENFORCE_MISMATCH", "0") == "1",
        };
        let free_router = router.clone();
        let free_metrics = metrics.clone();
        let free_stop = shutdown_tx.subscribe();
        tokio::spawn(async move {
            tokio::time::sleep(startup_jitter()).await;
            log::info!("[FreePool] staggered start (second supply line)");
            supervise_until(
                "free_pool",
                free_metrics.clone(),
                move || {
                    let w = FreePoolWorker::new(
                        free_router.clone(),
                        free_metrics.clone(),
                        free_config.clone(),
                    )
                    .with_geo(free_geo.clone());
                    // VPN-IMMUNE：共享 Client 禁系统代理（`free_pool::shared_client`）。
                    async move { w.run(free_pool::shared_client()).await }
                },
                free_stop,
            )
            .await;
        });
    }

    if let Some(ref conn) = redis_conn {
        // Telemetry batch worker → Redis Stream（OPT-4：传入共享丢弃计数器）。
        let tele_worker = TelemetryWorker::new(
            rx_opt.take().expect("telemetry rx held for worker"),
            conn.clone(),
            STREAM_KEY.to_string(),
            telemetry_dropped.clone(),
        );
        // OPT-R6 V1：telemetry 此前是裸 spawn。**有意不复用 `supervise_until`**：
        // `TelemetryWorker` 持有 `mpsc::Receiver`，它既非 `Clone` 又**一次性**——
        // 崩溃后流已被消费/关闭，重建 worker 拿不到第二个 Receiver，重启无语义。
        // 若强行套用 `supervise_until` 只会得到一个「记一次重启计数后反复
        // 拿不到数据」的空转循环，反而更坏。
        //
        // 因此本处采用**有尽监督**：崩溃/退出时记一次
        // `supervisor_restarts_total{worker="telemetry"}`（可观测、不静默），
        // 然后就此停止，不假装能自愈。遥测丢弃另有
        // `telemetry_dropped_total`/`telemetry_channel_dropped_total` 常驻计数，
        // 数据丢失本身始终可观测。
        //
        // `run(self)` 返回 `()`（内部死循环），因此「正常退出」等价于「异常退出」；
        // 真正的崩溃由 JoinHandle 捕获。
        let tele_metrics = metrics.clone();
        let tele_handle = tokio::spawn(tele_worker.run());
        tokio::spawn(async move {
            match tele_handle.await {
                Ok(()) => log::error!(
                    "[Telemetry] worker exited unexpectedly, not restarting (one-shot receiver)"
                ),
                Err(e) => log::error!(
                    "[Telemetry] worker panicked ({e:?}), not restarting (one-shot receiver)"
                ),
            }
            tele_metrics.note_supervisor_restart("telemetry");
        });

        // Passive circuit breaker consumer (R2-8: supervisor 包重启计数 + backoff)。
        let cb_conn = conn.clone();
        let cb_router = router.clone();
        let cb_metrics = metrics.clone();
        tokio::spawn(supervise_until(
            "circuit_breaker",
            cb_metrics,
            move || {
                PassiveCircuitBreaker::new(
                    cb_conn.clone(),
                    STREAM_KEY.to_string(),
                    CONSUMER_GROUP.to_string(),
                    "worker_01".to_string(),
                    cb_router.clone(),
                )
                .run()
            },
            shutdown_tx.clone().subscribe(),
        ));

        // PubSub delta sync → in-memory quarantine.
        // NEXT-A0：连接/订阅失败/流终止不再永久失联——包进 supervise，
        // 退出即 backoff 重连＋`supervisor_restarts_total{worker="pubsub_delta"}` 计数。
        let router_clone = router.clone();
        let sub_client = redis_client.clone();
        let sync_metrics = metrics.clone();
        tokio::spawn(supervise_until(
            "pubsub_delta",
            sync_metrics,
            move || {
                let router_clone = router_clone.clone();
                let sub_client = sub_client.clone();
                async move {
                    let mut pubsub = match sub_client.get_async_pubsub().await {
                        Ok(p) => p,
                        Err(e) => {
                            log::error!("[Sync] PubSub connect failed: {e:?}");
                            return;
                        }
                    };
                    if let Err(e) = pubsub.subscribe(DELTA_CHANNEL).await {
                        log::error!("[Sync] PubSub subscribe failed: {e:?}");
                        return;
                    }
                    let mut stream = pubsub.on_message();
                    use futures::StreamExt;
                    while let Some(msg) = stream.next().await {
                        let payload: String = msg.get_payload().unwrap_or_default();
                        if crate::circuit_breaker::apply_delta(&router_clone, &payload) {
                            log::info!("[Sync] Applied delta quarantine: {payload}");
                        }
                    }
                    log::warn!("[Sync] delta stream ended, restarting");
                }
            },
            shutdown_tx.clone().subscribe(),
        ));
    } else {
        // R2-4：降级收发两端都丢弃（tx 随 publisher 闭包释放、rx 在此释放），
        // 通道彻底关闭，无残留缓冲。
        drop(rx_opt);
    }

    // 4. Periodic canary prober over the current healthy pool.
    // R2-7：JoinSet + 信号量 20 并发探测（百节点串行堵死 ticker 已成历史；
    // 单节点 2.5s 超时 × 串行曾可达数分钟）+ 每轮淘汰闲置 Client + 启动错峰。
    // R2-8：探测间隔 env 化（`PROBE_INTERVAL_SECS`，默认 60s）。
    let probe_router = router.clone();
    let probe_interval = env_secs("PROBE_INTERVAL_SECS", 60);
    // OPT-R6 V1：prober 此前是裸 spawn——panic（如节点字段意外导致 unwrap）会让
    // 整个探测循环静默死亡且零指标，池健康度从此不可知。包进 supervise 后
    // panic 退避重启并记 `supervisor_restarts_total{worker="prober"}`。
    // Client 缓存与信号量**下沉进闭包内**：supervise 的 `make` 必须可重复调用，
    // 若留在闭包外，重启会复用同一批 Client（Client 内部连接池可能已随断连
    // 不可用，重启语义失真）。
    let probe_metrics = metrics.clone();
    // 订阅在 `async move` 块**外**取得：块内若用 `shutdown_tx.clone().subscribe()`，
    // 闭包会连带捕获 `shutdown_tx` 本身，与后续 sweep/arbitrage 处的借用冲突
    // （`shutdown_tx` 须保留到末尾 signal watcher 独占）。
    let probe_stop_rx = shutdown_tx.subscribe();
    tokio::spawn(async move {
        tokio::time::sleep(startup_jitter()).await;
        log::info!("[Prober] staggered start (R2-7 jitter)");
        supervise_until(
            "prober",
            probe_metrics,
            move || {
                let probe_router = probe_router.clone();
                async move {
                    let prober = Arc::new(CanaryProber::new());
                    let semaphore = Arc::new(Semaphore::new(prober::PROBE_MAX_CONCURRENT));
                    let mut ticker = tokio::time::interval(probe_interval);
                    loop {
                        ticker.tick().await;
                        // 闲置 Client 随 60s 滴答淘汰（R2-7；账密 rotation 后明文 key 不常驻）。
                        let evicted = prober.evict_idle_clients();
                        if evicted > 0 {
                            log::info!("[Prober] evicted {evicted} idle clients");
                        }
                        // NEXT-A3：三路并取（默认 http＋显式 socks5/socks4，沿 pool.rs 预热口径；
                        // 默认隔离下 socks 对 default-spec 不可见，不并取即漏探 socks 节点存活）。
                        let mut candidates =
                            probe_router.get_healthy_candidates(&RoutingSpec::default());
                        for proto in [
                            crate::model::EgressProto::Socks5,
                            crate::model::EgressProto::Socks4,
                        ] {
                            let spec = RoutingSpec {
                                proto: Some(proto),
                                ..Default::default()
                            };
                            candidates.extend(probe_router.get_healthy_candidates(&spec));
                        }
                        // OPT-5：复用 Client 池大小随滴答打 debug（稳定即无建链抖动）。
                        log::debug!(
                            "[Prober] tick start nodes={} clients={}",
                            candidates.len(),
                            prober.client_count()
                        );
                        let mut set = JoinSet::new();
                        for node in candidates {
                            let prober = prober.clone();
                            let sem = semaphore.clone();
                            set.spawn(async move {
                                // A3：对齐 pool.rs:92 ——信号量关闭（acquire Err）直接跳过本次探测，
                                // 记 Dead（既有 warn 口径），不执行 probe_node（关闭后不再全并发裸奔）。
                                let _permit = match sem.acquire_owned().await {
                                    Ok(p) => p,
                                    Err(_) => {
                                        log::debug!(
                                            "[Prober] semaphore closed, skipped probe for {}:{}",
                                            node.ip,
                                            node.port
                                        );
                                        return (node, skipped_probe_result());
                                    }
                                };
                                let result = prober.probe_node(&node).await;
                                (node, result)
                            });
                        }
                        while let Some(joined) = set.join_next().await {
                            match joined {
                                Ok((
                                    node,
                                    prober::ProbeResult::Healthy {
                                        latency_ms,
                                        exit_ip,
                                    },
                                )) => {
                                    log::info!(
                                        "[Prober] {}:{} healthy via {exit_ip} ({latency_ms}ms)",
                                        node.ip,
                                        node.port
                                    );
                                }
                                Ok((node, prober::ProbeResult::Degraded { reason })) => {
                                    log::warn!(
                                        "[Prober] {}:{} degraded: {reason}",
                                        node.ip,
                                        node.port
                                    );
                                }
                                Ok((node, prober::ProbeResult::Dead { error })) => {
                                    log::warn!("[Prober] {}:{} dead: {error}", node.ip, node.port);
                                }
                                Err(e) => {
                                    log::warn!("[Prober] probe task join failed: {e:?}");
                                }
                            }
                        }
                    }
                }
            },
            probe_stop_rx,
        )
        .await;
    });

    // 4b. GW-4 vendor SLA arbitrage (lazy CH: query errors hold weights).
    // R2-8：CH 连接 + 审计间隔 env 化；supervisor 包重启计数 + backoff。
    let analytics = Arc::new(AnalyticsEngine::new(
        &env_str("CLICKHOUSE_URL", CLICKHOUSE_URL),
        &env_str("CLICKHOUSE_USER", CLICKHOUSE_USER),
        &env_str("CLICKHOUSE_PASSWORD", CLICKHOUSE_PASSWORD),
        &env_str("CLICKHOUSE_DB", CLICKHOUSE_DB),
    ));
    match analytics.ping().await {
        Ok(()) => log::info!("[GW-4] ClickHouse online, arbitrage audits live"),
        Err(e) => log::warn!("[GW-4] ClickHouse unreachable ({e:?}), arbitrage holds weights"),
    }
    let arb_interval = env_secs("ARBITRAGE_INTERVAL_SECS", 60);
    let arb_analytics = analytics.clone();
    let arb_router = router.clone();
    let arb_metrics = metrics.clone();
    let arb_stop = shutdown_tx.subscribe();
    tokio::spawn(async move {
        tokio::time::sleep(startup_jitter()).await;
        log::info!("[Arbitrage] staggered start (R2-7 jitter)");
        supervise_until(
            "arbitrage",
            arb_metrics,
            move || {
                VendorArbitrageWorker::new(
                    arb_analytics.clone(),
                    arb_router.clone(),
                    vec![
                        "mock-a".to_string(),
                        "mock-b".to_string(),
                        "mock-c".to_string(),
                    ],
                    vec!["US".to_string(), "JP".to_string(), "GB".to_string()],
                )
                .with_interval(arb_interval)
                .run()
            },
            arb_stop,
        )
        .await;
    });

    // 4c. GW-R2 sink pump: Stream → ClickHouse (needs Redis; CH errors hold).
    // R2-8：supervisor 包重启计数 + backoff。
    if let Some(ref conn) = redis_conn {
        let pump_conn = conn.clone();
        let pump_analytics = analytics.clone();
        let pump_metrics = metrics.clone();
        tokio::spawn(supervise_until(
            "ch_sink",
            pump_metrics,
            move || {
                ChSinkWorker::new(
                    pump_conn.clone(),
                    pump_analytics.clone(),
                    STREAM_KEY.to_string(),
                )
                .run()
            },
            shutdown_tx.clone().subscribe(),
        ));
    } else {
        log::warn!("[GW-R2] sink pump offline (no Redis), warehouse landings paused");
    }

    // 4d. OPT-1 职守清理：每 60s 清过期会话/隔离 + 修剪游离臂，防长稳内存泄漏。
    // R2-7：启动错峰（与 prober/arbitrage 三 60s ticker 打散，不再对齐惊群）。
    // R2-8：清理间隔 env 化（`SWEEP_INTERVAL_SECS`，默认 60s）。
    // P2-7：同节拍淘汰 bridge 闲置 Client（socks 节点下线/账密 rotation 后 key 不常驻）。
    let sweep_router = router.clone();
    let sweep_arms = bandit_arms.clone();
    let sweep_metrics = metrics.clone();
    let sweep_interval = env_secs("SWEEP_INTERVAL_SECS", 60);
    // P2 SOCKS 翻译桥（显式 socks 请求出站执行器；env 见计划 §2）。
    let socks_bridge = Arc::new(SocksBridge::new(
        env_secs("SOCKS_BRIDGE_TIMEOUT_SECS", 20),
        env_str("SOCKS_MAX_BODY_BYTES", "10485760")
            .parse::<u64>()
            .unwrap_or(10 * 1024 * 1024),
    ));
    let sweep_bridge = socks_bridge.clone();
    // OPT-R6 V1：sweep 此前是裸 spawn——这正是 S2（P0）下溢缺陷的放大器：
    // debug 下 `before - len()` 的 panic 会静默杀死整个淘汰循环（JoinHandle 被
    // 丢弃、无日志、无指标），此后会话表/隔离表**无界增长直至 OOM**。
    // S2 已用饱和减法消除 panic 根因，V1 再补监督兜底——两层防护：
    // 即使将来引入新的 panic 源，淘汰循环也会退避重启而非永久死亡。
    // supervise 的 `make` 是 `Fn`（非 `FnOnce`），故两份 `Arc` 各克隆一次：
    // 一份交给 supervisor 记指标，一份被闭包捕获供每轮重建使用。
    let sweep_supervisor_metrics = sweep_metrics.clone();
    // 订阅在闭包外取得：`shutdown_tx` 本身要留给末尾的 signal watcher 独占，
    // 不能被本闭包捕获（prober 用的是 `clone().subscribe()` 的同口径写法）。
    let sweep_stop_rx = shutdown_tx.subscribe();
    tokio::spawn(async move {
        tokio::time::sleep(startup_jitter()).await;
        log::info!("[Sweep] staggered start (R2-7 jitter)");
        supervise_until(
            "sweep",
            sweep_supervisor_metrics,
            move || {
                let sweep_router = sweep_router.clone();
                let sweep_arms = sweep_arms.clone();
                let sweep_bridge = sweep_bridge.clone();
                let sweep_metrics = sweep_metrics.clone();
                async move {
                    let mut ticker = tokio::time::interval(sweep_interval);
                    loop {
                        ticker.tick().await;
                        let (sessions, quarantines) = sweep_router.sweep_expired();
                        // NEXT-A4：隔离水位同步（过期清理后读 len，gauge 语义）。
                        sweep_metrics
                            .set_quarantine_nodes(sweep_router.quarantine_len() as u64);
                        let arms = gateway::prune_stale_arms(&sweep_arms, &sweep_router);
                        let bridged = sweep_bridge.evict_idle();
                        if sessions + quarantines + arms + bridged > 0 {
                            log::info!(
                                "[Sweep] cleared sessions={sessions} quarantines={quarantines} arms={arms} bridge={bridged}"
                            );
                        }
                    }
                }
            },
            sweep_stop_rx,
        )
        .await;
    });

    // OPT-R4 S5: signal watcher holds the only shutdown Sender alive.
    // ctrl_c everywhere; SIGTERM additionally on unix. Firing stops
    // background respawns (data-plane drain stays governed by pingora grace).
    let signal_tx = shutdown_tx;
    tokio::spawn(async move {
        #[cfg(unix)]
        {
            let mut term =
                tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
                    .expect("SIGTERM handler");
            tokio::select! {
                _ = tokio::signal::ctrl_c() => {}
                _ = term.recv() => {}
            }
        }
        #[cfg(not(unix))]
        {
            let _ = tokio::signal::ctrl_c().await;
        }
        log::warn!("[Shutdown] signal received, stopping background respawns");
        let _ = signal_tx.send(true);
    });
    // 5. Pingora data plane.
    let opt = Opt::parse_args();
    let mut conf =
        pingora_core::server::configuration::ServerConf::new().expect("ServerConf defaults");
    conf.grace_period_seconds = Some(
        env_str("GATEWAY_GRACE_SECS", "300")
            .parse::<u64>()
            .unwrap_or(300),
    );
    let mut server = Server::new_with_opt_and_conf(Some(opt), conf);
    server.bootstrap();

    let mut proxy_service = pingora_proxy::http_proxy_service(
        &server.configuration,
        SmartProxyGateway {
            router: router.clone(),
            telemetry,
            bandit_engine: bandit_engine.clone(),
            bandit_arms: bandit_arms.clone(),
            tenant_mgr: tenant_mgr.clone(),
            metrics: metrics.clone(),
            require_api_key,
            bridge: Some(socks_bridge.clone()),
        },
    );
    // R2-8：网关监听地址 env 化（D3 起默认 127.0.0.1:8916 回环收紧；
    // 2026-09-24 由 8080 迁出，起因见 PORT-8916 计划；局域网/容器场景显式覆写
    // GATEWAY_ADDR=0.0.0.0:8916，见 OPERATION D2 节）。
    let gateway_addr = env_str("GATEWAY_ADDR", "127.0.0.1:8916");
    proxy_service.add_tcp(&gateway_addr);

    log::info!(
        "Pingora Smart Proxy Gateway (GW-4 tenants + arbitrage + metrics) on {gateway_addr}"
    );
    server.add_service(proxy_service);
    server
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn env_str_defaults_and_overrides() {
        // R2-8：缺省回落 code 默认；空串视为未设；显式值生效。
        assert_eq!(env_str("R2T_MISSING_KEY_XYZ", "dflt"), "dflt");
        std::env::set_var("R2T_STR_KEY", "v1");
        assert_eq!(env_str("R2T_STR_KEY", "dflt"), "v1");
        std::env::set_var("R2T_STR_KEY", "");
        assert_eq!(env_str("R2T_STR_KEY", "dflt"), "dflt");
        std::env::remove_var("R2T_STR_KEY");
    }

    #[test]
    fn env_secs_parses_and_falls_back() {
        // R2-8：合法秒数生效；缺失/非法/0 回落默认。
        assert_eq!(
            env_secs("R2T_MISSING_SECS_XYZ", 60),
            Duration::from_secs(60)
        );
        std::env::set_var("R2T_SECS_KEY", "30");
        assert_eq!(env_secs("R2T_SECS_KEY", 60), Duration::from_secs(30));
        std::env::set_var("R2T_SECS_KEY", "abc");
        assert_eq!(env_secs("R2T_SECS_KEY", 60), Duration::from_secs(60));
        std::env::set_var("R2T_SECS_KEY", "0");
        assert_eq!(env_secs("R2T_SECS_KEY", 60), Duration::from_secs(60));
        std::env::remove_var("R2T_SECS_KEY");
    }

    #[test]
    fn free_env_defaults() {
        // 免费线总开关默认关闭；显式开生效（key 唯一防并行污染，用后清理）。
        assert_eq!(env_str("R2T_FREE_ENABLED_XYZ", "0"), "0");
        std::env::set_var("R2T_FREE_FLAG", "1");
        assert_eq!(env_str("R2T_FREE_FLAG", "0"), "1");
        std::env::remove_var("R2T_FREE_FLAG");
    }

    #[test]
    fn clamp_free_ttl_bounds() {
        // 复审 FLAG：FREE_TTL 非法大值不得传导到 `now + ttl`（upsert/reverify 会 panic）。
        // 常规值原样过；u64::MAX 级钳到 30 天（远超合理 TTL，默认 30min）。
        assert_eq!(
            clamp_free_ttl(Duration::from_secs(1800)),
            Duration::from_secs(1800)
        );
        assert_eq!(
            clamp_free_ttl(Duration::from_secs(u64::MAX)),
            Duration::from_secs(30 * 86400)
        );
    }

    #[test]
    fn split_env_list_filters_non_http() {
        // SSRF 护栏：仅 http/https 源保留，file/dict/gopher 一律过滤。
        std::env::set_var(
            "R2T_FREE_URLS",
            "https://a.example/list, file:///etc/passwd ,dict://b:80/x,, http://c.example/p",
        );
        let urls = split_env_list("R2T_FREE_URLS", "https://dflt.example/");
        assert_eq!(urls, vec!["https://a.example/list", "http://c.example/p"]);
        std::env::remove_var("R2T_FREE_URLS");
        // 缺省值同样经过滤（默认皆 https，不受影响）。
        assert_eq!(
            split_env_list("R2T_FREE_URLS_MISSING_XYZ", "https://dflt.example/"),
            vec!["https://dflt.example/"]
        );
    }

    #[test]
    fn probe_skipped_result_is_dead_without_probing() {
        // A3：信号量关闭时的跳过结果必须为 Dead（含可辨文案），构造过程不触网络。
        match skipped_probe_result() {
            prober::ProbeResult::Dead { error } => {
                assert!(
                    error.contains("semaphore closed"),
                    "unexpected error: {error}"
                );
            }
            other => panic!("skipped probe must be Dead, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn probe_semaphore_closed_acquire_fails() {
        // A3 走查锚点：Semaphore::close 后 acquire_owned 必 Err（守卫的触发条件真实
        // 存在；原 `let _permit = sem.acquire_owned().await;` 把该 Err 吞掉，关闭后
        // 仍全并发执行 probe_node，与 pool.rs:92 口径不一致）。
        let sem = Arc::new(Semaphore::new(1));
        sem.close();
        assert!(sem.acquire_owned().await.is_err());
    }

    #[tokio::test]
    async fn supervise_until_immediate_stop_no_restart() {
        // OPT-R4 S5：预置 stop 即返，不起 worker、不计数（60s 上限防挂）。
        use crate::metrics::MetricsRegistry;
        let metrics = Arc::new(MetricsRegistry::new());
        let (tx, rx) = tokio::sync::watch::channel(false);
        tx.send(true).expect("preset stop");
        let runs = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let runs2 = runs.clone();
        tokio::time::timeout(
            Duration::from_secs(60),
            supervise_until(
                "t-stop",
                metrics,
                move || {
                    let runs2 = runs2.clone();
                    async move {
                        runs2.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                    }
                },
                rx,
            ),
        )
        .await
        .expect("stop must return promptly");
        assert_eq!(runs.load(std::sync::atomic::Ordering::Relaxed), 0);
    }

    #[tokio::test]
    async fn supervise_until_restart_then_stop() {
        // OPT-R4 S3/S5：worker 退出一次记一次重启；随后 stop 即停不再拉。
        use crate::metrics::MetricsRegistry;
        use std::sync::atomic::{AtomicUsize, Ordering};
        let metrics = Arc::new(MetricsRegistry::new());
        let (tx, rx) = tokio::sync::watch::channel(false);
        let runs = Arc::new(AtomicUsize::new(0));
        let runs2 = runs.clone();
        let h = tokio::spawn(supervise_until(
            "t-restart",
            metrics.clone(),
            move || {
                let runs2 = runs2.clone();
                async move {
                    runs2.fetch_add(1, Ordering::Relaxed);
                }
            },
            rx,
        ));
        // 等一次退出重启发生（worker 空转即返，backoff 首轮 1s）。
        let deadline = Instant::now() + Duration::from_secs(20);
        while runs.load(Ordering::Relaxed) == 0 && Instant::now() < deadline {
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        assert!(runs.load(Ordering::Relaxed) >= 1, "worker must run first");
        tx.send(true).expect("stop");
        tokio::time::timeout(Duration::from_secs(60), h)
            .await
            .expect("stop must join promptly")
            .expect("supervise task panicked");
        let n = runs.load(Ordering::Relaxed);
        // 停后不再拉：静置 1.5s（>首轮 backoff 1s）计数冻结。
        tokio::time::sleep(Duration::from_millis(1500)).await;
        assert_eq!(runs.load(Ordering::Relaxed), n, "no respawn after stop");
    }

    // ---- OPT-R6 V1：supervisor 覆盖补齐（P1）回归锁定 ----

    /// V1 核心语义：包进 supervise 的后台循环**崩溃后会被重启**。
    ///
    /// 本项覆盖 V1 新接入的三条循环（prewarmer/prober/sweep）所依赖的机制：
    /// worker panic → JoinHandle 返回 `Err` → supervisor 退避重启 → 指标计数。
    /// 修复前这三条是裸 spawn，panic 等于该功能永久静默死亡。
    #[tokio::test]
    async fn opt_r6_v1_supervisor_restarts_panicking_worker() {
        use crate::metrics::MetricsRegistry;
        use std::sync::atomic::{AtomicUsize, Ordering};

        let metrics = Arc::new(MetricsRegistry::new());
        let (tx, rx) = tokio::sync::watch::channel(false);
        let runs = Arc::new(AtomicUsize::new(0));
        let runs2 = runs.clone();
        let h = tokio::spawn(supervise_until(
            "opt-r6-v1-prober",
            metrics.clone(),
            move || {
                let runs2 = runs2.clone();
                async move {
                    runs2.fetch_add(1, Ordering::Relaxed);
                    // 模拟 V1 关心的场景：worker 内部 panic（旧实现下这条裸 spawn
                    // 会随 panic 静默死亡）。
                    panic!("simulated worker panic");
                }
            },
            rx,
        ));

        // 等两次重启发生（首轮 + 至少一次 backoff 后重启）。
        let deadline = Instant::now() + Duration::from_secs(20);
        while runs.load(Ordering::Relaxed) < 2 && Instant::now() < deadline {
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        let n = runs.load(Ordering::Relaxed);
        assert!(
            n >= 2,
            "panic 后必须被 supervisor 重启（实际重启到第 {n} 次）"
        );

        tx.send(true).expect("stop");
        tokio::time::timeout(Duration::from_secs(60), h)
            .await
            .expect("stop must join promptly")
            .expect("supervise task itself must not panic");
    }

    /// V1 telemetry 的**有尽监督**语义：`Receiver` 一次性，崩溃后不重启，
    /// 但必须**记一次指标**（否则就是静默死亡——正是 V1 要消灭的行为）。
    #[tokio::test]
    async fn opt_r6_v1_telemetry_finite_supervision_counts_but_does_not_restart() {
        use crate::metrics::MetricsRegistry;
        use std::sync::atomic::{AtomicUsize, Ordering};

        let metrics = Arc::new(MetricsRegistry::new());
        let runs = Arc::new(AtomicUsize::new(0));
        let runs2 = runs.clone();
        // 复刻 main.rs 中 telemetry 的有尽监督包装：spawn worker → await JoinHandle
        // → 无论 Ok/Err 都记一次重启计数，且**不再拉起第二轮**。
        let render_metrics = metrics.clone();
        let wrapped = tokio::spawn(async move {
            let handle = tokio::spawn(async move {
                runs2.fetch_add(1, Ordering::Relaxed);
                panic!("simulated telemetry panic");
            });
            match handle.await {
                Ok(()) => log::error!("[Telemetry] worker exited unexpectedly"),
                Err(e) => log::error!("[Telemetry] worker panicked ({e:?})"),
            }
            render_metrics.note_supervisor_restart("telemetry");
        });

        tokio::time::timeout(Duration::from_secs(30), wrapped)
            .await
            .expect("finite supervision must return promptly")
            .expect("wrapper must not panic");
        // 只跑了一轮（无重启）。
        assert_eq!(runs.load(Ordering::Relaxed), 1, "telemetry 不得重启");
        // 但指标已记——可观测，不静默。
        let rendered = metrics.render();
        assert!(
            rendered.contains(r#"supervisor_restarts_total{worker="telemetry"} 1"#),
            "telemetry 崩溃必须记 supervisor_restarts_total（渲染中未见）"
        );
    }

    /// V1 接入后 stop 语义不回归：stop 置位后 prober/sweep/prewarmer 立刻停止拉起，
    /// 不会在优雅退出窗口里继续 churn。
    #[tokio::test]
    async fn opt_r6_v1_stop_prevents_further_restarts() {
        use crate::metrics::MetricsRegistry;
        use std::sync::atomic::{AtomicUsize, Ordering};

        let metrics = Arc::new(MetricsRegistry::new());
        let (tx, rx) = tokio::sync::watch::channel(false);
        let runs = Arc::new(AtomicUsize::new(0));
        let runs2 = runs.clone();
        let h = tokio::spawn(supervise_until(
            "opt-r6-v1-sweep",
            metrics,
            move || {
                let runs2 = runs2.clone();
                async move {
                    runs2.fetch_add(1, Ordering::Relaxed);
                }
            },
            rx,
        ));

        let deadline = Instant::now() + Duration::from_secs(20);
        while runs.load(Ordering::Relaxed) == 0 && Instant::now() < deadline {
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        assert!(runs.load(Ordering::Relaxed) >= 1, "worker must run first");
        tx.send(true).expect("stop");
        tokio::time::timeout(Duration::from_secs(60), h)
            .await
            .expect("stop must join promptly")
            .expect("supervise task panicked");
        let n = runs.load(Ordering::Relaxed);
        tokio::time::sleep(Duration::from_millis(1500)).await;
        assert_eq!(runs.load(Ordering::Relaxed), n, "no respawn after stop");
    }
}
