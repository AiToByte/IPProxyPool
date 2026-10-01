# 功能说明文档 / Feature Catalog

> 双语：中文在前，English after.
> 口径基线：D3（Key 门默认开＋回环监听）、274 单测＋4 真 live（权威数字见 [`QUALITY_BASELINE.md`](QUALITY_BASELINE.md)）。每项按“是什么／为什么／怎么看／配置项”四段写。

## 中文

### F1 智能选路

- **是什么**：LinUCB（d=4，alpha 0.4）上下文选臂＋加权随机＋粘滞 fast path＋域级隔离/熔断＋失败集排除＋租户命名空间。
- **为什么**：付费节点质量分化（延迟/成功率/国家），静态轮询浪费好节点；粘滞保证会话亲和（支付/登录场景）；隔离防止坏节点拖死整域。
- **怎么看**：`bandit` 单测 12 项（release `<200ns` 断言）；`X-Proxy-Session` 同值两次命中同节点（curl/SDK 自检）；`quarantine:{domain}:{ip}` Redis 键；`free_pool_nodes_by_proto` 水位。
- **配置项**：`X-Proxy-Country/Session/Tier/Proto` 请求头；Basic 用户名嵌 `country-/session-/tier-` token 兼容。

### F2 鉴权计费

- **是什么**：网关 Key 门（D3 缺省开，无头 403）→租户鉴权（坏 Key 403／欠费 402／超限 429）→QPS/并发节流→按字节×tier 单价计量。
- **为什么**：企业多租户分账；无头流量默认拒绝（D3），误伤面为零（正常客户端都带 Key）。
- **怎么看**：`proxy_requests_total{status}` 四分类计数；`gateway_transferred_bytes_total`；`tenant.total_bytes × tier 单价` 周对账（OPERATION §3）。
- **配置项**：`REQUIRE_API_KEY=0` 显式关闭；租户注册在 main（默认 `default_key` 宽限额）。

### F3 免费第二供应线

- **是什么**：Geonode（api，分页预筛）＋free-proxy-list（html）＋clarketm/openproxylist/monosans/relayglass（github raw，共 7 URL）→TCP 初筛→FullCheck 复检（Elite/Anonymous/Transparent 三级＋canary）→merge 进池（tier=free 隔离档）。
- **为什么**：零成本补充 marrange 出口；Elite 偶发但真实（用户实操 104.245.245.218 三重证据）。
- **怎么看**：`free_pool_nodes_total` 水位（常态 0 属源站生态）；`free_pool_source_yield/elite_total{source}`；`free_pool_verify_total{result}` 四组漏斗＋B9 四指标；`FREE_BASELINE.md` 干旱记录。
- **配置项**：`FREE_ENABLED=1` 总开关；`FREE_API_PAGES`；`FREE_REQUIRE_ELITE`；`FREE_FETCH_TIMEOUT_SECS`/`FREE_INTAKE_FACTOR`；`FREE_FULL_CHECK_URLS` 多基址。

### F4 SOCKS 出站

- **是什么**：显式 `X-Proxy-Proto: socks5/socks4` 请求经翻译桥出站（socks_handshake 握手＋reqwest 执行＋响应合成回下游，logging/计量/遥测全复用）。
- **为什么**：免费 Elite 多为 socks5；标准 HTTP_PROXY 形态经适配器 200 可用。
- **怎么看**：`free_pool_nodes_by_proto`；`socks_bridge_errors_total`；`by_proto` 漏斗；E2 多语言 SDK 同语义。
- **配置项**：`SOCKS_BRIDGE_TIMEOUT_SECS`（默认 20）／`SOCKS_MAX_BODY_BYTES`（默认 10MB）；整轮总预算＝单跳＋8s（A9）。

### F5 指纹硬化与画像

- **是什么**：Chrome 124+ 头对齐＋代理/拓扑头 scrub；GeoIP（MaxMind）观察与 mismatch 执法开关；匿名度 canary。
- **为什么**：防上游按指纹歧视；执法只在库 Live＋无误报后开（默认观察）。
- **怎么看**：`geoip_lookups_total{result}`；`geoip_mismatch_total`（只收实锤）；`[GeoIP] live DB loaded` 启动行。
- **配置项**：`GEOIP_MMDB_PATH`；`GEOIP_ENFORCE_MISMATCH`（默认 0）；`deploy/geoip_update.py` 每周三更新。

### F6 遥测数仓

- **是什么**：MPSC ring（1 万）→批量 XADD→Redis Stream（`stream:proxy:telemetry`）→常驻泵→ClickHouse（90 天 TTL）→Grafana。
- **为什么**：全链路审计与 SLA 对账（vendor×country 5 分钟窗成功率）。
- **怎么看**：`XLEN`（消费组 lag=0 健康）；`proxy.proxy_telemetry_log` 行数随流量涨；`telemetry_dropped_total`（精确口径）；`[ChSink] landed N rows`。
- **配置项**：batch 5000/1s（代码常量）；CH 连接 env 四件套。

