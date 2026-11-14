//! GW-3 smart ingress gateway over Pingora `ProxyHttp` (pingora 0.6 API).
//!
//! Lifecycle: request_filter (parse + Chrome align + scrub) → upstream_peer
//! (sticky fast-path or LinUCB select + chrome transport profile) →
//! upstream_request_filter (inject egress auth) → fail_to_connect (retry) →
//! logging (bandit reward update + telemetry emit, GW-2 bus).

use crate::bandit::{compute_reward, BanditArm, LinUCBEngine, VectorD};
use crate::connect::parse_connect_authority;
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
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

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

/// GW-6：请求链路追踪 ID 发号器（`{unix_ms}-{pid}-{seq}`，本进程唯一）。
/// 下游带 `X-Trace-Id` 则沿用，否则本地生成，贯穿五阶段并落入遥测。
static TRACE_SEQ: AtomicU64 = AtomicU64::new(0);

fn next_trace_id() -> String {
    let n = TRACE_SEQ.fetch_add(1, Ordering::Relaxed);
    format!(
        "{}-{}-{}",
        TelemetryEvent::now_unix_ms(),
        std::process::id(),
        n
    )
}

/// 为门省省 API Key（OPT-R12 A3：单一真源）。
///
/// 真实定义已迁到 `tenant::DEFAULT_API_KEY`——密钥的持有者是
/// `TenantManager`，放在数据面模块 `gateway.rs` 里属结构性错位：改数据面
/// 的人会以为碰不到鉴权，改鉴权的人却要去数据面找常量。
/// 此处保留 `pub use` 转发，以保持已有引用点（SDK / 测试）不破坏。
pub use crate::tenant::DEFAULT_API_KEY;

// ---------------------------------------------------------------------------
// 子模块划分（OPT-R18 P2-1 第三批）
// ---------------------------------------------------------------------------
// 本文件曾 2275 行，把**四类职责**混在一起：鉴权/分档的纯判定、LinUCB 选路
// 上下文、上游 peer 装配、SOCKS 桥转发，外加 40 个单测。改任一处都要在
// 2200 行里定位上下文。现按职责拆分（全部**逐字搬迁**，行为零变更）：
//
// | 子模块 | 职责 |
// | --- | --- |
// | `policy.rs` | 纯判定：环境门、鉴权错误码映射、free 档凭据门、租户会话命名 |
// | `bandit.rs` | LinUCB：取臂、上下文解析、分档配额选路、游离臂修剪、重试记账 |
// | `upstream.rs` | 上游装配：TLS 判定、回环判定、`HttpPeer` 构造、桥出站 URL |
// | `socks.rs` | SOCKS 桥：候选选择、`serve_via_socks`、桥响应回写 |
// | `hooks.rs` | `impl ProxyHttp for SmartProxyGateway`（Pingora 的 9 个回调） |
// | `tests.rs` | 全部单测（原文件后半段） |
//
// **为什么 `impl SmartProxyGateway` 被拆到 bandit/socks 两处**：Rust 允许同一
// 类型的固有 impl 分布在同 crate 的任意模块。这些方法操作的是「选路」与
// 「SOCKS 桥」两件不同的事，按职责就近放置比硬塞进 mod.rs 更可读。
// 构造函数不在本文件——`SmartProxyGateway` 由 `main.rs` 用结构体字面量装配。

mod bandit;
mod hooks;
mod policy;
mod socks;
mod upstream;

pub use bandit::{
    arm_for, prune_stale_arms, record_failed_addr, reset_retry_accounting, resolve_bandit_context,
};
pub use policy::{
    apply_tenant_namespace, free_node_exits_with_credentials, free_tier_with_credentials,
    has_authorization, has_cookie, should_reject_missing_api_key, status_for_auth_error,
};
// `cfg_attr(not(test), allow(unused_imports))`：`is_tls_for_node` 的消费方是
// `tests.rs`（只在 cfg(test) 下编译），故非测试构建里它"没人用"——但它不是
// 死代码，删掉测试就编不过。与 free_pool 拆分时同一条处理（见该处注释）。
#[cfg_attr(not(test), allow(unused_imports))]
pub use upstream::{is_loopback_addr, is_tls_for_node};

/// 将 CONNECT 的 authority-form 转成路由所需的目标域名。
///
/// 复用独立 CONNECT ServerApp 的同一严格解析器，避免普通入口的兼容 API
/// 与 tunnel listener 产生不同的 authority 校验口径。
pub fn parse_connect_target(value: &str) -> Result<crate::connect::ConnectTarget, &'static str> {
    parse_connect_authority(value)
}

/// 解析普通 HTTP 路由指令：支持 Header 与 Basic 用户名中的兼容 token。
////// `user_country-us_session-xxx_tier-res` embedded in the Basic username.
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
mod tests;
