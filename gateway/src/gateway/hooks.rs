//! `impl ProxyHttp for SmartProxyGateway`：Pingora 的 9 个回调。
//!
//! 数据面的主干就在这里：`request_filter`（鉴权/环境门/租户）→
//! `upstream_peer`（选路/建连目标）→ `proxy_upstream_filter`（SOCKS 桥短路）→
//! `upstream_request_filter`（改写出站头）→ `fail_to_connect`（重试/隔离记账）→
//! `response_body_filter`（计量）→ `logging`（遥测+日志）。
//!
//! 拆到独立文件的原因：这 9 个方法是一个**完整的生命周期契约**，读它们应当
//! 连续无干扰；夹在工具函数与单测之间时，读者很难看出"一次请求依次经过谁"。

use super::upstream::build_http_peer;
use super::*;

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

        // GW-6：链路追踪 ID（下游带 `X-Trace-Id` 则沿用，否则本地生成，
        // 贯穿五阶段并落入遥测）。
        let trace_id = session
            .req_header()
            .headers
            .get("X-Trace-Id")
            .and_then(|v| v.to_str().ok())
            .filter(|v| !v.is_empty())
            .map(|s| s.to_string())
            .unwrap_or_else(next_trace_id);
        ctx.trace_id = trace_id;

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
        // GW-6：选路完成（建链就绪时刻），logging 用于计算 connect 阶段耗时；
        // 重试时被新选路覆盖，反映本次节点的连接耗时。
        ctx.peer_connected_at = Some(Instant::now());
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
                "[CredentialGuard] trace_id={} rejecting credentialed request routed to free node {} (tier={})",
                ctx.trace_id,
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
        // GW-6：重试前重置首字节时刻，下一轮重新计算 ttfb。
        ctx.first_byte_at = None;
        // R2-2：记下失败节点，重试选路时排除（只换节点，不换重试次数语义）。
        record_failed_addr(ctx);
        if ctx.retry_count < ctx.max_retries {
            ctx.retry_count += 1;
            log::warn!(
                "[Gateway] trace_id={} upstream connect error: {:?}, retry {}/{} target={}",
                ctx.trace_id,
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
        // GW-6：首次收到 body chunk 时记录首字节时刻，logging 用于计算 ttfb。
        if ctx.first_byte_at.is_none() && body.is_some() {
            ctx.first_byte_at = Some(Instant::now());
        }
        if let Some(ref chunk) = body {
            ctx.transferred_bytes += chunk.len() as u64;
        }
        Ok(None)
    }

    async fn logging(&self, session: &mut Session, e: Option<&Error>, ctx: &mut Self::CTX) {
        let duration = ctx.start_time.elapsed();
        // GW-6：五阶段耗时（近似：connect＝选路到 peer_connected_at，ttfb＝首字节到达，
        // transfer＝剩余耗时；tls 握手并入 connect）。
        let now_ms = duration.as_millis() as u64;
        let connect_ms = ctx
            .peer_connected_at
            .map(|t| t.elapsed().as_millis() as u64)
            .unwrap_or(0)
            .min(now_ms);
        let ttfb_offset_ms = ctx
            .first_byte_at
            .map(|t| t.elapsed().as_millis() as u64)
            .unwrap_or(now_ms);
        let ttfb_ms = ctx
            .peer_connected_at
            .and_then(|pc| {
                ctx.first_byte_at
                    .map(|fb| fb.duration_since(pc).as_millis() as u64)
            })
            .unwrap_or(0)
            .min(now_ms);
        let transfer_ms = now_ms.saturating_sub(ttfb_offset_ms);
        self.metrics.observe_stage("connect", connect_ms);
        self.metrics.observe_stage("ttfb", ttfb_ms);
        self.metrics.observe_stage("transfer", transfer_ms);
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
                // GW-6：链路追踪 ID，贯穿五阶段，便于 ClickHouse 钻取单请求全链路。
                trace_id: ctx.trace_id.clone(),
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
        // OPT-R18 P1-1：日志带 `trace_id`，与 ClickHouse 遥测**同键互查**。
        // 此前 trace_id 只进遥测，排障时"CH 里有这条 502、日志里对不上是哪行"。
        // 只加在 warn/info 这类低频行上，热路径不额外克隆 String。
        //
        // **2026-10-28 更正**：上面那句「与 CH 遥测同键互查」当时**并不成立**——
        // `TelemetryRow` 没有 `trace_id` 字段、CH 表也没有该列，`From` 转换时
        // 该字段被完整丢弃。日志侧有 ID、仓库侧无 ID，两边根本对不上。
        // 现已补齐（`001_schema.sql` 加列 + `TelemetryRow.trace_id` 透传 +
        // `manual/003_trace_id.sql` 存量迁移），并有 `row_columns_match_ddl_columns`
        // 把「结构体字段集合 ↔ DDL 列集合」钉成编译期契约（双向：DDL 多列会
        // 静默补默认值、结构体多字段会被 CH 拒绝，两个方向都必须拦）。
        // 互查查询示例见 `docs/OPERATION.md` §4.3。
        if self.metrics.sample_full_log(status) {
            log::info!(
                "[Telemetry] trace_id={} client={} target={} out={} status={} cost={:?}",
                ctx.trace_id,
                ctx.client_ip,
                ctx.routing_spec.target_domain,
                node_ip,
                status,
                duration
            );
        } else {
            log::debug!(
                "[Telemetry] trace_id={} client={} target={} out={} status={} cost={:?}",
                ctx.trace_id,
                ctx.client_ip,
                ctx.routing_spec.target_domain,
                node_ip,
                status,
                duration
            );
        }
    }
}
