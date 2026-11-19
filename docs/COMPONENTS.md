# Components 组件说明

> 双语：中文在前，English after. Bilingual: Chinese first, English second.
> 数据来源：`gateway/src/`。模块集合、每行「单测」数、标题里的模块计数，以及
> 每个「关键 API」标识符是否真实存在，全部由
> `tools/check_components_docs.py` 在 CI 断言——**模块集合与 API 列不需要手抄，也不能手改**。
> 单测列需随源码测试变更同步；本表合计含当前测试属性口径，权威通过/忽略测试数见 `QUALITY_BASELINE.md`。
> Data source: `gateway/src/`. Module set, per-row test counts, heading counts,
> and the existence of every "Key APIs" identifier are asserted in CI by
> `tools/check_components_docs.py`.
>
> 为何没有「行数」列：任何一行代码改动都会让它过期。设精确门禁则每个 PR 假红，
> 不设则它长期是假的——保留一个无法维护的数字等于保留一句对外失实。
> 体量信号改由「单测」承担（它被门禁守住，且合计与 `QUALITY_BASELINE.md` 自洽）。
>
> `startup_config.rs` 是新增加的纯启动配置辅助模块；当前表格单列其全部解析与环境采集测试，测试计数以源码和 `QUALITY_BASELINE.md` 为准。

## 中文

### 1. 模块表（27 个）

| 模块 | 单测 | 职责 | 关键 API |
|------|------|------|----------|
| `main.rs` | 25 | 装配：typed startup preflight、启动环境采集、supervisor/后台 ticker/Pingora 启动/平台感知优雅期 | `env_str/env_secs/split_env_list/supervise/clamp_free_ttl/default_grace_secs` |
| `gateway/mod.rs` | 44 | 数据面：鉴权/选路/转发/重试/计量/遥测/桥短路 | `SmartProxyGateway/should_reject_missing_api_key/resolve_bandit_context/status_for_auth_error` |
| `connect.rs` | 27 | 可选标准 HTTP/1.1 CONNECT：authority 解析、鉴权、独立 listener、双向隧道 | `ConnectTarget/parse_connect_authority/ConnectServerApp` |
| `router.rs` | 73 | 选路：加权/粘滞/隔离/原子替换/套利缩放/主动健康探测闸门 | `RouterEngine/NodeFilter/normalize_domain/ProbeFailureOutcome` |
| `model.rs` | 0 | 模型：节点/规格/上下文＋tier 归一 | `ProxyNode/canonical_tier/RoutingSpec/ProxyContext/EgressProto` |
| `free_pool/mod.rs` | 59 | 免费第二线：抓取/两级质检/注册/熔断/合并 | `FreePoolWorker/Source/Verifier/FreeProto/is_routable_ip` |
| `bandit.rs` | 18 | LinUCB d=4 选路＋分档遗忘＋免费风险溢价 | `forget_every_for_tier/cost_weight_for_tier/FREE_RISK_PREMIUM` |
| `telemetry.rs` | 7 | 遥测：MPSC 批量 XADD＋幂等＋双丢弃计数 | `TelemetryPublisher/TelemetryWorker/TelemetryEvent` |
| `circuit_breaker.rs` | 13 | 熔断：Stream 消费＋隔离＋目标合法性守卫 | `PassiveCircuitBreaker/parse_delta_message/valid_quarantine_target/quarantine_ttl_for_status` |
| `ch_sink.rs` | 11 | 数仓泵：游标分页认领/去重/批量落库/hold 重放 | `ChSinkWorker/parse_entry/SeenIds` |
| `analytics.rs` | 8 | 落库查询：SLA 窗口＋注入守卫 | `AnalyticsEngine/sla_sql/normalize_sla_rate` |
| `tenant.rs` | 26 | 租户：鉴权/QPS/并发/计费（Free $0） | `TenantManager/price_per_gb/resolve_api_key/is_loopback_listen` |
| `vendor_arbitrage.rs` | 5 | 套利：付费 80/95＋免费 50/80 分档 | `arbitrage_action/free_pool_action/VendorArbitrageWorker` |
| `startup_config.rs` | 6 | 启动配置：typed preflight、Admin/Gateway/Metrics/CONNECT/Redis/TenantStore/API key 解析与非 Unicode 错误拒绝、脱敏诊断 | `StartupConfigInput/StartupConfig/parse_startup_config` |
| `metrics/mod.rs` | 29 | 指标：:9091 exposition，新增 CONNECT admission、字节与隧道寿命指标 | `MetricsRegistry/FreePoolSnapshot/BreakerSnapshot` |
| `prober.rs` | 7 | 存活探针：双源降级＋Client 池＋闲置淘汰 | `CanaryProber/extract_exit_ip/ProbeResult` |
| `pool.rs` | 10 | 预热：建链/握手探测＋信号量＋票据 | `ConnectionPrewarmer/WarmStats` |
| `fingerprint.rs` | 2 | 指纹：Chrome 8 头对齐＋传输选项 | `FingerprintHardener/TCP_WINDOW_BYTES` |
| `geo.rs` | 4 | 画像：GeoIP 查询＋mismatch 判定（无库 Disabled） | `GeoDb/geo_verdict/GeoVerdict` |
| `socks_bridge.rs` | 8 | SOCKS 翻译桥：显式 socks 请求出站＋hop 过滤＋body 上限 | `SocksBridge/BridgeRequest/BridgeResponse` |
| `socks_handshake.rs` | 12 | 握手：RFC1928/1929＋SOCKS4/4a＋greeting-only | `establish/greet_only` |
| `admin/mod.rs` | 24 | Admin REST 面：:9092 独立服务；公开 `/livez`/`/readyz` 健康探针；`/api/v1/*` 统一 `X-Admin-Key`、fail-closed | `AdminState/router/RuntimeHealthState` |
| `node_registry.rs` | 14 | 控制面节点注册表：运行时增删＋原子持久化（进程重启不丢） | `NodeRegistry/is_free_provider/path_from_env/RegistryError` |
| `janitor.rs` | 3 | 淘汰计数助手：四处 sweep 共用一份「淘汰并报数」逻辑 | `removed_count` |
| `redis_shard.rs` | 9 | Redis Stream 分片预留（默认 1 分片＝行为逐字节不变） | `RedisStreamSharder/TELEMETRY_STREAM_BASE` |
| `net_connector.rs` | 2 | 传输层连接器抽象：默认 tokio，Linux 预留 io_uring 接入点 | `TcpConnector/TokioTcpConnector` |
| `health.rs` | 9 | 纯 liveness/readiness 状态决策模型与共享运行时快照、Redis required/optional 策略：starting/ready/degraded/draining/stopped | `RuntimeHealthState/redis_dependency_facts/evaluate_readiness` |

