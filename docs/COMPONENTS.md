# Components 组件说明

> 双语：中文在前，English after. Bilingual: Chinese first, English second.
> 数据来源：`gateway/src/`（行数/单测数为落库时实测；总数 156 通过＋4 ignored 真 live）。
> Data source: `gateway/src/` (lines/tests measured at filing time; total 156 passed + 4 ignored live).

## 中文

### 1. 模块表（19 个）

| 模块 | 行数 | 单测 | 职责 | 关键 API |
|------|------|------|------|----------|
| `main.rs` | 578 | 5 | 装配：env 接线/supervisor/后台 ticker/Pingora 启动 | `env_str/env_secs/split_env_list/supervise/clamp_free_ttl` |
| `gateway.rs` | 963 | 15 | 数据面：鉴权/选路/转发/重试/计量/遥测/桥短路 | `SmartProxyGateway/parse_routing_spec/serve_via_socks` |
| `router.rs` | 982 | 22 | 选路：加权/粘滞/隔离/原子替换/套利缩放 | `select_node_excluding/get_healthy_candidates_excluding/replace_vendor_nodes/scale_vendor_weights/sweep_expired` |
| `model.rs` | 144 | 0 | 模型：节点/规格/上下文＋tier 归一 | `ProxyNode::new/canonical_tier/RoutingSpec/ProxyContext` |
| `free_pool.rs` | 2315 | 32 | 免费第二线：抓取/两级质检/注册/熔断/合并 | `FreePoolWorker/Source/FullChecker/Registry/Health/SourceGuard/fetch_all` |
| `bandit.rs` | 417 | 11 | LinUCB d=4 选路＋分档遗忘＋免费风险溢价 | `LinUCBEngine/select_best_arm/compute_reward/forget_every_for_tier` |
| `telemetry.rs` | 314 | 5 | 遥测：MPSC 批量 XADD＋幂等＋双丢弃计数 | `TelemetryPublisher/TelemetryWorker/emit` |
| `circuit_breaker.rs` | 367 | 7 | 熔断：Stream 消费＋隔离＋目标合法性守卫 | `PassiveCircuitBreaker/parse_delta_message/valid_quarantine_target/quarantine_ttl_for_status` |
| `ch_sink.rs` | 578 | 7 | 数仓泵：认领/去重/批量落库/hold 重放 | `ChSinkWorker/classify_entry` |
| `analytics.rs` | 287 | 5 | 落库查询：SLA 窗口＋注入守卫 | `AnalyticsEngine/query_provider_sla` |
| `tenant.rs` | 279 | 9 | 租户：鉴权/QPS/并发/计费（Free $0） | `TenantManager/authenticate_and_throttle/release_and_meter/price_per_gb` |
| `vendor_arbitrage.rs` | 208 | 3 | 套利：付费 80/95＋免费 50/80 分档 | `arbitrage_action/free_pool_action` |
| `metrics.rs` | 596 | 10 | 指标：:9091 exposition，16 组信号 | `MetricsRegistry/render/serve_metrics` |
| `prober.rs` | 342 | 7 | 存活探针：双源降级＋Client 池＋闲置淘汰 | `CanaryProber/probe_node` |
| `pool.rs` | 347 | 9 | 预热：建链/握手探测＋信号量＋票据 | `ConnectionPrewarmer/warm_once` |
| `fingerprint.rs` | 128 | 2 | 指纹：Chrome 8 头对齐＋传输选项 | `apply_chrome_profile` |
| `geo.rs` | 127 | 4 | 画像：GeoIP 查询＋mismatch 判定（无库 Disabled） | `GeoDb/geo_verdict` |
| `socks_bridge.rs` | 345 | 3 | SOCKS 翻译桥：显式 socks 请求出站＋hop 过滤＋body 上限 | `SocksBridge/fetch` |
| `socks_handshake.rs` | 468 | 4 | 握手：RFC1928/1929＋SOCKS4/4a＋greeting-only | `socks_handshake` |

### 2. 外部依赖（`gateway/Cargo.toml`）

| 依赖 | 版本 | 用途 |
|------|------|------|
| pingora(+core/proxy/load-balancing) | 0.6 | 网关数据面（0.4 在 rustc 1.93 下构建失败，见注释） |
| tokio/futures/async-trait | 1/0.3/0.1 | 异步运行时 |
| arc-swap/dashmap/parking_lot | 1.7/6.0/0.12 | 无锁结构（锁不跨 await） |
| serde/serde_json/bytes/http | 1/1/1.6/1.1 | 序列化与 HTTP |
| reqwest | 0.12 | 出站取数（rustls-tls/json/socks/stream） |
| redis | 0.26 | Stream/隔离键/PubSub |
| clickhouse/chrono | 0.13/0.4 | 数仓落库 |
| nalgebra/atomic_float | 0.33/1.1 | Bandit 向量 |
| governor | 0.6 | 租户 QPS |
| maxminddb | 0.32 | GeoIP（库文件运营方配给） |
| rustls/base64 | 0.23/0.22 | TLS 提供方 pin＋认证头 |

### 3. 基础设施（`docker-compose.yml`）

