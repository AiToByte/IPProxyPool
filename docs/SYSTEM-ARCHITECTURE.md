# 系统架构文档 / System Architecture

> 双语：中文在前，English after. Bilingual: Chinese first, English second.
> 口径基线：网关数据面 `127.0.0.1:8916`（D3 回环收紧）／指标 `:9091`；Key 门默认开启（开发 Key `default_key`）；单测 209＋4 真 live；计划见 `plan/2026年9月25日-OPT-R5优化方案.md`。

## 中文

### 1. 系统定位与设计目标

IPProxyPool 是基于 Cloudflare Pingora（Rust）构建的企业级高可用智能 IP 代理池与网关。它不是简单的转发代理，而是一个**带选路大脑、双轨自愈、双供应线、全链路可观测**的 egress 网关系统。

设计目标（按优先级）：
1. **正确优先**：鉴权→选路→转发→计量→遥测五阶段顺序不可乱；错账（归因/计量/遥测）零容忍。
2. **韧性优先**：任何单点（Redis/CH/源站/Worker panic/进程退出）都有自愈路径；Windows 开发抖动由看护兜底，生产跑 Linux。
3. **可观测优先**：每个关键决策都有指标＋日志＋落库三重证据；回归验证以“包体/计数器 corroborate”为铁律。
4. **诚实降级**：做不到的宁可明确拒绝（400/403/501/503 各有语义），不静默错 behavior。

非目标：HTTPS CONNECT 隧道（诚实 501，见 spike 结论）、eBPF/io_uring/MASQUE（spike 不落地）、BGP（只给模板）。

### 2. 总览图

```
                        ┌─────────────────────────────────────────────────┐
                        │              IPProxyPool 宿主网关进程              │
                        │         (pingora-proxy-gateway, Rust)            │
                        │                                                   │
  客户端 ──HTTP──▶ ┌────┴─────┐  ┌──────────┐  ┌────────┐  ┌─────────────┐  │
  (curl/SDK/      │ request_ │  │upstream_ │  │proxy_  │  │   logging   │  │
   浏览器经适配器)  │ filter   │─▶│peer选路  │─▶│upstream│─▶│(计量/遥测/   │  │
                   │鉴权/解析 │  │LinUCB/粘滞│  │_filter │  │ bandit更新) │  │
                   └────┬─────┘  └──────────┘  └────────┘  └──────┬──────┘  │
                        │                                        │         │
                        │  ┌─────────────────────────────────┐     │         │
                        │  │        控制面后台 tickers        │     │         │
                        │  │ prober60s/prewarmer30s/sweep60s │     │         │
                        │  │ arbitrage60s/ch_sink/free600s   │     │         │
                        │  │ ＋supervisor(1s起指数退避60s封顶) │     │         │
                        │  └─────────────────────────────────┘     │         │
                        └──────────────────────────────────────────┼─────────┘
                                                                   ▼
                        ┌──────────────┐  ┌────────────────┐  ┌──────────────┐
                        │ Redis 7 6379 │  │ ClickHouse 24  │  │ Prometheus   │
                        │ Stream/ PubSub│  │ 8123 落库/90天 │  │ 9090＋告警   │
                        │ 会话/隔离/熔断 │  │ TTL＋Grafana   │  │ rules.yml    │
                        └──────────────┘  │ 3000 五＋2面板  │  └──────────────┘
                                          └────────────────┘
  免费第二供应线：Geonode(api)→openproxylist/monosans/relayglass 等 7 源
    → TCP 初筛 → FullCheck 复检（匿名分级）→ merge 进池（与付费池隔离档位）
  SOCKS 出站：显式 X-Proxy-Proto 经翻译桥（socks_handshake＋reqwest）出站
```

### 3. 组件职责表

