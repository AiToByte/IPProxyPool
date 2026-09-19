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
mod gateway;
mod metrics;
mod model;
mod pool;
mod prober;
mod router;
mod telemetry;
mod tenant;
mod vendor_arbitrage;

use analytics::AnalyticsEngine;
use bandit::{LinUCBEngine, DEFAULT_ALPHA};
use ch_sink::ChSinkWorker;
use circuit_breaker::{parse_delta_message, PassiveCircuitBreaker, DELTA_CHANNEL};
use dashmap::DashMap;
use gateway::{SmartProxyGateway, DEFAULT_API_KEY};
use metrics::{serve_metrics, MetricsRegistry, METRICS_ADDR};
use model::{ProxyNode, RoutingSpec};
use pingora_core::server::configuration::Opt;
use pingora_core::server::Server;
use pool::ConnectionPrewarmer;
use prober::CanaryProber;
use redis::aio::ConnectionManager;
use router::RouterEngine;
use std::sync::{atomic::AtomicU64, Arc};
use std::time::Duration;
use telemetry::{TelemetryEvent, TelemetryPublisher, TelemetryWorker, TELEMETRY_CHANNEL_CAP};
use tenant::TenantManager;
use vendor_arbitrage::VendorArbitrageWorker;

const REDIS_URL: &str = "redis://127.0.0.1:6379/";
const STREAM_KEY: &str = "stream:proxy:telemetry";
const CONSUMER_GROUP: &str = "circuit_breaker_group";
const PROBE_INTERVAL: Duration = Duration::from_secs(60);
const CLICKHOUSE_URL: &str = "http://127.0.0.1:8123";
const CLICKHOUSE_USER: &str = "proxy";
const CLICKHOUSE_PASSWORD: &str = "123456";
const CLICKHOUSE_DB: &str = "proxy";

