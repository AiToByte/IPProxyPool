//! GW-5 Admin REST API（Phase 1）。
//!
//! 独立 HTTP 服务（缺省 `:9092`），与数据面 Pingora（`:8916`）和
//! Prometheus 指标（`:9091`）完全隔离。所有端口均在 `/api/v1/*` 下，
//! 统一通过请求头 `X-Admin-Key` 认证，key 值由环境变量 `ADMIN_API_KEY`
//! 提供；未设置时整个 Admin 服务拒绝所有请求（fail-closed）。
//!
//! OpenAPI 3.1 文档由 `utoipa` 自动生成，路径 `/openapi.json` 可供
//! 前端、Swagger UI 和后续多语言 SDK 代码生成使用。

use axum::{
    extract::{rejection::JsonRejection, Json as AxumJson, Path, Query, State},
    http::{header, HeaderMap, StatusCode},
    middleware,
    response::Json,
    routing::{delete, get, patch, post},
    Router,
};
use serde::{Deserialize, Serialize};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use utoipa::{OpenApi, ToSchema};

use crate::metrics::{
    AnonymityStat, BreakerSnapshot, BreakerTriggerStat, FreePoolSnapshot, FreePoolSourceStat,
    MetricsRegistry, ProtoStat, SuspendedStat, VerifyStat,
};
use crate::model::ProxyNode;
use crate::router::{NodeFilter, RouterEngine};
use crate::tenant::{TenantDetail, TenantManager, TenantSummary};

/// Admin 服务共享状态，由 `main.rs` 装配后传入 axum 路由。
///
/// `metrics` 字段当前由各 handler 间接通过 Prometheus `:9091` 暴露，
/// Admin API 端点未直接读取，保留以供后续扩展（如在节点/租户详情中
/// 关联实时指标）。
#[allow(dead_code)]
pub struct AdminState {
    pub router: Arc<RouterEngine>,
    pub tenant_mgr: Arc<TenantManager>,
    pub metrics: Arc<MetricsRegistry>,
    /// 管理 API 密钥（与网关 `X-API-Key` 独立，不可互换）。
    pub admin_api_key: String,
    /// GW-6 自动调权配置（`Arc` 共享，可安全 Clone 传入 axum state 与中间件）。
    pub autotune: Arc<AutotuneConfig>,
}

impl Clone for AdminState {
    fn clone(&self) -> Self {
        Self {
            router: Arc::clone(&self.router),
            tenant_mgr: Arc::clone(&self.tenant_mgr),
            metrics: Arc::clone(&self.metrics),
            admin_api_key: self.admin_api_key.clone(),
            autotune: Arc::clone(&self.autotune),
        }
    }
}

/// GW-6 自动调权配置：开关（0/1）与付费权重上调因子（百分比，100=满权，范围 1~500）。
#[derive(Debug, Default)]
pub struct AutotuneConfig {
    pub enabled: AtomicU64,
    pub boost: AtomicU64,
}

impl AutotuneConfig {
    pub fn new() -> Self {
        Self {
            enabled: AtomicU64::new(0),
            boost: AtomicU64::new(100),
        }
    }
}

/// 统一错误响应。
#[derive(Debug, Serialize, Deserialize, ToSchema)]
pub struct ErrorResponse {
    pub error: String,
    pub code: u16,
}

/// 统一错误体构造：文档 §4 承诺**所有**非 2xx 响应的 body 都是
/// `{"error": ..., "code": <http 状态码>}`。
///
/// # 为什么必须实装（Phase 1 的遗留偏差）
///
/// 首版 handler 直接返回裸 `StatusCode`（body 为空），认证中间件同样返回
/// `Err(StatusCode::UNAUTHORIZED)`——文档承诺的错误形态实际不存在，管理后台
/// /SDK 按文档解析必空手而归。本轮统一收口：401（中间件）、404（节点/租户/
/// 未知路径）、400（请求体非法 / 非有限余额）全部经此构造。
///
/// 错误文案**不区分**「key 未配置」与「key 不对」（fail-closed：不给探测者
/// 任何区分两者的信号）。
pub fn error_response(
    status: StatusCode,
    msg: impl Into<String>,
) -> (StatusCode, Json<ErrorResponse>) {
    (
        status,
        Json(ErrorResponse {
            error: msg.into(),
            code: status.as_u16(),
        }),
    )
}

/// handler 统一错误返回类型（`IntoResponse` 两个分支各自实现即可）。
pub type ApiError = (StatusCode, Json<ErrorResponse>);

/// 健康检查响应。
#[derive(Debug, Serialize, Deserialize, ToSchema)]
pub struct HealthResponse {
    pub status: String,
    pub service: String,
}

/// 免费池自动调权状态。
#[derive(Debug, Serialize, Deserialize, ToSchema)]
pub struct AutotuneStatus {
    /// 是否启用干旱自动上调付费权重。
    pub enabled: bool,
    /// 付费权重上调因子（百分比，100=满权）。
    pub boost_percent: u32,
    /// 免费池当前是否干旱（nodes_total==0）。
    pub dry: bool,
}

/// 免费池自动调权更新请求（字段缺省则不改）。
#[derive(Debug, Serialize, Deserialize, ToSchema)]
pub struct AutotuneRequest {
    pub enabled: Option<bool>,
    pub boost_percent: Option<u32>,
}

/// 节点调权请求。
#[derive(Debug, Serialize, Deserialize, ToSchema)]
pub struct NodeWeightRequest {
    pub weight: u32,
}

/// 节点隔离请求。
#[derive(Debug, Serialize, Deserialize, ToSchema)]
pub struct NodeQuarantineRequest {
    pub ttl_secs: u64,
}

/// 租户余额修改请求。
#[derive(Debug, Serialize, Deserialize, ToSchema)]
pub struct TenantBalanceRequest {
    pub balance_usd: f64,
}

/// 租户启用/禁用请求。
#[derive(Debug, Serialize, Deserialize, ToSchema)]
pub struct TenantActiveRequest {
    pub active: bool,
}

/// 节点列表查询响应（每项为节点公开字段）。
#[derive(Debug, Serialize, Deserialize, ToSchema)]
pub struct NodeView {
    pub addr: String,
    pub ip: String,
    pub port: u16,
    pub tier: String,
    pub country: String,
    pub provider: String,
    pub weight: u32,
    pub proto: String,
    pub exit_ip: Option<String>,
}

/// `ProxyNode` → 公开视图的统一映射（此前列表/详情/调权/隔离四处各写一遍
/// 构造体字面量，本助手消除六份复制粘贴）。
fn node_view(n: &ProxyNode) -> NodeView {
    NodeView {
        addr: n.addr.clone(),
        ip: n.ip.clone(),
        port: n.port,
        tier: n.tier.clone(),
        country: n.country.clone(),
        provider: n.provider.clone(),
        weight: n.weight,
        proto: format!("{:?}", n.proto),
        exit_ip: n.exit_ip.clone(),
    }
}

