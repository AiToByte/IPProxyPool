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
use crate::model::{EgressProto, ProxyNode};
use crate::node_registry::{NodeRegistry, RegistryError, MAX_WEIGHT};
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
    /// 步骤 52：托管节点注册表（运行时增删 + 持久化）。
    ///
    /// 与 `router` 的分工：`router` 是**实时池**（托管节点 + FreePool 的
    /// `free-*` + `TEST_POOL_NODES` 注入）；`nodes` 只持有运维通过控制面
    /// 增删的托管节点，是 `NODES_FILE` 的唯一内容来源。
    pub nodes: Arc<NodeRegistry>,
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
            nodes: Arc::clone(&self.nodes),
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

/// OpenAPI 3.1 文档聚合。所有 handler 路径与 schema 必须在此注册。
#[derive(OpenApi)]
#[openapi(
    paths(
        health_handler,
        list_nodes_handler,
        add_node_handler,
        get_node_handler,
        delete_node_handler,
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
            AddNodeRequest,
            RemoveNodeResponse,
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
/// 读 FreePool 快照。绝大多数是**进程内内存操作**（`DashMap` / `ArcSwap`），
/// 正常应在亚毫秒级返回；步骤 52 起节点增删会同步落盘（临时文件 + fsync +
/// rename，KB 级），正常仍在毫秒级。因此超时取得很宽松——它不是性能目标，
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
        .route("/nodes", post(add_node_handler))
        .route("/nodes/:addr", get(get_node_handler))
        .route("/nodes/:addr", delete(delete_node_handler))
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
    //     无限占用连接的机会（绝大多数端点亚毫秒；节点增删含一次同步落盘，
    //     30s 仍远高于正常耗时）。
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

// ---------------------------------------------------------------------------
// 子模块划分（OPT-R18 P2-1 第五批）
// ---------------------------------------------------------------------------
// 本文件曾 1791 行，且**结构上是"一个 800 行的 build_router_with"**——路由表、
// 中间件、16 个 REST 处理器全挤在同一个函数区间里。改一个端点要在 1200 行里定位。
// 现按职责拆分（全部**逐字搬迁**，行为零变更）：
//
// | 子模块 | 职责 |
// | --- | --- |
// | `dto.rs` | 请求/响应 DTO 与错误体构造（`ErrorResponse`/`NodeView`/各 `*Request`） |
// | `middleware.rs` | 三层中间件：访问审计、认证（fail-closed）、未匹配端点 404 |
// | `handlers_node.rs` | 节点：列举/新增/详情/删除/调权/隔离/解隔离 |
// | `handlers_tenant.rs` | 租户：列举/详情/余额/启停 |
// | `handlers_ops.rs` | 运维面：健康、OpenAPI、免费池看板、断路器 |
// | `tests.rs` | 全部单测（原文件后半段） |
//
// **为什么按"资源"切处理器而不是按 HTTP 方法切**：读者查"删除节点会发生什么"时，
// 需要一次看到 `delete_node_handler` 与它相邻的 `delete_node_quarantine_handler`
// ——它们共享同一套"先 `get_node_by_addr` 再判存在"的前置逻辑。按方法切会把
// GET/PATCH/DELETE 拆到三个文件，反而要来回跳。
//
// **为什么 `node_view` 留在 `dto.rs`**：它是"领域对象 → 传输对象"的唯一转换点，
// 三个处理器模块都要用。放在 dto 里保证**序列化形状只有一处定义**，改字段不会
// 出现"某个端点忘了改"的漂移。

mod dto;
mod handlers_node;
mod handlers_ops;
mod handlers_tenant;
mod mw;

use handlers_node::*;
use handlers_ops::*;
use handlers_tenant::*;
use mw::*;

pub use dto::ApiError;
pub use dto::{
    error_response, AddNodeRequest, AutotuneRequest, AutotuneStatus, ErrorResponse, HealthResponse,
    NodeQuarantineRequest, NodeView, NodeWeightRequest, RemoveNodeResponse, TenantActiveRequest,
    TenantBalanceRequest,
};

#[cfg(test)]
mod tests;
