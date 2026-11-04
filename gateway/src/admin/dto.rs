//! 请求/响应 DTO 与错误体构造。
//!
//! 这一层的唯一职责是**定义线上形状**：JSON 字段名、可选性、OpenAPI schema。
//! 全部经 `utoipa::ToSchema` 暴露给 `/api/v1/openapi.json`，故字段名与文档是同一份
//! 真相——改字段名会同时改文档，不会出现"文档说 A、实际返回 B"。
//!
//! `node_view` 是领域对象 → 传输对象的**唯一**转换点；三个处理器模块都用它，
//! 保证序列化形状只有一处定义。

use super::*;

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

/// 步骤 52：`RegistryError` → HTTP 契约映射。
///
/// - `Validation` → 400（字段非法）
/// - `Duplicate` / `NotManaged` → 409（地址冲突 / 免费池或测试节点不可删）
/// - `NotFound` → 404
/// - `Persist` / `Apply` → 500（落盘/应用失败；不谎报成功）
///
/// 500 额外 `error!` 留完整原因（含文件路径），响应体仍只给用户可读摘要。
pub(super) fn registry_error_response(e: RegistryError) -> ApiError {
    use RegistryError::*;
    let status = match &e {
        Validation(_) => StatusCode::BAD_REQUEST,
        Duplicate(_) => StatusCode::CONFLICT,
        NotFound(_) => StatusCode::NOT_FOUND,
        Persist(_) | Apply(_) => StatusCode::INTERNAL_SERVER_ERROR,
    };
    if status == StatusCode::INTERNAL_SERVER_ERROR {
        log::error!("[Admin] node registry error: {e}");
    }
    error_response(status, e.to_string())
}

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

fn default_node_weight() -> u32 {
    100
}

/// 新增节点请求（步骤 52，`POST /api/v1/nodes`）。
///
/// 字段校验在 `NodeRegistry::add` 里统一做（返回 400 的具体原因）；
/// `proto` 缺省 `http`，`weight` 缺省 100（满权）。
#[derive(Debug, Serialize, Deserialize, ToSchema)]
pub struct AddNodeRequest {
    pub ip: String,
    pub port: u16,
    pub country: String,
    pub tier: String,
    pub provider: String,
    #[serde(default = "default_node_weight")]
    pub weight: u32,
    #[serde(default)]
    pub proto: Option<String>,
    #[serde(default)]
    pub username: Option<String>,
    #[serde(default)]
    pub password: Option<String>,
}

/// 删除节点响应（步骤 52，`DELETE /api/v1/nodes/{addr}`）。
#[derive(Debug, Serialize, Deserialize, ToSchema)]
pub struct RemoveNodeResponse {
    pub addr: String,
    pub removed: bool,
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
pub(super) fn node_view(n: &ProxyNode) -> NodeView {
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