**单元测试属性计数**：源码 `#[test]` / `#[tokio::test]` 总数为 455（其中 445 passed、10 ignored）；执行结果和忽略项以 `QUALITY_BASELINE.md` 为准。

### 1.1 `router` 内部子模块（OPT-R18 P2-1 拆分）

`router.rs` 曾是 3564 行的单文件巨型模块（选路＋粘滞＋隔离＋主动健康＋套利缩放
＋控制面增删混在一起）。P2-1 把它按职责拆为父模块 `router.rs`（结构定义＋构造器）
＋ 5 个子模块文件。**子模块的测试数已并入上表的 `router.rs` 行**（69 = 59 + 10），
故这张职责表目前有 27 行（含 `startup_config.rs` 与 `health.rs` 独立模块），`router` 的 7 个子模块仍折叠在父模块行中。

| 子模块 | 职责 | 关键 API |
|--------|------|----------|
| `router/select.rs` | 选路：候选过滤（tier/country/proto/隔离/主动健康）＋加权随机＋粘滞绑定 | `select_node_excluding/get_healthy_candidates_excluding/pick_weighted_streaming` |
| `router/health.rs` | 主动健康探测回灌：阈值判定＋安全阀＋冷却＋免费池抽样 | `record_probe_failure/probe_candidates/sweep_active_health` |
| `router/quarantine.rs` | 两级隔离表：归一 domain → ip → 到期时刻（零分配热路径） | `is_quarantined_normalized/set_quarantine` |
| `router/node_mgmt.rs` | 控制面：增删节点＋厂商调权＋原子换池（含僵尸健康条目清理） | `add_node/remove_node/replace_vendor_nodes` |
| `router/buckets.rs` | GW-6 分桶索引：按 (tier, country, proto) 建桶，供 O(1) 选路快路径 | `build_buckets` |
| `router/tests.rs` | `router` 的既有单测（原 `router.rs` 末尾 `mod tests` 迁出） | — |
| `router/health_tests.rs` | 主动健康单测：阈值/安全阀/粘滞失效/冷却/抽样/并发 | — |

**拆分是纯搬移**：方法逐字迁移，行为零变更；子模块经 `use super::*;` 继承父模块
的私有字段与自由函数，**不做 `pub(crate) use x::*` 再导出**——固有方法（inherent
method）调用方直接 `engine.foo()` 即可，再导出会触发 `unused_imports`（`use super::*`
的 glob-of-glob 不算"使用"）。

### 1.2 `free_pool` 内部子模块（OPT-R18 P2-1 第二批拆分）

`free_pool` 模块曾是 3727 行的单文件（抓取源＋两级质检＋注册表状态＋后台 worker
混在一起）。现按职责拆为父模块 `free_pool/mod.rs`（类型定义＋`shared_client`＋
再导出）＋ 5 个子模块。**子模块的测试数已并入上表的 `free_pool/mod.rs` 行**，职责表目前共 27 行，子模块仍不单独占行。