| 组件 | 文件 | 职责一句话 | 关键机制 |
|------|------|-----------|---------|
| 网关 filter | `gateway/src/gateway.rs` | 五阶段入口：Host 校验→Key 门→租户鉴权→解析→ scrub | 缺 Host 400；无头 403（D3 缺省开）；坏 Key 403／欠费 402／超限 429 |
| 选路 | `router.rs` | 粘滞 fast path＋LinUCB/加权＋隔离/ quarantine/排除集 | 会话租户隔离；约束复核；失败集排除；水位上限（A7） |
| Bandit | `bandit.rs` | LinUCB d=4 alpha 0.4 上下文选臂 | select <200ns（release 断言）；分档遗忘；风险溢价 |
| 池/预热 | `pool.rs` | 节点存活＋预热分波 | 100/波串行；信号量封顶；FD 有界 |
| 探针 | `prober.rs` | 三路并取存活＋Client 池复用＋淘汰 | per-代理 Client 缓存；信号量关闭跳过 |
| 熔断 | `circuit_breaker.rs` | 失败隔离＋Redis 持久＋PubSub 扇出 | 远端失败计数＋warn＋单次重试；脑裂可观测 |
| 遥测 | `telemetry.rs` | MPSC ring＋批量 XADD＋重试一次 | 丢数精确口径（queued/ser_failed 分计）；通道满计数 |
| 套利 | `vendor_arbitrage.rs` | 9 路 CH 并发 SLA 审计＋调权/hold | 单路 5s 超时；失败 hold 不摘除；免费独立因子 |
| 落库泵 | `ch_sink.rs` | Stream→CH 常驻＋消费组＋去重窗 | batch 5000/1s；hold-ack 等 CH 恢复 |
| 免费池 | `free_pool.rs` | 抓取→质检→合并第二供应线 | 预筛/分页/多源/负缓存/加权截断/熔断/intake 上限 |
| SOCKS 桥 | `socks_bridge.rs`/`socks_handshake.rs` | socks4/5 握手＋翻译出站 | 白名单头校验；总预算 deadline；桥失败记账 |
| 画像 | `geo.rs`/`fingerprint.rs` | GeoIP 观察/执法＋Chrome 头对齐 | 无库 Disabled 降级；mismatch 开关 |
| 租户 | `tenant.rs` | 鉴权＋QPS/并发＋计费 | CAS 扣槽防回绕；欠费下次拦截 |
| 指标 | `metrics.rs` | 全量渲染＋采样日志 | 30+ 序列；直方图饱和累加 |
| 主装配 | `main.rs` | env 接线＋supervise＋信号＋同步入口 | S1 同步 main；S5 stop 广播；grace 可配 |

### 4. 部署拓扑

```
宿主机（Windows 开发／Linux 生产）
├── 网关二进制 :8916（D3 缺省 127.0.0.1；容器显式 0.0.0.0）＋ :9091/metrics
├── mocks :8888/8889/8890（devtools，生产无）
├── 适配器 :18080（localhost only，形态翻译）
└── docker compose（四依赖＋可选 profile）
    ├── redis:6379（127.0.0.1 绑定＋requirepass，Stream/PubSub/会话/隔离）
    ├── clickhouse:8123/9010（遥测 90 天 TTL）
    ├── prometheus:9090（15s 抓取＋5 规则）
    ├── grafana:3000（provisioning 自带 7 面板）
    ├── [profile gateway] 网关容器版（需有网环境构建）
    └── [profile observability] redis-exporter＋alertmanager
```

### 5. 关键决策记录

1. **Pingora 0.6**（0.4 在 rustc 1.93 下依赖冲突）：proxy/lb/rustls 三件套，前端语义兼容。
2. **同步 main（S1）**：`#[tokio::main]` 内调 run_forever 会在 async 上下文 drop runtime 而 panic；改同步入口＋block_on 装配＋主线程 run_forever。Windows 仍按 grace 周期清洁退出（pingora 0.6 无 main_loop），看护兜底，长稳放 Linux。
3. **D3 收紧**：Key 门默认开＋监听收环；开发 Key `default_key`；SDK/脚本默认带；回退口径文档化。
4. **匿名免费线零信任**：free 档拒收 Authorization/Cookie（D1 403）；透明节点 REQUIRE_ELITE 门过滤。
5. **无 BOM＋LF＋中文 .ps1 在本机 PS5.1 下误解析**：全仓补 BOM；字面量内联保留（根因级教训）。

