//! 上游装配：TLS 判定、回环判定、`HttpPeer` 构造、桥出站 URL。
//!
//! 这里的每个函数都在回答"这条连接该怎么建"。两个判定（`is_tls_for_node` /
//! `is_loopback_addr`）刻意做成**单一口径**的公开函数：`main.rs` 的测试注入
//! 通道也用它，避免"通道里写一套匹配、这里写另一套"日后各自漂移。

use super::*;

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

/// TLS SNI 合法性：非空、不含空白与控制字符。
///
/// 取证（pingora-core 0.9.0 `src/connectors/tls/rustls/mod.rs:226-257`）：
/// `peer.sni().is_empty()` 会被映射成 `VerificationMode::SkipAll`，其
/// `verify_server_cert` 直接返回 `ServerCertVerified::assertion()`（:361）
/// ——证书链与主机名**全都不验**，且生产构建可达。`request_filter` 已在另一个
/// 回调里拒绝空 Host，但那里守不住 peer 装配的新调用方；这里是独立兜底。
///
/// 刻意只挡「空 / 空白 / 控制字符」：含端口的 Host 形态属于既有对外契约，
/// 本轮不改（其后果是 rustls 拒绝握手，而不是绕过校验）。
fn is_valid_tls_sni(host: &str) -> bool {
    !host.is_empty()
        && host
            .bytes()
            .all(|b| !b.is_ascii_control() && !b.is_ascii_whitespace())
}

/// P2：HttpPeer 装配（含 socks 守卫）。
///
/// 守卫正常走不到（显式 socks 请求被 `proxy_upstream_filter` 短路，默认请求被
/// router 默认隔离），但一旦走到必须硬 503——把 socks 地址当 HTTP 上游去连，
/// 连上也是协议错配（HTTP 正向代理握手发给 SOCKS 端口必失败，还浪费一次重试）。
pub(super) fn build_http_peer(node: &ProxyNode, target_host: &str) -> Result<Box<HttpPeer>> {
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
    // ⚠️ 纠正步骤 45/46 留下的两处错误说法（docs/OPERATION.md 同步更正）：
    //   1. 步骤 45 写「Pingora 默认跳过证书校验」——**错**，`PeerOptions::new()`
    //      的 `verify_cert` / `verify_hostname` 缺省都是 `true`。
    //   2. 步骤 46 写「rustls 后端 `verify_cert=false` 无效」——**错，且这句最危险**。
    //      pingora-core 0.9.0 `connectors/tls/rustls/mod.rs:213-258` 中，
    //      `HttpPeer::get_peer_options()` 恒为 `Some`（默认 `alpn=H1`），因此每次
    //      都会克隆出自定义 config；随后 `verify_cert=false` 或**空 SNI** 都被映射为
    //      `VerificationMode::SkipAll`（:361 直接放行证书）。⇒ 校验**是能关掉的**，
    //      「反正关不掉」只是自我安慰，不能当安全边界。
    //
    // ⇒ 正确做法：不依赖框架缺省值，显式钉死开关 + 拒绝空 SNI；信任源由
    //   `pingora-rustls` 的 `load_platform_certs_incl_env_into_store` 从平台 CA 及
    //   **`SSL_CERT_FILE` / `SSL_CERT_DIR`** 填充（零生产代码改动即可信任自建 CA）。
    let is_tls = is_tls_for_node(node);
    if is_tls && !is_valid_tls_sni(target_host) {
        return Err(Error::explain(
            pingora_core::ErrorType::HTTPStatus(400),
            "invalid upstream TLS SNI: empty or control characters in Host",
        ));
    }
    let mut peer = HttpPeer::new(node.addr.clone(), is_tls, target_host.to_string());

    // Fail-closed：把校验开关写成显式赋值，而不是「默认值应该是 true」。
    // 配合 `gateway/tests.rs::tls_peer_pins_certificate_verification_flags`，
    // 任何日后「为了排障临时关一下」的改动都会被单测当场拦下。
    peer.options.verify_cert = true;
    peer.options.verify_hostname = true;
    peer.options.connection_timeout = Some(Duration::from_millis(1500));
    peer.options.read_timeout = Some(Duration::from_millis(5000));
    peer.options.write_timeout = Some(Duration::from_millis(3000));
    // GW-6：启用 idle_timeout（60s 空闲保留），使网关→上游代理节点的 TCP/TLS 连接
    // 可跨请求复用。TLS 会话票据（session ticket）随连接复用自动获得，避免
    // 对同一上游节点的重复 TLS 握手（显著降低 P99 延迟与 CPU 开销）。
    // 与 `fingerprint::FingerprintHardener::apply_chrome_profile` 正交：后者只改
    // 客户端 TLS 指纹头，不影响连接层复用。
    peer.options.idle_timeout = Some(Duration::from_secs(60));
    FingerprintHardener::apply_chrome_profile(&mut peer);
    Ok(Box::new(peer))
}

/// P2：桥出站目标 URL。
///
/// - absolute-form（含 scheme＋authority，原样透传；https 由 reqwest 在隧道内建 TLS）；
/// - origin-form 按 Host 头拼 `http://`（本网关上游现全 plain-HTTP，TLS 指纹仍 out）；
/// - 无 Host 即 None（调用方按失败计，不 panic）。
pub(super) fn bridge_url_for(uri: &http::Uri, host: Option<&str>) -> Option<String> {
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
