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
use std::sync::Arc;
use std::time::Duration;

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
    /// 不进入租户查询（语义与坏 Key 一致：`tenant_account=None`，logging 不释放配额）。
    /// 默认关闭（`REQUIRE_API_KEY=1` 开启），保持 GW-1~GW-4 存量 curl 行为不变。
    pub require_api_key: bool,
    /// P2 SOCKS 翻译桥（`None`＝未装配：socks 显式请求直接 503；main 装配 Some，见 P2-7）。
    pub bridge: Option<Arc<SocksBridge>>,
}

/// OPT-2 纯谓词：环境门是否应拦截本次请求。
///
/// - 仅当开关打开 **且** 客户端完全没传 `X-API-Key` 头时返回 true；
/// - 传了 Key（即使是错 Key）走正常租户鉴权路径，由 `TenantManager` 判 403/429，
///   以便错误归因（坏 Key vs 缺 Key）保持可区分。
pub fn should_reject_missing_api_key(require_api_key: bool, has_api_key_header: bool) -> bool {
    require_api_key && !has_api_key_header
}

/// 网关缺省 API Key：客户端未传 `X-API-Key` 时使用，启动时注册宽限额。
pub const DEFAULT_API_KEY: &str = "default_key";

/// 取节点对应的 LinUCB 臂，没有则新建（首写竞态无害：新臂状态等价）。
pub fn arm_for(arms: &DashMap<String, Arc<BanditArm>>, node: &ProxyNode) -> Arc<BanditArm> {
    let key = node.addr.clone();
    if let Some(existing) = arms.get(&key) {
        return existing.value().clone();
    }
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
    before - arms.len()
}

/// P2：HttpPeer 装配（含 socks 守卫）。
///
/// 守卫正常走不到（显式 socks 请求被 `proxy_upstream_filter` 短路，默认请求被
/// router 默认隔离），但一旦走到必须硬 503——把 socks 地址当 HTTP 上游去连，
/// 连上也是协议错配（HTTP 正向代理握手发给 SOCKS 端口必失败，还浪费一次重试）。
fn build_http_peer(node: &ProxyNode, target_host: &str) -> Result<Box<HttpPeer>> {
    if node.proto != EgressProto::Http {
        return Err(Error::explain(
            pingora_core::ErrorType::HTTPStatus(503),
            "SOCKS node in HTTP path",
        ));
    }
    // GW-3 still plain-HTTP forward upstream; TLS/SNI customization stays
    // out of scope (frozen: no uTLS/Boring this round).
    let is_tls = false;
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
    fn select_bandit_node_excluding(
        &self,
        spec: &RoutingSpec,
        excluded: &[String],
        context: &VectorD,
    ) -> Option<Arc<ProxyNode>> {
        let candidates = self.router.get_healthy_candidates_excluding(spec, excluded);
        if candidates.is_empty() {
            return None;
        }
        let arms: Vec<Arc<BanditArm>> = candidates
            .iter()
            .map(|n| arm_for(&self.bandit_arms, n))
            .collect();
        let best = self.bandit_engine.select_best_arm(&arms, context)?;
        candidates.iter().find(|n| n.addr == best.key).cloned()
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
        for _ in 0..attempts {
            let Some(node) = self.pick_socks_candidate(
                &ctx.routing_spec,
                &ctx.failed_addrs,
                &context,
                has_session,
            ) else {
                break;
            };
            if node.proto == EgressProto::Http {
                continue; // 防御分支：正常走不到（router 已按 spec.proto 过滤）。
            }
            ctx.current_node = Some(Arc::clone(&node));
            ctx.transferred_bytes = 0; // OPT-3：只计最后 attempt
            let breq = BridgeRequest {
                method: method.clone(),
                url: url.clone(),
                headers: headers.clone(),
                body: body.clone(),
            };
            match bridge.fetch(&node, breq).await {
                Ok(resp) => {
                    // REVIEW-R2 Q9：egress 已发生即记账（写下游失败仍计量出站成本；
                    // OPT-3 只计最后 attempt 口径不变，`?` 前先赋值）。
                    ctx.transferred_bytes = resp.body.len() as u64;
                    self.write_bridge_response(session, resp).await?;
                    return Ok(());
                }
                Err(e) => {
                    log::warn!("[SocksBridge] via {} failed: {e}", node.addr);
                    record_failed_addr(ctx);
                    if ctx.retry_count < ctx.max_retries {
                        ctx.retry_count += 1;
                    }
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
}
