//! GW-3 smart ingress gateway over Pingora `ProxyHttp` (pingora 0.6 API).
//!
//! Lifecycle: request_filter (parse + Chrome align + scrub) → upstream_peer
//! (sticky fast-path or LinUCB select + chrome transport profile) →
//! upstream_request_filter (inject egress auth) → fail_to_connect (retry) →
//! logging (bandit reward update + telemetry emit, GW-2 bus).

use crate::bandit::{compute_reward, BanditArm, LinUCBEngine, VectorD};
use crate::fingerprint::FingerprintHardener;
use crate::metrics::MetricsRegistry;
use crate::model::{ProxyContext, ProxyNode, RoutingSpec};
use crate::router::{normalize_domain, RouterEngine};
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
        // R2-6：`peer_addr` 在 move 进 ctx 前克隆（`Arc` 句柄本身零成本 move）。
        let peer_addr = node.addr.clone();
        ctx.current_node = Some(node);

        let target_host = session
            .req_header()
            .headers
            .get(http::header::HOST)
            .and_then(|v| v.to_str().ok())
            .unwrap_or("default.target")
            .to_string();

        // GW-3 still plain-HTTP forward upstream; TLS/SNI customization stays
        // out of scope (frozen: no uTLS/Boring this round).
        let is_tls = false;
        let mut peer = HttpPeer::new(peer_addr, is_tls, target_host);
        peer.options.connection_timeout = Some(Duration::from_millis(1500));
        peer.options.read_timeout = Some(Duration::from_millis(5000));
        peer.options.write_timeout = Some(Duration::from_millis(3000));
        FingerprintHardener::apply_chrome_profile(&mut peer);
        Ok(Box::new(peer))
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
        let node_ip = node.as_ref().map(|n| n.ip.as_str()).unwrap_or("none");
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
}