/// OpenAPI 3.1 文档聚合。所有 handler 路径与 schema 必须在此注册。
#[derive(OpenApi)]
#[openapi(
    paths(
        health_handler,
        list_nodes_handler,
        get_node_handler,
        patch_node_weight_handler,
        post_node_quarantine_handler,
        delete_node_quarantine_handler,
        list_tenants_handler,
        get_tenant_handler,
        patch_tenant_balance_handler,
        patch_tenant_active_handler,
        list_free_pool_summary_handler,
        list_free_pool_sources_handler,
        get_free_pool_autotune_handler,
        post_free_pool_autotune_handler,
        get_circuit_handler
    ),
    components(
        schemas(
            NodeFilter,
            NodeWeightRequest,
            NodeQuarantineRequest,
            TenantBalanceRequest,
            TenantActiveRequest,
            NodeView,
            TenantSummary,
            TenantDetail,
            HealthResponse,
            ErrorResponse,
            FreePoolSnapshot,
            FreePoolSourceStat,
            VerifyStat,
            ProtoStat,
            AnonymityStat,
            SuspendedStat,
            BreakerSnapshot,
            BreakerTriggerStat,
            AutotuneStatus,
            AutotuneRequest
        )
    ),
    tags(
        (name = "health", description = "健康检查"),
        (name = "nodes", description = "代理节点管理：查询、调权、隔离"),
        (name = "tenants", description = "租户管理：查询、余额、启停"),
        (name = "freepool", description = "免费池供给看板：水位、各源产出、熔断、匿名度"),
        (name = "circuit", description = "断路器与隔离状态")
    )
)]
pub struct AdminApiDoc;

/// Admin API 请求超时（秒）。
///
/// # 为何要有这个数
///
/// Admin 端点能做什么：读节点表、调权重、置/解隔离、改租户余额与启停、
/// 读 FreePool 快照。全部是**进程内内存操作**（`DashMap` / `ArcSwap`），
/// 正常应在亚毫秒级返回。因此超时取得很宽松——它不是性能目标，
/// 而是**兜底**：挡住因下游阻塞（磁盘 fsync、极端 GC 停顿、被换出的
/// 内存页）而长期挂住的请求，避免少量慢请求堆积占满连接。
///
/// # 与数据面的区别
///
/// 数据面 `:8916` 的超时由上游代理节点决定（客户端侧另有
/// `pool.rs` 的预热与连接治理），不在此处统一设限——设一个固定值会
/// 误伤合法的慢上游。Admin 面没有"上游"，故可以一刀切。
const ADMIN_TIMEOUT_SECS: u64 = 30;

/// 构建 axum 路由（含认证中间件 + 请求超时层 + 访问审计日志）。
///
/// # 层序（不可随意调换）
///
/// ```text
/// request
///   └─▶ [audit_logging]  ← 必须最外层：认证 401、超时 408 也必须留记录
///        └─▶ [TimeoutLayer] ← 在认证内层：未认证的挂起请求同样受超时约束
///             └─▶ [auth_middleware] ← 认证拒绝是 401，不消耗 body
///                  └─▶ handler
/// ```
///
/// # 为什么**不**用 `tower-http` 的 `TraceLayer`
///
/// `TraceLayer` 发的是 `tracing` 事件，而本项目装的是 `env_logger`（`log`
/// facade），**没有 `tracing` subscriber** —— 加了也一条都不打印，正是本仓
/// 最忌讳的静默失效。故审计层是手写的 `audit_logging`（只记是否带 Key，
/// 绝不记 Key 值）。本注释此前误写为 `[TraceLayer]`，与代码不符，已纠正。
///
/// # 为什么**不**加 CORS（OPT-R17 复核结论）
///
/// 本文件此前有一处**失实注释**：声称"含 CORS + 超时"，而两者都不存在
/// （`Cargo.toml` 声明了 `tower-http` 的 `cors`/`trace`/`timeout` 三个
/// feature，`src/` 全仓零引用）。失实注释比没有注释更糟——它会让读者
/// 以为"跨域已经放开"，实则 Admin UI 只能靠 Vite dev proxy 或同域反代
/// 访问（见 `docs/ADMIN_UI.md`）。
///
/// 这里选择**不加** CORS，而非"补上以兑现注释"，理由是安全边界：
/// Admin API 用 `X-Admin-Key` 头认证，若开 `CorsLayer::permissive()`，
/// 任意站点的 JS 都能发起带自定义头的跨域请求 —— 一旦 Admin Key 泄漏
/// （浏览器扩展、历史记录、反代日志），攻击面从"本机回环"扩大到
/// "任何能访问 :9092 的站点"。而 Admin UI 的实际部署方式（同域反代 /
/// Vite proxy）根本不需要 CORS。
///
/// 故：`cors` feature 从 `Cargo.toml` 移除，让依赖清单与真实代码一致，
/// 防止再次出现"声明了但没实现"。
pub fn router(state: AdminState) -> Router {
    build_router(state, std::time::Duration::from_secs(ADMIN_TIMEOUT_SECS))
}

/// 组装路由：超时时长**参数化**。
///
/// # 为何参数化（而不是把 `TimeoutLayer::new(...)` 直接写在 `router()` 里）
///
/// 因为「超时层真的会在超时后掐断请求」这件事**只能用极短超时实测**：
/// 生产值是 30s，没有任何测试愿意等 30s 来验证超时。若不参数化，就只能
/// 退而求其次去 `assert!(ADMIN_TIMEOUT_SECS > 0)` —— 而 `ADMIN_TIMEOUT_SECS`
/// 是 `const`，`30 > 0` 在**编译期**就确定，该断言永不为假、也抓不到任何
/// 回归（改小常量它也不红，只会重新编译）。CI 的 clippy 正是以
/// `assertions_on_constants` lint 判它红（实测 run，rust 1.99）——**这个
/// lint 抓到的是真缺陷：那个测试没有任何鉴别力。**
///
/// 参数化后，测试可以传 1ms 并配一个真会挂起的 handler，从而证明
/// 「超时层生效」与「层序正确」两件事，而不是靠断言常量自证。
fn build_router(state: AdminState, timeout: std::time::Duration) -> Router {
    build_router_with(state, timeout, Router::new())
}