### 6. 非目标

CONNECT 隧道（501）、uTLS/boring 定制、eBPF/io_uring/MASQUE、BGP 引流、多租户 hard 隔离（命名空间级软隔离已够）。

### 相关文档 / Related docs

- 功能细节见 [`FEATURES.md`](FEATURES.md)；数据怎么流见 [`DATAFLOW.md`](DATAFLOW.md)；上手操作见 [`USER-GUIDE.md`](USER-GUIDE.md)；开源与依赖见 [`OPEN-SOURCE.md`](OPEN-SOURCE.md)。

## English

### 1. Positioning and Goals

IPProxyPool is an enterprise-grade HA smart IP proxy pool and egress gateway built on Cloudflare Pingora (Rust). It is not a dumb forwarder but a gateway with a routing brain, dual-track self-healing, dual supply lines, and full-path observability.

Goals in priority order:
1. **Correctness first**: the five phases (auth → route → forward → meter → telemetry) run in strict order; attribution/metering/telemetry mis-bookings are zero-tolerance.
2. **Resilience first**: every single point (Redis/CH/sources/worker panic/process exit) has a self-healing path; Windows dev jitter is covered by the watchdog, production runs Linux.
3. **Observability first**: every key decision carries metrics＋logs＋warehouse triple evidence; regression requires body/counter corroboration.
4. **Honest degradation**: what cannot be done is explicitly refused (400/403/501/503 each with semantics), never silently misbehaved.

Non-goals: HTTPS CONNECT tunneling (honest 501), eBPF/io_uring/MASQUE (spikes only), BGP (templates only).

### 2. Overview Diagram

```
                        ┌─────────────────────────────────────────────────┐
                        │            IPProxyPool host gateway               │
                        │         (pingora-proxy-gateway, Rust)            │
  Clients ──HTTP──▶ ┌────┴─────┐  ┌──────────┐  ┌────────┐  ┌─────────────┐  │
  (curl/SDK/        │ request_ │  │upstream_ │  │proxy_  │  │   logging   │  │
   browsers via     │ filter   │─▶│peer route│─▶│upstream│─▶│(meter/tele/  │  │
   adaptor)         │auth/parse│  │LinUCB/   │  │_filter │  │ bandit upd) │  │
                    │          │  │sticky    │  │        │  │             │  │
                    └────┬─────┘  └──────────┘  └────────┘  └──────┬──────┘  │
                         │                                       │         │
                         │  ┌────────────────────────────────┐     │         │
                         │  │     control-plane tickers      │     │         │
                         │  │ prober60s/prewarm30s/sweep60s  │     │         │
                         │  │ arbitrage60s/ch_sink/free600s  │     │         │
                         │  │ ＋supervisor (1s backoff–60s)  │     │         │
                         │  └────────────────────────────────┘     │         │
                         └─────────────────────────────────────────┼─────────┘
                                                                   ▼
                        ┌──────────────┐  ┌────────────────┐  ┌──────────────┐
                        │ Redis 7 6379 │  │ ClickHouse 24  │  │ Prometheus   │
                        │ Stream/PubSub│  │ 8123 warehouse │  │ 9090＋alerts  │
                        │ session/quar │  │ 90-day TTL＋   │  │ rules.yml    │
                        │antine/fuses  │  │ Grafana 3000   │  └──────────────┘
                        └──────────────┘  │ 7 panels       │
                                          └────────────────┘
  Free second line: Geonode(api)→openproxylist/monosans/relayglass (7 URLs)
    → TCP pre-check → FullCheck re-verify (anonymity grades) → merge (tier-isolated)
  SOCKS egress: explicit X-Proxy-Proto via translation bridge
```

### 3. Component Table

