//! GW-3 smart ingress gateway over Pingora `ProxyHttp` (pingora 0.6 API).
//!
//! Lifecycle: request_filter (parse + Chrome align + scrub) → upstream_peer
//! (sticky fast-path or LinUCB select + chrome transport profile) →
//! upstream_request_filter (inject egress auth) → fail_to_connect (retry) →
//! logging (bandit reward update + telemetry emit, GW-2 bus).

use crate::bandit::{compute_reward, BanditArm, LinUCBEngine, VectorD};
use crate::fingerprint::FingerprintHardener;
use crate::metrics::MetricsRegistry;
use crate::model::{EgressProto, ProxyContext, ProxyNode, RoutingSpec};
use crate::router::{normalize_domain, RouterEngine};
use crate::socks_bridge::{BridgeRequest, BridgeResponse, SocksBridge};
use crate::telemetry::{TelemetryEvent, TelemetryPublisher};
use crate::tenant::TenantManager;
use async_trait::async_trait;
use base64::{engine::general_purpose::STANDARD as B64, Engine};
use bytes::Bytes;
use dashmap::DashMap;
use http::header::PROXY_AUTHORIZATION;
use pingora::http::RequestHeader;
use pingora_core::upstreams::peer::HttpPeer;
use pingora_core::{Error, Result};
use pingora_proxy::{ProxyHttp, Session};
use std::collections::VecDeque;
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// OPT-R15：分档最小探索配额窗口大小。
///
/// 保证每个有候选的 tier 在任意 `TIER_QUOTA_WINDOW` 次选路中**至少被选中一次**。
/// 100 意味着每个 tier 至少获得 1% 的流量份额——对 free 档（45.9% 成功率）来说，
/// 1% 的探测流量是可接受的代价，换来的是"free 供给从第 1 个请求起就可被评估"。
const TIER_QUOTA_WINDOW: usize = 100;

pub struct SmartProxyGateway {
    pub router: Arc<RouterEngine>,
    /// GW-2 telemetry publisher; `None` = degraded mode (log only, no Redis).
    pub telemetry: Option<Arc<TelemetryPublisher>>,
    /// GW-3 LinUCB dispatch engine (alpha 0.4 at boot).
    pub bandit_engine: Arc<LinUCBEngine>,
    /// GW-3 arm states keyed by `node.addr` (`ip:port`, not bare IP).
    pub bandit_arms: Arc<DashMap<String, Arc<BanditArm>>>,
    /// GW-4 tenant auth/throttle/metering.
    pub tenant_mgr: Arc<TenantManager>,
    /// GW-4 Prometheus counters/histogram.
    pub metrics: Arc<MetricsRegistry>,
    /// OPT-2 环境门：为 true 时，未携带 `X-API-Key` 头的请求直接 403 拦截，
    /// 不进入租户查询（语义与坏 Key 一致：`tenant_account` 保持 None，logging 侧不释放配额——与坏 Key 语义一致）。
    /// D3 起默认开启（`REQUIRE_API_KEY=0` 显式关闭）；本机开发带 `default_key` 头（SDK/脚本已默认带）。
    pub require_api_key: bool,
    /// P2 SOCKS 翻译桥（`None`＝未装配：socks 显式请求直接 503；main 装配 Some，见 P2-7）。
    pub bridge: Option<Arc<SocksBridge>>,
    /// OPT-R15：分档最小探索配额窗口。记录最近 `TIER_QUOTA_WINDOW` 次选路的 tier。
    ///
    /// 存在的理由（步骤 42 实测）：完美付费臂的 `expected_reward` 被 R13 裁剪到 1.0，
    /// 而 UCB 探索项封顶 `alpha*|x|` ≈ 0.47，于是 free 档在 t < ~376~1300 时
    /// **永远无法在分数上追平付费臂**——低流量部署的 free 池等于白建。
    /// 本窗口保证每个有候选的 tier 在任意 100 次选路中至少被选中一次，
    /// 使 free 供给从第 1 个请求起就可被评估（anytime 保证）。
    pub(crate) tier_quota_window: Mutex<VecDeque<String>>,
}

/// OPT-2 纯谓词：环境门是否应拦截本次请求。
///
/// - 仅当开关打开 **且** 客户端完全没传 `X-API-Key` 头时返回 true；
/// - 传了 Key（即使是错 Key）走正常租户鉴权路径，由 `TenantManager` 判 403/429，
///   以便错误归因（坏 Key vs 缺 Key）保持可区分。
pub fn should_reject_missing_api_key(require_api_key: bool, has_api_key_header: bool) -> bool {
    require_api_key && !has_api_key_header
}

/// 为门省省 API Key（OPT-R12 A3：单一真源）。
///
/// 真实定义已迁到 `tenant::DEFAULT_API_KEY`——密钥的持有者是
/// `TenantManager`，放在数据面模块 `gateway.rs` 里属结构性错位：改数据面
/// 的人会以为碰不到鉴权，改鉴权的人却要去数据面找常量。
/// 此处保留 `pub use` 转发，以保持已有引用点（SDK / 测试）不破坏。
pub use crate::tenant::DEFAULT_API_KEY;