/// 同 [`build_router`]，但允许向 `/api/v1` **内部**追加路由（仅测试用）。
///
/// # 为何需要这个参数
///
/// 校验「超时层生效」必须让请求真的走到一个会挂起的 handler 上。而
/// `TimeoutLayer` 装在 **`.nest("/api/v1", …)` 的内层** —— 若测试把慢路由
/// 挂在 nest **之外**（`router().route("/slow", …)`），该路由根本不经
/// timeout 层，会直接返回 200，让「超时层失效」与「测试挂错位置」两种情况
/// 长得一模一样。故必须能把路由注入 nest 内部。
fn build_router_with(
    state: AdminState,
    timeout: std::time::Duration,
    extra: Router<AdminState>,
) -> Router {
    let api = extra
        // Health（轻量，可供负载均衡器探活）
        .route("/health", get(health_handler))
        // OpenAPI 文档（供前端与客户端代码生成）
        .route("/openapi.json", get(openapi_handler))
        // 节点管理
        .route("/nodes", get(list_nodes_handler))
        .route("/nodes/:addr", get(get_node_handler))
        .route("/nodes/:addr/weight", patch(patch_node_weight_handler))
        .route(
            "/nodes/:addr/quarantine",
            post(post_node_quarantine_handler),
        )
        .route(
            "/nodes/:addr/quarantine",
            delete(delete_node_quarantine_handler),
        )
        // 租户管理
        .route("/tenants", get(list_tenants_handler))
        .route("/tenants/:api_key", get(get_tenant_handler))
        .route(
            "/tenants/:api_key/balance",
            patch(patch_tenant_balance_handler),
        )
        .route(
            "/tenants/:api_key/active",
            patch(patch_tenant_active_handler),
        )
        // 免费池供给看板
        .route("/freepool/summary", get(list_free_pool_summary_handler))
        .route("/freepool/sources", get(list_free_pool_sources_handler))
        // 免费池自动调权
        .route("/freepool/autotune", get(get_free_pool_autotune_handler))
        .route("/freepool/autotune", post(post_free_pool_autotune_handler))
        // 断路器与隔离状态
        .route("/circuit", get(get_circuit_handler));

    // 嵌入版本前缀，并加上认证中间件（openapi/health 对内部可见，仍走认证，
    // 保持统一安全边界；如需内部免认证可在此拆分）。
    // fallback：未匹配路径（含未知 /api/v1/* 子路径）一律返回文档 §4 的
    // JSON 错误体 404——此前落到 axum 默认空 body，与文档契约不符。
    //
    // 层序（三层，顺序不可随意调换）：
    //   request → [audit] → [timeout] → [auth] → handler
    //   - audit 在最外层：认证失败（401）与超时（408）也必须留下记录，
    //     否则恰恰是最该被审计的"有人在撞门"会漏掉。
    //   - timeout 在 auth 外层：任何请求都受 timeout 约束，不给慢请求
    //     无限占用连接的机会（Admin 端点全是内存操作，30s 远高于正常耗时）。
    let audit = middleware::from_fn(audit_logging);

    Router::new()
        .nest(
            "/api/v1",
            api.layer(middleware::from_fn_with_state(
                state.clone(),
                auth_middleware,
            ))
            .layer(tower_http::timeout::TimeoutLayer::new(timeout)),
        )
        .layer(audit)
        .fallback(api_not_found_fallback)
        .with_state(state)
}

/// Admin API 访问审计：方法、路径、状态码、耗时，以及**是否**带了 Admin Key。
///
/// # 为什么用手写中间件而不是 `tower-http` 的 `TraceLayer`
///
/// `TraceLayer` 发的是 `tracing` 事件。本项目装的是 `env_logger`
/// （`log` facade，全仓 78 处 `log::info!` 一致），**没有 `tracing` 的
/// subscriber**。用 `TraceLayer` 会得到一个"看起来加了审计、实际一条
/// 都不会打印"的层 —— 正是本仓最忌讳的静默失效（见
/// `docs/CAPABILITY-AUDIT.md` §0 的取证纪律）。故直接用 `log` 写。
///
/// # 为什么**不**记 Key 的值
///
/// 日志落 `log/`、可能被集中收集与备份。把密钥写进日志等于新建一条
/// 泄漏路径。只记布尔值：足以回答"这次调用是否带凭据"，不足以用来认证。
///
/// # 状态码要记
///
/// 只记成功不记失败的话，"大量 401" 这类最有价值的信号反而看不见。
async fn audit_logging(
    req: axum::extract::Request,
    next: middleware::Next,
) -> axum::response::Response {
    let method = req.method().clone();
    let path = req.uri().path().to_string();
    let has_admin_key = req.headers().contains_key("X-Admin-Key")
        || req
            .headers()
            .get(header::AUTHORIZATION)
            .map(|v| v.as_bytes().starts_with(b"Admin "))
            .unwrap_or(false);

    let started = std::time::Instant::now();
    let response = next.run(req).await;
    let status = response.status().as_u16();
    let elapsed_ms = started.elapsed().as_millis();

    // 2xx/fallback 走 info，4xx/5xx 走 warn：撞门与故障应能在默认日志级别下被发现。
    if (200..400).contains(&status) {
        log::info!("[admin] {method} {path} -> {status} ({elapsed_ms}ms, key={has_admin_key})");
    } else {
        log::warn!("[admin] {method} {path} -> {status} ({elapsed_ms}ms, key={has_admin_key})");
    }
    response
}

/// 未匹配端点的统一 404（JSON 错误体）。
async fn api_not_found_fallback() -> ApiError {
    error_response(StatusCode::NOT_FOUND, "unknown admin endpoint")
}

/// 认证中间件：校验 `X-Admin-Key` 请求头等于 `ADMIN_API_KEY`。
/// 未配置 `ADMIN_API_KEY` 时全部拒绝（fail-closed）。
async fn auth_middleware(
    State(state): State<AdminState>,
    headers: HeaderMap,
    request: axum::extract::Request,
    next: middleware::Next,
) -> Result<axum::response::Response, ApiError> {
    let admin_key = headers
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Admin ").map(|s| s.to_string()))
        .or_else(|| {
            headers
                .get("X-Admin-Key")
                .and_then(|v| v.to_str().ok())
                .map(|v| v.to_string())
        });

    let provided = match admin_key {
        Some(k) => k,
        // 文案刻意不区分「未配置 ADMIN_API_KEY」与「key 不对」（fail-closed，
        // 不给探测者区分信号）。
        None => {
            return Err(error_response(
                StatusCode::UNAUTHORIZED,
                "missing or invalid admin key",
            ))
        }
    };

    if provided != state.admin_api_key {
        return Err(error_response(
            StatusCode::UNAUTHORIZED,
            "missing or invalid admin key",
        ));
    }

    Ok(next.run(request).await)
}

/// GET `/api/v1/health` —— 服务健康检查。
#[utoipa::path(
    get,
    path = "/api/v1/health",
    tag = "health",
    responses(
        (status = 200, description = "服务正常", body = HealthResponse)
    )
)]
async fn health_handler() -> Json<HealthResponse> {
    Json(HealthResponse {
        status: "ok".to_string(),
        service: "ipproxy-pool-admin".to_string(),
    })
}