| Component | File | One-line duty | Key mechanism |
|-----------|------|---------------|---------------|
| gateway filter | `gateway/src/gateway.rs` | five-phase entry: Host check→key gate→tenant auth→parse→scrub | missing Host 400; headerless 403 (D3 on); bad key 403 / unpaid 402 / limited 429 |
| router | `router.rs` | sticky fast path＋LinUCB/weighted＋quarantine/exclusion sets | tenant-namespaced sessions; constraint re-check; failed-set exclusion; table caps (A7) |
| bandit | `bandit.rs` | LinUCB d=4 alpha 0.4 contextual arms | select <200ns (release assert); tiered forgetting; risk premium |
| pool/prewarm | `pool.rs` | liveness＋wave prewarming | 100/wave serial waves; semaphore-capped FDs |
| prober | `prober.rs` | three-path liveness＋Client reuse＋eviction | per-proxy Client cache; skip on closed semaphore |
| breaker | `circuit_breaker.rs` | failure quarantine＋Redis persist＋PubSub fanout | remote-failure counts＋warn＋single retry |
| telemetry | `telemetry.rs` | MPSC ring＋batched XADD＋one retry | exact drop accounting (queued/ser_failed); channel-full counter |
| arbitrage | `vendor_arbitrage.rs` | 9-way CH concurrent SLA audit＋weight/hold | 5s per-query timeout; failures hold |
| sink pump | `ch_sink.rs` | Stream→CH resident＋consumer group＋dedup window | batch 5000/1s; hold-ack till CH recovers |
| free pool | `free_pool.rs` | fetch→verify→merge second line | prefilter/pages/multi-source/neg-cache/weighted-cap/fuses/intake cap |
| SOCKS bridge | `socks_bridge.rs`/`socks_handshake.rs` | socks4/5 handshake＋translated egress | allowlist header validation; overall deadline; bridge error ledger |
| profiling | `geo.rs`/`fingerprint.rs` | GeoIP observe/enforce＋Chrome header align | Disabled degradation without DB; mismatch switch |
| tenant | `tenant.rs` | auth＋QPS/concurrency＋billing | CAS slot release (no wraparound); block on next auth when broke |
| metrics | `metrics.rs` | full render＋sampled logging | 30+ series; saturating histogram sums |
| main assembly | `main.rs` | env wiring＋supervise＋signals＋sync entry | S1 sync main; S5 stop broadcast; tunable grace |

### 4. Deployment Topology

```
Host (Windows dev / Linux prod)
├── gateway binary :8916 (D3 default 127.0.0.1; containers override 0.0.0.0) + :9091/metrics
├── mocks :8888/8889/8890 (devtools, absent in prod)
├── adaptor :18080 (localhost only, shape translation)
└── docker compose (4 deps + optional profiles)
    ├── redis:6379 (127.0.0.1-bound + requirepass; Stream/PubSub/session/quarantine)
    ├── clickhouse:8123/9010 (warehouse, 90-day TTL)
    ├── prometheus:9090 (15s scrape + 5 rules)
    ├── grafana:3000 (7 panels via provisioning)
    ├── [profile gateway] containerized gateway (needs network to build)
    └── [profile observability] redis-exporter + alertmanager
```

### 5. Key Decisions

1. **Pingora 0.6** (0.4 breaks under rustc 1.93): proxy/lb/rustls trio, frontend-compatible.
2. **Sync main (S1)**: calling run_forever inside `#[tokio::main]` drops runtimes in async context → panic; changed to sync entry＋block_on assembly＋main-thread run_forever. Windows still recycles per grace (no main_loop in 0.6); watchdog covers it; long-run on Linux.
3. **D3 tightening**: key gate on by default＋loopback listen; dev key `default_key`; SDK/scripts carry it; rollback documented.
4. **Anonymous free-line zero trust**: free tier refuses Authorization/Cookie (D1 403); transparent nodes filtered by REQUIRE_ELITE.
5. **BOM-less LF＋Chinese .ps1 misparsed by local PS5.1**: BOM added repo-wide; inline literals kept (root-cause lesson).

### 6. Non-Goals

CONNECT tunneling (501), uTLS/boring customization, eBPF/io_uring/MASQUE, BGP steering, hard multi-tenant isolation (namespace-level soft isolation suffices).
