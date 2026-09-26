# 数据流文档 / Data Flow

> 双语：中文在前，English after.
> 口径基线：D3（带 Key）、8916／9091、Stream 名 `stream:proxy:telemetry`、消费组 `ch_sink_group`。代码位置精确到文件（行号以 2026-09-26 树为准，±30 行内）。

## 中文

### 1. 请求全生命周期（默认池，200 路径）

```
curl(带X-Api-Key) ── origin-form GET /path＋Host:上游 ─▶ :8916
        │
        ▼  gateway.rs request_filter
   ┌────────┬────────┬──────────┬─────────┬──────────┐
   │Host空? │无Key?  │坏Key/欠费 │解析路由  │ scrub 指纹│
   │400直返 │403直返 │429/402/403│RoutingSpec│对齐＋脱敏│
   └────────┴────────┴──────────┴─────────┴──────────┘
        │ (D1: tier=free＋Authorization/Cookie → 403 直返)
        ▼  upstream_peer：粘滞命中？→ LinUCB/加权选节点（失败集排除）
        ▼  upstream_request_filter：上游 auth 注入＋超时（1500/5000/3000ms）
        ▼  proxy_upstream_filter：socks 显式？→ 翻译桥出站并合成响应
        ▼  fail_to_connect：换节点重试（attempts 上限＋失败集）
        ▼  logging：bandit reward→租户释放＋计量→metrics→telemetry emit
        ▼
   上游 ◀── egress（付费直连／免费节点／socks 桥）── 响应原样回下游
```

关键代码：`gateway/src/gateway.rs`（filter/peer/logging）、`router.rs`（select_node_excluding）、`bandit.rs`（extract_context/select）、`tenant.rs`（authenticate_and_throttle/release_and_meter）、`metrics.rs`（observe）。

### 2. 遥测落库流（每请求 1 事件）

```
logging emit TelemetryEvent ─▶ MPSC ring(1万，满则 channel_dropped计数)
        │ TelemetryWorker 批量取（batch 5000/1s）
        ▼ XADD * payload/domain/status (+MAXLEN ~) → stream:proxy:telemetry
        │ 失败重试 1 次，再失败按 queued 精确记 telemetry_dropped_total
        ▼ ChSinkWorker 消费组 ch_sink_group 拉取 → SeenIds 窗去重 → insert CH
        │ CH 挂则 hold 住 ack（不丢位点），恢复后追齐
        ▼ proxy.proxy_telemetry_log（90 天 TTL）→ Grafana/SLA 查询
```

验证：`XLEN stream:proxy:telemetry`（消费组 `lag=0` 健康）；`XINFO GROUPS` 看 pending；`SELECT count()` 随流量涨；`[ChSink] landed N rows`。
关键代码：`telemetry.rs`（publisher/worker/encode_batch）、`ch_sink.rs`（pump_once/SeenIds）。

### 3. 免费线管道（tick 级，默认 600s 一轮）

```
fetch_all（api/html/github 并发＋15s 单源超时＋etag/304＋熔断守卫）
        ▼ 去重（ip:port+proto，首见＝源序获胜）
        ▼ B1 bogon 过滤 → B8 加权截断（Elite 历史＋源序 TopN，上限 max_nodes×factor）
        ▼ B5 失败指纹窗跳过（2×抓取间隔内死 IP 不建链）
        ▼ TCP 初筛（信号量 max_concurrent）→ FullCheck（信号量 20，三端点＋canary＋匿名分级）
        ▼ upsert_full（续期/health/权重）→ evict_if_over_capacity（backoff 优先＋composite 最低）
        ▼ merge_once → Router.replace_vendor_nodes（free- 前缀）＋水位计
        ▼ free_pool_nodes_total / by_proto / source_elite / B9 四指标
```

熔断语义：传输失败/超时视同零产出计轮次（SourceGuard），连续 `FREE_SOURCE_MAX_ZERO_CYCLES` 轮暂停，每 `FREE_SUSPEND_RETRY_EVERY` tick 试探；304 不计。
关键代码：`free_pool.rs`（fetch_all/cap_intake/Registry/run_once/merge_once）。

### 4. 隔离 PubSub 流（GW-2 双轨自愈一半）

```
某实例探针判节点死 → set_quarantine（内存，<50ms 生效）
        ├─▶ SETEX quarantine:{domain}:{ip}（持久化，失败记数＋warn＋单次重试）
        └─▶ PUBLISH delta（扇出 QUARANTINE|domain|ip|ttl，其他实例 apply_delta 落内存）
订阅断线 → supervise backoff 重连（pubsub_delta，可观测）；重连后全量对账靠 TTL 自然过期。
```

验证：`quarantine:{domain}:{ip}` Redis 键；`supervisor_restarts_total{worker="pubsub_delta"}`。
关键代码：`circuit_breaker.rs`（quarantine_redis_key/delta_message/RemoteSyncStats）。

### 5. 重试与换节点流

```
上游失败 → fail_to_connect：record_failed_addr（失败集，上限 16，A2 环形复用）
        → transferred_bytes 清零（A1：只计最后 attempt 出站字节）
        → upstream_peer 重选（排除失败集）→ 下一 attempt
        → SOCKS 整轮总预算（单跳 20s＋8s，A9 deadline 截尾）
        → 用尽：503（免费池空亦 503，语义一致）
```

### 6. 指标渲染流

```
各模块 AtomicU64/DashMap 写侧（单原子/饱和累加）→ serve_metrics(:9091/metrics)
→ Prometheus 15s 抓取 → 5 规则求值 → Grafana 7 面板
→ 成功率<99／干旱30m／重启>3／失联／Stream 积压 五告警
```