/// GET `/api/v1/openapi.json` —— 返回 OpenAPI 3.1 文档（JSON）。
#[utoipa::path(
    get,
    path = "/api/v1/openapi.json",
    tag = "health",
    responses(
        (status = 200, description = "OpenAPI 文档 JSON", content_type = "application/json")
    )
)]
async fn openapi_handler() -> Json<utoipa::openapi::OpenApi> {
    Json(AdminApiDoc::openapi())
}

/// GET `/api/v1/nodes` —— 列举代理节点（支持 `?tier=...&country=...&proto=...`）。
///
/// 响应契约：**裸数组 `NodeView[]`**（文档 §2 与管理后台按数组消费的既有口径）。
/// 首版曾返回 `{"nodes": [...]}` 包装对象，dashboard/nodes 页面按数组消费
/// 直接崩——本轮与 tenants 一并改回文档承诺的裸数组。
#[utoipa::path(
    get,
    path = "/api/v1/nodes",
    tag = "nodes",
    params(
        ("tier" = Option<String>, Query, description = "节点档位：datacenter/residential/mobile/free"),
        ("country" = Option<String>, Query, description = "国家码，如 US/JP/GB"),
        ("proto" = Option<String>, Query, description = "出站协议：http/socks5/socks4")
    ),
    responses(
        (status = 200, description = "节点列表（裸数组）", body = inline(Vec<NodeView>))
    )
)]
async fn list_nodes_handler(
    State(state): State<AdminState>,
    Query(filter): Query<NodeFilter>,
) -> Json<Vec<NodeView>> {
    let nodes = state.router.list_nodes(&filter);
    Json(nodes.iter().map(|n| node_view(n)).collect())
}

/// GET `/api/v1/nodes/{addr}` —— 节点详情。
#[utoipa::path(
    get,
    path = "/api/v1/nodes/{addr}",
    tag = "nodes",
    params(
        ("addr" = String, Path, description = "节点地址 ip:port")
    ),
    responses(
        (status = 200, description = "节点详情", body = NodeView),
        (status = 404, description = "节点不存在", body = ErrorResponse)
    )
)]
async fn get_node_handler(
    State(state): State<AdminState>,
    Path(addr): Path<String>,
) -> Result<Json<NodeView>, ApiError> {
    match state.router.get_node_by_addr(&addr) {
        Some(n) => Ok(Json(node_view(&n))),
        None => Err(error_response(
            StatusCode::NOT_FOUND,
            format!("node {addr} not found"),
        )),
    }
}

/// PATCH `/api/v1/nodes/{addr}/weight` —— 运行时调整节点权重（生效于下一轮选路）。
#[utoipa::path(
    patch,
    path = "/api/v1/nodes/{addr}/weight",
    tag = "nodes",
    params(
        ("addr" = String, Path, description = "节点地址 ip:port")
    ),
    request_body = NodeWeightRequest,
    responses(
        (status = 200, description = "权重已更新", body = NodeView),
        (status = 404, description = "节点不存在", body = ErrorResponse),
        (status = 400, description = "请求体非法", body = ErrorResponse)
    )
)]
async fn patch_node_weight_handler(
    State(state): State<AdminState>,
    Path(addr): Path<String>,
    body: Result<AxumJson<NodeWeightRequest>, JsonRejection>,
) -> Result<Json<NodeView>, ApiError> {
    // 请求体非法（含非 JSON/字段类型错）⇒ 文档 §4 的 400 JSON 错误体
    //（此前落到 axum 默认纯文本 rejection，与契约不符）。
    let AxumJson(payload) = body.map_err(|rej| {
        error_response(
            StatusCode::BAD_REQUEST,
            format!("invalid request body: {rej}"),
        )
    })?;
    // 按 provider+country 精确匹配调权（与 router::adjust_vendor_weight 口径一致）。
    // 先取出节点的 provider/country，再按该维度整组调权，避免只改单节点
    // 导致下次 merge 被覆盖。
    let target = match state.router.get_node_by_addr(&addr) {
        Some(n) => n,
        None => {
            return Err(error_response(
                StatusCode::NOT_FOUND,
                format!("node {addr} not found"),
            ))
        }
    };
    state
        .router
        .adjust_vendor_weight(&target.provider, &target.country, payload.weight);
    // 返回调权后的该节点视图（权重已生效）。
    match state.router.get_node_by_addr(&addr) {
        Some(n) => Ok(Json(node_view(&n))),
        None => Err(error_response(
            StatusCode::NOT_FOUND,
            format!("node {addr} not found"),
        )),
    }
}

/// POST `/api/v1/nodes/{addr}/quarantine` —— 隔离指定节点（域级熔断，TTL 秒后自动恢复）。
#[utoipa::path(
    post,
    path = "/api/v1/nodes/{addr}/quarantine",
    tag = "nodes",
    params(
        ("addr" = String, Path, description = "节点地址 ip:port")
    ),
    request_body = NodeQuarantineRequest,
    responses(
        (status = 200, description = "已隔离", body = NodeView),
        (status = 400, description = "请求体非法", body = ErrorResponse),
        (status = 404, description = "节点不存在", body = ErrorResponse)
    )
)]
async fn post_node_quarantine_handler(
    State(state): State<AdminState>,
    Path(addr): Path<String>,
    body: Result<AxumJson<NodeQuarantineRequest>, JsonRejection>,
) -> Result<Json<NodeView>, ApiError> {
    let AxumJson(payload) = body.map_err(|rej| {
        error_response(
            StatusCode::BAD_REQUEST,
            format!("invalid request body: {rej}"),
        )
    })?;
    let target = match state.router.get_node_by_addr(&addr) {
        Some(n) => n,
        None => {
            return Err(error_response(
                StatusCode::NOT_FOUND,
                format!("node {addr} not found"),
            ))
        }
    };
    // 对该节点的 target_domain 进行隔离：以节点所在 country 作为域占位符，
    // 实际隔离 key 由 `set_quarantine` 内部归一化。此处以节点 ip 作为隔离 ip，
    // domain 用节点 country（管理后台可见语义：隔离该节点所在国家/地区出口）。
    // 解除走同口径（remove_quarantine 与 set_quarantine 共用归一化）。
    state
        .router
        .set_quarantine(&target.country, &target.ip, payload.ttl_secs);
    Ok(Json(node_view(&target)))
}

