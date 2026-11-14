//! 运维面处理器：健康检查、OpenAPI 文档、免费池看板、断路器状态。
//!
//! 这些端点只读、不改变任何状态，供管理后台与巡检脚本使用。免费池看板的字段
//! 语义见 `docs/OPERATION.md` §4.2（三个指标各自回答什么问题、为什么节点数
//! 抓不到"池子已废"）。

use super::*;

#[derive(Debug, Serialize, Deserialize, ToSchema)]
pub struct ProbeResponse {
    pub status: &'static str,
    pub phase: &'static str,
    pub reason: &'static str,
}

/// `GET /livez` is intentionally unauthenticated and independent of dependencies.
pub(super) async fn livez_handler() -> Json<ProbeResponse> {
    Json(ProbeResponse {
        status: "ok",
        phase: "live",
        reason: "event_loop_responding",
    })
}

/// `GET /readyz` reports only fixed state labels and never probes dependencies inline.
pub(super) async fn readyz_handler(
    State(state): State<AdminState>,
) -> (StatusCode, Json<ProbeResponse>) {
    let decision = state.health.evaluate();
    let status = if decision.ready {
        StatusCode::OK
    } else {
        StatusCode::SERVICE_UNAVAILABLE
    };
    (
        status,
        Json(ProbeResponse {
            status: if decision.ready { "ok" } else { "unready" },
            phase: decision.phase.as_str(),
            reason: decision.reason.as_str(),
        }),
    )
}

#[utoipa::path(
    get,
    path = "/api/v1/health",
    tag = "health",
    responses(
        (status = 200, description = "服务正常", body = HealthResponse)
    )
)]
pub(super) async fn health_handler() -> Json<HealthResponse> {
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
pub(super) async fn openapi_handler() -> Json<utoipa::openapi::OpenApi> {
    Json(AdminApiDoc::openapi())
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
pub(super) async fn list_free_pool_summary_handler(
    State(state): State<AdminState>,
) -> Json<FreePoolSnapshot> {
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
pub(super) async fn list_free_pool_sources_handler(
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
pub(super) async fn get_free_pool_autotune_handler(
    State(state): State<AdminState>,
) -> Json<AutotuneStatus> {
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
pub(super) async fn post_free_pool_autotune_handler(
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
pub(super) async fn get_circuit_handler(State(state): State<AdminState>) -> Json<BreakerSnapshot> {
    Json(state.metrics.breaker_snapshot())
}
