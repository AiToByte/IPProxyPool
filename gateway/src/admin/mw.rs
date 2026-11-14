//! 三层中间件：访问审计、认证、未匹配端点 404。
//!
//! **层序是有意的**（见 `build_router_with`）：审计在最外层，故 401 也会被记录
//! ——"大量 401"恰恰是最有价值的信号。认证 fail-closed：`ADMIN_API_KEY` 未配置时
//! 全部拒绝，宁可拒绝服务也不接受一个"等于没配"的密钥。
//!
//! 审计用 `log` 手写而非 `tower-http::TraceLayer`：本项目装的是 `env_logger`
//! （`log` facade），**没有 `tracing` subscriber**，用 `TraceLayer` 会得到
//! "看起来加了审计、实际一条都不打印"的静默失效。详见 `audit_logging` 的注释。

use super::*;

/// 审计路径只允许固定模板，绝不把客户端提供的路径段写入日志。
///
/// 外层中间件拿到的 `MatchedPath` 可能只有嵌套前缀，未匹配时也不存在，
/// 不能回退到原始 URI。采用白名单后，Key 的原文、部分或完整 percent 编码、
/// 编码斜杠及未知后缀都不会回显；无需解码，也不会改变请求的路由与参数。
/// 未知路径统一占位，避免 404 或畸形路径成为凭据泄漏的旁路。
pub(super) fn audit_path(path: &str) -> &'static str {
    match path {
        "/api/v1/health" => "/api/v1/health",
        "/api/v1/openapi.json" => "/api/v1/openapi.json",
        "/api/v1/nodes" => "/api/v1/nodes",
        "/api/v1/tenants" => "/api/v1/tenants",
        "/api/v1/freepool/summary" => "/api/v1/freepool/summary",
        "/api/v1/freepool/sources" => "/api/v1/freepool/sources",
        "/api/v1/freepool/autotune" => "/api/v1/freepool/autotune",
        "/api/v1/circuit" => "/api/v1/circuit",
        _ => {
            let mut segments = path.strip_prefix("/api/v1/").unwrap_or("").split('/');
            match (
                segments.next(),
                segments.next(),
                segments.next(),
                segments.next(),
            ) {
                (Some("tenants"), Some(key), None, None) if !key.is_empty() => {
                    "/api/v1/tenants/{api_key}"
                }
                (Some("tenants"), Some(key), Some("balance"), None) if !key.is_empty() => {
                    "/api/v1/tenants/{api_key}/balance"
                }
                (Some("tenants"), Some(key), Some("active"), None) if !key.is_empty() => {
                    "/api/v1/tenants/{api_key}/active"
                }
                (Some("nodes"), Some(addr), None, None) if !addr.is_empty() => {
                    "/api/v1/nodes/{addr}"
                }
                (Some("nodes"), Some(addr), Some("weight"), None) if !addr.is_empty() => {
                    "/api/v1/nodes/{addr}/weight"
                }
                (Some("nodes"), Some(addr), Some("quarantine"), None) if !addr.is_empty() => {
                    "/api/v1/nodes/{addr}/quarantine"
                }
                _ => "/[unmatched]",
            }
        }
    }
}

/// 写入一条统一格式的 Admin 审计记录。
fn emit_audit_log(
    method: &http::Method,
    path: &'static str,
    status: u16,
    elapsed_ms: u128,
    has_admin_key: bool,
) {
    // 2xx/fallback 走 info，4xx/5xx 走 warn：撞门与故障应能在默认日志级别下被发现。
    if (200..400).contains(&status) {
        log::info!("[admin] {method} {path} -> {status} ({elapsed_ms}ms, key={has_admin_key})");
    } else {
        log::warn!("[admin] {method} {path} -> {status} ({elapsed_ms}ms, key={has_admin_key})");
    }
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
pub(super) async fn audit_logging(
    req: axum::extract::Request,
    next: middleware::Next,
) -> axum::response::Response {
    let method = req.method().clone();
    let path = audit_path(req.uri().path());
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

    emit_audit_log(&method, path, status, elapsed_ms, has_admin_key);
    response
}

/// 未匹配端点的统一 404（JSON 错误体）。
pub(super) async fn api_not_found_fallback(req: axum::extract::Request) -> ApiError {
    // Router 的 fallback 不经过外层 Service layer，必须在此补记审计；仍只使用
    // 固定路径模板，避免未匹配路径中的租户 Key 或 percent 编码值进入日志。
    let method = req.method().clone();
    let path = audit_path(req.uri().path());
    let has_admin_key = req.headers().contains_key("X-Admin-Key")
        || req
            .headers()
            .get(header::AUTHORIZATION)
            .map(|value| value.as_bytes().starts_with(b"Admin "))
            .unwrap_or(false);
    emit_audit_log(
        &method,
        path,
        StatusCode::NOT_FOUND.as_u16(),
        0,
        has_admin_key,
    );
    error_response(StatusCode::NOT_FOUND, "unknown admin endpoint")
}

/// 认证中间件：校验 `X-Admin-Key` 请求头等于 `ADMIN_API_KEY`。
/// 未配置 `ADMIN_API_KEY` 时全部拒绝（fail-closed）。
pub(super) async fn auth_middleware(
    State(state): State<AdminState>,
    headers: HeaderMap,
    request: axum::extract::Request,
    next: middleware::Next,
) -> Result<axum::response::Response, ApiError> {
    if state.admin_api_key.is_empty() {
        return Err(error_response(
            StatusCode::UNAUTHORIZED,
            "missing or invalid admin key",
        ));
    }

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