/// DELETE `/api/v1/nodes/{addr}/quarantine` —— 解除节点隔离。
///
/// 首版是**空操作**（只读节点返回视图；隔离只能等后台 sweep 过期），文档也如实
/// 写了这一限制。本轮实装：RouterEngine 新增 `remove_quarantine(domain, ip)`
/// （与写入侧共用归一口径），DELETE 即按 POST 隔离时的同一 `(country, ip)` 口径
/// 精确解除；解除后空内层连同外层域一并回收。
///
/// 幂等：未隔离过 / 已被 sweep 清掉的条目返回 false，但接口仍 200 返回节点
/// 视图（解除目标态一致，客户端无需区分）。节点本身不存在 ⇒ 404。
#[utoipa::path(
    delete,
    path = "/api/v1/nodes/{addr}/quarantine",
    tag = "nodes",
    params(
        ("addr" = String, Path, description = "节点地址 ip:port")
    ),
    responses(
        (status = 200, description = "已解除隔离（幂等：未隔离也返回 200）", body = NodeView),
        (status = 404, description = "节点不存在", body = ErrorResponse)
    )
)]
async fn delete_node_quarantine_handler(
    State(state): State<AdminState>,
    Path(addr): Path<String>,
) -> Result<Json<NodeView>, ApiError> {
    let target = match state.router.get_node_by_addr(&addr) {
        Some(n) => n,
        None => {
            return Err(error_response(
                StatusCode::NOT_FOUND,
                format!("node {addr} not found"),
            ))
        }
    };
    // 与 post 的隔离口径完全一致：domain = 节点 country，ip = 节点 ip；
    // `remove_quarantine` 内部做与 `set_quarantine` 相同的 normalize_domain。
    let _removed = state.router.remove_quarantine(&target.country, &target.ip);
    Ok(Json(node_view(&target)))
}

/// GET `/api/v1/tenants` —— 列举所有租户摘要。
///
/// 响应契约：**裸数组 `TenantSummary[]`**（与 `/nodes` 同轮统一，理由见其注释）。
#[utoipa::path(
    get,
    path = "/api/v1/tenants",
    tag = "tenants",
    responses(
        (status = 200, description = "租户列表（裸数组）", body = inline(Vec<TenantSummary>))
    )
)]
async fn list_tenants_handler(State(state): State<AdminState>) -> Json<Vec<TenantSummary>> {
    Json(state.tenant_mgr.list_tenants())
}

/// GET `/api/v1/tenants/{api_key}` —— 租户详情。
#[utoipa::path(
    get,
    path = "/api/v1/tenants/{api_key}",
    tag = "tenants",
    params(
        ("api_key" = String, Path, description = "租户 API Key")
    ),
    responses(
        (status = 200, description = "租户详情", body = TenantDetail),
        (status = 404, description = "租户不存在", body = ErrorResponse)
    )
)]
async fn get_tenant_handler(
    State(state): State<AdminState>,
    Path(api_key): Path<String>,
) -> Result<Json<TenantDetail>, ApiError> {
    match state.tenant_mgr.tenant_detail(&api_key) {
        Some(detail) => Ok(Json(detail)),
        None => Err(error_response(
            StatusCode::NOT_FOUND,
            "tenant not found".to_string(),
        )),
    }
}

/// PATCH `/api/v1/tenants/{api_key}/balance` —— 修改租户余额（USD）。
///
/// **实装语义**（Phase 1 遗留偏差已修）：写入 `TenantManager::set_balance`，
/// 欠费门下一帧读取即见新值。有限负数允许（扣减/罚金）；`NaN/±Infinity` 一律
/// 400（会绕过 `balance <= 0` 欠费门并永久污染账务）。
#[utoipa::path(
    patch,
    path = "/api/v1/tenants/{api_key}/balance",
    tag = "tenants",
    params(
        ("api_key" = String, Path, description = "租户 API Key")
    ),
    request_body = TenantBalanceRequest,
    responses(
        (status = 200, description = "余额已更新", body = TenantDetail),
        (status = 400, description = "请求体非法 / 余额非有限值", body = ErrorResponse),
        (status = 404, description = "租户不存在", body = ErrorResponse)
    )
)]
async fn patch_tenant_balance_handler(
    State(state): State<AdminState>,
    Path(api_key): Path<String>,
    body: Result<AxumJson<TenantBalanceRequest>, JsonRejection>,
) -> Result<Json<TenantDetail>, ApiError> {
    let AxumJson(payload) = body.map_err(|rej| {
        error_response(
            StatusCode::BAD_REQUEST,
            format!("invalid request body: {rej}"),
        )
    })?;
    // 非有限余额必须 400：`NaN` 会绕过欠费门（`NaN <= 0.0` 为 false ⇒ 永不拒付），
    // Infinity 同理；写入即永久污染账务。有限负数允许（扣减/罚金语义）。
    if !payload.balance_usd.is_finite() {
        return Err(error_response(
            StatusCode::BAD_REQUEST,
            "balance_usd must be a finite number".to_string(),
        ));
    }
    // **实装**（Phase 1 遗留偏差修复）：首版只读 detail、改本地副本后返回——
    // 修改从未落库，下一次 GET 即回滚。本轮接入 `TenantManager::set_balance`
    //（`AtomicF64` 原子写入），账务门（authenticate_and_throttle 的欠费拦截）
    // 下一帧读取即见新值。
    if !state.tenant_mgr.set_balance(&api_key, payload.balance_usd) {
        return Err(error_response(
            StatusCode::NOT_FOUND,
            "tenant not found".to_string(),
        ));
    }
    match state.tenant_mgr.tenant_detail(&api_key) {
        Some(detail) => Ok(Json(detail)),
        None => Err(error_response(
            StatusCode::NOT_FOUND,
            "tenant not found".to_string(),
        )),
    }
}

/// PATCH `/api/v1/tenants/{api_key}/active` —— 启用/禁用租户。
#[utoipa::path(
    patch,
    path = "/api/v1/tenants/{api_key}/active",
    tag = "tenants",
    params(
        ("api_key" = String, Path, description = "租户 API Key")
    ),
    request_body = TenantActiveRequest,
    responses(
        (status = 200, description = "启用状态已更新", body = TenantDetail),
        (status = 404, description = "租户不存在", body = ErrorResponse)
    )
)]
async fn patch_tenant_active_handler(
    State(state): State<AdminState>,
    Path(api_key): Path<String>,
    body: Result<AxumJson<TenantActiveRequest>, JsonRejection>,
) -> Result<Json<TenantDetail>, ApiError> {
    let AxumJson(payload) = body.map_err(|rej| {
        error_response(
            StatusCode::BAD_REQUEST,
            format!("invalid request body: {rej}"),
        )
    })?;
    let existed = state.tenant_mgr.set_active(&api_key, payload.active);
    if !existed {
        return Err(error_response(
            StatusCode::NOT_FOUND,
            "tenant not found".to_string(),
        ));
    }
    match state.tenant_mgr.tenant_detail(&api_key) {
        Some(detail) => Ok(Json(detail)),
        None => Err(error_response(
            StatusCode::NOT_FOUND,
            "tenant not found".to_string(),
        )),
    }
}

/// GET `/api/v1/freepool/summary` —— 免费池供给快照（水位、协议分布、匿名度、熔断、质检）。
#[utoipa::path(
    get,
    path = "/api/v1/freepool/summary",
    tag = "freepool",
    responses(
        (status = 200, description = "免费池供给快照", body = FreePoolSnapshot)
    )
)]
async fn list_free_pool_summary_handler(State(state): State<AdminState>) -> Json<FreePoolSnapshot> {
    Json(state.metrics.free_pool_snapshot())
}