Redis 7（6379）/ ClickHouse 24（8123）/ Prometheus 2.53（9090）/ Grafana 11.1（3000）；网关为宿主二进制（:8080 数据面，:9091 指标）。

## English

### 1. Modules (19)

| Module | Lines | Tests | Responsibility | Key APIs |
|--------|-------|-------|----------------|----------|
| `main.rs` | 578 | 5 | Wiring: env binding/supervisor/background tickers/Pingora bootstrap | `env_str/env_secs/split_env_list/supervise/clamp_free_ttl` |
| `gateway.rs` | 963 | 15 | Data plane: auth/routing/forwarding/retry/metering/telemetry/bridge short-circuit | `SmartProxyGateway/parse_routing_spec/serve_via_socks` |
| `router.rs` | 982 | 22 | Routing: weighted/sticky/quarantine/atomic swap/arbitrage scaling | `select_node_excluding/get_healthy_candidates_excluding/replace_vendor_nodes/scale_vendor_weights/sweep_expired` |
| `model.rs` | 144 | 0 | Models: node/spec/context + tier canonicalization | `ProxyNode::new/canonical_tier/RoutingSpec/ProxyContext` |
| `free_pool.rs` | 2315 | 32 | Free 2nd line: fetch/two-stage verify/registry/circuit-break/merge | `FreePoolWorker/Source/FullChecker/Registry/Health/SourceGuard/fetch_all` |
| `bandit.rs` | 417 | 11 | LinUCB d=4 routing + per-tier forgetting + free risk premium | `LinUCBEngine/select_best_arm/compute_reward/forget_every_for_tier` |
| `telemetry.rs` | 314 | 5 | Telemetry: MPSC batched XADD + idempotency + dual drop counters | `TelemetryPublisher/TelemetryWorker/emit` |
| `circuit_breaker.rs` | 367 | 7 | Breaker: stream consume + quarantine + target-validity guard | `PassiveCircuitBreaker/parse_delta_message/valid_quarantine_target/quarantine_ttl_for_status` |
| `ch_sink.rs` | 578 | 7 | Warehouse pump: claim/dedup/batch land/hold replay | `ChSinkWorker/classify_entry` |
| `analytics.rs` | 287 | 5 | Warehouse queries: SLA window + injection guard | `AnalyticsEngine/query_provider_sla` |
| `tenant.rs` | 279 | 9 | Tenants: auth/QPS/concurrency/billing (Free $0) | `TenantManager/authenticate_and_throttle/release_and_meter/price_per_gb` |
| `vendor_arbitrage.rs` | 218 | 3 | Arbitrage: paid 80/95 + free 50/80 bands | `arbitrage_action/free_pool_action` |
| `metrics.rs` | 596 | 10 | Metrics: :9091 exposition, 16 signal groups | `MetricsRegistry/render/serve_metrics` |
| `prober.rs` | 342 | 7 | Liveness probe: dual-source fallback + client pool + idle eviction | `CanaryProber/probe_node` |
| `pool.rs` | 347 | 9 | Prewarm: connect/handshake probe + semaphore + tickets | `ConnectionPrewarmer/warm_once` |
| `fingerprint.rs` | 128 | 2 | Fingerprint: Chrome 8-header alignment + transport opts | `apply_chrome_profile` |
| `geo.rs` | 127 | 4 | Profiling: GeoIP lookup + mismatch verdict (Disabled w/o DB) | `GeoDb/geo_verdict` |
| `socks_bridge.rs` | 345 | 3 | SOCKS egress bridge: explicit-socks requests + hop filter + body cap | `SocksBridge/fetch` |
| `socks_handshake.rs` | 468 | 4 | Handshake: RFC1928/1929 + SOCKS4/4a + greeting-only | `socks_handshake` |

### 2. External dependencies (`gateway/Cargo.toml`)

| Dependency | Version | Purpose |
|------------|---------|---------|
| pingora (+core/proxy/load-balancing) | 0.6 | Gateway data plane (0.4 fails to build on rustc 1.93, see comment) |
| tokio/futures/async-trait | 1/0.3/0.1 | Async runtime |
| arc-swap/dashmap/parking_lot | 1.7/6.0/0.12 | Lock-free structures (locks never cross await) |
| serde/serde_json/bytes/http | 1/1/1.6/1.1 | Serialization & HTTP |
| reqwest | 0.12 | Egress fetching (rustls-tls/json/socks/stream) |
| redis | 0.26 | Stream/quarantine keys/PubSub |
| clickhouse/chrono | 0.13/0.4 | Warehouse landing |
| nalgebra/atomic_float | 0.33/1.1 | Bandit vectors |
| governor | 0.6 | Tenant QPS |
| maxminddb | 0.32 | GeoIP (DB file provided by operator) |
| rustls/base64 | 0.23/0.22 | TLS provider pin + auth headers |

### 3. Infrastructure (`docker-compose.yml`)

Redis 7 (6379) / ClickHouse 24 (8123) / Prometheus 2.53 (9090) / Grafana 11.1 (3000); gateway runs as host binary (:8080 data, :9091 metrics).