### F7 套利调权

- **是什么**：9 路 CH SLA 并发审计（单路 5s 超时）→ vendor×country 权重 0/50/100 分档；失败 hold 不摘除；免费独立因子表 merge 应用。
- **为什么**：质量差的供应商自动降权，恢复自动回权，无需人工摘挂。
- **怎么看**：`[Arbitrage] vendor=.. weight->N (rate=..%)` 日志；`free_pool_source_elite_total`；权重查询 Redis/快照。
- **配置项**：`ARBITRAGE_INTERVAL_SECS`（默认 60）。

### F8 韧性自愈

- **是什么**：supervise（9 worker 全覆盖，指数 backoff 1s→60s，健康 60s 复位，stop 广播可停）＋进程外看护（30s 轮询＋3-strike＋退避重拉）＋Redis/CH/Mock 断电演练全自愈。
- **为什么**：Windows panic/有序退出约数分钟一次；看护保证 30s 级自恢复；生产跑 Linux。
- **怎么看**：`supervisor_restarts_total{worker}`；`log/ipp-watchdog.out`；kill 演练（C1：两次 kill 均重拉回 200）。
- **配置项**：`GATEWAY_GRACE_SECS`（默认 300；S1 后为清洁退出周期）；schtasks 开机自启（管理员注册）。

### F9 安全面

- **是什么**：D1 free 匿名出口拒收 Authorization/Cookie（403）；D3 Key 门＋回环监听；SSRF 护栏（抓取仅 http/https，file/dict/gopher 过滤；复检基址 https-only）；局域网端口全绑回环＋Redis 密码。
- **为什么**：免费陌生出口＋凭据＝泄露；默认安全（secure by default），放行靠显式覆写。
- **怎么看**：D1 四断言 live（403/403/503/200）；`request_filter` 单测；OPERATION D2 节。
- **配置项**：回退口径 `REQUIRE_API_KEY=0`＋`GATEWAY_ADDR=0.0.0.0:8916`（应急用）。

### F10 运维面

- **是什么**：一键启停（`ipp.ps1`）＋前置适配器＋SDK（5 语言）＋PAC＋CI/live 双 workflow＋Prometheus 5 规则＋Grafana 7 面板＋备份/恢复脚本＋CHANGELOG。
- **为什么**：10 分钟从零到可观测；合入前拦截漂移；灾时按手册恢复。
- **怎么看**：`ipp.ps1 status` 九行；CI 绿；`backup/<stamp>/` 四件套；`tools/restore.ps1 --dry-run`。
- **配置项**：`.env.example`→`.env`；compose profiles（gateway/observability）。

### 相关文档 / Related docs

- 架构全景见 [`SYSTEM-ARCHITECTURE.md`](SYSTEM-ARCHITECTURE.md)；数据怎么流见 [`DATAFLOW.md`](DATAFLOW.md)；上手操作见 [`USER-GUIDE.md`](USER-GUIDE.md)；开源与依赖见 [`OPEN-SOURCE.md`](OPEN-SOURCE.md)。

## English

### F1 Smart Routing

- **What**: LinUCB (d=4, alpha 0.4) contextual arms＋weighted random＋sticky fast path＋domain quarantine/breaker＋failed-set exclusion＋tenant namespaces.
- **Why**: paid nodes vary (latency/success/country); static round-robin wastes good nodes; stickiness keeps session affinity; quarantine stops one bad node killing a domain.
- **Observe**: 12 bandit tests (release `<200ns` assert); same `X-Proxy-Session` twice hits same node; `quarantine:{domain}:{ip}` keys; `free_pool_nodes_by_proto` levels.
- **Config**: `X-Proxy-Country/Session/Tier/Proto` headers; Basic-username embedded `country-/session-/tier-` tokens.

### F2 Auth & Billing

- **What**: gateway key gate (D3 on by default, headerless 403)→tenant auth (bad key 403 / broke 402 / limited 429)→QPS/concurrency throttle→bytes×tier metering.
- **Why**: enterprise multi-tenant billing; headerless traffic denied by default with zero false-positive surface.
- **Observe**: `proxy_requests_total{status}` four classes; `gateway_transferred_bytes_total`; weekly `tenant.total_bytes × tier price` reconciliation.
- **Config**: `REQUIRE_API_KEY=0` to disable; tenants registered in main (`default_key` wide limits).

### F3 Free Second Line