| 子模块 | 职责 | 关键 API |
|--------|------|----------|
| `free_pool/sources.rs` | 三类抓取源（JSON API / HTML / GitHub 裸列表）＋礼貌轮询缓存（ETag/304）＋全源并发抓取 | `ApiSource/HtmlSource/GitHubSource/fetch_all/API_MAX_PAGES` |
| `free_pool/verify.rs` | 两级质检：轻量 `Verifier`（TCP/协议）＋ `FullChecker`（canary 全量）＋匿名度分级 | `Verifier/FullChecker/classify_anonymity/egress_of/socks_target_of_base` |
| `free_pool/registry.rs` | 池状态：`Health`（EWMA）＋ `Registry`（TTL/淘汰/失败指纹）＋源站熔断＋intake 配额 | `Registry/Health/SourceGuard/cap_intake/intake_cap_limit/baseline_bases` |
| `free_pool/worker.rs` | 驱动层：env 组装、周期抓取→质检→geo 裁决→Router 合并 | `FreePoolWorker/FreePoolConfig/apply_geo_verdict/spawn_full_check` |
| `free_pool/tests.rs` | 全部单测（原文件后半段迁出） | — |

**与 `router` 拆分的两点差异**（都踩过）：
① `router` 保留 `router.rs` + `router/` 两处并存；`free_pool` 用 `free_pool/mod.rs`
单文件作父模块（父模块只剩类型与再导出，独立 `.rs` 反而多一层跳转）。故
`tools/check_components_docs.py` 必须同时认这两种写法——早期只 glob 顶层 `*.rs`，
`free_pool` 拆成目录模块后**整个模块从表里消失**、60+ 个单测也不再计入合计。
② 这里搬的是**类型与常量**，必须 `pub use` 再导出才能保住 `crate::free_pool::X`
路径（`router` 搬的是固有方法，不需要）；而被 `free_pool/tests.rs` 独用的再导出在非测试
构建下会被判 `unused_imports`，故加 `#[cfg_attr(not(test), allow(unused_imports))]`
——只放宽非测试构建，测试构建下仍必须真被用上。

### 1.3 `gateway` 内部子模块（OPT-R18 P2-1 第三批拆分）

`gateway` 模块曾 2275 行的单文件，把**四类职责**混在一起：鉴权/分档的纯判定、
LinUCB 选路上下文、上游 peer 装配、SOCKS 桥转发，外加 40 个单测。现按职责拆为
父模块 `gateway/mod.rs`（结构体＋追踪 ID＋`parse_routing_spec`＋再导出）＋ 6 个子模块。
**子模块的测试数已并入上表的 `gateway/mod.rs` 行**；组件职责表目前共 27 行，子模块仍折叠在父模块行中。

| 子模块 | 职责 | 关键 API |
|--------|------|----------|
| `gateway/policy.rs` | 纯判定：环境门、鉴权错误码映射、free 档凭据门、租户会话命名 | `should_reject_missing_api_key/status_for_auth_error/free_node_exits_with_credentials/apply_tenant_namespace` |
| `gateway/bandit.rs` | LinUCB：取臂、上下文解析、分档配额选路、游离臂修剪、重试记账 | `arm_for/resolve_bandit_context/prune_stale_arms/tier_quota_forced_tier/select_bandit_node_excluding` |
| `gateway/upstream.rs` | 上游装配：TLS/回环判定、`HttpPeer` 构造、桥出站 URL、桥预算 | `build_http_peer/bridge_url_for/is_loopback_addr/is_tls_for_node/should_skip_bridge_node/socks_overall_budget` |
| `gateway/socks.rs` | SOCKS 桥：候选选择、转发主循环、桥响应回写 | `pick_socks_candidate/serve_via_socks/write_bridge_response` |
| `gateway/hooks.rs` | `impl ProxyHttp`：Pingora 的 9 个生命周期回调 | `request_filter/upstream_peer/proxy_upstream_filter/upstream_request_filter/logging` |
| `gateway/tests.rs` | `gateway` 的全部单测（原文件后半段迁出） | — |

**为什么 `impl SmartProxyGateway` 分散在 bandit/socks 两处**：Rust 允许同一类型的
固有 impl 分布在同 crate 的任意模块。这些方法操作的是「选路」与「SOCKS 桥」两件
不同的事，按职责就近放置比硬塞进 `gateway/mod.rs` 更可读。构造函数不在本模块——
`SmartProxyGateway` 由 `main.rs` 用结构体字面量装配。

**拆分顺带修掉一处文档错挂**：原 `gateway` 单文件里 `build_http_peer` 的说明
（「P2：HttpPeer 装配（含 socks 守卫）」）被粘在 `is_tls_for_node` 上方，导致
`is_tls_for_node` 带着完全无关的说明、而 `build_http_peer` 反而没有文档。
搬迁时已各归各位。

### 1.4 `metrics` 内部子模块（OPT-R18 P2-1 第四批拆分）

`metrics` 模块曾 2097 行的单文件。切分轴是**数据流方向**，不是指标族：

