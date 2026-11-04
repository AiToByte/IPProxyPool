//! 纯判定：环境门、鉴权错误码、free 档凭据门、租户会话命名。
//!
//! 这一组全是**无状态纯函数**（除了两个读 `Session` 头的谓词），刻意不碰
//! 路由器与遥测——它们是"这个请求该不该被拒/该归谁"的判断，单测可直接断言，
//! 不需要起 Pingora。

use super::*;

/// OPT-2 纯谓词：环境门是否应拦截本次请求。
///
/// - 仅当开关打开 **且** 客户端完全没传 `X-API-Key` 头时返回 true；
/// - 传了 Key（即使是错 Key）走正常租户鉴权路径，由 `TenantManager` 判 403/429，
///   以便错误归因（坏 Key vs 缺 Key）保持可区分。
pub fn should_reject_missing_api_key(require_api_key: bool, has_api_key_header: bool) -> bool {
    require_api_key && !has_api_key_header
}

/// R2-3 鉴权错误 → HTTP 状态映射（纯函数，可单测）。
///
/// - 速率/并发超限 → 429（可重试）；
/// - 欠费 → 402（充值后恢复，与 403 身份问题区分，计费对账可观测）；
/// - 其余（坏 Key / 停用）→ 403。
pub fn status_for_auth_error(msg: &str) -> u16 {
    if msg.contains("Rate limit") || msg.contains("concurrency") {
        429
    } else if msg.contains("balance") {
        402
    } else {
        403
    }
}

/// D1 纯谓词：`tier=free` 请求是否夹带凭据头（网关回 403 拦截）。
///
/// - free 是匿名共享出口：下游凭据（`Authorization` Bearer/支付 token、
///   `Cookie` 会话/支付状态——支付类敏感信息现实中就走这两载体，
///   不存在独立的“支付头”标准）一旦经免费节点出站即泄露给陌生出口，
///   故直接拒绝；误伤面：free 匿名流量本就没有认证头（注释写明）。
/// - 网关自己的 `X-API-Key` 不在此列（那是网关身份，稍后 scrub 环节摘除）。
/// - tier 精确等于 `"free"` 才拦截（与 router 选路 `node.tier == *t` 同口径，
///   大小写敏感；`free-socks` 等节点档不是请求约束档，不在此列）。
/// - 非 free 档一律 false（付费/default 存量语义零变化）。
pub fn free_tier_with_credentials(
    tier: Option<&str>,
    has_authorization: bool,
    has_cookie: bool,
) -> bool {
    tier == Some("free") && (has_authorization || has_cookie)
}

/// 下游凭据载体探测：`Authorization`（Bearer／支付 token）。
///
/// `HeaderMap::get` 大小写不敏感，故 `authorization`／`Authorization`／全大写
/// 等形态全覆盖。抽成自由函数供 `request_filter`（显式 `tier=free` 提前拒绝）
/// 与 `upstream_peer`（OPT-R6 S1 实际节点档位兜底）共用，避免两处探测口径漂移。
#[inline]
pub fn has_authorization(session: &Session) -> bool {
    session
        .req_header()
        .headers
        .contains_key(http::header::AUTHORIZATION)
}

/// 下游凭据载体探测：`Cookie`（会话／支付状态）。口径同 [`has_authorization`]。
#[inline]
pub fn has_cookie(session: &Session) -> bool {
    session
        .req_header()
        .headers
        .contains_key(http::header::COOKIE)
}

/// OPT-R6 S1（P0 安全）：**按实际选中节点的档位**判定凭据外泄风险。
///
/// # 为什么 `free_tier_with_credentials` 不够（OPT-R6 S1 根因）
///
/// 旧护栏只看「请求声明的 tier」（`X-Proxy-Tier` 头），而选路 `RouterEngine::matches`
/// 在 `spec.tier == None` 时**直接跳过档位检查**（`router.rs`）——即无 tier 约束的请求
/// 按权重命中节点时，**完全可能命中免费节点**。于是存在一条绕过路径：
///
/// ```text
/// 请求带 Authorization: Bearer <token>（或 Cookie）
/// 但不发 X-Proxy-Tier 头（spec.tier = None）
///   → 旧护栏 tier != Some("free") → 放行
///   → 选路按权重命中一个匿名免费出口
///   → 凭据随请求出站，泄露给陌生第三方
/// ```
///
/// 选路是加权随机的，客户端**无法控制**命中结果，因此也不能靠「我声明 res 就一定
/// 走付费节点」来自证安全。**唯一正确的判定依据是实际将要出站的节点档位。**
///
/// # 本函数的定位
///
/// 在**选路之后、出站之前**调用（`upstream_peer` 与 `serve_via_socks` 两条出站路径），
/// 用实际选中的 `ProxyNode` 判定。显式 `tier=free` 的提前拒绝仍由
/// `free_tier_with_credentials` 保留（省一次选路），本函数是**兜底那道闸**。
///
/// # 参数口径
///
/// `node_tier` 取自 `ProxyNode::tier`——`ProxyNode::new` 入口已归一为小写长名
/// （见 `model::canonical_tier`，`free` 无短名故归一前后同为 `"free"`）。
/// 比较用 `eq_ignore_ascii_case` 而非 `canonical_tier`：后者会为每次调用分配
/// `String`，而本函数在**每请求的出站路径**上执行；且归一是幂等的，
/// 大小写不敏感比较与「归一后严格比较」对 `free` 档**完全等价**，
/// 同时对非归一输入（未来新增档位）保持防御性。
#[inline]
pub fn free_node_exits_with_credentials(
    node_tier: &str,
    has_authorization: bool,
    has_cookie: bool,
) -> bool {
    node_tier.eq_ignore_ascii_case("free") && (has_authorization || has_cookie)
}

/// R2-2 会话租户隔离：把裸 `session_id` 命名为 `{tenant}:{session}`。
///
/// - 网关在鉴权成功后调用，路由器只见命名后的不透明 key（零改动）；
/// - `tenant=None`（理论走不到，鉴权后必有）时沿用裸 key，保证单测/直调兼容；
/// - 已命名（含 `:`）的不重复包一层（幂等，重入安全）。
pub fn apply_tenant_namespace(spec: &mut RoutingSpec, tenant_id: Option<&str>) {
    let session = match spec.session_id.clone() {
        Some(s) => s,
        None => return,
    };
    let tenant = match tenant_id {
        Some(t) if !t.is_empty() => t,
        _ => return,
    };
    if session.starts_with(&format!("{tenant}:")) {
        return;
    }
    spec.session_id = Some(format!("{tenant}:{session}"));
}
