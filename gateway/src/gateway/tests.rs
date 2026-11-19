//! `gateway` 全部单测（原 `gateway.rs` 后半段，逐字搬迁）。
//!
//! `use super::*;` 可见父模块的全部条目（含 `pub use` 再导出的子模块条目）。

use super::upstream::{
    bridge_url_for, build_http_peer, should_skip_bridge_node, socks_overall_budget,
};
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
        health: crate::health::RuntimeHealthState::new(),
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

#[tokio::test]
async fn draining_http_request_is_rejected_before_tenant_authentication() {
    use pingora_core::protocols::l4::stream::Stream;
    use pingora_proxy::Session;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    let gateway = test_gateway(vec![]);
    gateway.health.request_drain();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("test listener");
    let addr = listener.local_addr().expect("listener address");
    let client = tokio::net::TcpStream::connect(addr);
    let (server, client) = tokio::join!(listener.accept(), client);
    let (server, _) = server.expect("accept client");
    let mut client = client.expect("connect client");
    let task = tokio::spawn(async move {
        let mut session = Session::new_h1(Box::new(Stream::from(server)));
        session.read_request().await.expect("parse request");
        let mut ctx = gateway.new_ctx();
        let response_sent = gateway
            .request_filter(&mut session, &mut ctx)
            .await
            .expect("request filter");
        assert!(response_sent);
    });
    client
        .write_all(b"GET http://example.com/ HTTP/1.1\r\nHost: example.com\r\n\r\n")
        .await
        .expect("send request");
    let mut response = Vec::new();
    tokio::time::timeout(Duration::from_secs(1), client.read_to_end(&mut response))
        .await
        .expect("response timeout")
        .expect("read response");
    assert!(response.starts_with(b"HTTP/1.1 503"));
    task.await.expect("request task");
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

/// 出站 TLS 校验契约（取证：pingora-core 0.9.0
/// `src/connectors/tls/rustls/mod.rs:213-258`）。
///
/// `HttpPeer::get_peer_options()` 恒为 `Some`（默认 `alpn=H1`），因此连接器
/// 每次都会克隆出 `updated_config_opt`；随后 `sni` 为空或 `verify_cert=false`
/// 都被映射成 `VerificationMode::SkipAll`，其 `verify_server_cert` 直接返回
/// `ServerCertVerified::assertion()`（同文件 :361）——证书链与主机名**一个都不验**。
/// 这不是「理论上可能」：把 SNI 置空即可在生产构建触发，且日志里看不出异常。
/// 故本仓必须显式把这几个字段钉住，而不是依赖框架默认值。
#[test]
fn tls_peer_pins_certificate_verification_flags() {
    let node = test_node("10.0.0.1", 443);
    let peer = build_http_peer(&node, "api.example.com").expect("valid TLS peer");
    assert!(peer.is_tls(), "端口 443 必须走 TLS");
    assert_eq!(peer.sni, "api.example.com");
    assert!(peer.options.verify_cert, "证书链校验不得关闭");
    assert!(peer.options.verify_hostname, "主机名校验不得关闭");
    assert!(
        peer.options.alternative_cn.is_none(),
        "不得用 alternative_cn 放宽主机名匹配"
    );
    assert!(
        peer.options.ca.is_none(),
        "信任源由平台/SSL_CERT_FILE 提供，不在代码里另设 per-peer CA"
    );
}

/// RED→GREEN：空 SNI = Pingora 的 fail-open 分支。
///
/// `request_filter` 现在会拒绝空 Host（400），但那个守卫在**另一个回调**里。
/// 上游选路路径变更、或将来出现 `build_http_peer` 的新调用方时，peer 装配
/// 必须自己失败关闭，不能把「不验证书」当作可接受的降级。
#[test]
fn tls_peer_rejects_blank_or_control_sni() {
    let node = test_node("10.0.0.1", 443);
    for host in [
        "",
        "   ",
        "\t",
        "api.example.com\u{0}",
        "api.example.com\r\n",
    ] {
        assert!(
            build_http_peer(&node, host).is_err(),
            "TLS peer 不得接受空/含控制字符的 SNI: {host:?}"
        );
    }
}

/// 边界：明文 peer 不进 rustls 连接器，SNI 不参与任何校验，行为保持不变。
#[test]
fn plain_http_peer_does_not_enter_rustls_connector() {
    let node = test_node("10.0.0.2", 8080);
    let peer = build_http_peer(&node, "").expect("plain HTTP peer does not use SNI");
    assert!(!peer.is_tls());
}

/// OPT-R15：`is_tls_for_node` 与 `is_loopback_addr` 各守各的口径。
///
/// **更正一处声明失实**：原名 `skip_cert_verify_requires_loopback_and_test_channel`
/// 描述的是一条**已经不存在**的生产分支（步骤 47 移除了「跳过证书校验」的尝试）。
/// 本仓现在没有任何按回环地址放宽 TLS 校验的代码——`upstream.rs` 显式固定
/// `verify_cert=true`，由 `tls_peer_pins_certificate_verification_flags` 守住。
/// 测试体实际只断言「公网 443 判为 TLS、但不判为回环」，故按真实行为改名：
/// 测试名也是声明，声明同样不能失实。
#[test]
fn tls_and_loopback_predicates_are_distinct() {
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

/// **红测（确定性）**：证明旧写法 `before - after` 在「并发生长」下确实是
/// 缺陷，从而说明饱和减**不是可有可无的宽容**。
///
/// 为何不用「真实并发」测试：要卡准 `len()` 与 `retain()` 之间的窗口需要
/// 精确的线程同步点，既脆弱又可能长期测不到（窗口极窄）——那种测试在
/// 没有命中时永远绿，等于没有门。此处直接对「下溢表达式本身」断言，
/// 确定性、无时序依赖，且正是缺陷的最小充分复现。
///
/// # OPT-R15 修正：断言必须与构建 profile 解耦
///
/// 旧写法为 `assert!(catch_unwind(|| before - after).is_err())`，即断言
/// 「普通减法必 panic」。**该断言只在 `overflow-checks = on` 时成立**：
/// release（`cargo bench --workspace -- --test`、`cargo test --release`）
/// 默认关闭溢出检查，下溢改为回绕 ⇒ 不 panic ⇒ 测试红。实测本仓
/// `cargo bench -- --test` 稳定复现该失败。
///
/// 现改为断言**两个 profile 都成立的语义**：`checked_sub` 下溢返 `None`
/// （这正是 debug 下普通减法 panic 的原因），且回绕值是天文数字（这正是
/// release 下的实际危害）。二者合起来完整刻画该缺陷，不再依赖 profile。
/// 修复后的饱和减行为由紧邻的绿测锁定。
///
/// 注：`arms` 被数据面 `arm_for()`（冷路径 `arms.insert`）并发写；
/// 后台 sweep 采样 `before` 后若有插入，则 `after > before`。
#[test]
fn opt_r11_a1_plain_subtraction_is_unsound_in_every_profile() {
    let before: usize = 0;
    let after: usize = 3;
    // ① 语义一：下溢可被检测（debug 下普通减法 panic 的根因）。
    //    该性质由 checked_sub 的定义保证，与 overflow-checks 无关。
    assert!(
        before.checked_sub(after).is_none(),
        "before=0 after=3 属下溢，checked_sub 必须返回 None"
    );
    // ② 语义二：若不检测（release 关闭溢出检查），下溢回绕成天文数字，
    //    使 arms 计数/日志永久失真——危害不比 panic 小，只是更隐蔽。
    assert_eq!(
        before.wrapping_sub(after),
        usize::MAX - 2,
        "release 下普通减法回绕成天文数字，日志/指标将永久失真"
    );
    // ③ 无论哪个 profile，饱和减都给出 sane 值（下界 0，绝不为天文数字）。
    assert_eq!(crate::janitor::removed_count(before, after), 0);
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