| 子模块 | 职责 | 关键 API |
|--------|------|----------|
| `metrics/record.rs` | **写入侧**：`observe*` / `note_*` / `set_*`，数据面与后台工人调用 | `observe/observe_stage/note_free_verify/note_breaker_trigger/mark_sink_ok/set_free_pool_supply` |
| `metrics/render.rs` | **读出侧**：Prometheus 文本渲染＋HTTP 服务 | `render/serve_metrics` |
| `metrics/tests.rs` | 全部单测 | — |

**为什么不按指标族切**（免费线 / 熔断 / geo / 探测各一个文件）：这些族的记录方法在
impl 里是**交织**的——免费线那组散落在 6 个不相邻区间。按族切会把一个 impl 拆成
十几段残片，读者要跳 6 个文件才能看完一个指标族的生命周期，反而更难读。
而「写入 vs 读出」是一次干净的一刀：写侧只改原子量、读侧只读原子量，两侧唯一的
契约是**指标名与标签集**（写进去的每个字段，必须在 `render()` 里有对应
`# HELP`/`# TYPE` 行，否则指标写了却抓不到——静默失效）。

**黄金快照当场抓出的真实缺陷**：`proxy_stage_duration_ms` 的 `# HELP`/`# TYPE` 被写在
per-stage 循环**内部**，3 个 stage 输出 3 份。Prometheus 文本格式规定同一指标名只能有
一条 `# HELP`；违反时抓取端**解析失败**，后果不是丢这一个指标，而是**整次抓取作废**
（该实例所有指标一起丢）。用官方 `promtool check metrics` 实测确认
（`second HELP line for metric name "proxy_stage_duration_ms"`），已把 HELP/TYPE 提到
循环外；当时 promtool 已确认重复 HELP 问题消失（仅剩 `_total` 后缀、`_ms` 缩写等命名
lint——那是既有对外契约，改名会破坏面板与告警规则，不动）。随后新增的
`render_is_valid_prometheus_text` 又抓出第二处错配：出口样本名和 Dashboard 都用
`free_pool_exit_total`，但 HELP/TYPE 误写成 `free_exit_total`，所以样本找不到自己的
TYPE 声明。已把 HELP/TYPE 改为 `free_pool_exit_total`；同时要求 HELP 与 TYPE 名称集合
一致，并验证每条样本都归属已声明的类型。`each_metric_has_single_help_and_type`
仍负责重复头行检查；三条测试（黄金快照 / 单头断言 / 完整 exposition 校验）互补。

**顺带归一了一处约定不一致**：原文件里有 **4 个 `#[test]` 直接写在模块级**
（`opt_r9_c2_*`，不在 `mod tests` 内），与其余 40 个测试两套写法。两者都合法
（`#[test]` 隐含 `cfg(test)` 门控），但同一个文件两套约定会让读者以为"模块级那些
是特殊测试"。拆文件时统一收进 `metrics/tests.rs`。

**`render()` 已拆为 7 段渲染器**（2026-11-01 后续步骤）：原 538 行单函数现只剩
「按序调用」10 行，7 段各一个私有方法（请求与探测 / 延迟 / 熔断 / 遥测 / 免费线供给 /
GeoIP / 免费线漏斗）。**段序即输出序**，调整会改变抓取端看到的行序。

拆分的安全网是**黄金快照测试**（`golden_render_full_exposition`）：用确定性状态渲染
整篇输出并**逐字节**断言（215 行）。它当场抓出一个真实缺陷 —— 见下。

### 1.5 `admin` 内部子模块（OPT-R18 P2-1 第五批拆分）

`admin` 模块曾 1791 行，结构上是"**一个 800 行的 `build_router_with`**"——路由表、
中间件、16 个 REST 处理器全挤在同一段区间，改一个端点要在 1200 行里定位。现按职责拆为
父模块 `admin/mod.rs`（新增公开探针并保留 admin auth），其子模块测试数已并入表中该行。

| 子模块 | 职责 | 关键 API |
|--------|------|----------|
| `admin/dto.rs` | 请求/响应 DTO 与错误体构造；领域→传输的唯一转换点 | `ErrorResponse/error_response/NodeView/node_view/AddNodeRequest/ApiError` |
| `admin/mw.rs` | 三层中间件：访问审计、认证（fail-closed）、未匹配端点 404 | `audit_logging/auth_middleware/api_not_found_fallback` |
| `admin/handlers_node.rs` | 节点：列举/新增/详情/删除/调权/隔离/解隔离 | `list_nodes_handler/add_node_handler/patch_node_weight_handler/post_node_quarantine_handler` |
| `admin/handlers_tenant.rs` | 租户：列举/详情/余额/启停 | `list_tenants_handler/get_tenant_handler/patch_tenant_balance_handler` |
| `admin/handlers_ops.rs` | 运维面：健康、OpenAPI、免费池看板、断路器 | `health_handler/openapi_handler/list_free_pool_summary_handler/get_circuit_handler` |
| `admin/tests.rs` | `admin` 的全部单测（原文件后半段） | — |

