//! 节点管理处理器：列举 / 新增 / 详情 / 删除 / 调权 / 隔离 / 解隔离。
//!
//! 六个端点共享同一套前置逻辑：先 `get_node_by_addr` 判存在（不存在即 404），
//! 再做动作。**幂等**是有意设计：解隔离对未隔离节点也返回 200，调权对相同权重
//! 也返回 200——管理后台的重试不该因为"已经是目标状态"而报错。

use super::dto::{node_view, registry_error_response};
use super::*;

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
pub(super) async fn list_nodes_handler(
    State(state): State<AdminState>,
    Query(filter): Query<NodeFilter>,
) -> Json<Vec<NodeView>> {
    let nodes = state.router.list_nodes(&filter);
    Json(nodes.iter().map(|n| node_view(n)).collect())
}

/// POST `/api/v1/nodes` —— 运行时新增节点（步骤 52）。
///
/// # 写入语义
///
/// 先由 `NodeRegistry` 校验并落盘（`NODES_FILE`），成功后才应用到
/// `RouterEngine`；落盘失败返回 500 且**内存不变**。详见
/// `node_registry.rs` 模块头。
///
/// # 为什么先查 `router` 再查 registry
///
/// `router` 是实时池，包含 FreePool 的 `free-*` 与 `TEST_POOL_NODES` 注入
/// ——它们不在托管清单里，但同 `addr` 同样必须判 409，否则会加出一个
/// “同地址重复节点”。
#[utoipa::path(
    post,
    path = "/api/v1/nodes",
    tag = "nodes",
    request_body = AddNodeRequest,
    responses(
        (status = 201, description = "节点已创建并生效", body = NodeView),
        (status = 400, description = "字段非法（IP/端口/tier/country/provider/weight/凭据）", body = ErrorResponse),
        (status = 409, description = "地址已存在", body = ErrorResponse),
        (status = 500, description = "持久化/应用失败（内存未变更）", body = ErrorResponse)
    )
)]
pub(super) async fn add_node_handler(
    State(state): State<AdminState>,
    payload: Result<AxumJson<AddNodeRequest>, JsonRejection>,
) -> Result<(StatusCode, Json<NodeView>), ApiError> {
    let AxumJson(req) = payload
        .map_err(|e| error_response(StatusCode::BAD_REQUEST, format!("invalid JSON body: {e}")))?;

    let proto = match req.proto.as_deref() {
        None => EgressProto::Http,
        Some(tok) => EgressProto::from_token(tok).ok_or_else(|| {
            error_response(
                StatusCode::BAD_REQUEST,
                format!("proto {tok:?} must be one of http/socks5/socks4"),
            )
        })?,
    };

    let node = ProxyNode::new(
        req.ip,
        req.port,
        req.username,
        req.password,
        req.country,
        req.tier,
        req.provider,
        req.weight,
    )
    .with_proto(proto);

    // 实时池判重（含 free-* 与测试注入）；registry 只覆盖托管节点。
    if state.router.get_node_by_addr(&node.addr).is_some() {
        return Err(error_response(
            StatusCode::CONFLICT,
            format!("node {} already exists", node.addr),
        ));
    }

    let added = state
        .nodes
        .add(node, |n| state.router.add_node(n.clone()))
        .await
        .map_err(registry_error_response)?;

    log::info!(
        "[Admin] node added: {} provider={} tier={} country={} proto={:?}",
        added.addr,
        added.provider,
        added.tier,
        added.country,
        added.proto
    );
    Ok((StatusCode::CREATED, Json(node_view(&added))))
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
pub(super) async fn get_node_handler(
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

/// DELETE `/api/v1/nodes/{addr}` —— 运行时删除**托管**节点（步骤 52）。
///
/// # 拒绝删除的两类节点（409，附可执行建议）
///
/// - `free-*`：由 FreePool 每轮 merge 重建，删了几秒后就会回来——假操作。
///   要排除请用 `PATCH /nodes/{addr}/weight {"weight":0}`（权重持久化到
///   托管节点；免费节点是运行时策略，重启后由 FreePool 重新决定）。
/// - `TEST_POOL_NODES` 注入：测试通道，不在托管清单里，重启即消失。
///
/// 真正的托管节点：删除会**先落盘**再移出实时池，重启后仍不在。
#[utoipa::path(
    delete,
    path = "/api/v1/nodes/{addr}",
    tag = "nodes",
    params(
        ("addr" = String, Path, description = "节点地址 ip:port")
    ),
    responses(
        (status = 200, description = "节点已删除", body = RemoveNodeResponse),
        (status = 404, description = "节点不存在", body = ErrorResponse),
        (status = 409, description = "非托管节点（free-* 或测试注入）", body = ErrorResponse),
        (status = 500, description = "持久化/应用失败（已尝试回滚）", body = ErrorResponse)
    )
)]
pub(super) async fn delete_node_handler(
    State(state): State<AdminState>,
    Path(addr): Path<String>,
) -> Result<Json<RemoveNodeResponse>, ApiError> {
    let Some(existing) = state.router.get_node_by_addr(&addr) else {
        return Err(error_response(
            StatusCode::NOT_FOUND,
            format!("node {addr} not found"),
        ));
    };

    if existing.provider.to_ascii_lowercase().starts_with("free-")
        || existing.provider.eq_ignore_ascii_case("free")
    {
        return Err(error_response(
            StatusCode::CONFLICT,
            format!(
                "node {addr} is managed by FreePool and will be re-added on the next merge; \
                 use PATCH /api/v1/nodes/{addr}/weight with weight=0 instead"
            ),
        ));
    }

    let removed = state
        .nodes
        .remove(&addr, |a| state.router.remove_node(a).is_some())
        .await
        .map_err(|e| match e {
            RegistryError::NotFound(_) => error_response(
                StatusCode::CONFLICT,
                format!(
                    "node {addr} is not operator-managed (e.g. injected via TEST_POOL_NODES); \
                     it is not persisted and will disappear on restart"
                ),
            ),
            other => registry_error_response(other),
        })?;

    log::info!(
        "[Admin] node removed: {} provider={} (persisted)",
        removed.addr,
        removed.provider
    );
    Ok(Json(RemoveNodeResponse {
        addr: removed.addr,
        removed: true,
    }))
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
pub(super) async fn patch_node_weight_handler(
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
    //
    // 步骤 52：权重范围收口到 0..=100（0=摘除），并把**托管节点**的新权重
    // 同步落盘——否则“调 0 摘除一个坏节点”会在重启后满血复活。自动调权
    // （`set_paid_weight_boost`）是运行时策略，刻意不落盘。
    if payload.weight > MAX_WEIGHT {
        return Err(error_response(
            StatusCode::BAD_REQUEST,
            format!("weight must be 0..={MAX_WEIGHT}"),
        ));
    }
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
        .nodes
        .set_group_weight(
            &target.provider,
            &target.country,
            payload.weight,
            |p, c, w| {
                state.router.adjust_vendor_weight(p, c, w);
                Ok(())
            },
        )
        .await
        .map_err(registry_error_response)?;
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
pub(super) async fn post_node_quarantine_handler(
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
pub(super) async fn delete_node_quarantine_handler(
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