#[tokio::main]
async fn main() {
    // Must run before any rustls use (ring + aws-lc-rs both compiled in).
    let _ = rustls::crypto::ring::default_provider().install_default();

    env_logger::init_from_env(env_logger::Env::default().default_filter_or("info"));

    // 1. Redis connection (degraded mode when unavailable, e.g. no Docker).
    let redis_client = redis::Client::open(REDIS_URL).expect("Invalid Redis URL");
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
        ProxyNode {
            ip: "127.0.0.1".to_string(),
            port: 8888,
            username: None,
            password: None,
            country: "US".to_string(),
            tier: "residential".to_string(),
            provider: "mock-a".to_string(),
            weight: 100,
        },
        ProxyNode {
            ip: "127.0.0.1".to_string(),
            port: 8889,
            username: None,
            password: None,
            country: "JP".to_string(),
            tier: "datacenter".to_string(),
            provider: "mock-b".to_string(),
            weight: 80,
        },
        ProxyNode {
            ip: "127.0.0.1".to_string(),
            port: 8890,
            username: None,
            password: None,
            country: "GB".to_string(),
            tier: "mobile".to_string(),
            provider: "mock-c".to_string(),
            weight: 60,
        },
    ];
    let router = Arc::new(RouterEngine::new(initial_nodes));

    // 2b. GW-4 tenants: default key with wide limits keeps GW-1~3 curls green.
    let tenant_mgr = Arc::new(TenantManager::new());
    tenant_mgr.register_tenant("default", DEFAULT_API_KEY, 10_000, 10_000);

    // 2c. GW-4 Prometheus registry + exposition endpoint.
    // OPT-4：落库丢弃计数由 worker 与 metrics 共享（worker 直写、metrics 只读渲染）。
    let telemetry_dropped = Arc::new(AtomicU64::new(0));
    let metrics = Arc::new(MetricsRegistry::new_with_dropped(telemetry_dropped.clone()));
    tokio::spawn(serve_metrics(metrics.clone(), METRICS_ADDR));

    // 3. Zero-blocking telemetry pipe (capacity 10,000).
    let (tx, rx) = tokio::sync::mpsc::channel::<TelemetryEvent>(TELEMETRY_CHANNEL_CAP);
    let telemetry_pub = Arc::new(TelemetryPublisher::new(tx));

    // 3b. GW-3 LinUCB engine + arm table + warm-ticket keeper.
    let bandit_engine = Arc::new(LinUCBEngine::new(DEFAULT_ALPHA));
    let bandit_arms = Arc::new(DashMap::new());
    let prewarmer = ConnectionPrewarmer::new(
        router.clone(),
        vec!["cloudflare.com".to_string(), "www.google.com".to_string()],
    );
    tokio::spawn(prewarmer.run());

    if let Some(ref conn) = redis_conn {
        // Telemetry batch worker → Redis Stream（OPT-4：传入共享丢弃计数器）。
        let tele_worker = TelemetryWorker::new(
            rx,
            conn.clone(),
            STREAM_KEY.to_string(),
            telemetry_dropped.clone(),
        );
        tokio::spawn(tele_worker.run());

        // Passive circuit breaker consumer.
        let cb_worker = PassiveCircuitBreaker::new(
            conn.clone(),
            STREAM_KEY.to_string(),
            CONSUMER_GROUP.to_string(),
            "worker_01".to_string(),
            router.clone(),
        );
        tokio::spawn(cb_worker.run());

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
        // Drop the receiver side so `emit` stays non-blocking and cheap.
        drop(rx);
    }

    // 4. Periodic canary prober over the current healthy pool.
    let probe_router = router.clone();
    tokio::spawn(async move {
        let prober = CanaryProber::new();
        let mut ticker = tokio::time::interval(PROBE_INTERVAL);
        loop {
            ticker.tick().await;
            let candidates = probe_router.get_healthy_candidates(&RoutingSpec::default());
            // OPT-5：复用 Client 池大小随滴答打 debug（稳定即无建链抖动）。
            log::debug!(
                "[Prober] tick start nodes={} clients={}",
                candidates.len(),
                prober.client_count()
            );
            for node in candidates {
                match prober.probe_node(&node).await {
                    prober::ProbeResult::Healthy {
                        latency_ms,
                        exit_ip,
                    } => {
                        log::info!(
                            "[Prober] {}:{} healthy via {exit_ip} ({latency_ms}ms)",
                            node.ip,
                            node.port
                        );
                    }
                    prober::ProbeResult::Degraded { reason } => {
                        log::warn!("[Prober] {}:{} degraded: {reason}", node.ip, node.port);
                    }
                    prober::ProbeResult::Dead { error } => {
                        log::warn!("[Prober] {}:{} dead: {error}", node.ip, node.port);
                    }
                }
            }
        }
    });

    // 4b. GW-4 vendor SLA arbitrage (lazy CH: query errors hold weights).
    let analytics = Arc::new(AnalyticsEngine::new(
        CLICKHOUSE_URL,
        CLICKHOUSE_USER,
        CLICKHOUSE_PASSWORD,
        CLICKHOUSE_DB,
    ));
    match analytics.ping().await {
        Ok(()) => log::info!("[GW-4] ClickHouse online, arbitrage audits live"),
        Err(e) => log::warn!("[GW-4] ClickHouse unreachable ({e:?}), arbitrage holds weights"),
    }
    let arbitrage = VendorArbitrageWorker::new(
        analytics.clone(),
        router.clone(),
        vec![
            "mock-a".to_string(),
            "mock-b".to_string(),
            "mock-c".to_string(),
        ],
        vec!["US".to_string(), "JP".to_string(), "GB".to_string()],
    );
    tokio::spawn(arbitrage.run());

    // 4c. GW-R2 sink pump: Stream → ClickHouse (needs Redis; CH errors hold).
    if let Some(ref conn) = redis_conn {
        let pump = ChSinkWorker::new(conn.clone(), analytics.clone(), STREAM_KEY.to_string());
        tokio::spawn(pump.run());
    } else {
        log::warn!("[GW-R2] sink pump offline (no Redis), warehouse landings paused");
    }

    // 4d. OPT-1 职守清理：每 60s 清过期会话/隔离 + 修剪游离臂，防长稳内存泄漏。
    let sweep_router = router.clone();
    let sweep_arms = bandit_arms.clone();
    tokio::spawn(async move {
        let mut ticker = tokio::time::interval(Duration::from_secs(60));
        loop {
            ticker.tick().await;
            let (sessions, quarantines) = sweep_router.sweep_expired();
            let arms = gateway::prune_stale_arms(&sweep_arms, &sweep_router);
            if sessions + quarantines + arms > 0 {
                log::info!(
                    "[Sweep] cleared sessions={sessions} quarantines={quarantines} arms={arms}"
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
            telemetry: Some(telemetry_pub.clone()),
            bandit_engine: bandit_engine.clone(),
            bandit_arms: bandit_arms.clone(),
            tenant_mgr: tenant_mgr.clone(),
            metrics: metrics.clone(),
            require_api_key,
        },
    );
    proxy_service.add_tcp("0.0.0.0:8080");

    log::info!("Pingora Smart Proxy Gateway (GW-4 tenants + arbitrage + metrics) on 0.0.0.0:8080");
    server.add_service(proxy_service);
    server.run_forever();
}