**为什么按"资源"切处理器而不是按 HTTP 方法切**：读者查"删除节点会发生什么"时，
需要一次看到 `delete_node_handler` 与相邻的 `delete_node_quarantine_handler`——它们
共享同一套「先 `get_node_by_addr` 判存在（否则 404）再动作」的前置逻辑。按方法切会把
GET/PATCH/DELETE 拆到三个文件，反而要来回跳。

**为什么中间件模块叫 `mw` 而不是 `middleware`**：`axum::middleware` 已在父模块导入
（`middleware::from_fn` / `from_fn_with_state`），同名子模块会与之冲突（实测 E0255
「name defined multiple times」）。改名比给 axum 那个起别名更不容易误读。

**为什么 `node_view` 留在 `admin/dto.rs`**：它是领域对象→传输对象的唯一转换点，三个处理器
模块都要用。放在 `admin/dto.rs` 里保证**序列化形状只有一处定义**，改字段不会出现"某个端点忘了改"
的漂移。它与其搭档 `registry_error_response` 用 `pub(super)` 暴露给兄弟模块
（`pub(super)` 不能 `pub use` 再导出，实测 E0364，故处理器侧显式 `use super::dto::{…}`）。

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

Redis 7（6379）/ ClickHouse 24（8123）/ Prometheus 2.53（9090）/ Grafana 11.1（3000）；网关为宿主二进制（:8916 数据面，:9091 指标，:9092 Admin）。

## English

### 1. Modules (27)

| Module | Tests | Responsibility | Key APIs |
|--------|-------|----------------|----------|
| `main.rs` | 25 | Wiring: typed startup preflight/env binding/supervisor/background tickers/Pingora bootstrap/platform-aware grace | `env_str/env_secs/split_env_list/supervise/clamp_free_ttl/default_grace_secs` |
| `gateway/mod.rs` | 44 | Data plane: auth/routing/forwarding/retry/metering/telemetry/bridge short-circuit | `SmartProxyGateway/should_reject_missing_api_key/resolve_bandit_context/status_for_auth_error` |
| `connect.rs` | 27 | Optional standard HTTP/1.1 CONNECT: authority parsing, auth, standalone listener, bidirectional tunnel | `ConnectTarget/parse_connect_authority/ConnectServerApp` |
| `router.rs` | 73 | Routing: weighted/sticky/quarantine/atomic swap/arbitrage scaling/active-health gate | `RouterEngine/NodeFilter/normalize_domain/ProbeFailureOutcome` |
| `model.rs` | 0 | Models: node/spec/context + tier canonicalization | `ProxyNode/canonical_tier/RoutingSpec/ProxyContext/EgressProto` |
| `startup_config.rs` | 6 | Startup configuration: typed preflight, Admin/Gateway/Metrics/CONNECT/Redis/TenantStore/API key parsing, non-Unicode rejection, and sanitized diagnostics | `StartupConfigInput/StartupConfig/parse_startup_config` |
| `free_pool/mod.rs` | 59 | Free 2nd line: fetch/two-stage verify/registry/circuit-break/merge | `FreePoolWorker/Source/Verifier/FreeProto/is_routable_ip` |
| `bandit.rs` | 18 | LinUCB d=4 routing + per-tier forgetting + free risk premium | `forget_every_for_tier/cost_weight_for_tier/FREE_RISK_PREMIUM` |
| `telemetry.rs` | 7 | Telemetry: MPSC batched XADD + idempotency + dual drop counters | `TelemetryPublisher/TelemetryWorker/TelemetryEvent` |
| `circuit_breaker.rs` | 13 | Breaker: stream consume + quarantine + target-validity guard | `PassiveCircuitBreaker/parse_delta_message/valid_quarantine_target/quarantine_ttl_for_status` |
| `ch_sink.rs` | 11 | Warehouse pump: cursor-paged claim/dedup/batch land/hold replay | `ChSinkWorker/parse_entry/SeenIds` |
| `analytics.rs` | 8 | Warehouse queries: SLA window + injection guard | `AnalyticsEngine/sla_sql/normalize_sla_rate` |
| `tenant.rs` | 26 | Tenants: auth/QPS/concurrency/billing (Free $0) | `TenantManager/price_per_gb/resolve_api_key/is_loopback_listen` |
| `vendor_arbitrage.rs` | 5 | Arbitrage: paid 80/95 + free 50/80 bands | `arbitrage_action/free_pool_action/VendorArbitrageWorker` |
| `metrics/mod.rs` | 29 | Metrics: :9091 exposition, including CONNECT admission, bytes, and tunnel lifetime | `MetricsRegistry/FreePoolSnapshot/BreakerSnapshot` |
| `prober.rs` | 7 | Liveness probe: dual-source fallback + client pool + idle eviction | `CanaryProber/extract_exit_ip/ProbeResult` |
| `pool.rs` | 10 | Prewarm: connect/handshake probe + semaphore + tickets | `ConnectionPrewarmer/WarmStats` |
| `fingerprint.rs` | 2 | Fingerprint: Chrome 8-header alignment + transport opts | `FingerprintHardener/TCP_WINDOW_BYTES` |
| `geo.rs` | 4 | Profiling: GeoIP lookup + mismatch verdict (Disabled w/o DB) | `GeoDb/geo_verdict/GeoVerdict` |
| `socks_bridge.rs` | 8 | SOCKS egress bridge: explicit-socks requests + hop filter + body cap | `SocksBridge/BridgeRequest/BridgeResponse` |
| `socks_handshake.rs` | 12 | Handshake: RFC1928/1929 + SOCKS4/4a + greeting-only | `establish/greet_only` |
| `admin/mod.rs` | 24 | Admin REST surface: standalone :9092, public `/livez`/`/readyz` probes, `/api/v1/*` behind `X-Admin-Key` (fail-closed) | `AdminState/router/RuntimeHealthState` |
| `node_registry.rs` | 14 | Control-plane node registry: runtime add/remove + atomic persistence (survives restart) | `NodeRegistry/is_free_provider/path_from_env/RegistryError` |
| `janitor.rs` | 3 | Eviction-count helper: one shared "evict and report" path for four sweeps | `removed_count` |
| `redis_shard.rs` | 9 | Redis Stream sharding scaffold (1 shard by default = byte-identical behaviour) | `RedisStreamSharder/TELEMETRY_STREAM_BASE` |
| `net_connector.rs` | 2 | Transport connector abstraction: tokio today, io_uring hook on Linux | `TcpConnector/TokioTcpConnector` |
| `health.rs` | 9 | Pure liveness/readiness decision model, shared runtime snapshot and Redis required/optional policy: starting/ready/degraded/draining/stopped | `RuntimeHealthState/redis_dependency_facts/evaluate_readiness` |