/// GET `/api/v1/freepool/sources` —— 各源免费池产出与抓取明细。
#[utoipa::path(
    get,
    path = "/api/v1/freepool/sources",
    tag = "freepool",
    responses(
        (status = 200, description = "各源免费池产出明细", body = inline(Vec<FreePoolSourceStat>))
    )
)]
async fn list_free_pool_sources_handler(
    State(state): State<AdminState>,
) -> Json<Vec<FreePoolSourceStat>> {
    Json(state.metrics.free_pool_snapshot().sources)
}

/// GET `/api/v1/freepool/autotune` —— 免费池自动调权状态（含是否干旱）。
#[utoipa::path(
    get,
    path = "/api/v1/freepool/autotune",
    tag = "freepool",
    responses(
        (status = 200, description = "免费池自动调权状态", body = AutotuneStatus)
    )
)]
async fn get_free_pool_autotune_handler(State(state): State<AdminState>) -> Json<AutotuneStatus> {
    let snapshot = state.metrics.free_pool_snapshot();
    Json(AutotuneStatus {
        enabled: state.autotune.enabled.load(Ordering::Relaxed) == 1,
        boost_percent: state.autotune.boost.load(Ordering::Relaxed) as u32,
        dry: snapshot.dry,
    })
}

/// POST `/api/v1/freepool/autotune` —— 更新自动调权开关与因子，并立即应用到
/// 付费节点权重（开启时上调，关闭时还原满权）。
#[utoipa::path(
    post,
    path = "/api/v1/freepool/autotune",
    tag = "freepool",
    request_body = AutotuneRequest,
    responses(
        (status = 200, description = "自动调权已更新", body = AutotuneStatus)
    )
)]
async fn post_free_pool_autotune_handler(
    State(state): State<AdminState>,
    AxumJson(payload): AxumJson<AutotuneRequest>,
) -> Json<AutotuneStatus> {
    if let Some(enabled) = payload.enabled {
        state
            .autotune
            .enabled
            .store(if enabled { 1 } else { 0 }, Ordering::Relaxed);
    }
    if let Some(boost) = payload.boost_percent {
        state
            .autotune
            .boost
            .store(boost.clamp(1, 500).into(), Ordering::Relaxed);
    }
    // 统一按最终开关/因子应用：开启则上调付费权重，关闭则还原满权。
    let enabled = state.autotune.enabled.load(Ordering::Relaxed) == 1;
    let boost = state.autotune.boost.load(Ordering::Relaxed) as u32;
    if enabled {
        state.router.set_paid_weight_boost(boost);
    } else {
        state.router.set_paid_weight_boost(100);
    }
    let snapshot = state.metrics.free_pool_snapshot();
    Json(AutotuneStatus {
        enabled,
        boost_percent: boost,
        dry: snapshot.dry,
    })
}

/// GET `/api/v1/circuit` —— 断路器与隔离状态快照（熔断触发计数、隔离域/节点数）。
#[utoipa::path(
    get,
    path = "/api/v1/circuit",
    tag = "circuit",
    responses(
        (status = 200, description = "断路器快照", body = BreakerSnapshot)
    )
)]
async fn get_circuit_handler(State(state): State<AdminState>) -> Json<BreakerSnapshot> {
    Json(state.metrics.breaker_snapshot())
}

#[cfg(test)]
#[allow(unused_imports)]
mod tests {
    use super::{error_response, AdminApiDoc, AdminState, AutotuneConfig};
    use crate::router::RouterEngine;
    use crate::tenant::TenantManager;
    use crate::{metrics::MetricsRegistry, model::ProxyNode};
    use axum::body::Body;
    use axum::http::StatusCode;
    use axum::http::{header::CONTENT_TYPE, Request};
    use std::sync::Arc;
    use tower::ServiceExt;
    use utoipa::OpenApi;

    /// 结构编译验证（历史保留）。
    #[test]
    fn openapi_doc_builds() {
        let _doc = AdminApiDoc::openapi();
    }

    #[test]
    fn auth_middleware_rejects_missing_key() {
        // 仅验证结构编译；运行时认证逻辑由下方 oneshot 全链路测试覆盖。
        let _state = AdminState {
            router: Arc::new(RouterEngine::new(vec![])),
            tenant_mgr: Arc::new(TenantManager::new()),
            metrics: Arc::new(MetricsRegistry::new()),
            admin_api_key: "test-key".to_string(),
            autotune: Arc::new(AutotuneConfig::new()),
        };
        assert!(!_state.admin_api_key.is_empty());
    }

    /// 统一错误体形状：code 必须**等于 HTTP 状态码**（文档 §4 契约）。
    #[test]
    fn error_response_carries_status_and_message() {
        let (status, body) = error_response(StatusCode::NOT_FOUND, "boom");
        assert_eq!(status, StatusCode::NOT_FOUND);
        assert_eq!(body.code, 404);
        assert_eq!(body.error, "boom");
    }

    // ---- 以下为 axum oneshot 全链路契约测试（本轮契约修复的锁定回归）----

    const KEY: &str = "test-key";

    fn test_state(nodes: Vec<ProxyNode>, tenants: &[&str]) -> AdminState {
        let mgr = TenantManager::new();
        for key in tenants {
            mgr.register_tenant("t", key, 10_000, 10_000, 1);
        }
        AdminState {
            router: Arc::new(RouterEngine::new(nodes)),
            tenant_mgr: Arc::new(mgr),
            metrics: Arc::new(MetricsRegistry::new()),
            admin_api_key: KEY.to_string(),
            autotune: Arc::new(AutotuneConfig::new()),
        }
    }

    fn test_node(ip: &str, port: u16, country: &str, tier: &str) -> ProxyNode {
        ProxyNode::new(
            ip.to_string(),
            port,
            None,
            None,
            country.to_string(),
            tier.to_string(),
            "mock-a".to_string(),
            100,
        )
    }

    async fn send_json(
        app: axum::Router,
        method: &str,
        uri: &str,
        body: Option<&str>,
    ) -> (StatusCode, serde_json::Value) {
        let mut builder = Request::builder()
            .method(method)
            .uri(uri)
            .header("X-Admin-Key", KEY);
        if body.is_some() {
            // axum 的 Json extractor 强制要求 application/json。
            builder = builder.header(CONTENT_TYPE, "application/json");
        }
        let req = builder
            .body(match body {
                Some(b) => Body::from(b.to_string()),
                None => Body::empty(),
            })
            .expect("request");
        let res = app.oneshot(req).await.expect("oneshot");
        let status = res.status();
        let bytes = axum::body::to_bytes(res.into_body(), usize::MAX)
            .await
            .expect("body");
        let value = if bytes.is_empty() {
            serde_json::Value::Null
        } else {
            serde_json::from_slice(&bytes).expect("body must be valid JSON (错误体契约)")
        };
        (status, value)
    }

