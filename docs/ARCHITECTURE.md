# Architecture 架构说明

> 双语：中文在前，English after. Bilingual: Chinese first, English second.
> 锚点：模块定义见 `docs/COMPONENTS.md`；运维口径见 `docs/OPERATION.md`。

## 中文

### 1. 分层总览

```text
                    ┌──────────── 客户端 ────────────┐
                    │  X-API-Key / X-Session-Id /     │
                    │  X-Tenant-Country / X-Proxy-*   │
                    └──────────────┬──────────────────┘
                                   ▼ :8080
┌──────────────────────────────────────────────────────────────┐
│ 数据面 Data plane (`gateway.rs` Pingora ProxyHttp)            │
│  request_filter → upstream_peer → response_filter →           │
│  response_body_filter → logging                               │
└──────┬───────────────┬───────────────┬───────────────┬───────┘
       ▼               ▼               ▼               ▼
   选路 Router    学习 Bandit      租户 Tenant    遥测 Telemetry
   (权重/粘滞/    (LinUCB d=4,     (鉴权/QPS/     (MPSC→Redis
    隔离/合并)     8臂<200ns)       并发/计费)     Stream→CH)
┌──────────────────────────────────────────────────────────────┐
│ 控制面 Control plane（后台 ticker，常驻 supervisor）          │
│ prober 60s / prewarmer 30s / sweep 60s / arbitrage 60s /     │
│ free_pool 600s / ch_sink 常驻泵                               │
└──────┬───────────────┬───────────────┬───────────────────────┘
       ▼               ▼               ▼
   Redis 7        ClickHouse 24    Prometheus+Grafana
   (Stream/隔离键/  (数仓 telemetry (16 组指标＋5 面板)
    PubSub)         _log)
```

网关为宿主二进制（不在 compose 内）；依赖一键起：`docker compose up -d`。

### 2. 请求五阶段（数据面）

1. `request_filter`：Host 校验（缺 Host 400）→ API Key 门（`REQUIRE_API_KEY=1` 时无头 403）→ 租户鉴权（坏 Key 403／欠费 402／超限 429）→ 解析 `RoutingSpec`（country/tier/session/proto）→ SOCKS 显式请求短路进翻译桥。
2. `upstream_peer`：粘滞 fast path（复核权重/proto/隔离/TTL）→ 健康候选过滤 → LinUCB/加权选择 → 上游 auth 注入＋超时（1500/5000/3000ms）。
3. `response_filter`：透传状态码，失败进重试（换节点＋失败集上限 16）。
4. `response_body_filter`：字节计量（OPT-3：只计最后 attempt 出站字节）。
5. `logging`：bandit reward 更新 → 租户释放＋计量 → metrics → 遥测 emit（event_id 配号，满队列丢弃计数）。

### 3. 双轨自愈（GW-2）

遥测 MPSC（1 万封顶）→ Redis Stream（`MAXLEN ~10万`，自动 ID，幂等靠 payload `event_id`＋sink 10k 去重窗）→ 熔断消费组（429→60s／403→600s／502,504→30s；内存先行 <50ms，再 Redis SETEX＋PubSub 广播）→ 域级隔离（`quarantine:{domain}:{ip}`，内存 TTL 独立）。

### 4. 免费第二供应线（FreePool，默认关闭）

`Source` 三适配器（JSON API／HTML／GitHub raw，ETag 礼貌轮询＋15s 超时＋源序归一）→ TCP 初筛（3s，并发 50 许可实持有）→ FullCheck 实转复检（出口比对＋7 头匿名分级＋canary 防篡改，并发 20）→ Registry（TTL 30min、EWMA α=0.3 健康分→权重 1..20、指数 backoff、容量淘汰、SourceGuard 熔断）→ `replace_vendor_nodes("free-", …)` 原子合并 → 既有选路/熔断/遥测全复用。零信任：复检基址 https-only，禁敏感流量，Transparent 默认只服务无归属流量。

### 5. SOCKS 与画像学习

- SOCKS：`EgressProto` 单源 → Router 默认隔离（无 proto 头永不命中 socks）→ `proxy_upstream_filter` 短路＋合成响应 → `socks_bridge` per-node Client（body 10MB 上限，hop 头过滤）。
- 画像：LinUCB 分档遗忘（free 1k／他 10k 更新）＋免费风险溢价 0.15＋免费独立套利（<50 摘除／50~80 半权／≥80 hold，付费 80/95 冻结）＋GeoIP（无库 Disabled，只观察；`GEOIP_ENFORCE_MISMATCH=1` 经观察无误报后才开）。

### 6. 韧性模型（Phase 5 演练结论）