`startup_config.rs` is a pure startup parser helper, including environment collection and invalid-Unicode rejection. The responsibility table also includes `health.rs`, which owns only pure readiness decisions; HTTP wiring remains a separate approved slice.

### 1.1 `router` internal submodules (OPT-R18 P2-1 split)

`router.rs` used to be a 3564-line single-file mega-module (routing + sticky +
quarantine + active health + arbitrage scaling + control-plane mutation all mixed
together). P2-1 splits it by responsibility into the parent module `router.rs`
(structure + constructors) plus 5 submodule files. **Submodule tests are folded
into the `router.rs` row above** (62 = 52 + 10), so the module table stays at 24
rows — readers build their mental model by responsibility, and 7 rows would
mislead them.

| Submodule | Responsibility | Key APIs |
|-----------|----------------|-----------|
| `router/select.rs` | Selection: candidate filtering (tier/country/proto/quarantine/active health) + weighted random + sticky binding | `select_node_excluding/get_healthy_candidates_excluding/pick_weighted_streaming` |
| `router/health.rs` | Active-health probe feedback: threshold decision + safety valve + cooldown + free-pool sampling | `record_probe_failure/probe_candidates/sweep_active_health` |
| `router/quarantine.rs` | Two-level quarantine table: normalized domain → ip → expiry (zero-allocation hot path) | `is_quarantined_normalized/set_quarantine` |
| `router/node_mgmt.rs` | Control plane: add/remove node + vendor reweight + atomic pool swap (incl. zombie health cleanup) | `add_node/remove_node/replace_vendor_nodes` |
| `router/buckets.rs` | GW-6 bucket index: bucket by (tier, country, proto) for the O(1) selection fast path | `build_buckets` |
| `router/tests.rs` | `router`'s existing unit tests (moved out of `router.rs`'s trailing `mod tests`) | — |
| `router/health_tests.rs` | Active-health unit tests: threshold/safety valve/sticky invalidation/cooldown/sampling/concurrency | — |

**The split is a pure move**: methods migrated verbatim, zero behaviour change;
submodules inherit the parent's private fields and free functions via
`use super::*;`, and deliberately do **not** re-export with
`pub(crate) use x::*` — inherent methods need no import (callers just write
`engine.foo()`), and re-exporting would trip `unused_imports` (a glob-of-glob
via `use super::*` does not count as a "use").

### 1.2 `free_pool` internal submodules (OPT-R18 P2-1, second batch)

The `free_pool` module used to be a 3727-line single file (fetch sources + two-stage
verify + registry state + background worker all mixed together). It is now the
parent module `free_pool/mod.rs` (types + `shared_client` + re-exports) plus 5
submodules. **Submodule tests are folded into the `free_pool/mod.rs` row above**;
the component table has 27 rows total and keeps submodules folded into their parent row.