关键代码：`metrics.rs`（render＋note_*）；`deploy/prometheus/rules.yml`；`deploy/grafana/` 面板 5→7（E5 加延迟直方图＋exit TopK）。
### 相关文档 / Related docs

- 架构全景见 [SYSTEM-ARCHITECTURE.md](SYSTEM-ARCHITECTURE.md)；功能细节见 [FEATURES.md](FEATURES.md)；上手操作见 [USER-GUIDE.md](USER-GUIDE.md)；开源与依赖见 [OPEN-SOURCE.md](OPEN-SOURCE.md)。


## English

### 1. Request Lifecycle (default pool, 200 path)

```
curl (with X-Api-Key) ── origin-form GET /path + Host: upstream ─▶ :8916
        │
        ▼  gateway.rs request_filter
   ┌────────┬────────┬──────────┬─────────┬──────────┐
   │no Host?│no key? │bad/broke │parse     │scrub     │
   │400      │403     │429/402/  │Routing   │fingerprint│
   │direct   │direct  │403       │Spec      │align      │
   └────────┴────────┴──────────┴─────────┴──────────┘
        │ (D1: tier=free + Authorization/Cookie → direct 403)
        ▼  upstream_peer: sticky hit? → LinUCB/weighted pick (failed-set excluded)
        ▼  upstream_request_filter: upstream auth inject + timeouts (1500/5000/3000ms)
        ▼  proxy_upstream_filter: explicit socks? → bridge egress + synthesized response
        ▼  fail_to_connect: rotate node (attempt cap + failed set)
        ▼  logging: bandit reward → tenant release + meter → metrics → telemetry emit
        ▼
   upstream ◀── egress (paid direct / free node / socks bridge) ── response passes through
```

Key code: `gateway/src/gateway.rs` (filter/peer/logging), `router.rs` (select_node_excluding), `bandit.rs`, `tenant.rs`, `metrics.rs` (observe).

### 2. Telemetry Warehouse Flow (one event per request)

```
logging emits TelemetryEvent ─▶ MPSC ring (10k, full → channel_dropped)
        │ TelemetryWorker batches (5000/1s)
        ▼ XADD * payload/domain/status (+MAXLEN ~) → stream:proxy:telemetry
        │ one retry on failure, then exact `queued` counting to telemetry_dropped_total
        ▼ ChSinkWorker consumer group ch_sink_group pulls → SeenIds dedup window → insert CH
        │ CH down → hold ack (no position loss), catch up on recovery
        ▼ proxy.proxy_telemetry_log (90-day TTL) → Grafana/SLA queries
```

Verify: `XLEN stream:proxy:telemetry` (group lag=0 healthy); `XINFO GROUPS` for pending; `SELECT count()` grows; `[ChSink] landed N rows`.
Key code: `telemetry.rs`, `ch_sink.rs`.

### 3. Free Pipeline (per tick, 600s default)

```
fetch_all (api/html/github concurrent + 15s per-source timeout + etag/304 + fuse guards)
        ▼ dedup (ip:port+proto, first-seen by source order wins)
        ▼ B1 bogon filter → B8 weighted cap (Elite history + source order TopN, max_nodes×factor)
        ▼ B5 fail-fingerprint skip (dead IPs skip TCP within 2× fetch interval)
        ▼ TCP pre-check (semaphore max_concurrent) → FullCheck (semaphore 20, 3 endpoints + canary + grades)
        ▼ upsert_full (renew/health/weight) → evict_if_over_capacity (backed-off first, lowest composite)
        ▼ merge_once → Router.replace_vendor_nodes (free- prefix) + gauges
        ▼ free_pool_nodes_total / by_proto / source_elite / B9 four metrics
```

Fuse semantics: transport failure/timeout counts as zero-yield round; suspend after `FREE_SOURCE_MAX_ZERO_CYCLES` consecutive, probe every `FREE_SUSPEND_RETRY_EVERY` ticks; 304 exempt.
Key code: `free_pool.rs`.

### 4. Quarantine PubSub Flow (half of GW-2 dual-track healing)

```
instance prober condemns node → set_quarantine (in-memory, <50ms live)
        ├─▶ SETEX quarantine:{domain}:{ip} (persisted; failures counted＋warned＋retried once)
        └─▶ PUBLISH delta (QUARANTINE|domain|ip|ttl; peers apply_delta to memory)
Reconnects via supervise backoff (pubsub_delta, observable); TTL expiry reconciles.
```

Verify: `quarantine:{domain}:{ip}` keys; `supervisor_restarts_total{worker="pubsub_delta"}`.
Key code: `circuit_breaker.rs`.

### 5. Retry & Node-Rotation Flow

```
upstream failure → fail_to_connect: record_failed_addr (failed set, cap 16, ring reuse)
        → transferred_bytes reset (A1: meter last attempt only)
        → upstream_peer re-pick (failed set excluded) → next attempt
        → SOCKS round budget (per-hop 20s＋8s, A9 deadline cuts tail)
        → exhausted: 503 (empty free pool also 503, consistent)
```

### 6. Metrics Render Flow

```
per-module AtomicU64/DashMap writers (single-atom/saturating) → serve_metrics(:9091/metrics)
→ Prometheus 15s scrape → 5 rules eval → Grafana 7 panels
→ success<99 / dry-30m / restarts>3 / down / stream-lag five alerts
```

Key code: `metrics.rs`; `deploy/prometheus/rules.yml`; `deploy/grafana/` (E5 added latency histogram＋exit TopK).