Redis 中断：数据面 200 不降级（内存隔离独立），恢复后行数追齐；CH 中断：hold-ack＋有界内存，恢复追齐；Mock 全挂：正确 503，重拉后首请求即 200（connect 失败不进隔离）。后台工人 panic/退出由 supervisor 指数 backoff（1s→60s）重启并计数。

## English

### 1. Layered overview

```text
                    ┌──────────── Clients ───────────────┐
                    │  X-API-Key / X-Session-Id /         │
                    │  X-Tenant-Country / X-Proxy-*       │
                    └──────────────┬──────────────────────┘
                                   ▼ :8080
┌──────────────────────────────────────────────────────────────┐
│ Data plane (`gateway.rs` Pingora ProxyHttp)                   │
│  request_filter → upstream_peer → response_filter →           │
│  response_body_filter → logging                               │
└──────┬───────────────┬───────────────┬───────────────┬───────┘
       ▼               ▼               ▼               ▼
   Router          Bandit          Tenant          Telemetry
   (weighted/      (LinUCB d=4,    (auth/QPS/      (MPSC→Redis
    sticky/         8-arm <200ns)   conc/billing)   Stream→CH)
    quar/merge)
┌──────────────────────────────────────────────────────────────┐
│ Control plane (background tickers, supervised)                │
│ prober 60s / prewarmer 30s / sweep 60s / arbitrage 60s /     │
│ free_pool 600s / ch_sink resident pump                        │
└──────┬───────────────┬───────────────┬───────────────────────┘
       ▼               ▼               ▼
   Redis 7        ClickHouse 24    Prometheus+Grafana
   (Stream/quar    (warehouse       (16 metric groups
    keys/PubSub)   telemetry_log)   + 5 panels)
```

The gateway runs as a host binary (not in compose); dependencies start with `docker compose up -d`.

### 2. Five request phases (data plane)

1. `request_filter`: Host check (missing Host → 400) → API key gate (no header → 403 when `REQUIRE_API_KEY=1`) → tenant auth (bad key 403 / no balance 402 / over-limit 429) → parse `RoutingSpec` (country/tier/session/proto) → explicit SOCKS requests short-circuit into the translation bridge.
2. `upstream_peer`: sticky fast path (re-check weight/proto/quarantine/TTL) → healthy-candidate filter → LinUCB/weighted pick → upstream auth injection + timeouts (1500/5000/3000ms).
3. `response_filter`: pass through status; failures retry (node rotation + failed-set cap 16).
4. `response_body_filter`: byte metering (OPT-3: only the last attempt's egress bytes).
5. `logging`: bandit reward update → tenant release + metering → metrics → telemetry emit (numbered `event_id`, drop-counted when full).

### 3. Dual-track self-healing (GW-2)

Telemetry MPSC (cap 10k) → Redis Stream (`MAXLEN ~100k`, auto IDs, idempotency via payload `event_id` + sink 10k dedup window) → breaker consumer group (429→60s / 403→600s / 502,504→30s; memory first <50ms, then Redis SETEX + PubSub broadcast) → domain-scoped quarantine (`quarantine:{domain}:{ip}`, independent memory TTL).

### 4. Free second supply line (FreePool, off by default)

Three `Source` adapters (JSON API / HTML / GitHub raw, ETag politeness + 15s timeout + source-order merge) → TCP pre-screen (3s, 50 held permits) → FullCheck live re-verify (exit-IP compare + 7-header anonymity grading + canary anti-tamper, 20 permits) → Registry (30min TTL, EWMA α=0.3 health → weight 1..20, exponential backoff, capacity eviction, SourceGuard breaker) → atomic `replace_vendor_nodes("free-", …)` merge → full reuse of routing/breaker/telemetry. Zero-trust: https-only recheck base, no sensitive traffic, Transparent serves only unattributed traffic by default.

### 5. SOCKS and profile learning

- SOCKS: single-source `EgressProto` → router default isolation (no proto header never hits socks) → `proxy_upstream_filter` short-circuit + synthesized response → `socks_bridge` per-node clients (10MB body cap, hop-header filter).
- Profiling: LinUCB per-tier forgetting (free 1k / others 10k updates) + free risk premium 0.15 + free-only arbitrage (<50 cut / 50–80 half-weight / ≥80 hold; paid 80/95 frozen) + GeoIP (Disabled without DB, observe-only; `GEOIP_ENFORCE_MISMATCH=1` only after clean observation).

### 6. Resilience model (Phase 5 drill conclusions)

Redis outage: data plane stays 200 (memory isolation independent), rows catch up after recovery; CH outage: hold-ack + bounded memory, catch-up on recovery; all mocks down: correct 503, first request 200 immediately after restart (connect failures never quarantine). Background worker panic/exit is restarted by the supervisor with exponential backoff (1s→60s) and counted.