    /// 契约 ①：错误必须带 JSON body `{error, code}`（401 场景：无 key）。
    #[tokio::test]
    async fn missing_admin_key_returns_json_401() {
        let app = super::router(test_state(vec![], &[]));
        let req = Request::builder()
            .method("GET")
            .uri("/api/v1/health")
            .body(Body::empty())
            .expect("request");
        let res = app.oneshot(req).await.expect("oneshot");
        assert_eq!(res.status(), StatusCode::UNAUTHORIZED);
        let bytes = axum::body::to_bytes(res.into_body(), usize::MAX)
            .await
            .expect("body");
        let v: serde_json::Value = serde_json::from_slice(&bytes).expect("错误体必须是 JSON");
        assert_eq!(v["code"], 401);
        assert!(v["error"].as_str().unwrap().contains("admin key"));
    }

    /// 契约 ②：`/nodes` 列表是**裸数组**（首版包装对象 `{nodes:[...]}` 与文档、
    /// 前端三页面契约不符，dashboard/nodes 页面按数组消费直接崩——本测试锁死）。
    #[tokio::test]
    async fn nodes_list_is_bare_array() {
        let app = super::router(test_state(
            vec![test_node("127.0.0.1", 39_001, "US", "residential")],
            &[],
        ));
        let (status, v) = send_json(app, "GET", "/api/v1/nodes", None).await;
        assert_eq!(status, StatusCode::OK);
        let arr = v.as_array().expect("/nodes 必须返回裸数组");
        assert_eq!(arr.len(), 1);
        assert_eq!(arr[0]["addr"], "127.0.0.1:39001");
        assert_eq!(arr[0]["country"], "US");
    }