| Submodule | Responsibility | Key APIs |
|-----------|----------------|----------|
| `free_pool/sources.rs` | Three fetch sources (JSON API / HTML / GitHub raw list) + polite polling cache (ETag/304) + concurrent fetch-all | `ApiSource/HtmlSource/GitHubSource/fetch_all/API_MAX_PAGES` |
| `free_pool/verify.rs` | Two-stage verify: lightweight `Verifier` (TCP/proto) + `FullChecker` (canary) + anonymity grading | `Verifier/FullChecker/classify_anonymity/egress_of/socks_target_of_base` |
| `free_pool/registry.rs` | Pool state: `Health` (EWMA) + `Registry` (TTL/eviction/failure marks) + source breaker + intake quota | `Registry/Health/SourceGuard/cap_intake/intake_cap_limit/baseline_bases` |
| `free_pool/worker.rs` | Driver: env assembly, periodic fetch→verify→geo verdict→Router merge | `FreePoolWorker/FreePoolConfig/apply_geo_verdict/spawn_full_check` |
| `free_pool/tests.rs` | All unit tests (moved out of the old file's second half) | — |

**Two differences from the `router` split** (both hit in practice): (1) `router`
keeps both `router.rs` and `router/`, while `free_pool` uses `free_pool/mod.rs`
as the parent (it holds only types and re-exports, so a separate `.rs` would just
add a hop) — the docs gate must recognise both layouts, since an earlier version
globbed only top-level `*.rs` and silently dropped the whole module (and its 60+
tests) from the table; (2) here we move **types and constants**, which need
`pub use` re-exports to keep `crate::free_pool::X` paths working (`router` moved
inherent methods, which need no import); re-exports used only by `free_pool/tests.rs` trip
`unused_imports` in non-test builds, hence
`#[cfg_attr(not(test), allow(unused_imports))]` — relaxed for non-test builds
only, still mandatory in test builds.

### 1.3 `gateway` internal submodules (OPT-R18 P2-1, third batch)

The `gateway` module used to be a 2275-line single file mixing **four concerns**:
pure auth/tier decisions, LinUCB routing context, upstream peer assembly, and SOCKS
bridge forwarding — plus 40 unit tests. It is now the parent module `gateway/mod.rs`
(struct + trace id + `parse_routing_spec` + re-exports) plus 6 submodules.
**Submodule tests are folded into the `gateway/mod.rs` row above**; the component table has 27 rows total and keeps submodules folded into their parent row.

| Submodule | Responsibility | Key APIs |
|-----------|----------------|----------|
| `gateway/policy.rs` | Pure decisions: env gate, auth error mapping, free-tier credential gate, tenant session naming | `should_reject_missing_api_key/status_for_auth_error/free_node_exits_with_credentials/apply_tenant_namespace` |
| `gateway/bandit.rs` | LinUCB: arm lookup, context resolution, tier-quota routing, stale-arm pruning, retry accounting | `arm_for/resolve_bandit_context/prune_stale_arms/tier_quota_forced_tier/select_bandit_node_excluding` |
| `gateway/upstream.rs` | Upstream assembly: TLS/loopback checks, `HttpPeer` construction, bridge URL, bridge budget | `build_http_peer/bridge_url_for/is_loopback_addr/is_tls_for_node/should_skip_bridge_node/socks_overall_budget` |
| `gateway/socks.rs` | SOCKS bridge: candidate selection, forwarding loop, bridge response write-back | `pick_socks_candidate/serve_via_socks/write_bridge_response` |
| `gateway/hooks.rs` | `impl ProxyHttp`: Pingora's 9 lifecycle callbacks | `request_filter/upstream_peer/proxy_upstream_filter/upstream_request_filter/logging` |
| `gateway/tests.rs` | All `gateway` unit tests (moved out of the old file) | — |

**Why `impl SmartProxyGateway` is split across bandit/socks**: Rust allows inherent
impls of one type in any module of the same crate. These methods serve two different
concerns (routing vs. SOCKS bridge), so keeping each next to its concern reads better
than cramming everything into `gateway/mod.rs`. The constructor is not here — `main.rs`
assembles `SmartProxyGateway` with a struct literal.

**The split also fixed a doc mis-attachment**: in the old single-file `gateway` module, the doc for
`build_http_peer` ("P2: HttpPeer assembly (with socks guard)") was attached to
`is_tls_for_node`, so the latter carried an unrelated description while the former had
no doc at all. Both are now with their own functions.

### 1.4 `metrics` internal submodules (OPT-R18 P2-1, fourth batch)

The `metrics` module used to be a 2097-line single file. The split axis is the
**direction of data flow**, not metric families:

| Submodule | Responsibility | Key APIs |
|-----------|----------------|----------|
| `metrics/record.rs` | **Write side**: `observe*` / `note_*` / `set_*`, called by the data plane and background workers | `observe/observe_stage/note_free_verify/note_breaker_trigger/mark_sink_ok/set_free_pool_supply` |
| `metrics/render.rs` | **Read side**: Prometheus text rendering + HTTP server | `render/serve_metrics` |
| `metrics/tests.rs` | All unit tests | — |

**Why not split by metric family** (free-pool / breaker / geo / probe in separate
files): the recording methods are **interleaved** in the impl — the free-pool group
alone sits in 6 non-adjacent ranges. Splitting by family would shatter one impl into a
dozen fragments, forcing readers to jump across 6 files to follow a single family's
lifecycle. "Write vs read" is one clean cut: the write side only mutates atomics, the
read side only loads them, and the only contract between them is **metric names and
label sets** (every field written must have a matching `# HELP`/`# TYPE` line in
`render()`, otherwise the metric is recorded but never scraped — a silent failure).

**A real defect the golden snapshot caught immediately**: `proxy_stage_duration_ms`
emitted its `# HELP`/`# TYPE` **inside** the per-stage loop, so 3 stages produced 3 copies.
The Prometheus text format allows exactly one `# HELP` per metric name; violating it makes
the scraper **fail to parse**, which loses **the entire scrape** (every metric from that
instance), not just one series. Confirmed with the official `promtool check metrics`
(`second HELP line for metric name "proxy_stage_duration_ms"`); HELP/TYPE were hoisted out
of the loop, eliminating that duplicate-header error (naming lints remain — `_total` suffix
and `_ms` abbreviations are existing external contracts; renaming would break dashboards
and alert rules). The new `render_is_valid_prometheus_text` then caught a second mismatch:
samples and the Grafana dashboard use `free_pool_exit_total`, but its HELP/TYPE headers said
`free_exit_total`, leaving the samples without a matching TYPE family. The headers now use
`free_pool_exit_total`; validation also requires HELP and TYPE name sets to match and every
sample to belong to a declared family. `each_metric_has_single_help_and_type` continues to
check duplicate headers; the golden snapshot, single-header assertion, and full exposition
validator are complementary.

**Also unified a convention inconsistency**: the old file had **4 `#[test]` functions at
module level** (`opt_r9_c2_*`, outside `mod tests`) alongside 40 inside it — two
conventions in one file, which makes readers think the module-level ones are special.
Both are legal (`#[test]` implies `cfg(test)` gating); they now all live in
`metrics/tests.rs`.

**`render()` is now split into 7 section renderers** (follow-up step, 2026-11-01): the
former 538-line function is a 10-line orchestrator calling 7 private methods (requests &
probe / latency / breaker / telemetry / free-pool supply / GeoIP / free-pool funnel).
**Section order is output order** — reordering changes what the scraper sees.

The safety net was a **golden snapshot test** (`golden_render_full_exposition`): it renders
the whole exposition from a deterministic state and asserts it **byte-for-byte** (215
lines). It immediately caught a real defect — see below.

### 1.5 `admin` internal submodules (OPT-R18 P2-1, fifth batch)

The Admin module used to be 1791 lines with route registration, middleware, and REST handlers in one file. It is now split by responsibility, with unauthenticated health probes mounted outside the protected API prefix. Submodule tests roll up into the `admin/mod.rs` row.

| Submodule | Responsibility | Key APIs |
|-----------|----------------|----------|
| `admin/dto.rs` | Request/response DTOs and error-body construction; the single domain→wire conversion point | `ErrorResponse/error_response/NodeView/node_view/AddNodeRequest/ApiError` |
| `admin/mw.rs` | Three middleware layers: access audit, auth (fail-closed), unmatched-endpoint 404 | `audit_logging/auth_middleware/api_not_found_fallback` |
| `admin/handlers_node.rs` | Nodes: list/add/detail/delete/reweight/quarantine/unquarantine | `list_nodes_handler/add_node_handler/patch_node_weight_handler/post_node_quarantine_handler` |
| `admin/handlers_tenant.rs` | Tenants: list/detail/balance/active | `list_tenants_handler/get_tenant_handler/patch_tenant_balance_handler` |
| `admin/handlers_ops.rs` | Ops surface: health, OpenAPI, free-pool dashboard, circuit breaker | `health_handler/openapi_handler/list_free_pool_summary_handler/get_circuit_handler` |
| `admin/tests.rs` | All `admin` unit tests (moved out of the old file) | — |

**Why handlers are split by resource, not by HTTP method**: someone asking "what happens
when I delete a node" needs to see `delete_node_handler` next to
`delete_node_quarantine_handler` — they share the same "look up via `get_node_by_addr`,
404 if absent, then act" preamble. Splitting by method would scatter GET/PATCH/DELETE
across three files and force back-and-forth jumping.

**Why the middleware module is named `mw`**: `axum::middleware` is already imported in the
parent (`middleware::from_fn` / `from_fn_with_state`), and a same-named submodule collides
with it (measured: E0255 "name defined multiple times"). Renaming our module reads clearer
than aliasing axum's.

**Why `node_view` stays in `admin/dto.rs`**: it is the only domain→wire conversion point and is
used by three handler modules. Keeping it in `admin/dto.rs` guarantees **one definition of the wire
shape**, so a field change cannot drift ("one endpoint forgot to update"). It and its
partner `registry_error_response` are exposed to sibling modules via `pub(super)` —
`pub(super)` items cannot be re-exported with `pub use` (measured: E0364), so handlers
import them explicitly with `use super::dto::{…}`.

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

Redis 7 (6379) / ClickHouse 24 (8123) / Prometheus 2.53 (9090) / Grafana 11.1 (3000); gateway runs as host binary (:8916 data, :9091 metrics, :9092 Admin).
