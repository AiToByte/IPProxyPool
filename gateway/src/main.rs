//! GW-4 entrypoint: enterprise gateway (self-healing + LinUCB + tenants).
//!
//! Assembly: telemetry batch worker → circuit-breaker consumer → PubSub delta
//! sync → canary prober → warm-ticket keeper → vendor SLA arbitrage →
//! Prometheus exposition (:9091) → Pingora data plane on :8080 (tenant gate +
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
use circuit_breaker::{parse_delta_message, PassiveCircuitBreaker, DELTA_CHANNEL};
use dashmap::DashMap;
use free_pool::{
    FreePoolConfig, FreePoolWorker, DEFAULT_API_URL, DEFAULT_FULL_CHECK_BASE, DEFAULT_GITHUB_URL,
    DEFAULT_HTML_URL,
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

/// R2-8 后台 supervisor：worker 退出（panic 由 `JoinHandle` 捕获/意外返回）
/// 即计数进 metrics + 指数 backoff（1s 起，封顶 60s）重启。本轮包 CB/sink/
/// arbitrage 三个常驻消费组（telemetry/prober/sweep/prewarmer 维持现状）。
async fn supervise<Make, Fut>(worker: &'static str, metrics: Arc<MetricsRegistry>, make: Make)
where
    Make: Fn() -> Fut,
    Fut: std::future::Future<Output = ()> + Send + 'static,
{
    let mut backoff = Duration::from_secs(1);
    loop {
        match tokio::spawn(make()).await {
            Ok(()) => log::error!("[Supervisor] {worker} exited unexpectedly, restarting"),
            Err(e) => log::error!("[Supervisor] {worker} panicked ({e:?}), restarting"),
        }
        metrics.note_supervisor_restart(worker);
        tokio::time::sleep(backoff).await;
        backoff = (backoff * 2).min(Duration::from_secs(60));
    }
}

#[tokio::main]
async fn main() {
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

    // 2a. OPT-2 API Key 环境门：`REQUIRE_API_KEY=1` 即开，默认关闭。
    // 开启后，未带 `X-API-Key` 头的请求在网关入口直接 403（见 gateway.rs），
    // 关闭时沿用 GW-1~GW-4 行为（无头走 `default_key` 宽限额）。
    let require_api_key = std::env::var("REQUIRE_API_KEY").as_deref() == Ok("1");
    if require_api_key {
        log::info!("[OPT-2] REQUIRE_API_KEY=1, missing X-API-Key requests get 403");
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
    let metrics_addr = env_str("METRICS_ADDR", METRICS_ADDR);
    let metrics_for_serve = metrics.clone();
    tokio::spawn(async move {
        serve_metrics(metrics_for_serve, &metrics_addr).await;
    });

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
    let prewarmer = ConnectionPrewarmer::new(router.clone())
        .with_interval(env_secs("PREWARM_INTERVAL_SECS", 30));
    tokio::spawn(prewarmer.run());

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
            html_urls: split_env_list("FREE_HTML_URLS", DEFAULT_HTML_URL),
            github_urls: split_env_list("FREE_GITHUB_URLS", DEFAULT_GITHUB_URL),
            fetch_interval: env_secs("FREE_FETCH_INTERVAL_SECS", 600),
            ttl: clamp_free_ttl(env_secs("FREE_TTL_SECS", 1800)),
            verify_timeout: env_secs("FREE_VERIFY_TIMEOUT_SECS", 3),
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
        tokio::spawn(async move {
            tokio::time::sleep(startup_jitter()).await;
            log::info!("[FreePool] staggered start (second supply line)");
            supervise("free_pool", free_metrics.clone(), move || {
                let w = FreePoolWorker::new(
                    free_router.clone(),
                    free_metrics.clone(),
                    free_config.clone(),
                )
                .with_geo(free_geo.clone());
                async move { w.run(reqwest::Client::new()).await }
            })
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
        tokio::spawn(tele_worker.run());

        // Passive circuit breaker consumer (R2-8: supervisor 包重启计数 + backoff)。
        let cb_conn = conn.clone();
        let cb_router = router.clone();
        let cb_metrics = metrics.clone();
        tokio::spawn(supervise("circuit_breaker", cb_metrics, move || {
            PassiveCircuitBreaker::new(
                cb_conn.clone(),
                STREAM_KEY.to_string(),
                CONSUMER_GROUP.to_string(),
                "worker_01".to_string(),
                cb_router.clone(),
            )
            .run()
        }));

        // PubSub delta sync → in-memory quarantine.
        let router_clone = router.clone();
        let sub_client = redis_client.clone();
        tokio::spawn(async move {
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
                if let Some((domain, ip, ttl)) = parse_delta_message(&payload) {
                    router_clone.set_quarantine(&domain, &ip, ttl);
                    log::info!("[Sync] Applied delta quarantine on gateway memory: {domain}:{ip}");
                }
            }
        });
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
    tokio::spawn(async move {
        tokio::time::sleep(startup_jitter()).await;
        log::info!("[Prober] staggered start (R2-7 jitter)");
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
            let candidates = probe_router.get_healthy_candidates(&RoutingSpec::default());
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
                    let _permit = sem.acquire_owned().await;
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
                        log::warn!("[Prober] {}:{} degraded: {reason}", node.ip, node.port);
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
    tokio::spawn(async move {
        tokio::time::sleep(startup_jitter()).await;
        log::info!("[Arbitrage] staggered start (R2-7 jitter)");
        supervise("arbitrage", arb_metrics, move || {
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
        })
        .await;
    });

    // 4c. GW-R2 sink pump: Stream → ClickHouse (needs Redis; CH errors hold).
    // R2-8：supervisor 包重启计数 + backoff。
    if let Some(ref conn) = redis_conn {
        let pump_conn = conn.clone();
        let pump_analytics = analytics.clone();
        let pump_metrics = metrics.clone();
        tokio::spawn(supervise("ch_sink", pump_metrics, move || {
            ChSinkWorker::new(
                pump_conn.clone(),
                pump_analytics.clone(),
                STREAM_KEY.to_string(),
            )
            .run()
        }));
    } else {
        log::warn!("[GW-R2] sink pump offline (no Redis), warehouse landings paused");
    }

    // 4d. OPT-1 职守清理：每 60s 清过期会话/隔离 + 修剪游离臂，防长稳内存泄漏。
    // R2-7：启动错峰（与 prober/arbitrage 三 60s ticker 打散，不再对齐惊群）。
    // R2-8：清理间隔 env 化（`SWEEP_INTERVAL_SECS`，默认 60s）。
    // P2-7：同节拍淘汰 bridge 闲置 Client（socks 节点下线/账密 rotation 后 key 不常驻）。
    let sweep_router = router.clone();
    let sweep_arms = bandit_arms.clone();
    let sweep_interval = env_secs("SWEEP_INTERVAL_SECS", 60);
    // P2 SOCKS 翻译桥（显式 socks 请求出站执行器；env 见计划 §2）。
    let socks_bridge = Arc::new(SocksBridge::new(
        env_secs("SOCKS_BRIDGE_TIMEOUT_SECS", 20),
        env_str("SOCKS_MAX_BODY_BYTES", "10485760")
            .parse::<u64>()
            .unwrap_or(10 * 1024 * 1024),
    ));
    let sweep_bridge = socks_bridge.clone();
    tokio::spawn(async move {
        tokio::time::sleep(startup_jitter()).await;
        log::info!("[Sweep] staggered start (R2-7 jitter)");
        let mut ticker = tokio::time::interval(sweep_interval);
        loop {
            ticker.tick().await;
            let (sessions, quarantines) = sweep_router.sweep_expired();
            let arms = gateway::prune_stale_arms(&sweep_arms, &sweep_router);
            let bridged = sweep_bridge.evict_idle();
            if sessions + quarantines + arms + bridged > 0 {
                log::info!(
                    "[Sweep] cleared sessions={sessions} quarantines={quarantines} arms={arms} bridge={bridged}"
                );
            }
        }
    });

    // 5. Pingora data plane.
    let opt = Opt::parse_args();
    let mut server = Server::new(Some(opt)).expect("Failed to create Pingora server");
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
    // R2-8：网关监听地址 env 化（`GATEWAY_ADDR`，默认 0.0.0.0:8080）。
    let gateway_addr = env_str("GATEWAY_ADDR", "0.0.0.0:8080");
    proxy_service.add_tcp(&gateway_addr);

    log::info!(
        "Pingora Smart Proxy Gateway (GW-4 tenants + arbitrage + metrics) on {gateway_addr}"
    );
    server.add_service(proxy_service);
    server.run_forever();
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
}
