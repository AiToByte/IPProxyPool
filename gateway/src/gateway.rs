//! GW-3 smart ingress gateway over Pingora `ProxyHttp` (pingora 0.6 API).
//!
//! Lifecycle: request_filter (parse + Chrome align + scrub) → upstream_peer
//! (sticky fast-path or LinUCB select + chrome transport profile) →
//! upstream_request_filter (inject egress auth) → fail_to_connect (retry) →
//! logging (bandit reward update + telemetry emit, GW-2 bus).

use crate::bandit::{compute_reward, BanditArm, LinUCBEngine};
use crate::fingerprint::FingerprintHardener;
use crate::metrics::MetricsRegistry;
use crate::model::{ProxyContext, ProxyNode, RoutingSpec};
use crate::router::RouterEngine;
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
    /// GW-3 arm states keyed by `node.addr()` (`ip:port`, not bare IP).
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
    let key = node.addr();
    if let Some(existing) = arms.get(&key) {
        return existing.value().clone();
    }
    let arm = Arc::new(BanditArm::new(key.clone(), node.tier.clone()));
    arms.insert(key, arm.clone());
    arm
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
    let alive: std::collections::HashSet<String> =
        router.snapshot_all().iter().map(|n| n.addr()).collect();
    let before = arms.len();
    arms.retain(|key, _| alive.contains(key));
    before - arms.len()
}

impl SmartProxyGateway {
    /// 无状态请求的 LinUCB 选路。
    fn select_bandit_node(&self, spec: &RoutingSpec) -> Option<ProxyNode> {
        let context = self.bandit_engine.extract_context(&spec.target_domain);
        let candidates = self.router.get_healthy_candidates(spec);
        if candidates.is_empty() {
            return None;
        }
        let arms: Vec<Arc<BanditArm>> = candidates
            .iter()
            .map(|n| arm_for(&self.bandit_arms, n))
            .collect();
        let best = self.bandit_engine.select_best_arm(&arms, &context)?;
        candidates.iter().find(|n| n.addr() == best.key).cloned()
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
                let status = if msg.contains("Rate limit") || msg.contains("concurrency") {
                    429
                } else {
                    403
                };
                session.respond_error(status).await?;
                return Ok(true);
            }
        }

        ctx.routing_spec = parse_routing_spec(session.req_header_mut());

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
        let node = if ctx.routing_spec.session_id.is_some() {
            self.router.select_node(&ctx.routing_spec)
        } else {
            self.select_bandit_node(&ctx.routing_spec)
        }
        .ok_or_else(|| {
            Error::explain(
                pingora_core::ErrorType::HTTPStatus(503),
                "No active proxy node available",
            )
        })?;
        ctx.current_node = Some(node.clone());

        let target_host = session
            .req_header()
            .headers
            .get(http::header::HOST)
            .and_then(|v| v.to_str().ok())
            .unwrap_or("default.target")
            .to_string();

        let peer_addr = node.addr();
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
        if let Some(ref n) = node {
            let context = self
                .bandit_engine
                .extract_context(&ctx.routing_spec.target_domain);
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
        log::info!(
            "[Telemetry] client={} target={} out={} status={} cost={:?}",
            ctx.client_ip,
            ctx.routing_spec.target_domain,
            node_ip,
            status,
            duration
        );
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
        spec.target_domain = host.to_string();
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
        ProxyNode {
            ip: ip.to_string(),
            port,
            username: None,
            password: None,
            country: "US".to_string(),
            tier: "residential".to_string(),
            provider: "mock-a".to_string(),
            weight: 100,
        }
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
}