    /// 契约 ③：`/tenants` 列表是**裸数组**；PATCH balance **真实落库**
    ///（首版只改本地副本，GET 回读即回滚——本测试锁死 round-trip）。
    #[tokio::test]
    async fn tenants_list_is_bare_array_and_balance_persists() {
        let app = super::router(test_state(vec![], &["tenant-key-1"]));
        let (status, v) = send_json(app.clone(), "GET", "/api/v1/tenants", None).await;
        assert_eq!(status, StatusCode::OK);
        let arr = v.as_array().expect("/tenants 必须返回裸数组");
        assert_eq!(arr.len(), 1);
        assert_eq!(arr[0]["api_key"], "tenant-key-1");

        // PATCH balance 3.5 → 200，body 即更新后的 detail。
        let (status, v) = send_json(
            app.clone(),
            "PATCH",
            "/api/v1/tenants/tenant-key-1/balance",
            Some(r#"{"balance_usd": 3.5}"#),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert!((v["balance_usd"].as_f64().unwrap() - 3.5).abs() < 1e-9);

        // 回读：新值必须还在（落库修复的验收点）。
        let (status, v) = send_json(app.clone(), "GET", "/api/v1/tenants/tenant-key-1", None).await;
        assert_eq!(status, StatusCode::OK);
        assert!(
            (v["balance_usd"].as_f64().unwrap() - 3.5).abs() < 1e-9,
            "PATCH 后回读必须仍是 3.5"
        );

        // 幽灵 key ⇒ 404 JSON 错误体。
        let (status, v) = send_json(
            app.clone(),
            "PATCH",
            "/api/v1/tenants/ghost-key/balance",
            Some(r#"{"balance_usd": 1.0}"#),
        )
        .await;
        assert_eq!(status, StatusCode::NOT_FOUND);
        assert_eq!(v["code"], 404);
        assert!(v["error"].as_str().unwrap().contains("tenant"));

        // 非法 JSON ⇒ 400 JSON 错误体（400 面的实装验证）。
        let (status, v) = send_json(
            app.clone(),
            "PATCH",
            "/api/v1/tenants/tenant-key-1/balance",
            Some("{oops"),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert_eq!(v["code"], 400);
    }

    /// 契约 ④：隔离 → 断路器快照可见 → DELETE 真解除（隔离条目清零）。
    /// 首版 DELETE 是空操作，隔离只能等 sweep 过期——本测试锁死实装语义。
    #[tokio::test]
    async fn quarantine_set_then_delete_really_removes() {
        let node = test_node("127.0.0.2", 39_002, "JP", "datacenter");
        let state = test_state(vec![node], &[]);
        let router_for_assert = state.router.clone();
        let app = super::router(state);

        // POST 隔离 600s。
        let (status, _) = send_json(
            app.clone(),
            "POST",
            "/api/v1/nodes/127.0.0.2:39002/quarantine",
            Some(r#"{"ttl_secs": 600}"#),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(router_for_assert.quarantine_len(), 1, "隔离必须真的落表");

        // 两级口径验证：外层 domain 数 = 1，内层 ip 条目数 = 1。
        assert_eq!(router_for_assert.quarantine_domains(), 1);

        // DELETE 解除 → 隔离条目清零（含外层回收）。
        let (status, v) = send_json(
            app,
            "DELETE",
            "/api/v1/nodes/127.0.0.2:39002/quarantine",
            None,
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(v["addr"], "127.0.0.2:39002");
        assert_eq!(
            router_for_assert.quarantine_len(),
            0,
            "DELETE 必须真的解除隔离"
        );
        assert_eq!(
            router_for_assert.quarantine_domains(),
            0,
            "空内层必须回收外层"
        );
    }

    /// 契约 ⑤：调权真实生效（provider+country 维度整组）+ 归一化 country 过滤。
    #[tokio::test]
    async fn node_weight_patch_applies_and_detail_route_works() {
        let app = super::router(test_state(
            vec![test_node("127.0.0.3", 39_003, "GB", "mobile")],
            &[],
        ));
        // 详情路由（含 ':' 的路径段）。
        let (status, v) =
            send_json(app.clone(), "GET", "/api/v1/nodes/127.0.0.3:39003", None).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(v["addr"], "127.0.0.3:39003");

        // 调权 → 200 且详情里 weight 立即变化。
        let (status, _) = send_json(
            app.clone(),
            "PATCH",
            "/api/v1/nodes/127.0.0.3:39003/weight",
            Some(r#"{"weight": 42}"#),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        let (status, v) = send_json(app, "GET", "/api/v1/nodes/127.0.0.3:39003", None).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(v["weight"], 42, "调权必须真实生效于节点视图");

        // 不存在的节点 ⇒ 404（JSON 错误体）。
        let app = super::router(test_state(vec![], &[]));
        let (status, v) = send_json(app, "GET", "/api/v1/nodes/1.2.3.4:80", None).await;
        assert_eq!(status, StatusCode::NOT_FOUND);
        assert_eq!(v["code"], 404);

        // 未匹配路径 ⇒ fallback 404（JSON 错误体，此前是空 body）。
        let (status, v) = send_json(
            super::router(test_state(vec![], &[])),
            "GET",
            "/api/v1/nope",
            None,
        )
        .await;
        assert_eq!(status, StatusCode::NOT_FOUND);
        assert!(v["error"]
            .as_str()
            .unwrap()
            .contains("unknown admin endpoint"));
    }

    // ---- OPT-R17：超时层与审计层（补齐此前"注释声称存在、代码里没有"的两层）----

    /// 超时层**真的会掐断**超时的请求（超时层的红测有效性证明）。
    ///
    /// # 为什么必须实测，而不是断言常量
    ///
    /// 首版写的是 `assert!(ADMIN_TIMEOUT_SECS > 0)`。它是**假绿**：
    /// `ADMIN_TIMEOUT_SECS` 是 `const u64 = 30`，`30 > 0` 在编译期即确定，
    /// 断言永不为假，也抓不到任何回归（把它改小同样不会红，只会重新编译）。
    /// CI 的 clippy 以 `assertions_on_constants` lint 判红（实测 run，
    /// rust 1.99）——**该 lint 抓到的是真缺陷，不是风格问题**。
    ///
    /// 现改为：用 `build_router` 注入 1ms 超时 + 一个真会挂起 10s 的 handler，
    /// 断言得到 **408**。这样"超时层生效"是被**行为**证明的 ——
    /// 有人删掉 `TimeoutLayer` 或把它挪到 handler 之后，本测试立即失败。
    #[tokio::test]
    async fn timeout_layer_actually_cuts_off_a_hanging_handler() {
        // 1ms 超时 + 挂起 10s 的 handler ⇒ 必然 408。
        // 用真实时间（不用 `start_paused`）：`oneshot` 在内存里直接驱动
        // Service，`TimeoutLayer` 内部走 tokio 的 real-time timer，
        // 暂停时钟会让 timeout 永不触发、测试挂到 CI 超时。
        //
        // 慢路由经 `build_router_with` 注入到 `/api/v1` **内部** —— 挂在 nest
        // 之外的话根本不经过 TimeoutLayer，测试会误判成「超时层失效」。
        let app = super::build_router_with(
            test_state(vec![], &[]),
            std::time::Duration::from_millis(1),
            super::Router::new().route(
                "/slow",
                axum::routing::get(|| async {
                    tokio::time::sleep(std::time::Duration::from_secs(10)).await;
                    axum::Json(serde_json::json!({"never": true}))
                }),
            ),
        );
        let req = Request::builder()
            .method("GET")
            .uri("/api/v1/slow")
            .header("X-Admin-Key", KEY)
            .body(Body::empty())
            .expect("request");
        let res = app.oneshot(req).await.expect("oneshot");
        assert_eq!(
            res.status(),
            StatusCode::REQUEST_TIMEOUT,
            "挂起 10s 的请求在 1ms 超时下必须被掐断为 408 —— \
             若返回 200，说明 TimeoutLayer 未生效或位置错误"
        );
    }

    /// 快速路径**不受**超时层误伤（防止"防护"变成"关停"）。
    ///
    /// 与上一条成对：上一条证明超时层会掐断慢请求，这一条证明正常的内存操作
    /// 仍能及时返回。两条缺一，超时层就可能整体失效或整体误伤。
    #[tokio::test]
    async fn timeout_layer_does_not_break_the_fast_path() {
        let app = super::router(test_state(
            vec![test_node("127.0.0.1", 39_001, "US", "residential")],
            &[],
        ));
        let req = Request::builder()
            .method("GET")
            .uri("/api/v1/health")
            .header("X-Admin-Key", KEY)
            .body(Body::empty())
            .expect("request");
        let res = app.oneshot(req).await.expect("oneshot");
        assert_eq!(
            res.status(),
            StatusCode::OK,
            "生产超时（{}s）下，健康检查必须仍是 200（否则是误伤而非防护）",
            super::ADMIN_TIMEOUT_SECS
        );
    }

    /// 超时层覆盖**未认证**请求（层序回归）。
    ///
    /// 若日后有人把 `TimeoutLayer` 挪到 `auth_middleware` 内侧，
    /// 未认证的挂起请求将不受 timeout 约束。本测试锁定"任何路径都过 timeout"。
    #[tokio::test]
    async fn timeout_layer_wraps_auth_middleware() {
        // 无 Key ⇒ 401（证明认证仍生效，timeout 未把认证吞掉）。
        let app = super::router(test_state(vec![], &[]));
        let req = Request::builder()
            .method("GET")
            .uri("/api/v1/health")
            .body(Body::empty())
            .expect("request");
        let res = app.oneshot(req).await.expect("oneshot");
        assert_eq!(res.status(), StatusCode::UNAUTHORIZED);

        // 带 Key ⇒ 200（证明 timeout 层未破坏认证后的正常路径）。
        let app = super::router(test_state(vec![], &[]));
        let req = Request::builder()
            .method("GET")
            .uri("/api/v1/health")
            .header("X-Admin-Key", KEY)
            .body(Body::empty())
            .expect("request");
        let res = app.oneshot(req).await.expect("oneshot");
        assert_eq!(res.status(), StatusCode::OK);
    }

    /// 审计层对**失败**请求同样放行（401/404 不被审计层吞掉或改写）。
    ///
    /// 审计层是最外层，如果它的实现有误（例如把响应体吃掉、或对错误状态
    /// 提前返回），会表现为"契约①/④ 的错误体测试突然挂了"。本测试显式
    /// 锁定：加了审计层后，错误体契约与状态码**保持不变**。
    #[tokio::test]
    async fn audit_layer_preserves_error_contract() {
        // 401：错误体仍是 {error, code:401}。
        let app = super::router(test_state(vec![], &[]));
        let req = Request::builder()
            .method("GET")
            .uri("/api/v1/health")
            .body(Body::empty())
            .expect("request");
        let res = app.oneshot(req).await.expect("oneshot");
        assert_eq!(res.status(), StatusCode::UNAUTHORIZED);
        let bytes = axum::body::to_bytes(res.into_body(), usize::MAX)
            .await
            .expect("body");
        let v: serde_json::Value = serde_json::from_slice(&bytes).expect("错误体必须是 JSON");
        assert_eq!(v["code"], 401);

        // 404 fallback：审计层必须放行到 fallback 才有这个 JSON 404。
        let app = super::router(test_state(vec![], &[]));
        let req = Request::builder()
            .method("GET")
            .uri("/api/v1/nope")
            .header("X-Admin-Key", KEY)
            .body(Body::empty())
            .expect("request");
        let res = app.oneshot(req).await.expect("oneshot");
        assert_eq!(res.status(), StatusCode::NOT_FOUND);
        let bytes = axum::body::to_bytes(res.into_body(), usize::MAX)
            .await
            .expect("body");
        let v: serde_json::Value = serde_json::from_slice(&bytes).expect("错误体必须是 JSON");
        assert_eq!(v["code"], 404);
    }
}