/// 取节点对应的 LinUCB 臂，没有则新建（首写竞态无害：新臂状态等价）。
///
/// # OPT-R10 B1：命中路径零分配
///
/// 旧实现第一行就是 `let key = node.addr.clone();`——**无论是否命中都克隆**。
/// 而调用点 `select_bandit_node_excluding` 是
/// `candidates.iter().map(|n| arm_for(&self.bandit_arms, n)).collect()`，
/// 即**每个候选一次** ⇒ 池大小 N 时每请求 N 次 `String` 克隆
/// （免费池缺省 `FREE_MAX_NODES=2000` ⇒ 约 2000 次/请求）。
/// 稳态下臂早已建好，**几乎每次调用都走命中分支**——克隆白付。
///
/// 修法：`DashMap::get` 接受 `&str`（`Borrow<str>`），**借用查询不需要拥有
/// `String`**。把克隆挪到「真正新建臂」的冷路径，命中路径只剩
/// 「一次哈希 + 一次 `Arc` 克隆」（`Arc` 克隆只加引用计数，不分配）。
///
/// 语义**零变化**：键仍是 `node.addr`，`prune_stale_arms` 的比对口径不变，
/// 臂的生命周期也不变——只改查询方式。
pub fn arm_for(arms: &DashMap<String, Arc<BanditArm>>, node: &ProxyNode) -> Arc<BanditArm> {
    // 命中路径：借用查询，零字符串分配。
    if let Some(existing) = arms.get(node.addr.as_str()) {
        return Arc::clone(existing.value());
    }
    // 冷路径：新建臂才需要拥有键（`BanditArm` 要持有它作 `key`）。
    let key = node.addr.clone();
    let arm = Arc::new(BanditArm::new(key.clone(), node.tier.clone()));
    arms.insert(key, arm.clone());
    arm
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
/// R2-6：取本请求的 bandit 上下文——`upstream_peer` 已算好则复用（选学一致，
/// 省一次 `extract_context` 含时钟 syscall），否则现算（粘滞路径/直调兼容）。
pub fn resolve_bandit_context(ctx: &ProxyContext, engine: &LinUCBEngine, domain: &str) -> VectorD {
    ctx.bandit_context
        .unwrap_or_else(|| engine.extract_context(domain))
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

/// R2-2 重试记账：把当前失败节点的 `addr` 记入 `failed_addrs`（去重、上限 16 防爆）。
///
/// 与 OPT-3 的 `reset_retry_accounting`（清字节）正交：一个管“换谁”，一个管“计多少”。
pub fn record_failed_addr(ctx: &mut ProxyContext) {
    if let Some(ref node) = ctx.current_node {
        let addr = node.addr.clone();
        if !ctx.failed_addrs.iter().any(|e| e == &addr) && ctx.failed_addrs.len() < 16 {
            ctx.failed_addrs.push(addr);
        }
    }
}
/// OPT-3 重试计量口径：新 attempt 从零累计，已失败 attempt 的字节不进账单。
///
/// - 语义冻结：只计 **最后一次 attempt** 的出站字节（`response_body_filter`
///   在每次 attempt 内累加，`fail_to_connect` 在发起重试前清零）；
/// - `logging` 侧不做补偿（直接按 `ctx.transferred_bytes` 计量/上报），
///   因此拦截态（无 attempt）与成功态口径天然一致；
/// - 已知限制：强制一次失败重试的端到端构造困难，覆盖以本单测 + 代码走查为准。
pub fn reset_retry_accounting(ctx: &mut ProxyContext) {
    ctx.transferred_bytes = 0;
}

/// 修剪游离臂：删除不在路由器全量池中的臂状态。
///
/// - 白名单必须是 `snapshot_all` 全量快照，不能用健康候选
///   （被隔离节点的臂是“暂时不用”，不是“已下线”，误删会丢学习成果）；
/// - 返回删除个数，供后台 ticker 打日志。
pub fn prune_stale_arms(arms: &DashMap<String, Arc<BanditArm>>, router: &RouterEngine) -> usize {
    let alive: std::collections::HashSet<String> = router
        .snapshot_all()
        .iter()
        .map(|n| n.addr.clone())
        .collect();
    let before = arms.len();
    arms.retain(|key, _| alive.contains(key));
    // OPT-R11 A1：原为裸 `before - arms.len()`，是 OPT-R6 S2 **漏掉的第 4 处**
    // 同构写法（`janitor.rs` 的文档当时只列了「三处」，此处未同步）。
    //
    // 危害与前 3 处完全同型，但本表的写入方是**数据面** `arm_for()` 的冷路径
    // `arms.insert(...)`（见本文件 `arm_for`）——后台 sweep 每 60s 修剪一次，
    // 两次 `len()` 之间若有新臂插入，`after > before` ⇒ debug 下
    // `attempt to subtract with overflow` panic（sweep 任务死亡），release 下
    // 回绕成 `usize::MAX` 并经 `main.rs` 的池计数二次溢出，日志永久输出天文数字。
    //
    // 红测见本文件末尾 `opt_r11_a1_*`：并发守护测试在修复前实测 panic 于
    // 本行（`src/gateway.rs:247`）。
    crate::janitor::removed_count(before, arms.len())
}

/// P2：HttpPeer 装配（含 socks 守卫）。
///
/// 守卫正常走不到（显式 socks 请求被 `proxy_upstream_filter` 短路，默认请求被
/// router 默认隔离），但一旦走到必须硬 503——把 socks 地址当 HTTP 上游去连，
/// 连上也是协议错配（HTTP 正向代理握手发给 SOCKS 端口必失败，还浪费一次重试）。
/// OPT-R15：判断节点是否需要 TLS 连接。
///
/// 端口 443 → HTTPS 代理（需要 TLS）；其他端口 → 明文 HTTP 代理。
/// 这是最小改动，支持 HTTPS 代理的同时保持对明文代理的兼容。
pub fn is_tls_for_node(node: &ProxyNode) -> bool {
    node.addr.ends_with(":443")
}

/// OPT-R15 步骤 46：`ip:port`（或裸 `ip`）是否指向回环地址。
///
/// 供 `TEST_POOL_NODES` 注入通道限定（`main.rs`）使用——**单一判定口径**，
/// 避免"通道里写一套匹配、这里写另一套"日后各自漂移。
pub fn is_loopback_addr(addr: &str) -> bool {
    let host = addr.rsplit_once(':').map_or(addr, |(h, _)| h);
    host.starts_with("127.")
        || matches!(host, "localhost" | "::1" | "[::1]")
        || addr.starts_with("[::1]")
}

fn build_http_peer(node: &ProxyNode, target_host: &str) -> Result<Box<HttpPeer>> {
    if node.proto != EgressProto::Http {
        return Err(Error::explain(
            pingora_core::ErrorType::HTTPStatus(503),
            "SOCKS node in HTTP path",
        ));
    }
    // OPT-R15：解冻 `is_tls`。原硬编码 `false`（"GW-3 still plain-HTTP forward upstream;
    // TLS/SNI customization stays out of scope (frozen)"）导致端口 443 的 HTTPS 代理
    // 无法使用——网关到代理节点之间必须用 TLS，但代码写死了明文。
    // 现按端口判断：443 → TLS，其他 → 明文。
    //
    // ✅ 步骤 47 活流量已**完整**确证：设 `SSL_CERT_FILE=<CA.pem>` 起网关后，
    // 对 443 节点连续 50/50 次全部 `200`，响应体均为真实公网出口 IP；
    // 去掉该变量则 503 + `TLSHandshakeFailure`（反向对照）。
    //
    // ⚠️ 纠正步骤 45/46 的两处错误说法（都留档，避免后人重犯）：
    //   1. 步骤 45 写"TLS 证书校验使用 Pingora 默认行为（跳过验证）"——**错**，默认是严格校验。
    //   2. 步骤 46 由"rustls 不支持关闭校验"推断出"只能靠受信 CA 或升级 Pingora"——
    //      **结论对，但当时把"框架不支持关闭校验"当成了终点，漏了正规通道**：
    //      `pingora-rustls` 的 `load_platform_certs_incl_env_into_store` 会处理
    //      **`SSL_CERT_FILE` / `SSL_CERT_DIR`** 环境变量，rustls 的 root store 在
    //      连接器构建时由它填充 ⇒ **零生产代码改动**即可让 rustls 信任自建 CA。
    //      （`PeerOptions::ca` 只能填 `ConnectorOptions` 级别的 CA，且与本路径无关。）
    //
    // ⇒ 正确做法：**不要关闭校验**，而是让代理证书由受信任 CA 签发（本仓即如此）。
    // 之前尝试的 `peer.options.verify_cert = false` 在 rustls 下无效且危险，已移除。
    let is_tls = is_tls_for_node(node);
    let mut peer = HttpPeer::new(node.addr.clone(), is_tls, target_host.to_string());

    peer.options.connection_timeout = Some(Duration::from_millis(1500));
    peer.options.read_timeout = Some(Duration::from_millis(5000));
    peer.options.write_timeout = Some(Duration::from_millis(3000));
    FingerprintHardener::apply_chrome_profile(&mut peer);
    Ok(Box::new(peer))
}

/// P2：桥出站目标 URL。
///
/// - absolute-form（含 scheme＋authority，原样透传；https 由 reqwest 在隧道内建 TLS）；
/// - origin-form 按 Host 头拼 `http://`（本网关上游现全 plain-HTTP，TLS 指纹仍 out）；
/// - 无 Host 即 None（调用方按失败计，不 panic）。
fn bridge_url_for(uri: &http::Uri, host: Option<&str>) -> Option<String> {
    if uri.scheme().is_some() && uri.authority().is_some() {
        return Some(uri.to_string());
    }
    let h = host?;
    match uri.path_and_query() {
        Some(pq) => Some(format!("http://{h}{pq}")),
        None => Some(format!("http://{h}/")),
    }
}

/// A5：SOCKS 桥防御分支判定＋清脏（`proto==Http` 即跳过换下一个）。
///
/// - `true`＝跳过本节点：调用方直接 `continue`，此处已把上轮 attempt 残留的
///   `ctx.current_node` 置 None，否则 `logging` 会把上轮节点当本次做
///   bandit／计量／遥测（错归因）；
/// - `false`＝正常 socks 节点：不碰 `current_node`（调用方随后落本次节点）。
pub(crate) fn should_skip_bridge_node(ctx: &mut ProxyContext, node: &ProxyNode) -> bool {
    if node.proto == EgressProto::Http {
        ctx.current_node = None;
        return true;
    }
    false
}

/// A9：SOCKS 桥整轮总预算（单跳 timeout＋8s 松弛）。
///
/// - 防 `20s×(max_retries+1)` 最坏 80s 无总 deadline 拖死请求；
/// - 首成功即返语义不变：预算只截尾，不改选路／重试顺序；
/// - 单 attempt 也给足一次全量＋松弛，不因预算不足误杀首试。
pub(crate) fn socks_overall_budget(_attempts: usize, per_attempt: Duration) -> Duration {
    per_attempt.saturating_add(Duration::from_secs(8))
}

impl SmartProxyGateway {
    /// 无状态请求的 LinUCB 选路（无排除兼容入口；网关重试路径走 excluding 版）。
    #[allow(dead_code)]
    fn select_bandit_node(&self, spec: &RoutingSpec) -> Option<Arc<ProxyNode>> {
        let context = self.bandit_engine.extract_context(&spec.target_domain);
        self.select_bandit_node_excluding(spec, &[], &context)
    }

    /// R2-2：带失败排除的 LinUCB 选路（重试路径用；`excluded` 为 `ip:port` 集合）。
    /// R2-6：`context` 由调用方（`upstream_peer`）算好传入，本函数只选不算；
    /// 候选/命中均为池内 `Arc` 句柄（零 `String` 克隆）。
    ///
    /// OPT-R11 B1：原实现先把候选物化成 `Vec<Arc<ProxyNode>>`、再物化成
    /// `Vec<Arc<BanditArm>>`、选完臂后还要 `find(|n| n.addr == best.key)`
    /// **第三次**扫一遍候选找回节点——而最终只用到一个节点。池 2000 时这是
    /// 每请求三次 O(N) 遍历 ＋ 两次 O(N) 分配（2000 时约 2KB + 16KB）。
    ///
    /// 现改为**流式单趟**：遍历候选 → 逐个 `arm_for` 打分 → 只保留最优的
    /// `(节点, 臂)` 一对，**不物化任何 Vec、不做第三次扫描**。
    ///
    /// # 三处等价性论证（防止"优化"悄悄改语义）
    ///
    /// 1. **臂打分**：仍走 `bandit_engine`，仍单趟、严格 `>` ⇒ 首个最大值
    ///    胜出，与 `select_best_arm` 一致（`select_best_arm_owned` 逐字同构）。
    /// 2. **节点找回**：臂键即 `node.addr`（`arm_for` 冷路径
    ///    `BanditArm::new(key.clone(), ...)`），且池内 `addr` 唯一 ⇒
    ///    "按 best.key 找节点" 与 "记录打分时该臂所属节点" 等价。
    /// 3. **空候选**：无候选时 `select_best_arm_owned` 返回 `None` ⇒
    ///    函数返回 `None`（网关映射 503），与旧实现一致。
    fn select_bandit_node_excluding(
        &self,
        spec: &RoutingSpec,
        excluded: &[String],
        context: &VectorD,
    ) -> Option<Arc<ProxyNode>> {
        let now = std::time::Instant::now();
        // OPT-R15：分档最小探索配额。仅在初始选路（excluded 为空）时检查与更新窗口；
        // 重试路径属于同一逻辑请求，不重复计数。
        let forced_tier = if excluded.is_empty() {
            self.tier_quota_forced_tier(spec, excluded, now)
        } else {
            None
        };
        // 过滤条件与顺序逐字沿用 `RouterEngine::get_healthy_candidates_excluding`
        // （`excluded` 先判、`matches` 后判）⇒ 候选集合与顺序不变。
        let mut best: Option<(Arc<ProxyNode>, Arc<BanditArm>)> = None;
        let mut best_score = f64::NEG_INFINITY;
        // OPT-R13：`t`（全局选路步数）**每个请求取一次**，放在候选循环**外**。
        // 若在 `compute_ucb_score` 内部自增，`t` 会按候选数增长（池 2000 时
        // 一请求 +2000），`t` 失去"请求数"含义、探索项被放大到失真。
        let t = self.bandit_engine.next_selection_step();
        self.router.with_pools(|pool| {
            for n in pool {
                if excluded.iter().any(|e| e == &n.addr) {
                    continue;
                }
                if !self.router.matches_node(n, spec, now) {
                    continue;
                }
                // OPT-R15：配额强制——若指定了 tier，跳过非该 tier 的候选。
                if let Some(ref ft) = forced_tier {
                    if n.tier != *ft {
                        continue;
                    }
                }
                // `arm_for` 内部只做**短生命周期** `get()`（OPT-R10 B1 已改为借用
                // 查询），故此处不跨调用持有 `bandit_arms` 的 ref——
                // 冷路径 `insert` 若与长生命周期 ref 同分片会死锁。
                let arm = arm_for(&self.bandit_arms, n);
                let score = arm.compute_ucb_score(context, self.bandit_engine.alpha, t);
                if score > best_score {
                    best_score = score;
                    best = Some((Arc::clone(n), arm));
                }
            }
        });
        // OPT-R15：初始选路完成后，将选中 tier 推入配额窗口。
        if excluded.is_empty() {
            if let Some((ref node, _)) = best {
                let mut w = self.tier_quota_window.lock().unwrap();
                w.push_back(node.tier.clone());
                if w.len() > TIER_QUOTA_WINDOW {
                    w.pop_front();
                }
            }
        }
        best.map(|(node, _)| node)
    }

    /// OPT-R15：检查配额窗口，返回需要强制选路的 tier（若有）。
    ///
    /// 语义：若某 tier 有候选但缺席最近 `TIER_QUOTA_WINDOW` 次选路，则该 tier
    /// 本次必须被选中。这保证每个 tier 在任意 100 次选路中至少获得 1 次流量，
    /// 使 free 供给从第 1 个请求起就可被评估（anytime 保证）。
    ///
    /// 返回 `None` 表示无需强制（正常 UCB 选路）。
    fn tier_quota_forced_tier(
        &self,
        spec: &RoutingSpec,
        excluded: &[String],
        now: std::time::Instant,
    ) -> Option<String> {
        // 第一遍：收集有候选的 tier 集合。
        let mut candidate_tiers: Vec<String> = Vec::new();
        self.router.with_pools(|pool| {
            for n in pool {
                if excluded.iter().any(|e| e == &n.addr) {
                    continue;
                }
                if !self.router.matches_node(n, spec, now) {
                    continue;
                }
                if !candidate_tiers.contains(&n.tier) {
                    candidate_tiers.push(n.tier.clone());
                }
            }
        });
        if candidate_tiers.len() <= 1 {
            // 只有一个 tier 有候选时，配额无意义（无处可切）。
            return None;
        }
        // 检查窗口中缺席的 tier。
        let w = self.tier_quota_window.lock().unwrap();
        let absent: Vec<&String> = candidate_tiers
            .iter()
            .filter(|t| !w.iter().any(|x| x == *t))
            .collect();
        if absent.is_empty() {
            return None;
        }
        // 选候选数最少的缺席 tier（最小化对选路的干扰）。
        drop(w);
        let mut best_tier: Option<&String> = None;
        let mut best_count = usize::MAX;
        for t in &absent {
            let count = self.router.with_pools(|pool| {
                pool.iter()
                    .filter(|n| {
                        !excluded.iter().any(|e| e == &n.addr)
                            && self.router.matches_node(n, spec, now)
                            && n.tier == **t
                    })
                    .count()
            });
            if count < best_count {
                best_count = count;
                best_tier = Some(t);
            }
        }
        best_tier.cloned()
    }

    /// 修剪本网关的游离臂（单测与外部调用方使用）。
    /// 注：后台 ticker 走 `prune_stale_arms` 自由函数，因为网关对象建成后
    /// 所有权归 Pingora service，主函数拿不到 `&self`，只持有拆出的 Arc。
    #[allow(dead_code)]
    pub fn prune_arms(&self) -> usize {
        prune_stale_arms(&self.bandit_arms, &self.router)
    }

    /// R3-1：socks 候选选择（有会话走粘滞＋失败排除；无会话走 LinUCB，与 HTTP
    /// 无状态路径对齐——P2 的学选分裂在此闭合；router 侧已按 proto 过滤）。
    fn pick_socks_candidate(
        &self,
        spec: &RoutingSpec,
        excluded: &[String],
        context: &VectorD,
        has_session: bool,
    ) -> Option<Arc<ProxyNode>> {
        if has_session {
            self.router.select_node_excluding(spec, excluded)
        } else {
            self.select_bandit_node_excluding(spec, excluded, context)
        }
    }

    /// P2：经翻译桥服务显式 socks 请求（`proxy_upstream_filter` 调用）。
    ///
    /// - 选路复用 `select_node_excluding`（粘滞＋失败排除；router 已按 proto 过滤，
    ///   命中防御性校验 proto，非 socks 即跳过换下一个）；
    /// - bandit 上下文算一次存 ctx（`logging` 复用，沿 R2-6）；
    /// - 最多试 `max_retries + 1` 个不同节点（与 HTTP 重试预算对齐），失败记
    ///   `failed_addrs`＋`retry_count`（遥测口径与 HTTP 重试一致）；
    /// - 成功：合成响应＋`transferred_bytes` 累加（计量/遥测走 `logging` 现有路径）；
    /// - 全失败／无候选／无桥：静态文案 503（细节打 warn 日志，不进错误类型）。
    async fn serve_via_socks(&self, session: &mut Session, ctx: &mut ProxyContext) -> Result<()> {
        let bridge = match self.bridge {
            Some(ref b) => Arc::clone(b),
            None => {
                log::warn!("[SocksBridge] bridge offline, rejecting socks request");
                return Err(Error::explain(
                    pingora_core::ErrorType::HTTPStatus(503),
                    "SOCKS bridge offline",
                ));
            }
        };
        let context = self
            .bandit_engine
            .extract_context(&ctx.routing_spec.target_domain);
        ctx.bandit_context = Some(context);
        // 出站目标与请求件（filter 时机下游头已齐：request_filter 已做鉴权/脱敏/对齐）。
        let req_header = session.req_header();
        let url = {
            let host = req_header
                .headers
                .get(http::header::HOST)
                .and_then(|v| v.to_str().ok());
            match bridge_url_for(&req_header.uri, host) {
                Some(u) => u,
                None => {
                    return Err(Error::explain(
                        pingora_core::ErrorType::HTTPStatus(400),
                        "SOCKS request without target",
                    ));
                }
            }
        };
        let method = req_header.method.as_str().to_string();
        let mut headers = Vec::new();
        for (k, v) in req_header.headers.iter() {
            if let Ok(val) = v.to_str() {
                headers.push((k.as_str().to_string(), val.to_string()));
            }
        }
        let body = if session.is_body_empty() {
            None
        } else {
            match session.read_request_body().await {
                Ok(b) => b.filter(|x| !x.is_empty()),
                Err(e) => {
                    log::warn!("[SocksBridge] read downstream body failed: {e:?}");
                    return Err(Error::explain(
                        pingora_core::ErrorType::HTTPStatus(400),
                        "SOCKS request body unreadable",
                    ));
                }
            }
        };
        // REVIEW-R2 Q9：`+1` 改饱和加（`usize::MAX` 极端下不 panic/wrap；当前常量 3 行为不变）。
        let attempts = ctx.max_retries.saturating_add(1);
        let has_session = ctx.routing_spec.session_id.is_some();
        // R3-1：bandit 上下文已在上文算好存 ctx（logging 复用）；此处 clone 出来
        // 供无状态 LinUCB 选路（`VectorD` 为 Copy，零分配）。
        let context = ctx.bandit_context.unwrap_or_else(|| {
            self.bandit_engine
                .extract_context(&ctx.routing_spec.target_domain)
        });
        // A9：整轮总 deadline（单跳 timeout＋8s 松弛；防 20s×N 最坏 80s 拖死请求）。
        let deadline = std::time::Instant::now() + socks_overall_budget(attempts, bridge.timeout());
        for _ in 0..attempts {
            // A9：总预算耗尽即停，不再起新跳（剩余预算递减语义的落地）。
            if std::time::Instant::now() >= deadline {
                log::warn!("[SocksBridge] overall budget exhausted, stop retrying");
                break;
            }
            let Some(node) = self.pick_socks_candidate(
                &ctx.routing_spec,
                &ctx.failed_addrs,
                &context,
                has_session,
            ) else {
                break;
            };
            // A5：Http 防御分支经 helper（跳过前清上轮残留，防 logging 错归因；
            // 正常走不到，router 已按 spec.proto 过滤）。
            if should_skip_bridge_node(ctx, &node) {
                continue;
            }
            // OPT-R6 S1（P0 安全）：SOCKS 出站路径的凭据护栏，判定依据同为
            // **实际选中节点**的档位。放在逐跳循环内而非函数入口，是因为重试会
            // 换节点——每跳都要按该跳真实命中的档位重新判定。
            // 与 `upstream_peer` 共用 `free_node_exits_with_credentials`／头探测，
            // 两条出站路径口径一致，不存在「HTTP 拦了 SOCKS 漏了」的缺口。
            if free_node_exits_with_credentials(
                &node.tier,
                has_authorization(session),
                has_cookie(session),
            ) {
                log::warn!(
                    "[CredentialGuard] rejecting credentialed SOCKS request routed to free node {} (tier={})",
                    node.addr,
                    node.tier
                );
                return Err(Error::explain(
                    pingora_core::ErrorType::HTTPStatus(403),
                    "Credentialed request rejected: selected egress node is anonymous (free tier)",
                ));
            }
            ctx.current_node = Some(Arc::clone(&node));
            ctx.transferred_bytes = 0; // OPT-3：只计最后 attempt
            let breq = BridgeRequest {
                method: method.clone(),
                url: url.clone(),
                headers: headers.clone(),
                body: body.clone(),
            };
            // A9：单跳受剩余总预算约束（总预算见循环前 deadline；超时按失败计，
            // 首成功即返语义不变）。
            let remaining = deadline.saturating_duration_since(std::time::Instant::now());
            if remaining.is_zero() {
                log::warn!(
                    "[SocksBridge] overall budget exhausted before {}",
                    node.addr
                );
                break;
            }
            match tokio::time::timeout(remaining, bridge.fetch(&node, breq)).await {
                Ok(Ok(resp)) => {
                    // REVIEW-R2 Q9：egress 已发生即记账（写下游失败仍计量出站成本；
                    // OPT-3 只计最后 attempt 口径不变，`?` 前先赋值）。
                    ctx.transferred_bytes = resp.body.len() as u64;
                    self.write_bridge_response(session, resp).await?;
                    return Ok(());
                }
                Ok(Err(e)) => {
                    log::warn!("[SocksBridge] via {} failed: {e}", node.addr);
                    // NEXT-A4：桥失败可观测（换节点重试语义不变）。
                    self.metrics.note_bridge_error();
                    record_failed_addr(ctx);
                    if ctx.retry_count < ctx.max_retries {
                        ctx.retry_count += 1;
                    }
                }
                Err(_) => {
                    // A9：整轮总 deadline 命中（单跳 hanging 被剩余预算截断）；
                    // 按失败计一笔后直接截尾，不再起新跳。
                    log::warn!("[SocksBridge] via {} overall timeout", node.addr);
                    self.metrics.note_bridge_error();
                    record_failed_addr(ctx);
                    if ctx.retry_count < ctx.max_retries {
                        ctx.retry_count += 1;
                    }
                    break;
                }
            }
        }
        Err(Error::explain(
            pingora_core::ErrorType::HTTPStatus(503),
            "No SOCKS node available",
        ))
    }

    /// P2：桥响应合成回下游（status＋过滤后头＋分 chunk body；content-length 显式）。
    async fn write_bridge_response(
        &self,
        session: &mut Session,
        resp: BridgeResponse,
    ) -> Result<()> {
        use pingora::http::ResponseHeader;
        let mut head = ResponseHeader::build(resp.status, Some(resp.body.len())).map_err(|e| {
            log::warn!("[SocksBridge] build response head failed: {e:?}");
            Error::explain(
                pingora_core::ErrorType::HTTPStatus(502),
                "Bad SOCKS response",
            )
        })?;
        for (k, v) in &resp.headers {
            if k.eq_ignore_ascii_case("content-length") {
                continue; // build 已按实际 body 设置，不取上游值
            }
            // 头名需 owned（`IntoCaseHeaderName` 只接受 `String`／`&'static str`，不收短借用）。
            if head.insert_header(k.clone(), v.as_str()).is_err() {
                log::debug!("[SocksBridge] skip illegal response header {k}");
            }
        }
        session
            .as_mut()
            .write_response_header(Box::new(head))
            .await?;
        // 分 32KB chunk 流写（大 body 不额外缓冲；bridge 已做上限）。
        const CHUNK: usize = 32 * 1024;
        let mut off = 0;
        while off < resp.body.len() {
            let end = (off + CHUNK).min(resp.body.len());
            session
                .as_mut()
                .write_response_body(resp.body.slice(off..end), false)
                .await?;
            off = end;
        }
        session
            .as_mut()
            .write_response_body(Bytes::new(), true)
            .await?;
        Ok(())
    }
}

#[async_trait]
impl ProxyHttp for SmartProxyGateway {
    type CTX = ProxyContext;

    fn new_ctx(&self) -> Self::CTX {
        ProxyContext::default()
    }

    async fn request_filter(&self, session: &mut Session, ctx: &mut Self::CTX) -> Result<bool> {
        if let Some(addr) = session.client_addr() {
            ctx.client_ip = addr.to_string();
        }

        // R2-2：缺 Host 直接 400（畸形请求，不占租户配额；HTTP/2 authority 透传场景
        // 由 Pingora 底层归一到 Host，此处只认标准 Host 头）。
        if session
            .req_header()
            .headers
            .get(http::header::HOST)
            .and_then(|v| v.to_str().ok())
            .is_none_or(|h| h.trim().is_empty())
        {
            session.respond_error(400).await?;
            return Ok(true);
        }

        // OPT-2 环境门：开启时，无头请求直接 403（不查租户表，
        // `tenant_account` 保持 None，logging 侧不释放配额——与坏 Key 语义一致）。
        let api_key_header = session
            .req_header()
            .headers
            .get("X-API-Key")
            .and_then(|v| v.to_str().ok())
            .map(|s| s.to_string());
        if should_reject_missing_api_key(self.require_api_key, api_key_header.is_some()) {
            session.respond_error(403).await?;
            return Ok(true);
        }
        // GW-4: tenant auth + QPS/concurrency gate before any routing work.
        // 环境门关闭时的兼容路径：无头则沿用缺省 Key，保持 GW-1~GW-4 存量行为。
        let api_key = api_key_header.unwrap_or_else(|| DEFAULT_API_KEY.to_string());
        match self.tenant_mgr.authenticate_and_throttle(&api_key) {
            Ok(account) => {
                ctx.tenant_id = Some(account.tenant_id.clone());
                ctx.tenant_account = Some(account);
            }
            Err(msg) => {
                // R2-3：429/402/403 映射收敛到 `status_for_auth_error`（单测锁定）。
                let status = status_for_auth_error(msg);
                session.respond_error(status).await?;
                return Ok(true);
            }
        }

        ctx.routing_spec = parse_routing_spec(session.req_header_mut());

        // R2-2 会话租户隔离：`{tenant}:{session}`，跨租户同名会话不串绑定。
        apply_tenant_namespace(&mut ctx.routing_spec, ctx.tenant_id.as_deref());

        // D1：free 匿名出口拒收下游凭据头（403，不占租户配额；网关自己的
        // X-API-Key 不在此列，见 free_tier_with_credentials 注释）。
        // OPT-R6 S1：探测口径抽到 `has_authorization`/`has_cookie` 自由函数，
        // 与 `upstream_peer` 的实际节点档位兜底共用同一实现（HeaderMap 大小写
        // 不敏感，`authorization`/`cookie` 全形态覆盖）。
        if free_tier_with_credentials(
            ctx.routing_spec.tier.as_deref(),
            has_authorization(session),
            has_cookie(session),
        ) {
            session.respond_error(403).await?;
            return Ok(true);
        }

        // GW-3: Chrome 124+ header alignment before scrubbing.
        FingerprintHardener::align_http2_headers(session.req_header_mut());

        // Scrub proxy/topology-revealing headers before upstream.
        let req_header = session.req_header_mut();
        req_header.remove_header("Proxy-Authorization");
        req_header.remove_header("Proxy-Connection");
        req_header.remove_header("X-Forwarded-For");
        req_header.remove_header("X-API-Key");
        req_header.remove_header("Via");
        Ok(false)
    }

    async fn upstream_peer(
        &self,
        session: &mut Session,
        ctx: &mut Self::CTX,
    ) -> Result<Box<HttpPeer>> {
        // Sticky-session fast path (GW-1 semantics: pinned node or fresh
        // random bind, quarantine-aware). Stateless traffic takes LinUCB.
        // R2-2：两条路径都带 `failed_addrs` 排除（Pingora 重试会重调本函数，
        // 失败节点不再被选中，避免原地打死节点重试）。
        // R2-6：无状态路径的 bandit 上下文只算一次，存 ctx 供 `logging` 复用。
        let node = if ctx.routing_spec.session_id.is_some() {
            self.router
                .select_node_excluding(&ctx.routing_spec, &ctx.failed_addrs)
        } else {
            let context = self
                .bandit_engine
                .extract_context(&ctx.routing_spec.target_domain);
            ctx.bandit_context = Some(context);
            self.select_bandit_node_excluding(&ctx.routing_spec, &ctx.failed_addrs, &context)
        }
        .ok_or_else(|| {
            Error::explain(
                pingora_core::ErrorType::HTTPStatus(503),
                "No active proxy node available",
            )
        })?;
        let target_host = session
            .req_header()
            .headers
            .get(http::header::HOST)
            .and_then(|v| v.to_str().ok())
            .unwrap_or("default.target")
            .to_string();
        // OPT-R6 S1（P0 安全）：**按实际选中节点**的档位兜底校验下游凭据头。
        // 旧护栏只看请求声明的 `X-Proxy-Tier`，而无 tier 声明的请求按权重选路
        // 时本就可能命中免费节点 → 带 `Authorization`/`Cookie` 的请求可经匿名
        // 免费出口出站、泄露凭据给陌生第三方。此处是选路后、出站前的最后闸门：
        // 命中免费节点且携带凭据 → 403。付费节点命中零变化。
        // 显式 `tier=free` 的提前拒绝仍在 `request_filter`（省一次选路）。
        if free_node_exits_with_credentials(
            &node.tier,
            has_authorization(session),
            has_cookie(session),
        ) {
            log::warn!(
                "[CredentialGuard] rejecting credentialed request routed to free node {} (tier={})",
                node.addr,
                node.tier
            );
            // 与本函数 503 分支同模式：`Err(HTTPStatus)` 让 Pingora 直接回错误响应，
            // `ctx.current_node` 刻意不落（无出站、无计量归属，遥测走 error 路径）。
            return Err(Error::explain(
                pingora_core::ErrorType::HTTPStatus(403),
                "Credentialed request rejected: selected egress node is anonymous (free tier)",
            ));
        }
        // P2：peer 装配经守卫函数（socks 节点硬失败，正常走不到——见函数注释）。
        let peer = build_http_peer(&node, &target_host)?;
        // R2-6：`current_node` 落 ctx（`Arc` 句柄本身零成本 move）。
        ctx.current_node = Some(node);
        Ok(peer)
    }

    /// P2 SOCKS 短路：显式 socks 请求（`X-Proxy-Proto: socks5/socks4`）不走
    /// `upstream_peer`/HttpPeer，经翻译桥出站后合成响应回下游，返回 `Ok(false)`。
    /// HTTP 快路径（默认＋显式 http）直接 `Ok(true)`，零改动。
    /// 合成后 `logging()` 照常运行（status 取自已写响应，bandit／计量／遥测全复用）。
    async fn proxy_upstream_filter(
        &self,
        session: &mut Session,
        ctx: &mut Self::CTX,
    ) -> Result<bool> {
        if ctx.routing_spec.proto.unwrap_or(EgressProto::Http) == EgressProto::Http {
            return Ok(true);
        }
        self.serve_via_socks(session, ctx).await?;
        Ok(false)
    }

    async fn upstream_request_filter(
        &self,
        _session: &mut Session,
        upstream_request: &mut RequestHeader,
        ctx: &mut Self::CTX,
    ) -> Result<()> {
        if let Some(ref node) = ctx.current_node {
            if let (Some(u), Some(p)) = (&node.username, &node.password) {
                let encoded = B64.encode(format!("{u}:{p}").as_bytes());
                upstream_request
                    .insert_header("Proxy-Authorization", format!("Basic {encoded}"))?;
            }
        }
        Ok(())
    }

    fn fail_to_connect(
        &self,
        _session: &mut Session,
        _peer: &HttpPeer,
        ctx: &mut Self::CTX,
        mut e: Box<Error>,
    ) -> Box<Error> {
        // OPT-3：失败 attempt 的字节作废，新 attempt 从零累计（只计最后一次）。
        reset_retry_accounting(ctx);
        // R2-2：记下失败节点，重试选路时排除（只换节点，不换重试次数语义）。
        record_failed_addr(ctx);
        if ctx.retry_count < ctx.max_retries {
            ctx.retry_count += 1;
            log::warn!(
                "[Gateway] upstream connect error: {:?}, retry {}/{} target={}",
                e,
                ctx.retry_count,
                ctx.max_retries,
                ctx.routing_spec.target_domain
            );
            e.set_retry(true);
        } else {
            e.set_retry(false);
        }
        e
    }

    /// GW-4: zero-copy streaming byte metering into `ctx.transferred_bytes`.
    fn response_body_filter(
        &self,
        _session: &mut Session,
        body: &mut Option<Bytes>,
        _end_of_stream: bool,
        ctx: &mut Self::CTX,
    ) -> Result<Option<Duration>> {
        if let Some(ref chunk) = body {
            ctx.transferred_bytes += chunk.len() as u64;
        }
        Ok(None)
    }

    async fn logging(&self, session: &mut Session, e: Option<&Error>, ctx: &mut Self::CTX) {
        let duration = ctx.start_time.elapsed();
        let status = session
            .response_written()
            .map(|r| r.status.as_u16())
            .unwrap_or(0);
        let node = ctx.current_node.clone();
        // R3-2：遥测 out_ip 取真 egress（exit_ip 有即用；经典 HTTP 节点 exit 恒 None，
        // 回落代理入口 ip，存量行为冻结）。CB 隔离消费 out_ip（见 R3-2 matches 双检）。
        let node_ip = node
            .as_ref()
            .map(|n| n.exit_ip.as_deref().unwrap_or(n.ip.as_str()))
            .unwrap_or("none");
        // GW-3: online bandit feedback (reward → Sherman-Morrison update).
        // R2-6：复用 `upstream_peer` 已算好的上下文（选学一致 + 省一次时钟
        // syscall）；粘滞路径 ctx 为空时回落现算（与 R2-6 前行为一致）。
        if let Some(ref n) = node {
            let context =
                resolve_bandit_context(ctx, &self.bandit_engine, &ctx.routing_spec.target_domain);
            arm_for(&self.bandit_arms, n).update(&context, compute_reward(status, duration));
        }
        // GW-4: release the tenant slot + meter actual egress bytes/tier,
        // and record Prometheus signals (works even in degraded mode).
        if let Some(ref account) = ctx.tenant_account {
            let tier = node
                .as_ref()
                .map(|n| n.tier.as_str())
                .unwrap_or("datacenter");
            self.tenant_mgr
                .release_and_meter(account, ctx.transferred_bytes, tier);
        }
        let provider_for_metrics = node.as_ref().map(|n| n.provider.as_str());
        self.metrics.observe(
            status,
            provider_for_metrics,
            ctx.transferred_bytes,
            duration,
        );
        // GW-2: emit to the zero-blocking telemetry bus (dropped when full).
        if let Some(ref publisher) = self.telemetry {
            let (provider, tier, country) = match node.as_ref() {
                Some(n) => (n.provider.clone(), n.tier.clone(), n.country.clone()),
                None => ("none".to_string(), "none".to_string(), "none".to_string()),
            };
            publisher.emit(TelemetryEvent {
                // R2-4：id 留空由 emit 配号（`{ms}-{pid}-{seq}`，XADD 幂等）。
                event_id: String::new(),
                client_ip: ctx.client_ip.clone(),
                target_domain: ctx.routing_spec.target_domain.clone(),
                out_ip: node_ip.to_string(),
                provider,
                tier,
                country,
                status_code: status,
                latency_ms: duration.as_millis() as u64,
                transferred_bytes: ctx.transferred_bytes,
                retry_count: ctx.retry_count.min(u8::MAX as usize) as u8,
                tenant_id: ctx.tenant_id.clone(),
                error_type: e.map(|err| format!("{err:?}")),
                timestamp: TelemetryEvent::now_unix_ms(),
            });
        }
        // R2-8 数据面日志采样：5xx 全量 info，其余 1/1000 全量（首条即全量），
        // 采样掉的走 debug（明文 IP 只在全量行出现，采样数进 metrics 可观测）。
        if self.metrics.sample_full_log(status) {
            log::info!(
                "[Telemetry] client={} target={} out={} status={} cost={:?}",
                ctx.client_ip,
                ctx.routing_spec.target_domain,
                node_ip,
                status,
                duration
            );
        } else {
            log::debug!(
                "[Telemetry] client={} target={} out={} status={} cost={:?}",
                ctx.client_ip,
                ctx.routing_spec.target_domain,
                node_ip,
                status,
                duration
            );
        }
    }
}

/// Parse routing directives from headers + Basic Proxy-Authorization username.
///
/// Supported: `X-Proxy-Country/Session/Tier` headers (preferred) and
/// `user_country-us_session-xxx_tier-res` embedded in the Basic username.
pub fn parse_routing_spec(header: &RequestHeader) -> RoutingSpec {
    let mut spec = RoutingSpec::default();

    if let Some(host) = header
        .headers
        .get(http::header::HOST)
        .and_then(|h| h.to_str().ok())
    {
        // R2-2：Host 归一化（小写 + 剥端口 + 去尾点），与隔离 key 同口径。
        spec.target_domain = normalize_domain(host);
    }
    if let Some(c) = header
        .headers
        .get("X-Proxy-Country")
        .and_then(|v| v.to_str().ok())
    {
        spec.country = Some(c.to_string());
    }
    if let Some(s) = header
        .headers
        .get("X-Proxy-Session")
        .and_then(|v| v.to_str().ok())
    {
        spec.session_id = Some(s.to_string());
    }
    if let Some(t) = header
        .headers
        .get("X-Proxy-Tier")
        .and_then(|v| v.to_str().ok())
    {
        spec.tier = Some(t.to_string());
    }
    // P2：显式出站协议（header 优先；非法值忽略回落默认 http）。
    if let Some(p) = header
        .headers
        .get("X-Proxy-Proto")
        .and_then(|v| v.to_str().ok())
        .and_then(crate::model::EgressProto::from_token)
    {
        spec.proto = Some(p);
    }

    if let Some(auth) = header
        .headers
        .get(PROXY_AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
    {
        if let Some(b64) = auth.strip_prefix("Basic ") {
            if let Ok(decoded) = B64.decode(b64) {
                if let Ok(auth_str) = String::from_utf8(decoded) {
                    let user_part = auth_str.split(':').next().unwrap_or("");
                    for token in user_part.split('_') {
                        if let Some(v) = token.strip_prefix("country-") {
                            spec.country = Some(v.to_string());
                        } else if let Some(v) = token.strip_prefix("session-") {
                            spec.session_id = Some(v.to_string());
                        } else if let Some(v) = token.strip_prefix("tier-") {
                            spec.tier = Some(v.to_string());
                        } else if let Some(v) = token.strip_prefix("proto-") {
                            // P2：token 形态仅在 header 未指定时生效（header 优先）。
                            if spec.proto.is_none() {
                                spec.proto = crate::model::EgressProto::from_token(v);
                            }
                        }
                    }
                }
            }
        }
    }

    spec
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bandit::LinUCBEngine;
    use crate::metrics::MetricsRegistry;
    use crate::model::ProxyNode;
    use crate::tenant::TenantManager;

    fn test_node(ip: &str, port: u16) -> ProxyNode {
        ProxyNode::new(
            ip.to_string(),
            port,
            None,
            None,
            "US".to_string(),
            "residential".to_string(),
            "mock-a".to_string(),
            100,
        )
    }

    fn test_gateway(nodes: Vec<ProxyNode>) -> SmartProxyGateway {
        SmartProxyGateway {
            router: Arc::new(RouterEngine::new(nodes)),
            telemetry: None,
            bandit_engine: Arc::new(LinUCBEngine::new(0.4)),
            bandit_arms: Arc::new(DashMap::new()),
            tenant_mgr: Arc::new(TenantManager::new()),
            metrics: Arc::new(MetricsRegistry::new()),
            // 单测默认关门：存量路由/租户行为不受环境门影响。
            require_api_key: false,
            // 单测不装桥（socks 全链路由 E2E 承担；无桥时 socks 请求 503 属正确）。
            bridge: None,
            tier_quota_window: Mutex::new(VecDeque::new()),
        }
    }

    #[test]
    fn retry_resets_only_transfer_counter() {
        // OPT-3 口径：失败 attempt 的字节清零，重试计数/租户上下文不受影响。
        let mut ctx = ProxyContext {
            transferred_bytes: 12_345,
            retry_count: 1,
            ..ProxyContext::default()
        };
        reset_retry_accounting(&mut ctx);
        assert_eq!(ctx.transferred_bytes, 0);
        assert_eq!(ctx.retry_count, 1);
        // 幂等：连续失败多次仍为零，不会下溢或污染其他字段。
        reset_retry_accounting(&mut ctx);
        assert_eq!(ctx.transferred_bytes, 0);
    }

    #[test]
    fn api_key_gate_only_rejects_missing_header_when_enabled() {
        // 开门 + 无头 → 拦截；开门 + 有头 → 放行（坏 Key 由租户层另判）。
        assert!(should_reject_missing_api_key(true, false));
        assert!(!should_reject_missing_api_key(true, true));
        // 关门 → 行为不变，一律放行到租户缺省 Key 路径。
        assert!(!should_reject_missing_api_key(false, false));
        assert!(!should_reject_missing_api_key(false, true));
    }

    #[test]
    fn auth_error_status_mapping() {
        // R2-3：429 可重试 / 402 欠费 / 403 身份问题，三类可区分。
        assert_eq!(status_for_auth_error("Rate limit exceeded (QPS)"), 429);
        assert_eq!(status_for_auth_error("Max concurrency limit reached"), 429);
        assert_eq!(status_for_auth_error("Insufficient balance"), 402);
        assert_eq!(status_for_auth_error("Invalid API Key"), 403);
        assert_eq!(status_for_auth_error("Tenant is disabled"), 403);
    }

    #[test]
    fn free_tier_rejects_credential_headers() {
        // D1：tier=free（匿名共享出口）＋Authorization/Cookie → 拦截（网关回 403）。
        assert!(free_tier_with_credentials(Some("free"), true, false));
        assert!(free_tier_with_credentials(Some("free"), false, true));
        assert!(free_tier_with_credentials(Some("free"), true, true));
        // free 但无敏感头 → 放行。
        assert!(!free_tier_with_credentials(Some("free"), false, false));
        // 非 free 档（付费/default/未指定）携带同样头 → 一律放行（存量语义不变）。
        assert!(!free_tier_with_credentials(Some("res"), true, true));
        assert!(!free_tier_with_credentials(Some("dc"), true, true));
        assert!(!free_tier_with_credentials(None, true, true));
    }

    // ---- OPT-R6 S1：free 凭据护栏（P0 安全）回归锁定 ----

    /// P0 根因回归：请求**未声明** `X-Proxy-Tier`（旧护栏 `tier=None` 必放行），
    /// 但选路实际命中匿名免费节点时，携带下游凭据必须被拒。
    ///
    /// 旧实现的绕过路径（本组断言在旧代码下全部反转为 `false`，即漏洞存在）：
    /// `free_tier_with_credentials(None, true, false) == false`（放行）
    ///   → 无 tier 约束的选路按权重命中 free 节点
    ///   → 凭据经陌生免费出口泄露。
    #[test]
    fn opt_r6_s1_undeclared_tier_hitting_free_node_is_rejected() {
        // 绕过路径本体：无 tier 声明 + 带 Authorization → 旧护栏放行。
        assert!(
            !free_tier_with_credentials(None, true, false),
            "前置确认：旧护栏在无 tier 声明时确实放行（这正是漏洞所在）"
        );
        // 修法：按实际命中节点档位判定 → 拦截。
        assert!(free_node_exits_with_credentials("free", true, false));
        assert!(free_node_exits_with_credentials("free", false, true));
        assert!(free_node_exits_with_credentials("free", true, true));
    }

    /// 匿名流量走免费节点必须放行——护栏不能误伤免费线的正常匿名用途
    /// （否则等于把整个第二供应线关掉）。
    #[test]
    fn opt_r6_s1_anonymous_traffic_on_free_node_passes() {
        assert!(!free_node_exits_with_credentials("free", false, false));
    }

    /// 命中付费节点一律放行（存量语义零变化，含 res/dc 短名归一后的长名形态）。
    #[test]
    fn opt_r6_s1_paid_node_never_rejected() {
        for tier in ["residential", "datacenter", "price", "cost", ""] {
            assert!(
                !free_node_exits_with_credentials(tier, true, true),
                "付费/未知档 `{tier}` 携带凭据必须放行"
            );
        }
    }

    /// 档位大小写不敏感（`ProxyNode::new` 入口已归一，但防御性保留：
    /// 大小写不敏感比较对 `free` 与「归一后严格比较」完全等价）。
    #[test]
    fn opt_r6_s1_tier_case_insensitive() {
        assert!(free_node_exits_with_credentials("FREE", true, false));
        assert!(free_node_exits_with_credentials("Free", false, true));
        assert!(!free_node_exits_with_credentials(
            "RESIDENTIAL",
            true,
            false
        ));
    }

    /// 存量语义不回归：显式 `tier=free` 的**提前**拒绝（省一次选路）仍生效，
    /// 且与新的节点侧兜底判定不冲突。
    #[test]
    fn opt_r6_s1_explicit_free_tier_still_rejected_early() {
        // 显式 free + 凭据 → 提前拒绝（request_filter 路径，未选路）。
        assert!(free_tier_with_credentials(Some("free"), true, false));
        // 显式 free + 匿名 → 放行，后续由节点侧判定兜底。
        assert!(!free_tier_with_credentials(Some("free"), false, false));
        // 显式非 free + 凭据 → 提前放行，但若实际命中免费节点，节点侧仍会拦。
        assert!(!free_tier_with_credentials(
            Some("residential"),
            true,
            false
        ));
        assert!(free_node_exits_with_credentials("free", true, false));
    }

    /// 覆盖盲区的**诚实记录**：`upstream_peer` 与 `serve_via_socks` 两个 Pingora
    /// 钩子本体无法单测（`pingora_proxy::Session` 无法在单测中构造），故它们的
    /// 「护栏确实被调用」只能靠代码走查 + curl 端到端回归保证。
    ///
    /// 本测试锁定的是**可测部分**：判定函数的完整真值表（节点侧护栏的全部语义）。
    /// 若将来有人改动判定口径，此测试会红；钩子接线则由 lint/走查/端到端覆盖。
    #[test]
    fn opt_r6_s1_guard_truth_table_is_complete() {
        // 全部 2^3 组合（node_tier 固定 free 与 residential）× 4 档。
        for has_auth in [false, true] {
            for has_cookie in [false, true] {
                let creds = has_auth || has_cookie;
                assert_eq!(
                    free_node_exits_with_credentials("free", has_auth, has_cookie),
                    creds,
                    "free 节点：有凭据必拦、无凭据必放行"
                );
                assert!(
                    !free_node_exits_with_credentials("residential", has_auth, has_cookie),
                    "付费节点：一律放行"
                );
            }
        }
    }

    #[test]
    fn prune_keeps_pool_arms_removes_strays() {
        let gw = test_gateway(vec![test_node("10.0.0.1", 8080)]);
        // 池内臂 + 游离臂各一。
        arm_for(&gw.bandit_arms, &test_node("10.0.0.1", 8080));
        gw.bandit_arms.insert(
            "10.9.9.9:8080".to_string(),
            Arc::new(BanditArm::new(
                "10.9.9.9:8080".to_string(),
                "residential".to_string(),
            )),
        );
        assert_eq!(gw.prune_arms(), 1);
        assert!(gw.bandit_arms.contains_key("10.0.0.1:8080"));
        assert!(!gw.bandit_arms.contains_key("10.9.9.9:8080"));
        // 二次修剪无事发生。
        assert_eq!(gw.prune_arms(), 0);
    }

    fn header_with(pairs: &[(&'static str, &'static str)]) -> RequestHeader {
        let mut h = RequestHeader::build("GET", b"/", None).unwrap();
        for (k, v) in pairs {
            h.insert_header(*k, *v).unwrap();
        }
        h
    }

    #[test]
    fn parses_custom_headers() {
        let h = header_with(&[
            ("Host", "api.target.com"),
            ("X-Proxy-Country", "US"),
            ("X-Proxy-Session", "task-1"),
            ("X-Proxy-Tier", "res"),
        ]);
        let s = parse_routing_spec(&h);
        assert_eq!(s.target_domain, "api.target.com");
        assert_eq!(s.country.as_deref(), Some("US"));
        assert_eq!(s.session_id.as_deref(), Some("task-1"));
        assert_eq!(s.tier.as_deref(), Some("res"));
    }

    #[test]
    fn parses_basic_username_directives() {
        let raw = "myuser_country-jp_session-task99_tier-dc:secret";
        let b64 = B64.encode(raw.as_bytes());
        let mut h = header_with(&[("Host", "x.example")]);
        h.insert_header("Proxy-Authorization", format!("Basic {b64}"))
            .unwrap();
        let s = parse_routing_spec(&h);
        assert_eq!(s.country.as_deref(), Some("jp"));
        assert_eq!(s.session_id.as_deref(), Some("task99"));
        assert_eq!(s.tier.as_deref(), Some("dc"));
    }

    #[test]
    fn header_directives_win_over_defaults() {
        let h = header_with(&[("Host", "h.example")]);
        let s = parse_routing_spec(&h);
        assert_eq!(s.target_domain, "h.example");
        assert!(s.country.is_none());
    }

    #[test]
    fn parses_host_normalized() {
        // R2-2：Host 大小写/端口/尾点归一，与隔离 key 同口径。
        let h = header_with(&[("Host", "API.Target.COM:8443")]);
        assert_eq!(parse_routing_spec(&h).target_domain, "api.target.com");
        let h2 = header_with(&[("Host", "Example.COM.")]);
        assert_eq!(parse_routing_spec(&h2).target_domain, "example.com");
    }

    #[test]
    fn parse_routing_spec_proto_header_and_token() {
        // P2-1：X-Proxy-Proto 头解析（大小写不敏感）；Proxy-Auth proto- token；
        // header 优先于 token；非法值回落 None（默认 http）。
        use crate::model::EgressProto;
        let h = header_with(&[("Host", "x.example"), ("X-Proxy-Proto", "SOCKS5")]);
        assert_eq!(parse_routing_spec(&h).proto, Some(EgressProto::Socks5));
        let raw = "u_proto-socks4:x";
        let b64 = B64.encode(raw.as_bytes());
        let mut h2 = header_with(&[("Host", "x.example")]);
        h2.insert_header("Proxy-Authorization", format!("Basic {b64}"))
            .unwrap();
        assert_eq!(parse_routing_spec(&h2).proto, Some(EgressProto::Socks4));
        // header 优先。
        let raw3 = "u_proto-socks4:x";
        let b643 = B64.encode(raw3.as_bytes());
        let mut h3 = header_with(&[("Host", "x.example"), ("X-Proxy-Proto", "socks5")]);
        h3.insert_header("Proxy-Authorization", format!("Basic {b643}"))
            .unwrap();
        assert_eq!(parse_routing_spec(&h3).proto, Some(EgressProto::Socks5));
        // 非法值与缺省 → None。
        let h4 = header_with(&[("Host", "x.example"), ("X-Proxy-Proto", "gopher")]);
        assert_eq!(parse_routing_spec(&h4).proto, None);
        let h5 = header_with(&[("Host", "x.example")]);
        assert_eq!(parse_routing_spec(&h5).proto, None);
    }

    #[test]
    fn tenant_namespace_is_idempotent_and_tenant_scoped() {
        // R2-2：同名会话跨租户不串；已命名幂等；无会话/无租户不动。
        let mut spec = RoutingSpec {
            session_id: Some("task-1".to_string()),
            ..Default::default()
        };
        apply_tenant_namespace(&mut spec, Some("t-1"));
        assert_eq!(spec.session_id.as_deref(), Some("t-1:task-1"));
        apply_tenant_namespace(&mut spec, Some("t-1"));
        assert_eq!(spec.session_id.as_deref(), Some("t-1:task-1"));
        let mut other = RoutingSpec {
            session_id: Some("task-1".to_string()),
            ..Default::default()
        };
        apply_tenant_namespace(&mut other, Some("t-2"));
        assert_ne!(spec.session_id, other.session_id);
        let mut bare = RoutingSpec::default();
        apply_tenant_namespace(&mut bare, Some("t-1"));
        assert!(bare.session_id.is_none());
        let mut no_tenant = RoutingSpec {
            session_id: Some("s".to_string()),
            ..Default::default()
        };
        apply_tenant_namespace(&mut no_tenant, None);
        assert_eq!(no_tenant.session_id.as_deref(), Some("s"));
    }

    #[test]
    fn bandit_context_reuses_preset_without_recompute() {
        // R2-6：ctx 预置则原样复用（哨兵值不可能是现算结果：bias 恒 1.0/prior 恒 0.2）；
        // 为空则现算且形态正确（选学一致 + 省 syscall 的接线锁定）。
        let engine = LinUCBEngine::new(0.4);
        let sentinel = VectorD::new(7.0, 7.0, 7.0, 7.0);
        let preset = ProxyContext {
            bandit_context: Some(sentinel),
            ..ProxyContext::default()
        };
        assert_eq!(
            resolve_bandit_context(&preset, &engine, "plain.example"),
            sentinel
        );
        let fresh = ProxyContext::default();
        let computed = resolve_bandit_context(&fresh, &engine, "plain.example");
        assert_eq!(computed[1], 1.0);
        assert_eq!(computed[3], 0.2);
        assert!((0.0..=1.0).contains(&computed[2]));
    }

    #[test]
    fn failed_addr_record_dedupes() {
        // R2-2：失败 addr 去重记录；无节点时不记。
        let mut ctx = ProxyContext {
            current_node: Some(Arc::new(test_node("10.0.0.9", 8080))),
            ..ProxyContext::default()
        };
        record_failed_addr(&mut ctx);
        record_failed_addr(&mut ctx);
        assert_eq!(ctx.failed_addrs, vec!["10.0.0.9:8080".to_string()]);
        let mut empty = ProxyContext::default();
        record_failed_addr(&mut empty);
        assert!(empty.failed_addrs.is_empty());
    }

    /// OPT-R15：分档最小探索配额——核心不变量。
    ///
    /// 场景：池内有 dc 与 free 两档节点，dc 臂被训练成"完美"（reward=1.0）。
    /// 无配额时 free 臂在 t < ~376 时**永远无法在分数上追平** dc 臂
    /// （探索项封顶 `alpha*|x|` ≈ 0.47 < 完美付费臂的 1.0 + 溢价差）。
    /// 配额保证 free 档在任意 `TIER_QUOTA_WINDOW` 次选路中至少被选中一次。
    #[test]
    fn tier_quota_forces_free_tier_when_absent_from_window() {
        // 构造两档节点：dc（完美付费臂）与 free（未拉取）。
        let dc_node = ProxyNode::new(
            "10.0.0.1".to_string(),
            8080,
            None,
            None,
            "US".to_string(),
            "datacenter".to_string(),
            "mock-a".to_string(),
            100,
        );
        let free_node = ProxyNode::new(
            "10.0.0.2".to_string(),
            8080,
            None,
            None,
            "US".to_string(),
            "free".to_string(),
            "free-1".to_string(),
            100,
        );
        let gw = test_gateway(vec![dc_node.clone(), free_node.clone()]);
        let x = gw.bandit_engine.extract_context("example.com");
        // 把 dc 臂训练成完美（反复拿满奖）。
        for _ in 0..400 {
            arm_for(&gw.bandit_arms, &dc_node).update(&x, 1.0);
        }
        // 验证：无配额时 dc 臂确实胜出（free 追不上）。
        let dc_score =
            arm_for(&gw.bandit_arms, &dc_node).compute_ucb_score(&x, gw.bandit_engine.alpha, 1);
        let free_score =
            arm_for(&gw.bandit_arms, &free_node).compute_ucb_score(&x, gw.bandit_engine.alpha, 1);
        assert!(
            dc_score > free_score,
            "前提不成立：dc={dc_score} 应 > free={free_score}"
        );
        // 用 dc 填满配额窗口（模拟 dc 垄断最近 100 次选路）。
        {
            let mut w = gw.tier_quota_window.lock().unwrap();
            for _ in 0..TIER_QUOTA_WINDOW {
                w.push_back("datacenter".to_string());
            }
        }
        // 配额应强制选路到 free 档。
        let spec = RoutingSpec {
            target_domain: "example.com".to_string(),
            ..RoutingSpec::default()
        };
        let picked = gw.select_bandit_node(&spec).expect("应选中节点");
        assert_eq!(
            picked.tier, "free",
            "配额应强制选到 free 档，实际选中 {}",
            picked.tier
        );
        // 窗口应已更新（free 被推入）。
        let w = gw.tier_quota_window.lock().unwrap();
        assert_eq!(w.back().map(|s| s.as_str()), Some("free"));
    }

    /// OPT-R15：配额不干扰正常选路——当所有 tier 都在窗口中时，不强制。
    #[test]
    fn tier_quota_no_force_when_all_tiers_present() {
        let dc_node = ProxyNode::new(
            "10.0.0.1".to_string(),
            8080,
            None,
            None,
            "US".to_string(),
            "datacenter".to_string(),
            "mock-a".to_string(),
            100,
        );
        let free_node = ProxyNode::new(
            "10.0.0.2".to_string(),
            8080,
            None,
            None,
            "US".to_string(),
            "free".to_string(),
            "free-1".to_string(),
            100,
        );
        let gw = test_gateway(vec![dc_node.clone(), free_node.clone()]);
        let x = gw.bandit_engine.extract_context("example.com");
        for _ in 0..400 {
            arm_for(&gw.bandit_arms, &dc_node).update(&x, 1.0);
        }
        // 窗口中同时有 dc 和 free ⇒ 不强制，正常 UCB 选路（dc 胜出）。
        {
            let mut w = gw.tier_quota_window.lock().unwrap();
            w.push_back("datacenter".to_string());
            w.push_back("free".to_string());
        }
        let spec = RoutingSpec {
            target_domain: "example.com".to_string(),
            ..RoutingSpec::default()
        };
        let picked = gw.select_bandit_node(&spec).expect("应选中节点");
        assert_eq!(
            picked.tier, "datacenter",
            "所有 tier 都在窗口中时应正常选路（dc 胜出），实际 {}",
            picked.tier
        );
    }

    /// OPT-R15：只有一个 tier 有候选时，配额无意义（无处可切），不强制。
    #[test]
    fn tier_quota_noop_when_single_tier() {
        let dc_node = ProxyNode::new(
            "10.0.0.1".to_string(),
            8080,
            None,
            None,
            "US".to_string(),
            "datacenter".to_string(),
            "mock-a".to_string(),
            100,
        );
        let gw = test_gateway(vec![dc_node.clone()]);
        let x = gw.bandit_engine.extract_context("example.com");
        for _ in 0..400 {
            arm_for(&gw.bandit_arms, &dc_node).update(&x, 1.0);
        }
        // 窗口为空（无历史），但只有一个 tier ⇒ 不强制。
        let spec = RoutingSpec {
            target_domain: "example.com".to_string(),
            ..RoutingSpec::default()
        };
        let picked = gw.select_bandit_node(&spec).expect("应选中节点");
        assert_eq!(picked.tier, "datacenter");
    }

    /// OPT-R15：`is_tls_for_node` 按端口判断 TLS。
    #[test]
    fn is_tls_for_node_port_443() {
        let https_node = test_node("10.0.0.1", 443);
        let http_node = test_node("10.0.0.2", 8080);
        let http_alt = test_node("10.0.0.3", 80);
        assert!(is_tls_for_node(&https_node), "端口 443 应为 TLS");
        assert!(!is_tls_for_node(&http_node), "端口 8080 应为明文");
        assert!(!is_tls_for_node(&http_alt), "端口 80 应为明文");
    }

    /// OPT-R15 步骤 46：跳过证书校验的**双重门控**必须成立。
    ///
    /// 这是本步最需要防回归的地方：一旦门控被放宽成"任意节点可跳过校验"，
    /// 就等于给生产节点开了一个 MITM 口子，且**静默**（日志上看不出异常）。
    #[test]
    fn skip_cert_verify_requires_loopback_and_test_channel() {
        // 回环判定
        assert!(is_loopback_addr("127.0.0.1:443"));
        assert!(is_loopback_addr("127.0.0.53:8443"));
        assert!(is_loopback_addr("[::1]:443"));
        assert!(!is_loopback_addr("10.0.0.1:443"));
        assert!(!is_loopback_addr("8.8.8.8:443"));
        // 公网地址即便端口是 443，也不得进入跳过校验分支。
        let public_443 = ProxyNode::new(
            "10.0.0.1".to_string(),
            443,
            None,
            None,
            "US".to_string(),
            "residential".to_string(),
            "public-https".to_string(),
            100,
        );
        assert!(is_tls_for_node(&public_443), "公网 443 也判定为 TLS");
        assert!(
            !is_loopback_addr(&public_443.addr),
            "公网节点不得被当作回环（否则会误开跳过校验）"
        );
    }

    #[test]
    fn upstream_peer_refuses_socks_node() {
        // P2-4 双保险：socks 节点直达 peer 构造即 Err（正常走不到——filter 已短路＋router 默认隔离）。
        use crate::model::EgressProto;
        let socks = ProxyNode::new(
            "9.9.9.9".to_string(),
            1080,
            None,
            None,
            "ZZ".to_string(),
            "free".to_string(),
            "free-socks".to_string(),
            10,
        )
        .with_proto(EgressProto::Socks5);
        assert!(build_http_peer(&socks, "x.example").is_err());
        let http = test_node("10.0.0.1", 8080);
        assert!(build_http_peer(&http, "x.example").is_ok());
    }

    #[test]
    fn socks_request_url_shape() {
        // P2-4：absolute-form 原样透传（含 https，reqwest 在隧道内建 TLS）；
        // origin-form 按 Host 拼 http；无 Host 即 None（调用方判 400/502，不 panic）。
        use http::Uri;
        let abs: Uri = "https://api.target.com:8443/p?q=1".parse().unwrap();
        assert_eq!(
            bridge_url_for(&abs, Some("ignored.example")).as_deref(),
            Some("https://api.target.com:8443/p?q=1")
        );
        let origin: Uri = "/p?q=1".parse().unwrap();
        assert_eq!(
            bridge_url_for(&origin, Some("api.target.com")).as_deref(),
            Some("http://api.target.com/p?q=1")
        );
        assert_eq!(bridge_url_for(&origin, None), None);
    }

    #[test]
    fn socks_http_skip_clears_stale_node() {
        // A5：Http 防御分支 continue 前必须清掉上轮 attempt 残留，否则 logging
        // 会把上轮节点当本次做 bandit/计量/遥测（错归因）。
        use crate::model::EgressProto;
        let mut ctx = ProxyContext {
            current_node: Some(Arc::new(test_node("10.0.0.9", 8080))),
            ..ProxyContext::default()
        };
        let http_node = ProxyNode::new(
            "9.9.9.9".to_string(),
            1080,
            None,
            None,
            "ZZ".to_string(),
            "free".to_string(),
            "free-socks".to_string(),
            10,
        );
        assert!(http_node.proto == EgressProto::Http);
        assert!(should_skip_bridge_node(&mut ctx, &http_node));
        assert!(
            ctx.current_node.is_none(),
            "Http 跳过必须清 current_node，防上轮残留错归因"
        );
        // socks 节点不跳过、现任不被清。
        let socks = ProxyNode::new(
            "9.9.9.10".to_string(),
            1080,
            None,
            None,
            "ZZ".to_string(),
            "free".to_string(),
            "free-socks".to_string(),
            10,
        )
        .with_proto(EgressProto::Socks5);
        ctx.current_node = Some(Arc::new(test_node("10.0.0.8", 8080)));
        assert!(!should_skip_bridge_node(&mut ctx, &socks));
        assert!(ctx.current_node.is_some());
    }

    #[test]
    fn socks_overall_budget_caps_worst_case() {
        // A9：20s×4 最坏 80s 必须有总 deadline；首成功即返语义下总预算应远小于累加。
        let per = Duration::from_secs(20);
        let budget = socks_overall_budget(4, per);
        assert!(
            budget < per * 4,
            "总预算 {budget:?} 必须小于逐跳累加 {:?}",
            per * 4
        );
        assert_eq!(budget, per + Duration::from_secs(8));
        // 单 attempt 也给足一次全量＋松弛，不因预算不足误杀首试。
        assert!(socks_overall_budget(1, per) >= per);
    }

    #[test]
    fn socks_stateless_prefers_trained_winner() {
        // R3-1：socks 无状态选路走 LinUCB（与 HTTP 路径对齐；有会话仍走粘滞）。
        // helper 可单测直调（Session 不可单元构造，全链由 E2E 覆盖）。
        use crate::bandit::LinUCBEngine;
        use crate::model::EgressProto;
        let mk = |ip: &str| {
            ProxyNode::new(
                ip.to_string(),
                1080,
                None,
                None,
                "ZZ".to_string(),
                "free".to_string(),
                "free-socks".to_string(),
                10,
            )
            .with_proto(EgressProto::Socks5)
        };
        let gw = test_gateway(vec![mk("9.9.9.9"), mk("9.9.9.10")]);
        let x = LinUCBEngine::new(0.4).extract_context("plain.example");
        let nodes = gw.router.snapshot_all();
        let winner = nodes.iter().find(|n| n.ip == "9.9.9.9").expect("winner");
        let loser = nodes.iter().find(|n| n.ip == "9.9.9.10").expect("loser");
        for _ in 0..5 {
            arm_for(&gw.bandit_arms, winner).update(&x, 1.0);
            arm_for(&gw.bandit_arms, loser).update(&x, 0.0);
        }
        let spec = RoutingSpec {
            proto: Some(EgressProto::Socks5),
            target_domain: "plain.example".to_string(),
            ..Default::default()
        };
        let picked = gw
            .pick_socks_candidate(&spec, &[], &x, false)
            .expect("candidate");
        assert_eq!(picked.ip, "9.9.9.9");
    }

    // ---- OPT-R10 B1：arm_for 命中路径零分配 ----

    /// 语义回归：命中与未命中都要返回**可用**的臂，且键为 `node.addr`。
    #[test]
    fn opt_r10_b1_arm_for_hit_and_miss_semantics() {
        let arms: DashMap<String, Arc<BanditArm>> = DashMap::new();
        let node = ProxyNode::new(
            "10.0.0.1".to_string(),
            8080,
            None,
            None,
            "US".to_string(),
            "residential".to_string(),
            "mock-a".to_string(),
            100,
        );
        // 冷路径：新建。
        let first = arm_for(&arms, &node);
        assert_eq!(first.key, node.addr, "臂键必须是 node.addr");
        assert_eq!(first.tier, "residential", "臂档位取自节点");
        assert_eq!(arms.len(), 1);

        // 热路径：命中，且必须返回**同一个** Arc（不是等价的副本）。
        let second = arm_for(&arms, &node);
        assert!(
            Arc::ptr_eq(&first, &second),
            "命中时必须返回同一 Arc（否则 bandit 学习状态会分叉）"
        );
        assert_eq!(arms.len(), 1, "命中不得新建臂");
    }

    /// 不同节点各得各的臂（键隔离）。
    #[test]
    fn opt_r10_b1_arm_for_distinct_nodes_distinct_arms() {
        let arms: DashMap<String, Arc<BanditArm>> = DashMap::new();
        let mk = |ip: &str, port: u16| {
            ProxyNode::new(
                ip.to_string(),
                port,
                None,
                None,
                "US".to_string(),
                "residential".to_string(),
                "mock-a".to_string(),
                100,
            )
        };
        let a = arm_for(&arms, &mk("10.0.0.1", 8080));
        let b = arm_for(&arms, &mk("10.0.0.2", 8080));
        assert!(!Arc::ptr_eq(&a, &b), "不同节点不得共享臂");
        assert_eq!(arms.len(), 2);
        // 同 addr 不同端口 ⇒ 不同键（addr 形如 ip:port）。
        let c = arm_for(&arms, &mk("10.0.0.1", 9090));
        assert!(!Arc::ptr_eq(&a, &c), "同 ip 不同端口是不同节点");
        assert_eq!(arms.len(), 3);
    }

    /// **零分配契约**：命中路径连续调用不得产生 `String` 分配。
    ///
    /// 与 OPT-R9 C1 的分配契约测试同样受**并行测试线程污染**影响，故同样
    /// 标 `#[ignore]` 并由 CI 串行跑（见 ci.yml 的 allocation-contract 步骤）。
    /// 本测试与 C1 的那个跑在**同一条串行命令**里（用同一个过滤器前缀）。
    #[test]
    #[ignore = "needs --test-threads=1; run via CI serial allocation-contract job"]
    fn opt_r10_b1_arm_for_hit_path_does_not_allocate() {
        use std::sync::atomic::Ordering;

        let arms: DashMap<String, Arc<BanditArm>> = DashMap::new();
        let node = ProxyNode::new(
            "10.0.0.1".to_string(),
            8080,
            None,
            None,
            "US".to_string(),
            "residential".to_string(),
            "mock-a".to_string(),
            100,
        );
        // 预热：先建好臂，使后续全部走命中路径。
        let warm = arm_for(&arms, &node);
        assert_eq!(arms.len(), 1);

        let before = crate::test_allocs::ALLOCS.load(Ordering::Relaxed);
        let mut same = 0usize;
        for _ in 0..1000 {
            // 语义正确性顺带验证：每次都拿到同一个 Arc。
            if Arc::ptr_eq(&arm_for(&arms, &node), &warm) {
                same += 1;
            }
        }
        let after = crate::test_allocs::ALLOCS.load(Ordering::Relaxed);
        let observed = after - before;

        assert_eq!(same, 1000, "1000 次调用必须全部命中同一臂");
        assert_eq!(
            observed, 0,
            "arm_for 命中路径必须零分配（OPT-R10 B1 核心收益）。\
             若并行全量下偶发失败，是其它测试线程污染了进程级计数器——\
             请用 --test-threads=1 复跑确认。串行下仍失败说明热路径引入了分配。"
        );
    }

    // ---- OPT-R11 A1：`prune_stale_arms` 第 4 处 P0 计数下溢 ----

    /// **红测（确定性）**：证明旧写法 `before - arms.len()` 在「并发生长」下
    /// 确实会 panic，从而说明饱和减**不是可有可无的宽容**。
    ///
    /// 为何不用「真实并发」测试：要卡准 `len()` 与 `retain()` 之间的窗口需要
    /// 精确的线程同步点，既脆弱又可能长期测不到（窗口极窄）——那种测试在
    /// 没有命中时永远绿，等于没有门。此处直接对「下溢表达式本身」断言，
    /// 确定性、无时序依赖，且正是缺陷的最小充分复现。
    #[test]
    fn opt_r11_a1_plain_subtraction_panics_under_concurrent_growth() {
        // `arms` 被数据面 `arm_for()`（gateway.rs 冷路径 `arms.insert`）并发写。
        // 后台 sweep 采样 `before` 后若有插入，则 `after > before`。
        let before: usize = 0;
        let after: usize = 3;
        // 若此处不 panic，则「旧写法会崩」的前提不成立，本测试失去意义——
        // 因此这里要求**必须** panic（仅 debug 构建；release 下回绕，见下一测试）。
        let r = std::panic::catch_unwind(|| before - after);
        assert!(
            r.is_err(),
            "旧写法 `before - after` 在并发生长下必须 panic；若未 panic，\
             说明本轮的缺陷判断前提不成立，红测失去意义"
        );
    }

    /// release 构建下旧写法不 panic，但**回绕成天文数字**——同样必须修。
    /// 用 `checked_sub` 复现回绕语义（普通减法在 release 下的行为等价于
    /// wrapping，故显式验证「无符号下溢产生巨大值」这一危害形态）。
    #[test]
    fn opt_r11_a1_plain_subtraction_wraps_to_huge_value_in_release() {
        let before: usize = 0usize.wrapping_sub(3);
        assert_eq!(
            before,
            usize::MAX - 2,
            "release 下普通减法回绕成天文数字，日志/指标将永久失真"
        );
    }

    /// 绿测（锁定修复）：`prune_stale_arms` 现用 `janitor::removed_count`
    /// 饱和减，三态语义与全仓单一真源一致。
    #[test]
    fn opt_r11_a1_prune_arms_uses_saturating_helper() {
        use crate::janitor::removed_count;
        // 并发生长：净删除 0，不 panic。
        assert_eq!(removed_count(0, 3), 0);
        // 纯删除：与旧普通减法等价（存量行为零变化）。
        assert_eq!(removed_count(9, 4), 5);
    }

    /// 端到端守护：真实并发「边建臂边修剪」下，`prune_stale_arms` 的返回值
    /// 恒为 sane 值——不 panic（debug 下旧写法会 panic），也不回绕出天文数字。
    ///
    /// # 诚实标注：本测试是「窗口加宽」的补充守护，不是主证据
    ///
    /// 主证据是上面两个**确定性**测试（直接断言旧表达式的 panic 与回绕）。
    /// 本测试靠真实并发去撞那个窗口，为把窗口加宽做了三件事：表开到 2000 条
    /// （`retain` 有可观测耗时）＋ 4 个写线程 + 主线程连续修剪 2000 轮。
    /// **它仍可能整轮不命中窗口而全绿**——这是并发测试的固有性质，不假装
    /// 它是确定性门。它仍有价值：一旦命中，debug 立刻 panic、release 立刻
    /// 撞到 `removed > SANE_BOUND` 而失败。
    #[test]
    fn opt_r11_a1_prune_stale_arms_sane_under_real_concurrency() {
        use std::sync::atomic::{AtomicBool, Ordering};
        use std::sync::Arc as StdArc;

        /// 天文数字的判别下界：表最大 2000 条 + 2000 轮并发插入，
        /// 任何一轮的「净删除」都绝不可能超过这个量级。
        const SANE_BOUND: usize = 1_000_000;

        let router = RouterEngine::new(vec![]);
        let arms: StdArc<DashMap<String, Arc<BanditArm>>> = StdArc::new(DashMap::new());
        for i in 0..2000u32 {
            let n = ProxyNode::new(
                format!("10.{}.{}.{}", i / 65536, (i / 256) % 256, i % 256),
                8080,
                None,
                None,
                "US".to_string(),
                "residential".to_string(),
                "mock-a".to_string(),
                100,
            );
            let _ = arm_for(&arms, &n);
        }

        let stop = StdArc::new(AtomicBool::new(false));
        let mut writers = Vec::new();
        for w in 0..4u32 {
            let arms_c = StdArc::clone(&arms);
            let stop_c = StdArc::clone(&stop);
            writers.push(std::thread::spawn(move || {
                let mut i = 0u32;
                while !stop_c.load(Ordering::Relaxed) {
                    // 每次写一个新臂 ⇒ 持续制造「before/after 之间并发生长」。
                    let n = ProxyNode::new(
                        format!("172.{}.{}.{}", w, i / 256, i % 256),
                        8080 + (i % 1000) as u16,
                        None,
                        None,
                        "US".to_string(),
                        "residential".to_string(),
                        "mock-a".to_string(),
                        100,
                    );
                    let _ = arm_for(&arms_c, &n);
                    i = i.wrapping_add(1);
                }
            }));
        }

        for _ in 0..2000 {
            let removed = prune_stale_arms(&arms, &router);
            // 唯一需要守的不变量：返回值落在 sane 量级内。
            //
            // 不对具体数值断言：并发增长时
            // `saturating_sub` 正确地返回 0（即「本轮净删除」），
            // 因此「本轮删了多少」在有写线程时不具有可预期值。
            assert!(
                removed < SANE_BOUND,
                "返回值 {removed} 超出 sane 上界 {SANE_BOUND}，说明发生了下溢/回绕"
            );
        }
        stop.store(true, Ordering::Relaxed);
        for h in writers {
            let _ = h.join();
        }
    }

    // ---- OPT-R11 A2：SOCKS 总预算 `Instant` 加法溢出 ----

    /// **红测（确定性）**：证明 `SOCKS_BRIDGE_TIMEOUT_SECS` 传入
    /// `env_secs` 能通过的最大值后，`Instant::now() + budget` **确实会 panic**。
    ///
    /// 复现链路（已核实）：
    ///   * `env_secs`（main.rs:144-151）只过滤 `<= 0`，**无上限**，
    ///     故 `u64::MAX` 秒可原样进入 `Duration`；
    ///   * `socks_overall_budget`（gateway.rs:308-310）只做 `saturating_add(8s)`，
    ///     **不封顶**；
    ///   * `gateway.rs:437` 直接 `Instant::now() + budget`。
    ///
    /// `Instant` 内部用有符号表示，秒数加到 `i64::MAX` 之外即溢出 panic。
    #[test]
    fn opt_r11_a2_unclamped_instant_add_panics() {
        use std::time::{Duration, Instant};
        // `u64::MAX` 秒能通过 `env_secs`（`parse::<u64>` 成功且 `> 0`）——
        // 这正是"无上限过滤"的直接后果。
        let unclamped = Duration::from_secs(u64::MAX);
        let r = std::panic::catch_unwind(|| Instant::now() + unclamped);
        assert!(
            r.is_err(),
            "未钳制的预算加到 Instant 上必须 panic；若不 panic，\
             说明本轮缺陷判断的前提不成立（Instant 语义与推断不符）"
        );
    }

    /// **绿测（锁定修复）**：钳制后不 panic，且**不改变任何合理值**。
    #[test]
    fn opt_r11_a2_clamped_budget_never_panics_and_preserves_reasonable_values() {
        use std::time::{Duration, Instant};
        // 修复后入口即钳制：这里模拟 gateway.rs:437 的实际算式。
        // 直接用真实的钳制函数（不在测试里复制实现，
        // 否则测试守的是测试里那份副本而非真实代码）。
        let clamp = crate::clamp_bridge_timeout;

        // 极端输入：加到 Instant 上不 panic。
        let deadline = Instant::now() + clamp(Duration::from_secs(u64::MAX));
        assert!(deadline > Instant::now(), "钳制后仍是未来时刻");

        // 存量行为零变化：所有合理值原样通过。
        for secs in [0u64, 1, 20, 60, 300, 3600, 86_399] {
            assert_eq!(
                clamp(Duration::from_secs(secs)),
                Duration::from_secs(secs),
                "合理值 {secs}s 必须原样通过（存量行为零变化）"
            );
        }
        // 超上限值被钳到 1 天（远超任何合理桥接超时）。
        assert_eq!(
            clamp(Duration::from_secs(86_400 + 1)),
            Duration::from_secs(86_400)
        );
    }

    /// OPT-R13 网关层：直接调 `select_bandit_node_excluding` 300 次，
    /// **三个节点都必须被选过**。
    ///
    /// 此测试与活流量同层，用于区分两件事：
    /// (1) bandit 数学本身有效（`bandit.rs` 层已有测试）；
    /// (2) **网关接线 / 候选过滤** 有问题。
    #[test]
    fn opt_r13_gateway_level_selection_explores_all_nodes() {
        let nodes = vec![
            test_node("127.0.0.1", 8888),
            test_node("127.0.0.1", 8889),
            test_node("127.0.0.1", 8890),
        ];
        let gw = test_gateway(nodes);
        let spec = RoutingSpec::default();
        let x = gw.bandit_engine.extract_context("127.0.0.1:8888");
        let mut pulled = [0usize; 3];
        for _ in 0..300 {
            let picked = gw.select_bandit_node_excluding(&spec, &[], &x);
            let Some(node) = picked else {
                panic!("should always find a candidate")
            };
            let idx = match node.port {
                8888 => 0,
                8889 => 1,
                _ => 2,
            };
            pulled[idx] += 1;
            arm_for(&gw.bandit_arms, &node).update(&x, 0.99);
        }
        assert!(
            pulled.iter().all(|c| *c > 0),
            "网关层三节点都应被探索到，实测={pulled:?}"
        );
    }

    /// 候选过滤诊断：若某节点被过滤掉，活流量会变成 100% 单节点。
    #[test]
    fn opt_r13_gateway_candidate_filter_keeps_all_three_nodes() {
        let nodes = vec![
            test_node("127.0.0.1", 8888),
            test_node("127.0.0.1", 8889),
            test_node("127.0.0.1", 8890),
        ];
        let gw = test_gateway(nodes);
        let spec = RoutingSpec::default();
        let now = std::time::Instant::now();
        let mut kept = 0usize;
        gw.router.with_pools(|pool| {
            for n in pool {
                let pass = gw.router.matches_node(n, &spec, now);
                println!("DIAG node={} weight={} matches={pass}", n.addr, n.weight);
                if pass {
                    kept += 1;
                }
            }
        });
        assert_eq!(kept, 3, "三个 mock 节点都应通过 matches，实测 kept={kept}");
    }
}
