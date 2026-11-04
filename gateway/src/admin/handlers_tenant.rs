//! 租户管理处理器：列举 / 详情 / 余额 / 启停。
//!
//! 响应契约是**裸数组**（`TenantSummary[]`），不是 `{"tenants": [...]}` 包装对象
//! ——首版曾用包装对象，dashboard 按数组消费直接崩。与 `/nodes` 同轮统一。

use super::*;

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
pub(super) async fn list_tenants_handler(
    State(state): State<AdminState>,
) -> Json<Vec<TenantSummary>> {
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
pub(super) async fn get_tenant_handler(
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
pub(super) async fn patch_tenant_balance_handler(
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
pub(super) async fn patch_tenant_active_handler(
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