- **What**: Geonode (api, paged prefilter)＋free-proxy-list (html)＋clarketm/openproxylist/monosans/relayglass (github raw, 7 URLs)→TCP pre-check→FullCheck re-verify (Elite/Anonymous/Transparent＋canary)→merge (tier=free isolated).
- **Why**: zero-cost extra exits; Elite is sporadic but real (user demo triple evidence).
- **Observe**: `free_pool_nodes_total` (0 is normal ecology); per-source yield/elite; verify funnel＋B9 four metrics; `FREE_BASELINE.md` drought log.
- **Config**: `FREE_ENABLED=1`; `FREE_API_PAGES`; `FREE_REQUIRE_ELITE`; `FREE_FETCH_TIMEOUT_SECS`/`FREE_INTAKE_FACTOR`; `FREE_FULL_CHECK_URLS`.

### F4 SOCKS Egress

- **What**: explicit `X-Proxy-Proto: socks5/socks4` requests exit via translation bridge (handshake＋reqwest execute＋synthesized response; logging/metering/telemetry reused).
- **Why**: most free Elites are socks5; standard HTTP_PROXY shape works via adaptor at 200.
- **Observe**: `free_pool_nodes_by_proto`; `socks_bridge_errors_total`; by_proto funnel; E2 SDKs share semantics.
- **Config**: `SOCKS_BRIDGE_TIMEOUT_SECS` (20)／`SOCKS_MAX_BODY_BYTES` (10MB); round budget＝per-hop＋8s (A9).

### F5 Fingerprint & Profiling

- **What**: Chrome 124+ header align＋proxy/topology header scrub; GeoIP (MaxMind) observe＋mismatch enforcement switch; anonymity canary.
- **Why**: avoid upstream fingerprint discrimination; enforce only with live DB and zero false positives (observe by default).
- **Observe**: `geoip_lookups_total{result}`; `geoip_mismatch_total` (confirmed only); `[GeoIP] live DB loaded` boot line.
- **Config**: `GEOIP_MMDB_PATH`; `GEOIP_ENFORCE_MISMATCH` (default 0); weekly `deploy/geoip_update.py`.

### F6 Telemetry Warehouse

- **What**: MPSC ring (10k)→batched XADD→Redis Stream→resident pump→ClickHouse (90-day TTL)→Grafana.
- **Why**: full-path audit and SLA reconciliation (vendor×country 5-min success windows).
- **Observe**: `XLEN` (consumer lag=0 healthy); row count grows with traffic; `telemetry_dropped_total` (exact); `[ChSink] landed N rows`.
- **Config**: batch 5000/1s (code const); CH connection env quartet.

### F7 Arbitrage Weights

- **What**: 9-way CH concurrent SLA audit (5s per-query timeout)→vendor×country weights 0/50/100; failures hold; free factor table merged.
- **Why**: bad vendors auto-derated, auto-restored, no manual pinning.
- **Observe**: `[Arbitrage] vendor=.. weight->N (rate=..%)` logs; source elite totals; weight snapshot.
- **Config**: `ARBITRAGE_INTERVAL_SECS` (60).

### F8 Resilience

- **What**: supervise (9 workers, exp backoff 1s→60s, healthy-60s reset, stop broadcast)＋out-of-process watchdog (30s probe＋3-strike＋backoff relaunch)＋Redis/CH/Mock outage drills all self-heal.
- **Why**: Windows panic/orderly exits every minutes; watchdog guarantees ~30s recovery; prod on Linux.
- **Observe**: `supervisor_restarts_total{worker}`; `log/ipp-watchdog.out`; kill drills (C1: two kills both relaunched to 200).
- **Config**: `GATEWAY_GRACE_SECS` (300); schtasks autostart (admin register).

### F9 Security

- **What**: D1 free anonymous egress refuses Authorization/Cookie (403); D3 key gate＋loopback; SSRF guards (fetch http/https only; https-only check bases); LAN ports loopback-bound＋Redis password.
- **Why**: stranger free exits＋credentials＝leak; secure by default, open by explicit override.
- **Observe**: D1 four live asserts (403/403/503/200); `request_filter` tests; OPERATION D2.
- **Config**: rollback `REQUIRE_API_KEY=0`＋`GATEWAY_ADDR=0.0.0.0:8916` (emergency).

### F10 Operations

- **What**: one-click ops (`ipp.ps1`)＋adaptor＋SDKs (5 langs)＋PAC＋CI/live workflows＋5 Prometheus rules＋7 Grafana panels＋backup/restore＋CHANGELOG.
- **Why**: zero→observable in 10 minutes; drift blocked pre-merge; disaster recovery by handbook.
- **Observe**: `ipp.ps1 status` nine lines; CI green; `backup/<stamp>/` four artifacts; `tools/restore.ps1 --dry-run`.
- **Config**: `.env.example`→`.env`; compose profiles (gateway/observability).
