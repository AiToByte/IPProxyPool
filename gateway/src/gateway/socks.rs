//! SOCKS 桥：候选选择、`serve_via_socks`、桥响应回写。
//!
//! Phase 1 只收 HTTP(S)，SOCKS 节点走**翻译桥**（网关把 HTTP 正向代理请求
//! 翻译成 SOCKS CONNECT 再转发），因此这条路径要自己做响应合成：状态行 +
//! 过滤后的头 + 分 chunk body（content-length 显式），不能交给 Pingora 的
//! 上游路径。

use super::upstream::{bridge_url_for, should_skip_bridge_node, socks_overall_budget};
use super::*;

impl SmartProxyGateway {
    /// R3-1：socks 候选选择（有会话走粘滞＋失败排除；无会话走 LinUCB，与 HTTP
    /// 无状态路径对齐——P2 的学选分裂在此闭合；router 侧已按 proto 过滤）。
    pub(super) fn pick_socks_candidate(
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
    pub(super) async fn serve_via_socks(
        &self,
        session: &mut Session,
        ctx: &mut ProxyContext,
    ) -> Result<()> {
        let bridge = match self.bridge {
            Some(ref b) => Arc::clone(b),
            None => {
                log::warn!(
                    "[SocksBridge] trace_id={} bridge offline, rejecting socks request",
                    ctx.trace_id
                );
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
                    log::warn!(
                        "[SocksBridge] trace_id={} via {} failed: {e}",
                        ctx.trace_id,
                        node.addr
                    );
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
                    log::warn!(
                        "[SocksBridge] trace_id={} via {} overall timeout",
                        ctx.trace_id,
                        node.addr
                    );
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
