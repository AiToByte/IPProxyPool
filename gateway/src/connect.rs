//! HTTP/1.1 CONNECT authority 解析、独立 listener 应用和隧道转发。
//!
//! 本路径是可选的独立 Pingora ServerApp，不改变普通 ProxyHttp 端口。只支持
//! HTTP/1.1 CONNECT 到现有 HTTP/SOCKS 上游，不做 H2/H3、MITM 或上游 TLS proxy。

use crate::metrics::MetricsRegistry;
use crate::model::{EgressProto, RoutingSpec};
use crate::router::RouterEngine;
use crate::tenant::{TenantAccount, TenantManager};
use async_trait::async_trait;
use base64::{engine::general_purpose::STANDARD as B64, Engine};
use pingora_core::apps::ServerApp;
use pingora_core::protocols::Stream;
use pingora_core::server::ShutdownWatch;
use std::pin::Pin;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::task::{Context, Poll};
use std::time::Duration;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

const MAX_CONNECT_HEADER_BYTES: usize = 16 * 1024;
const CONNECT_IO_TIMEOUT: Duration = Duration::from_secs(10);
/// 建链预算与隧道空闲期限分离；活跃隧道没有十秒总寿命限制。
const CONNECT_IDLE_TIMEOUT: Duration = Duration::from_secs(60);
/// 单条 CONNECT 隧道最大绝对寿命，防止持续低速流量永久占用并发槽。
const CONNECT_MAX_LIFETIME: Duration = Duration::from_secs(3600);
/// 默认独立 CONNECT listener 并发上限。
#[allow(dead_code)]
const DEFAULT_MAX_CONCURRENT: usize = 256;

/// 已验证的 CONNECT 目标。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConnectTarget {
    /// 域名、IPv4 或去掉方括号的 IPv6 字面量。
    pub host: String,
    /// 目标端口，0 不允许。
    pub port: u16,
}

/// 解析 RFC 9112 authority-form；拒绝 URI、路径、userinfo 和未加括号的 IPv6。
pub fn parse_connect_authority(value: &str) -> Result<ConnectTarget, &'static str> {
    if value.is_empty()
        || value.trim() != value
        || value.bytes().any(|byte| byte.is_ascii_control())
        || value.contains(['/', '?', '#', '@'])
    {
        return Err(
            "CONNECT target must be an authority without path, query, fragment, or userinfo",
        );
    }

    let (host, port_text) = if let Some(rest) = value.strip_prefix('[') {
        let end = rest
            .find(']')
            .ok_or("CONNECT IPv6 target has no closing bracket")?;
        let host = &rest[..end];
        let port = rest[end + 1..]
            .strip_prefix(':')
            .ok_or("CONNECT IPv6 target must include a port")?;
        if host.parse::<std::net::Ipv6Addr>().is_err() {
            return Err("CONNECT bracketed host must be a valid IPv6 address");
        }
        (host, port)
    } else {
        let (host, port) = value
            .rsplit_once(':')
            .ok_or("CONNECT target must include host and port")?;
        if host.is_empty() || host.contains(':') {
            return Err("CONNECT IPv6 targets must use brackets");
        }
        (host, port)
    };
    if host.is_empty() || port_text.is_empty() || host.contains(['[', ']']) {
        return Err("CONNECT target host or port is empty");
    }
    let port = port_text
        .parse::<u16>()
        .map_err(|_| "CONNECT target port must be in 1..=65535")?;
    if port == 0 {
        return Err("CONNECT target port must be in 1..=65535");
    }
    if host
        .bytes()
        .any(|byte| byte.is_ascii_whitespace() || byte.is_ascii_control())
    {
        return Err("CONNECT target host contains whitespace or control characters");
    }
    Ok(ConnectTarget {
        host: host.to_string(),
        port,
    })
}

/// HTTP/1.1 CONNECT 头部解析结果；头后字节必须原样交给隧道。
#[derive(Debug, PartialEq, Eq)]
struct ConnectRequest {
    target: ConnectTarget,
    api_key: Option<String>,
    country: Option<String>,
    tier: Option<String>,
    session_id: Option<String>,
    proto: Option<String>,
    prefetched: Vec<u8>,
}

/// 解析完整 CONNECT 请求头，并保留同一 TCP 读取中多带出的隧道数据。
fn parse_connect_request(wire: &[u8]) -> Result<ConnectRequest, &'static str> {
    let header_end = wire
        .windows(4)
        .position(|window| window == b"\r\n\r\n")
        .ok_or("incomplete CONNECT header")?;
    let header = std::str::from_utf8(&wire[..header_end]).map_err(|_| "invalid header encoding")?;
    let mut lines = header.split("\r\n");
    let request_line = lines.next().ok_or("missing request line")?;
    let mut parts = request_line.split(' ');
    if parts.next() != Some("CONNECT") {
        return Err("only CONNECT is supported");
    }
    let authority = parts.next().ok_or("missing CONNECT authority")?;
    if parts.next() != Some("HTTP/1.1") || parts.next().is_some() {
        return Err("only HTTP/1.1 CONNECT is supported");
    }
    let target = crate::gateway::parse_connect_target(authority)?;
    let mut api_key = None;
    let mut country = None;
    let mut tier = None;
    let mut session_id = None;
    let mut proto = None;
    for line in lines {
        let (name, value) = line.split_once(':').ok_or("malformed CONNECT header")?;
        if name.parse::<http::header::HeaderName>().is_err() {
            return Err("malformed CONNECT header name");
        }
        // 仅剥离 HTTP OWS；禁止先 trim 掉非法控制字符后再校验。
        let value = value.trim_matches([' ', '\t']);
        if value.bytes().any(|b| b.is_ascii_control() && b != b'\t') {
            return Err("malformed CONNECT header value");
        }
        let slot = if name.eq_ignore_ascii_case("content-length")
            || name.eq_ignore_ascii_case("transfer-encoding")
            || name.eq_ignore_ascii_case("expect")
        {
            return Err("CONNECT request framing headers are not supported");
        } else if name.eq_ignore_ascii_case("x-api-key") {
            &mut api_key
        } else if name.eq_ignore_ascii_case("x-proxy-country") {
            &mut country
        } else if name.eq_ignore_ascii_case("x-proxy-tier") {
            &mut tier
        } else if name.eq_ignore_ascii_case("x-proxy-session") {
            &mut session_id
        } else if name.eq_ignore_ascii_case("x-proxy-proto") {
            &mut proto
        } else {
            continue;
        };
        if slot.replace(value.to_string()).is_some() {
            return Err("duplicate CONNECT routing/auth header");
        }
    }
    Ok(ConnectRequest {
        target,
        api_key,
        country,
        tier,
        session_id,
        proto,
        prefetched: wire[header_end + 4..].to_vec(),
    })
}

/// 每次成功写入即时累计；计数器归结算守卫持有，取消复制 future 不丢用量。
#[derive(Default)]
struct RelayUsage {
    to_upstream: AtomicU64,
    to_downstream: AtomicU64,
}

impl RelayUsage {
    fn counts(&self) -> (u64, u64) {
        (
            self.to_upstream.load(Ordering::Relaxed),
            self.to_downstream.load(Ordering::Relaxed),
        )
    }

    fn total(&self) -> u64 {
        let (up, down) = self.counts();
        up.saturating_add(down)
    }
}

/// 透传 AsyncRead/Write，记录成功写入并通知空闲监测；不将 HTTP 握手算作隧道用量。
struct MeteredIo<'a, T> {
    inner: &'a mut T,
    metrics: &'a MetricsRegistry,
    bytes: &'a AtomicU64,
    activity: &'a tokio::sync::watch::Sender<tokio::time::Instant>,
    direction: &'static str,
}

impl<T: AsyncRead + Unpin> AsyncRead for MeteredIo<'_, T> {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut tokio::io::ReadBuf<'_>,
    ) -> Poll<std::io::Result<()>> {
        Pin::new(&mut *self.inner).poll_read(cx, buf)
    }
}

impl<T: AsyncWrite + Unpin> AsyncWrite for MeteredIo<'_, T> {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<std::io::Result<usize>> {
        let result = Pin::new(&mut *self.inner).poll_write(cx, buf);
        if let Poll::Ready(Ok(n)) = result {
            if n > 0 {
                self.bytes.fetch_add(n as u64, Ordering::Relaxed);
                self.metrics.connect_bytes_written(self.direction, n as u64);
                self.activity.send_replace(tokio::time::Instant::now());
            }
        }
        result
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        Pin::new(&mut *self.inner).poll_flush(cx)
    }

    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        Pin::new(&mut *self.inner).poll_shutdown(cx)
    }
}

/// 预读写入与双向复制并行运行，避免双方预读均超过缓冲容量时顺序写入互锁。
async fn relay_metered<D, U>(
    downstream: &mut D,
    upstream: &mut U,
    downstream_prefetched: &[u8],
    upstream_prefetched: &[u8],
    usage: &RelayUsage,
    metrics: &MetricsRegistry,
    idle: Duration,
) -> std::io::Result<(u64, u64)>
where
    D: AsyncRead + AsyncWrite + Unpin,
    U: AsyncRead + AsyncWrite + Unpin,
{
    let (activity, mut changes) = tokio::sync::watch::channel(tokio::time::Instant::now());
    let mut downstream = MeteredIo {
        inner: downstream,
        metrics,
        bytes: &usage.to_downstream,
        activity: &activity,
        direction: "downstream",
    };
    let mut upstream = MeteredIo {
        inner: upstream,
        metrics,
        bytes: &usage.to_upstream,
        activity: &activity,
        direction: "upstream",
    };
    let (mut dr, mut dw) = tokio::io::split(&mut downstream);
    let (mut ur, mut uw) = tokio::io::split(&mut upstream);
    let copy = async {
        tokio::try_join!(
            copy_direction(&mut dr, &mut uw, downstream_prefetched),
            copy_direction(&mut ur, &mut dw, upstream_prefetched),
        )?;
        Ok(usage.counts())
    };
    tokio::pin!(copy);
    loop {
        let deadline = *changes.borrow_and_update() + idle;
        tokio::select! {
            biased;
            result = &mut copy => return result,
            _ = changes.changed() => {},
            _ = tokio::time::sleep_until(deadline) => {
                // 与最后一次写入并发触发时再核实，避免期限边界误断活跃连接。
                if changes.borrow().elapsed() >= idle {
                    return Err(std::io::Error::new(std::io::ErrorKind::TimedOut, "CONNECT tunnel idle"));
                }
            },
        }
    }
}

/// 半关闭只关闭对端写半部，另一方向仍能发送尾部数据；所有 flush 受空闲期限约束。
async fn copy_direction<R, W>(reader: &mut R, writer: &mut W, prefix: &[u8]) -> std::io::Result<()>
where
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin,
{
    writer.write_all(prefix).await?;
    writer.flush().await?;
    tokio::io::copy(reader, writer).await?;
    writer.shutdown().await
}

/// 无租户依赖的可复用 relay core；与生产路径共用相同预读、半关闭及空闲机制。
#[cfg(test)]
async fn relay_prefetched<D, U>(
    downstream: &mut D,
    upstream: &mut U,
    dp: &[u8],
    up: &[u8],
) -> std::io::Result<(u64, u64)>
where
    D: AsyncRead + AsyncWrite + Unpin,
    U: AsyncRead + AsyncWrite + Unpin,
{
    relay_metered(
        downstream,
        upstream,
        dp,
        up,
        &RelayUsage::default(),
        &MetricsRegistry::new(),
        CONNECT_IDLE_TIMEOUT,
    )
    .await
}

/// CONNECT listener 槽位与活动连接指标绑定，覆盖认证前失败和 future 取消。
struct AdmissionGuard {
    _permit: tokio::sync::OwnedSemaphorePermit,
    metrics: Arc<MetricsRegistry>,
}

impl Drop for AdmissionGuard {
    fn drop(&mut self) {
        self.metrics.connect_admission_released();
    }
}

/// 认证后的租户槽位与最终字节结算守卫。
///
/// 隧道生命周期指标由 `TunnelGuard` 单独负责：认证、选路或上游握手失败
/// 不能被记作“已建立隧道关闭”。
struct SettlementGuard<'a> {
    manager: &'a TenantManager,
    account: Arc<TenantAccount>,
    tier: String,
    usage: RelayUsage,
}

impl<'a> SettlementGuard<'a> {
    fn new(
        manager: &'a TenantManager,
        account: Arc<TenantAccount>,
        tier: impl Into<String>,
    ) -> Self {
        Self {
            manager,
            account,
            tier: tier.into(),
            usage: RelayUsage::default(),
        }
    }

    fn set_tier(&mut self, tier: &str) {
        self.tier = tier.to_string();
    }
}

impl Drop for SettlementGuard<'_> {
    fn drop(&mut self) {
        self.manager
            .release_and_meter(&self.account, self.usage.total(), &self.tier);
    }
}

struct TunnelGuard<'a> {
    metrics: &'a MetricsRegistry,
    started: tokio::time::Instant,
}

impl Drop for TunnelGuard<'_> {
    fn drop(&mut self) {
        self.metrics.connect_closed(
            "closed",
            self.started.elapsed().as_millis().min(u64::MAX as u128) as u64,
        );
    }
}

/// 可选启用的独立 HTTP/1.1 CONNECT 服务，不改变普通 ProxyHttp 请求处理路径。
pub struct ConnectServerApp {
    max_concurrent: Arc<tokio::sync::Semaphore>,
    router: Arc<RouterEngine>,
    tenants: Arc<TenantManager>,
    metrics: Arc<MetricsRegistry>,
}

impl ConnectServerApp {
    #[allow(dead_code)]
    pub fn new(router: Arc<RouterEngine>, tenants: Arc<TenantManager>) -> Self {
        Self::with_max_concurrent_and_metrics(
            router,
            tenants,
            DEFAULT_MAX_CONCURRENT as u32,
            Arc::new(MetricsRegistry::new()),
        )
    }

    /// 生产入口注入共享指标；测试构造器保留默认本地注册表。
    pub fn with_max_concurrent_and_metrics(
        router: Arc<RouterEngine>,
        tenants: Arc<TenantManager>,
        max_concurrent: u32,
        metrics: Arc<MetricsRegistry>,
    ) -> Self {
        Self {
            max_concurrent: Arc::new(tokio::sync::Semaphore::new(max_concurrent.max(1) as usize)),
            router,
            tenants,
            metrics,
        }
    }

    /// 构造独立 listener；全局连接上限覆盖未认证请求，避免只依赖租户限流。
    #[allow(dead_code)]
    pub fn with_max_concurrent(
        router: Arc<RouterEngine>,
        tenants: Arc<TenantManager>,
        max_concurrent: u32,
    ) -> Self {
        Self::with_max_concurrent_and_metrics(
            router,
            tenants,
            max_concurrent,
            Arc::new(MetricsRegistry::new()),
        )
    }

    async fn serve_connection<D>(&self, mut downstream: D) -> Option<Stream>
    where
        D: AsyncRead + AsyncWrite + Unpin + Send,
    {
        // 连接 permit 从读头开始持有，覆盖未认证阶段及整个隧道寿命。
        let _admission = match self.max_concurrent.clone().try_acquire_owned() {
            Ok(permit) => {
                self.metrics.connect_admission_acquired(true);
                AdmissionGuard {
                    _permit: permit,
                    metrics: Arc::clone(&self.metrics),
                }
            }
            Err(_) => {
                self.metrics.connect_admission_acquired(false);
                let _ = write_connect_error(&mut downstream, 503).await;
                return None;
            }
        };
        let request =
            match tokio::time::timeout(CONNECT_IO_TIMEOUT, read_connect_request(&mut downstream))
                .await
            {
                Ok(Ok(request)) => request,
                Ok(Err((status, message))) => {
                    log::debug!("[CONNECT] rejected request: {message}");
                    let _ = write_connect_error(&mut downstream, status).await;
                    return None;
                }
                Err(_) => {
                    let _ = write_connect_error(&mut downstream, 408).await;
                    return None;
                }
            };

        let Some(api_key) = request.api_key.as_deref() else {
            let _ = write_connect_error(&mut downstream, 403).await;
            return None;
        };
        let account = match self.tenants.authenticate_and_throttle(api_key) {
            Ok(account) => account,
            Err(reason) => {
                let status = if reason.contains("Rate limit") || reason.contains("concurrency") {
                    429
                } else if reason.contains("balance") {
                    402
                } else {
                    403
                };
                let _ = write_connect_error(&mut downstream, status).await;
                return None;
            }
        };
        // 从这里开始槽位由守卫拥有，任何 early return 或 future 取消都不会泄漏。
        let mut settlement = SettlementGuard::new(&self.tenants, account, "datacenter");
        let spec = RoutingSpec {
            country: request.country,
            session_id: request
                .session_id
                .map(|session| format!("{}:{session}", settlement.account.tenant_id)),
            tier: request.tier,
            target_domain: request.target.host.clone(),
            proto: request.proto.as_deref().and_then(EgressProto::from_token),
        };
        let Some(node) = self.router.select_node(&spec) else {
            let _ = write_connect_error(&mut downstream, 503).await;
            return None;
        };
        settlement.set_tier(&node.tier);

        let upstream_result = tokio::time::timeout(CONNECT_IO_TIMEOUT, async {
            match node.proto {
                EgressProto::Http => connect_http_upstream(&node, &request.target).await,
                EgressProto::Socks4 | EgressProto::Socks5 => crate::socks_handshake::establish(
                    &node.ip,
                    node.port,
                    node.username.as_deref(),
                    node.password.as_deref(),
                    node.proto,
                    &request.target.host,
                    request.target.port,
                )
                .await
                .map(|stream| (stream, Vec::new()))
                .map_err(|_| (502, "SOCKS upstream CONNECT failed")),
            }
        })
        .await;
        let (mut upstream, upstream_prefetched) = match upstream_result {
            Ok(Ok(result)) => result,
            Ok(Err((status, reason))) => {
                log::debug!("[CONNECT] upstream {} failed: {reason}", node.addr);
                let _ = write_connect_error(&mut downstream, status).await;
                return None;
            }
            Err(_) => {
                let _ = write_connect_error(&mut downstream, 502).await;
                return None;
            }
        };

        // 只有上游代理已接受 CONNECT，才向客户端发送 200。
        if !matches!(
            tokio::time::timeout(CONNECT_IO_TIMEOUT, async {
                downstream
                    .write_all(b"HTTP/1.1 200 Connection Established\r\n\r\n")
                    .await?;
                downstream.flush().await
            })
            .await,
            Ok(Ok(()))
        ) {
            return None;
        }
        self.metrics.connect_established(match node.proto {
            EgressProto::Http => "http",
            EgressProto::Socks4 => "socks4",
            EgressProto::Socks5 => "socks5",
        });
        let _tunnel = TunnelGuard {
            metrics: &self.metrics,
            started: tokio::time::Instant::now(),
        };
        if let Err(error) = tokio::time::timeout(
            CONNECT_MAX_LIFETIME,
            relay_metered(
                &mut downstream,
                &mut upstream,
                &request.prefetched,
                &upstream_prefetched,
                &settlement.usage,
                &self.metrics,
                CONNECT_IDLE_TIMEOUT,
            ),
        )
        .await
        .unwrap_or_else(|_| {
            Err(std::io::Error::new(
                std::io::ErrorKind::TimedOut,
                "CONNECT tunnel lifetime",
            ))
        }) {
            log::debug!("[CONNECT] relay closed: {error}");
        }
        None
    }
}

#[async_trait]
impl ServerApp for ConnectServerApp {
    async fn process_new(
        self: &Arc<Self>,
        session: Stream,
        shutdown: &ShutdownWatch,
    ) -> Option<Stream> {
        let mut shutdown = shutdown.clone();
        // 取消覆盖读头、上游建链与 relay 全阶段；局部结算守卫自动结算并释放。
        tokio::select! {
            biased;
            _ = shutdown.wait_for(|stop| *stop) => None,
            result = self.serve_connection(session) => result,
        }
    }
}

/// 读取 CONNECT 请求头，任何同批读到的额外字节均作为预读隧道数据保留。
async fn read_connect_request<R: AsyncRead + Unpin>(
    stream: &mut R,
) -> Result<ConnectRequest, (u16, &'static str)> {
    let mut wire = Vec::with_capacity(1024);
    let mut chunk = [0u8; 2048];
    loop {
        let read = stream
            .read(&mut chunk)
            .await
            .map_err(|_| (400, "CONNECT header read failed"))?;
        if read == 0 {
            return Err((400, "client closed before CONNECT header completed"));
        }
        wire.extend_from_slice(&chunk[..read]);
        if let Some(header_end) = wire.windows(4).position(|window| window == b"\r\n\r\n") {
            if header_end + 4 > MAX_CONNECT_HEADER_BYTES {
                return Err((431, "CONNECT header too large"));
            }
            return parse_connect_request(&wire).map_err(|_| (400, "invalid CONNECT request"));
        }
        if wire.len() >= MAX_CONNECT_HEADER_BYTES {
            return Err((431, "CONNECT header too large"));
        }
    }
}

/// 经 HTTP 正向代理发送 CONNECT；节点 TLS 目前明确不支持，不能明文误连 443。
async fn connect_http_upstream(
    node: &crate::model::ProxyNode,
    target: &ConnectTarget,
) -> Result<(tokio::net::TcpStream, Vec<u8>), (u16, &'static str)> {
    if crate::gateway::is_tls_for_node(node) {
        return Err((502, "TLS HTTP proxy nodes are not supported by CONNECT"));
    }
    let mut upstream = tokio::net::TcpStream::connect((node.ip.as_str(), node.port))
        .await
        .map_err(|_| (502, "HTTP proxy TCP connection failed"))?;
    let authority = if target.host.contains(':') {
        format!("[{}]:{}", target.host, target.port)
    } else {
        format!("{}:{}", target.host, target.port)
    };
    let mut request = format!("CONNECT {authority} HTTP/1.1\r\nHost: {authority}\r\n");
    if let (Some(username), Some(password)) = (&node.username, &node.password) {
        let credentials = B64.encode(format!("{username}:{password}"));
        request.push_str(&format!("Proxy-Authorization: Basic {credentials}\r\n"));
    }
    request.push_str("Proxy-Connection: Keep-Alive\r\n\r\n");
    upstream
        .write_all(request.as_bytes())
        .await
        .map_err(|_| (502, "HTTP proxy CONNECT write failed"))?;

    let mut response = Vec::with_capacity(1024);
    let mut chunk = [0u8; 2048];
    let mut consumed = 0;
    loop {
        if let Some(end) = response.windows(4).position(|window| window == b"\r\n\r\n") {
            consumed += end + 4;
            if consumed > MAX_CONNECT_HEADER_BYTES {
                return Err((502, "HTTP proxy response header too large"));
            }
            let status = parse_upstream_status(&response[..end])?;
            if (200..300).contains(&status) {
                return Ok((upstream, response[end + 4..].to_vec()));
            }
            // 1xx 不是最终响应；总头部预算同时限制连续 informational 响应。
            if !(100..200).contains(&status) || status == 101 {
                return Err((502, "HTTP proxy rejected CONNECT"));
            }
            response.drain(..end + 4);
            continue;
        }
        if consumed + response.len() >= MAX_CONNECT_HEADER_BYTES {
            return Err((502, "HTTP proxy response header too large"));
        }
        let read = upstream
            .read(&mut chunk)
            .await
            .map_err(|_| (502, "HTTP proxy response read failed"))?;
        if read == 0 {
            return Err((502, "HTTP proxy closed before CONNECT response"));
        }
        response.extend_from_slice(&chunk[..read]);
    }
}

/// 校验上游状态行及字段语法；只把合法 HTTP/1.x 的三位状态码交给建链逻辑。
fn parse_upstream_status(header: &[u8]) -> Result<u16, (u16, &'static str)> {
    let text = std::str::from_utf8(header).map_err(|_| (502, "invalid HTTP proxy response"))?;
    let mut lines = text.split("\r\n");
    let mut parts = lines.next().unwrap_or_default().splitn(3, ' ');
    if !matches!(parts.next(), Some("HTTP/1.1" | "HTTP/1.0")) {
        return Err((502, "invalid HTTP proxy version"));
    }
    let code = parts.next().unwrap_or_default();
    if code.len() != 3 || !code.bytes().all(|b| b.is_ascii_digit()) || parts.next().is_none() {
        return Err((502, "invalid HTTP proxy status"));
    }
    for line in lines {
        let (name, value) = line
            .split_once(':')
            .ok_or((502, "invalid HTTP proxy header"))?;
        if name.parse::<http::header::HeaderName>().is_err()
            || http::HeaderValue::from_bytes(value.as_bytes()).is_err()
        {
            return Err((502, "invalid HTTP proxy header"));
        }
    }
    code.parse().map_err(|_| (502, "invalid HTTP proxy status"))
}

/// 失败响应统一关闭连接，错误响应后的内容不得进入隧道。
async fn write_connect_error<W: AsyncWrite + Unpin>(
    stream: &mut W,
    status: u16,
) -> std::io::Result<()> {
    let reason = match status {
        400 => "Bad Request",
        402 => "Payment Required",
        403 => "Forbidden",
        408 => "Request Timeout",
        429 => "Too Many Requests",
        431 => "Request Header Fields Too Large",
        502 => "Bad Gateway",
        _ => "Service Unavailable",
    };
    let response =
        format!("HTTP/1.1 {status} {reason}\r\nConnection: close\r\nContent-Length: 0\r\n\r\n");
    tokio::time::timeout(CONNECT_IO_TIMEOUT, async {
        stream.write_all(response.as_bytes()).await?;
        stream.flush().await
    })
    .await
    .map_err(|_| {
        std::io::Error::new(
            std::io::ErrorKind::TimedOut,
            "CONNECT error response timeout",
        )
    })?
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_domain_and_port() {
        let target = parse_connect_authority("example.com:443").expect("valid target");
        assert_eq!(target.host, "example.com");
        assert_eq!(target.port, 443);
    }

    #[test]
    fn parses_ipv4_and_bracketed_ipv6() {
        let ipv4 = parse_connect_authority("192.0.2.10:8443").expect("valid ipv4");
        assert_eq!(ipv4.host, "192.0.2.10");
        assert_eq!(ipv4.port, 8443);
        let ipv6 = parse_connect_authority("[2001:db8::10]:443").expect("valid ipv6");
        assert_eq!(ipv6.host, "2001:db8::10");
        assert_eq!(ipv6.port, 443);
    }

    #[test]
    fn rejects_missing_invalid_and_non_authority_targets() {
        for value in [
            "example.com",
            "example.com:",
            "example.com:0",
            "example.com:65536",
            "example.com:nope",
            "example.com:443:extra",
            "http://example.com:443",
            "example.com:443/path",
            "user:pass@example.com:443",
            "[2001:db8::1",
            "2001:db8::1:443",
            "[]:443",
        ] {
            assert!(
                parse_connect_authority(value).is_err(),
                "must reject {value:?}"
            );
        }
    }

    #[test]
    fn parses_http11_connect_and_preserves_prefetched_tunnel_bytes() {
        let wire = b"CONNECT example.com:443 HTTP/1.1\r\nHost: example.com:443\r\nX-API-Key: secret\r\n\r\nTLS ClientHello";
        let parsed = parse_connect_request(wire).expect("valid CONNECT");
        assert_eq!(parsed.target.host, "example.com");
        assert_eq!(parsed.target.port, 443);
        assert_eq!(parsed.api_key.as_deref(), Some("secret"));
        assert_eq!(parsed.prefetched, b"TLS ClientHello");
    }

    #[test]
    fn rejects_non_connect_http2_duplicate_auth_and_unsupported_framing() {
        for wire in [
            &b"GET / HTTP/1.1\r\nHost: example.com\r\n\r\n"[..],
            &b"CONNECT example.com:443 HTTP/2.0\r\n\r\n"[..],
            &b"CONNECT example.com:443 HTTP/1.1\r\nX-API-Key: a\r\nX-API-Key: b\r\n\r\n"[..],
            &b"CONNECT example.com:443 HTTP/1.1\r\nContent-Length: 1\r\n\r\n"[..],
            &b"CONNECT example.com:443 HTTP/1.1\r\nTransfer-Encoding: chunked\r\n\r\n"[..],
            &b"CONNECT example.com:443 HTTP/1.1\r\nExpect: 100-continue\r\n\r\n"[..],
        ] {
            assert!(parse_connect_request(wire).is_err(), "must reject {wire:?}");
        }
    }

    #[tokio::test]
    async fn relay_forwards_prefetched_and_future_bytes_both_directions() {
        let (mut downstream, mut client) = tokio::io::duplex(128);
        let (mut upstream, mut origin) = tokio::io::duplex(128);
        let relay = tokio::spawn(async move {
            relay_prefetched(
                &mut downstream,
                &mut upstream,
                b"client-early",
                b"server-early",
            )
            .await
            .expect("relay")
        });
        let mut server_bytes = [0u8; 12];
        origin
            .read_exact(&mut server_bytes)
            .await
            .expect("early bytes");
        assert_eq!(&server_bytes, b"client-early");
        let mut client_bytes = [0u8; 12];
        client
            .read_exact(&mut client_bytes)
            .await
            .expect("early bytes");
        assert_eq!(&client_bytes, b"server-early");
        client
            .write_all(b"client-later")
            .await
            .expect("client write");
        let mut server_later = [0u8; 12];
        origin
            .read_exact(&mut server_later)
            .await
            .expect("client relay");
        assert_eq!(&server_later, b"client-later");
        origin
            .write_all(b"server-later")
            .await
            .expect("server write");
        let mut client_later = [0u8; 12];
        client
            .read_exact(&mut client_later)
            .await
            .expect("server relay");
        assert_eq!(&client_later, b"server-later");
        drop(client);
        drop(origin);
        let _ = relay.await.expect("relay task");
    }

    /// 测试时观察成功转发字节总数，并与生产逻辑保持同一计数口径。
    #[tokio::test]
    async fn relay_counts_prefetched_bytes_in_usage() {
        let (mut downstream, mut client) = tokio::io::duplex(64);
        let (mut upstream, mut origin) = tokio::io::duplex(64);
        let relay = tokio::spawn(async move {
            relay_prefetched(&mut downstream, &mut upstream, b"client", b"origin")
                .await
                .expect("relay")
        });
        let mut client_bytes = [0u8; 6];
        origin
            .read_exact(&mut client_bytes)
            .await
            .expect("client prefetched");
        let mut origin_bytes = [0u8; 6];
        client
            .read_exact(&mut origin_bytes)
            .await
            .expect("origin prefetched");
        assert_eq!(&client_bytes, b"client");
        assert_eq!(&origin_bytes, b"origin");
        drop(client);
        drop(origin);
        assert_eq!(relay.await.expect("relay task"), (6, 6));
    }

    #[tokio::test]
    async fn http_proxy_connect_returns_prefetched_response_payload() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("listener");
        let port = listener.local_addr().expect("address").port();
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.expect("accept");
            let mut request = Vec::new();
            let mut byte = [0u8; 1];
            while !request.ends_with(b"\r\n\r\n") {
                stream.read_exact(&mut byte).await.expect("request");
                request.push(byte[0]);
            }
            assert!(request.starts_with(b"CONNECT example.com:443 HTTP/1.1"));
            stream
                .write_all(b"HTTP/1.1 200 Connection Established\r\n\r\nserver-early")
                .await
                .expect("response");
        });
        let node = crate::model::ProxyNode::new(
            "127.0.0.1".to_string(),
            port,
            None,
            None,
            "US".to_string(),
            "datacenter".to_string(),
            "stub".to_string(),
            100,
        );
        let (mut upstream, prefetched) = connect_http_upstream(
            &node,
            &parse_connect_authority("example.com:443").expect("target"),
        )
        .await
        .expect("upstream CONNECT");
        let mut received = prefetched;
        upstream.read_to_end(&mut received).await.unwrap();
        assert_eq!(received, b"server-early");
        server.await.expect("stub task");
    }

    #[tokio::test]
    async fn serve_connection_authenticates_then_returns_200_and_relays_prefetched_bytes() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("listener");
        let upstream_port = listener.local_addr().expect("upstream address").port();
        let upstream_server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.expect("upstream accept");
            let mut request = Vec::new();
            let mut byte = [0u8; 1];
            while !request.ends_with(b"\r\n\r\n") {
                stream
                    .read_exact(&mut byte)
                    .await
                    .expect("upstream request");
                request.push(byte[0]);
            }
            assert!(request.starts_with(b"CONNECT example.com:443 HTTP/1.1"));
            stream
                .write_all(b"HTTP/1.1 200 Connection Established\r\n\r\norigin-early")
                .await
                .expect("upstream response");
            let mut client_early = [0u8; 11];
            stream
                .read_exact(&mut client_early)
                .await
                .expect("prefetched client bytes");
            assert_eq!(&client_early, b"hello-world");
            stream
                .write_all(b"origin-later")
                .await
                .expect("tunnel reply");
            let mut later = [0u8; 1];
            let _ = stream.read(&mut later).await;
        });

        let node = crate::model::ProxyNode::new(
            "127.0.0.1".to_string(),
            upstream_port,
            None,
            None,
            "US".to_string(),
            "datacenter".to_string(),
            "http-stub".to_string(),
            100,
        );
        let router = Arc::new(RouterEngine::new(vec![node]));
        let tenants = Arc::new(TenantManager::new());
        tenants.register_tenant("tenant-a", "connect-key", 10_000, 10, 100);
        let app = Arc::new(ConnectServerApp::new(router, tenants.clone()));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("listener");
        let addr = listener.local_addr().expect("address");
        let service = tokio::spawn(async move {
            let (client, _) = listener.accept().await.expect("accept");
            app.serve_connection(Box::new(pingora_core::protocols::l4::stream::Stream::from(
                client,
            )))
            .await;
        });
        let mut client = tokio::net::TcpStream::connect(addr).await.expect("connect");
        client
            .write_all(
                b"CONNECT example.com:443 HTTP/1.1\r\nX-API-Key: connect-key\r\n\r\nhello-world",
            )
            .await
            .expect("CONNECT request");
        let mut response = Vec::new();
        let mut chunk = [0u8; 128];
        while !response.windows(4).any(|window| window == b"\r\n\r\n") {
            let n = client.read(&mut chunk).await.expect("response");
            response.extend_from_slice(&chunk[..n]);
        }
        assert!(response.starts_with(b"HTTP/1.1 200 Connection Established"));
        let body = &response[response
            .windows(4)
            .position(|window| window == b"\r\n\r\n")
            .unwrap()
            + 4..];
        let mut tunnel_bytes = body.to_vec();
        let expected = b"origin-earlyorigin-later";
        while tunnel_bytes.len() < expected.len() {
            let n = client.read(&mut chunk).await.expect("tunnel payload");
            assert_ne!(n, 0, "tunnel closed before complete payload");
            tunnel_bytes.extend_from_slice(&chunk[..n]);
        }
        assert_eq!(tunnel_bytes, expected);
        drop(client);
        service.await.expect("CONNECT service");
        upstream_server.await.expect("upstream stub");
        assert_eq!(tenants.tenant_detail("connect-key").unwrap().in_flight, 0);
    }

    #[tokio::test]
    async fn serve_connection_rejects_missing_or_invalid_api_key() {
        let router = Arc::new(RouterEngine::new(vec![]));
        let tenants = Arc::new(TenantManager::new());
        tenants.register_tenant("tenant-a", "valid", 10_000, 10, 100);
        let app = Arc::new(ConnectServerApp::new(router, tenants));
        for (header, expected) in [("", "403"), ("X-API-Key: invalid\r\n", "403")] {
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
                .await
                .expect("listener");
            let addr = listener.local_addr().expect("address");
            let app = app.clone();
            let server = tokio::spawn(async move {
                let (client, _) = listener.accept().await.expect("accept");
                app.serve_connection(Box::new(pingora_core::protocols::l4::stream::Stream::from(
                    client,
                )))
                .await;
            });
            let mut peer = tokio::net::TcpStream::connect(addr).await.expect("connect");
            peer.write_all(format!("CONNECT example.com:443 HTTP/1.1\r\n{header}\r\n").as_bytes())
                .await
                .expect("write request");
            let mut response = [0u8; 256];
            let n = peer.read(&mut response).await.expect("read error response");
            assert!(String::from_utf8_lossy(&response[..n])
                .starts_with(&format!("HTTP/1.1 {expected}")));
            server.await.expect("server task");
        }
    }

    /// 正常客户端等待 200 后才发送 TLS 字节；生产缓冲不能令握手互等。
    #[tokio::test]
    async fn buffered_listener_flushes_200_before_client_payload() {
        tokio::time::timeout(Duration::from_secs(2), async {
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let port = listener.local_addr().unwrap().port();
            let origin = tokio::spawn(async move {
                let (mut stream, _) = listener.accept().await.unwrap();
                let mut byte = [0];
                let mut header = Vec::new();
                while !header.ends_with(b"\r\n\r\n") {
                    stream.read_exact(&mut byte).await.unwrap();
                    header.push(byte[0]);
                }
                stream.write_all(b"HTTP/1.1 200 OK\r\n\r\n").await.unwrap();
                let mut payload = [0; 5];
                stream.read_exact(&mut payload).await.unwrap();
                assert_eq!(&payload, b"hello");
                stream.write_all(b"world").await.unwrap();
                stream.shutdown().await.unwrap();
            });
            let node = crate::model::ProxyNode::new(
                "127.0.0.1".into(),
                port,
                None,
                None,
                "US".into(),
                "datacenter".into(),
                "stub".into(),
                100,
            );
            let tenants = Arc::new(TenantManager::new());
            tenants.register_tenant("t", "key", 1000, 10, 100);
            let metrics = Arc::new(MetricsRegistry::new());
            let app = Arc::new(ConnectServerApp::with_max_concurrent_and_metrics(
                Arc::new(RouterEngine::new(vec![node])),
                tenants.clone(),
                256,
                metrics.clone(),
            ));
            let (server, mut client) = tokio::io::duplex(1024);
            let task = tokio::spawn(async move {
                app.serve_connection(Box::new(tokio::io::BufStream::new(server)))
                    .await;
            });
            client
                .write_all(b"CONNECT example.com:443 HTTP/1.1\r\nX-API-Key: key\r\n\r\n")
                .await
                .unwrap();
            let mut response = vec![0; b"HTTP/1.1 200 Connection Established\r\n\r\n".len()];
            client.read_exact(&mut response).await.unwrap();
            client.write_all(b"hello").await.unwrap();
            client.shutdown().await.unwrap();
            let mut payload = Vec::new();
            client.read_to_end(&mut payload).await.unwrap();
            assert_eq!(&payload, b"world");
            task.await.unwrap();
            origin.await.unwrap();
            assert_eq!(tenants.tenant_detail("key").unwrap().total_bytes, 10);
            let rendered = metrics.render();
            assert!(rendered.contains("connect_bytes_total{direction=\"upstream\"} 5"));
            assert!(rendered.contains("connect_bytes_total{direction=\"downstream\"} 5"));
            assert!(rendered.contains("connect_tunnel_active 0"));
        })
        .await
        .expect("CONNECT must not deadlock");
    }

    /// 代理返回任意成功 2xx 均建立隧道，非法状态行则必须失败。
    #[tokio::test]
    async fn upstream_status_class_and_syntax_are_checked() {
        for (response, success) in [
            (&b"HTTP/1.1 201 Created\r\n\r\n"[..], true),
            (
                &b"HTTP/1.1 100 Continue\r\n\r\nHTTP/1.1 204 No Content\r\n\r\n"[..],
                true,
            ),
            (&b"garbage 200 OK\r\n\r\n"[..], false),
            (&b"HTTP/1.1 407 Auth Required\r\n\r\n"[..], false),
        ] {
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let port = listener.local_addr().unwrap().port();
            let server = tokio::spawn(async move {
                let (mut stream, _) = listener.accept().await.unwrap();
                let mut request = Vec::new();
                let mut byte = [0];
                while !request.ends_with(b"\r\n\r\n") {
                    stream.read_exact(&mut byte).await.unwrap();
                    request.push(byte[0]);
                }
                stream.write_all(response).await.unwrap();
            });
            let node = crate::model::ProxyNode::new(
                "127.0.0.1".into(),
                port,
                None,
                None,
                "US".into(),
                "datacenter".into(),
                "stub".into(),
                100,
            );
            let result =
                connect_http_upstream(&node, &parse_connect_authority("example.com:443").unwrap())
                    .await;
            assert_eq!(result.is_ok(), success, "{response:?}");
            server.await.unwrap();
        }
    }

    /// 认证后的连接取消，已成功传给上游的字节仍须结算且只释放一次。
    #[tokio::test]
    async fn cancellation_settles_transferred_bytes() {
        tokio::time::timeout(Duration::from_secs(2), async {
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let port = listener.local_addr().unwrap().port();
            let (seen_tx, seen_rx) = tokio::sync::oneshot::channel();
            let origin = tokio::spawn(async move {
                let (mut stream, _) = listener.accept().await.unwrap();
                let mut header = Vec::new();
                let mut byte = [0];
                while !header.ends_with(b"\r\n\r\n") {
                    stream.read_exact(&mut byte).await.unwrap();
                    header.push(byte[0]);
                }
                stream.write_all(b"HTTP/1.1 200 OK\r\n\r\n").await.unwrap();
                let mut payload = [0; 5];
                stream.read_exact(&mut payload).await.unwrap();
                seen_tx.send(payload).unwrap();
                std::future::pending::<()>().await;
            });
            let node = crate::model::ProxyNode::new(
                "127.0.0.1".into(),
                port,
                None,
                None,
                "US".into(),
                "datacenter".into(),
                "stub".into(),
                100,
            );
            let tenants = Arc::new(TenantManager::new());
            tenants.register_tenant("t", "key", 1000, 1, 100);
            let metrics = Arc::new(MetricsRegistry::new());
            let app = ConnectServerApp::with_max_concurrent_and_metrics(
                Arc::new(RouterEngine::new(vec![node])),
                tenants.clone(),
                256,
                metrics.clone(),
            );
            let (server, mut client) = tokio::io::duplex(1024);
            let task = tokio::spawn(async move {
                app.serve_connection(server).await;
            });
            client
                .write_all(b"CONNECT example.com:443 HTTP/1.1\r\nX-API-Key: key\r\n\r\nhello")
                .await
                .unwrap();
            assert_eq!(&seen_rx.await.unwrap(), b"hello");
            task.abort();
            assert!(task.await.unwrap_err().is_cancelled());
            origin.abort();
            let detail = tenants.tenant_detail("key").unwrap();
            assert_eq!(detail.in_flight, 0);
            assert_eq!(detail.total_bytes, 5);
            assert!(detail.balance_usd < 100.0);
            assert!(metrics.render().contains("connect_tunnel_active 0"));
        })
        .await
        .unwrap();
    }

    /// ShutdownWatch 关闭连接时也必须走守卫释放；停机中的入口不得处理新隧道。
    #[tokio::test]
    async fn shutdown_cancels_pending_request() {
        let app = Arc::new(ConnectServerApp::new(
            Arc::new(RouterEngine::new(vec![])),
            Arc::new(TenantManager::new()),
        ));
        let (server, mut client) = tokio::io::duplex(128);
        let (stop, watch) = tokio::sync::watch::channel(false);
        let task = tokio::spawn(async move {
            app.process_new(Box::new(server), &watch).await;
        });
        stop.send(true).unwrap();
        tokio::time::timeout(Duration::from_millis(200), task)
            .await
            .expect("shutdown must not wait for header timeout")
            .unwrap();
        let mut byte = [0];
        assert_eq!(client.read(&mut byte).await.unwrap(), 0);
    }

    /// 两端半关闭仍保留另一方向；空闲到期保留已写字节计数。
    #[tokio::test]
    async fn relay_idle_timeout_keeps_partial_usage() {
        let (mut downstream, mut client) = tokio::io::duplex(64);
        let (mut upstream, mut origin) = tokio::io::duplex(64);
        let usage = RelayUsage::default();
        let metrics = MetricsRegistry::new();
        let result = tokio::join!(
            relay_metered(
                &mut downstream,
                &mut upstream,
                b"sent",
                b"",
                &usage,
                &metrics,
                Duration::from_millis(30)
            ),
            async {
                let mut bytes = [0; 4];
                origin.read_exact(&mut bytes).await.unwrap();
                assert_eq!(&bytes, b"sent");
                client.shutdown().await.unwrap();
            }
        )
        .0;
        assert_eq!(result.unwrap_err().kind(), std::io::ErrorKind::TimedOut);
        assert_eq!(usage.counts(), (4, 0));
    }

    /// 使用真实 Pingora Listening Service 的默认缓冲，验证接线、错误响应与停机。
    #[tokio::test]
    async fn pingora_listener_serves_and_shuts_down() {
        use pingora_core::services::Service;
        let reserve = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = reserve.local_addr().unwrap();
        drop(reserve);
        let app = ConnectServerApp::new(
            Arc::new(RouterEngine::new(vec![])),
            Arc::new(TenantManager::new()),
        );
        let mut service =
            pingora_core::services::listening::Service::new("CONNECT test".into(), app);
        service.add_tcp(&addr.to_string());
        let (stop, watch) = tokio::sync::watch::channel(false);
        let task = tokio::spawn(async move {
            service
                .start_service(
                    #[cfg(unix)]
                    None,
                    watch,
                    1,
                )
                .await;
        });
        tokio::time::timeout(Duration::from_secs(2), async {
            let mut client = loop {
                match tokio::net::TcpStream::connect(addr).await {
                    Ok(stream) => break stream,
                    Err(_) => tokio::task::yield_now().await,
                }
            };
            client
                .write_all(b"CONNECT example.com:443 HTTP/1.1\r\n\r\n")
                .await
                .unwrap();
            let mut response = Vec::new();
            client.read_to_end(&mut response).await.unwrap();
            assert!(response.starts_with(b"HTTP/1.1 403 Forbidden\r\n"));
            assert!(response.ends_with(b"\r\n\r\n"));
            stop.send(true).unwrap();
            task.await.unwrap();
        })
        .await
        .expect("Pingora listener must serve and stop");
    }

    /// 用严格协议桩验证现有 SOCKS helper，远端只选择客户端实际提供的方法。
    #[tokio::test]
    async fn socks4a_and_socks5_tunnels_relay_after_acceptance() {
        tokio::time::timeout(Duration::from_secs(3), async {
            for proto in [EgressProto::Socks4, EgressProto::Socks5] {
                let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
                let port = listener.local_addr().unwrap().port();
                let (ready, accepted) = tokio::sync::oneshot::channel();
                let (allow, proceed) = tokio::sync::oneshot::channel();
                let origin = tokio::spawn(async move {
                    let (mut stream, _) = listener.accept().await.unwrap();
                    if proto == EgressProto::Socks5 {
                        let mut greeting = [0; 3];
                        stream.read_exact(&mut greeting).await.unwrap();
                        assert_eq!(greeting, [5, 1, 0]);
                        stream.write_all(&[5, 0]).await.unwrap();
                        let mut request = [0; 18];
                        stream.read_exact(&mut request).await.unwrap();
                        assert_eq!(&request[..5], &[5, 1, 0, 3, 11]);
                        assert_eq!(&request[5..16], b"example.com");
                        assert_eq!(&request[16..], &443u16.to_be_bytes());
                    } else {
                        let mut request = [0; 21];
                        stream.read_exact(&mut request).await.unwrap();
                        assert_eq!(&request[..4], &[4, 1, 1, 187]);
                        assert_eq!(&request[4..9], &[0, 0, 0, 1, 0]);
                        assert_eq!(&request[9..], b"example.com\0");
                    }
                    ready.send(()).unwrap();
                    proceed.await.unwrap();
                    let reply: &[u8] = if proto == EgressProto::Socks5 {
                        &[5, 0, 0, 1, 0, 0, 0, 0, 0, 0]
                    } else { &[0, 0x5a, 0, 0, 0, 0, 0, 0] };
                    stream.write_all(reply).await.unwrap();
                    let mut payload = [0; 5];
                    stream.read_exact(&mut payload).await.unwrap();
                    assert_eq!(&payload, b"hello");
                    stream.write_all(b"world").await.unwrap();
                    stream.shutdown().await.unwrap();
                });
                let node = crate::model::ProxyNode::new("127.0.0.1".into(), port, None, None,
                    "US".into(), "datacenter".into(), "stub".into(), 100).with_proto(proto);
                let tenants = Arc::new(TenantManager::new());
                tenants.register_tenant("t", "key", 1000, 10, 100);
                let app = ConnectServerApp::new(Arc::new(RouterEngine::new(vec![node])), tenants.clone());
                let (server, mut client) = tokio::io::duplex(1024);
                let task = tokio::spawn(async move { app.serve_connection(server).await; });
                let token = if proto == EgressProto::Socks5 { "socks5" } else { "socks4a" };
                client.write_all(format!("CONNECT example.com:443 HTTP/1.1\r\nX-API-Key: key\r\nX-Proxy-Proto: {token}\r\n\r\nhello").as_bytes()).await.unwrap();
                accepted.await.unwrap();
                let mut byte = [0];
                {
                    let read = client.read(&mut byte);
                    tokio::pin!(read);
                    assert!(futures::poll!(read.as_mut()).is_pending(), "200 must wait for SOCKS acceptance");
                }
                allow.send(()).unwrap();
                client.shutdown().await.unwrap();
                let mut response = Vec::new();
                client.read_to_end(&mut response).await.unwrap();
                assert_eq!(response, b"HTTP/1.1 200 Connection Established\r\n\r\nworld");
                task.await.unwrap();
                origin.await.unwrap();
                let detail = tenants.tenant_detail("key").unwrap();
                assert_eq!(detail.total_bytes, 10);
                assert_eq!(detail.in_flight, 0);
            }
        }).await.unwrap();
    }

    /// 请求头额度只计头部，不能把同次读取的隧道预读算成超限。
    #[tokio::test]
    async fn header_limits_and_split_delimiter() {
        for size in [MAX_CONNECT_HEADER_BYTES, MAX_CONNECT_HEADER_BYTES + 1] {
            let start = b"CONNECT example.com:443 HTTP/1.1\r\nX-Padding: ";
            let mut wire = start.to_vec();
            wire.resize(size - 4, b'a');
            wire.extend_from_slice(b"\r\n\r\n");
            wire.extend_from_slice(b"payload");
            let mut reader = wire.as_slice();
            let result = read_connect_request(&mut reader).await;
            if size == MAX_CONNECT_HEADER_BYTES {
                let mut payload = result.unwrap().prefetched;
                payload.extend_from_slice(reader);
                assert_eq!(payload, b"payload");
            } else {
                assert_eq!(result.unwrap_err().0, 431);
            }
        }
        let (mut server, mut client) = tokio::io::duplex(128);
        let task = tokio::spawn(async move {
            client
                .write_all(b"CONNECT example.com:443 HTTP/1.1\r\nX-API-Key: k\r\n\r")
                .await
                .unwrap();
            tokio::task::yield_now().await;
            client.write_all(b"\npayload").await.unwrap();
        });
        assert_eq!(
            read_connect_request(&mut server).await.unwrap().prefetched,
            b"payload"
        );
        task.await.unwrap();
    }

    /// 身份失败、空池以及上游建链失败均不会产生隧道用量或遗留并发槽。
    #[tokio::test]
    async fn admission_and_upstream_failures_release_slots() {
        for (mode, status) in [
            ("disabled", 403),
            ("balance", 402),
            ("concurrency", 429),
            ("empty", 503),
            ("tls", 502),
            ("missing_key", 403),
        ] {
            let tenants = Arc::new(TenantManager::new());
            tenants.register_tenant(
                "t",
                "key",
                1000,
                if mode == "concurrency" { 0 } else { 1 },
                100,
            );
            if mode == "disabled" {
                tenants.set_active("key", false);
            }
            if mode == "balance" {
                tenants.set_balance("key", 0.0);
            }
            let nodes = if mode == "tls" {
                vec![crate::model::ProxyNode::new(
                    "127.0.0.1".into(),
                    443,
                    None,
                    None,
                    "US".into(),
                    "datacenter".into(),
                    "tls".into(),
                    100,
                )]
            } else {
                vec![]
            };
            let metrics = Arc::new(MetricsRegistry::new());
            let app = ConnectServerApp::with_max_concurrent_and_metrics(
                Arc::new(RouterEngine::new(nodes)),
                tenants.clone(),
                256,
                metrics.clone(),
            );
            let (server, mut client) = tokio::io::duplex(1024);
            let task = tokio::spawn(async move {
                app.serve_connection(server).await;
            });
            let request = if mode == "missing_key" {
                b"CONNECT example.com:443 HTTP/1.1\r\n\r\n".as_slice()
            } else {
                b"CONNECT example.com:443 HTTP/1.1\r\nX-API-Key: key\r\n\r\n".as_slice()
            };
            client.write_all(request).await.unwrap();
            let mut response = Vec::new();
            client.read_to_end(&mut response).await.unwrap();
            assert!(
                response.starts_with(format!("HTTP/1.1 {status} ").as_bytes()),
                "{mode}: {response:?}"
            );
            task.await.unwrap();
            let detail = tenants.tenant_detail("key").unwrap();
            assert_eq!(detail.in_flight, 0);
            assert_eq!(detail.total_bytes, 0);
            assert!(
                metrics.render().contains("connect_tunnel_active 0"),
                "{mode} must release listener admission accounting"
            );
            assert!(
                metrics.render().contains("connect_tunnel_closed_total 0"),
                "{mode} must not count an unestablished CONNECT as a closed tunnel"
            );
            assert!(
                metrics
                    .render()
                    .contains("connect_tunnel_duration_ms_sum 0"),
                "{mode} must not add handshake time to tunnel duration"
            );
        }
    }

    /// 真实十秒预算验证 408/502，外层 watchdog 防止实现失效后测试永久悬挂。
    #[tokio::test]
    async fn request_and_upstream_handshake_timeouts_close_and_release() {
        tokio::time::timeout(CONNECT_IO_TIMEOUT + Duration::from_secs(3), async {
            let request_timeout = async {
                let app = ConnectServerApp::new(
                    Arc::new(RouterEngine::new(vec![])),
                    Arc::new(TenantManager::new()),
                );
                let (server, mut client) = tokio::io::duplex(1024);
                let task = tokio::spawn(async move {
                    app.serve_connection(server).await;
                });
                client
                    .write_all(b"CONNECT example.com:443 HTTP/1.1\r\n")
                    .await
                    .unwrap();
                let mut response = Vec::new();
                client.read_to_end(&mut response).await.unwrap();
                assert!(response.starts_with(b"HTTP/1.1 408 "));
                task.await.unwrap();
            };
            let upstream_timeout = async {
                let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
                let port = listener.local_addr().unwrap().port();
                let origin = tokio::spawn(async move {
                    let (_stream, _) = listener.accept().await.unwrap();
                    std::future::pending::<()>().await;
                });
                let node = crate::model::ProxyNode::new(
                    "127.0.0.1".into(),
                    port,
                    None,
                    None,
                    "US".into(),
                    "datacenter".into(),
                    "stall".into(),
                    100,
                );
                let tenants = Arc::new(TenantManager::new());
                tenants.register_tenant("t", "key", 1000, 1, 100);
                let app =
                    ConnectServerApp::new(Arc::new(RouterEngine::new(vec![node])), tenants.clone());
                let (server, mut client) = tokio::io::duplex(1024);
                let task = tokio::spawn(async move {
                    app.serve_connection(server).await;
                });
                client
                    .write_all(b"CONNECT example.com:443 HTTP/1.1\r\nX-API-Key: key\r\n\r\n")
                    .await
                    .unwrap();
                let mut response = Vec::new();
                client.read_to_end(&mut response).await.unwrap();
                assert!(response.starts_with(b"HTTP/1.1 502 "));
                task.await.unwrap();
                origin.abort();
                assert_eq!(tenants.tenant_detail("key").unwrap().in_flight, 0);
                assert_eq!(tenants.tenant_detail("key").unwrap().total_bytes, 0);
            };
            tokio::join!(request_timeout, upstream_timeout);
        })
        .await
        .unwrap();
    }

    /// 活跃隧道应可超过握手预算；客户端半关闭后仍必须收到上游尾包。
    #[tokio::test]
    async fn active_tunnel_outlives_handshake_budget() {
        tokio::time::timeout(Duration::from_secs(15), async {
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let port = listener.local_addr().unwrap().port();
            let origin = tokio::spawn(async move {
                let (mut stream, _) = listener.accept().await.unwrap();
                let mut byte = [0];
                let mut header = Vec::new();
                while !header.ends_with(b"\r\n\r\n") {
                    stream.read_exact(&mut byte).await.unwrap();
                    header.push(byte[0]);
                }
                stream.write_all(b"HTTP/1.1 200 OK\r\n\r\n").await.unwrap();
                for _ in 0..12 {
                    stream.read_exact(&mut byte).await.unwrap();
                    stream.write_all(&byte).await.unwrap();
                }
                assert_eq!(stream.read(&mut byte).await.unwrap(), 0);
                stream.write_all(b"tail").await.unwrap();
                stream.shutdown().await.unwrap();
            });
            let node = crate::model::ProxyNode::new(
                "127.0.0.1".into(),
                port,
                None,
                None,
                "US".into(),
                "datacenter".into(),
                "echo".into(),
                100,
            );
            let tenants = Arc::new(TenantManager::new());
            tenants.register_tenant("t", "key", 1000, 1, 100);
            let app =
                ConnectServerApp::new(Arc::new(RouterEngine::new(vec![node])), tenants.clone());
            let (server, mut client) = tokio::io::duplex(1024);
            let task = tokio::spawn(async move {
                app.serve_connection(server).await;
            });
            client
                .write_all(b"CONNECT example.com:443 HTTP/1.1\r\nX-API-Key: key\r\n\r\n")
                .await
                .unwrap();
            let mut response = vec![0; b"HTTP/1.1 200 Connection Established\r\n\r\n".len()];
            client.read_exact(&mut response).await.unwrap();
            let mut ticker = tokio::time::interval(Duration::from_secs(1));
            for _ in 0..12 {
                ticker.tick().await;
                client.write_all(b"x").await.unwrap();
                let mut byte = [0];
                client.read_exact(&mut byte).await.unwrap();
                assert_eq!(byte, [b'x']);
            }
            client.shutdown().await.unwrap();
            let mut tail = Vec::new();
            client.read_to_end(&mut tail).await.unwrap();
            assert_eq!(tail, b"tail");
            task.await.unwrap();
            origin.await.unwrap();
            assert_eq!(tenants.tenant_detail("key").unwrap().total_bytes, 28);
        })
        .await
        .unwrap();
    }

    /// 成功写入后下一次写入失败，计数器不能因最终 Err 被清零。
    #[tokio::test]
    async fn relay_io_error_retains_successful_write_count() {
        struct FailAfterThree {
            written: usize,
        }
        impl AsyncRead for FailAfterThree {
            fn poll_read(
                self: Pin<&mut Self>,
                _cx: &mut Context<'_>,
                _buf: &mut tokio::io::ReadBuf<'_>,
            ) -> Poll<std::io::Result<()>> {
                Poll::Pending
            }
        }
        impl AsyncWrite for FailAfterThree {
            fn poll_write(
                mut self: Pin<&mut Self>,
                _cx: &mut Context<'_>,
                buf: &[u8],
            ) -> Poll<std::io::Result<usize>> {
                if self.written == 3 {
                    return Poll::Ready(Err(std::io::Error::new(
                        std::io::ErrorKind::BrokenPipe,
                        "injected write error",
                    )));
                }
                let n = buf.len().min(3 - self.written);
                self.written += n;
                Poll::Ready(Ok(n))
            }
            fn poll_flush(
                self: Pin<&mut Self>,
                _cx: &mut Context<'_>,
            ) -> Poll<std::io::Result<()>> {
                Poll::Ready(Ok(()))
            }
            fn poll_shutdown(
                self: Pin<&mut Self>,
                _cx: &mut Context<'_>,
            ) -> Poll<std::io::Result<()>> {
                Poll::Ready(Ok(()))
            }
        }
        let (mut downstream, _client) = tokio::io::duplex(64);
        let mut upstream = FailAfterThree { written: 0 };
        let usage = RelayUsage::default();
        let metrics = MetricsRegistry::new();
        let result = relay_metered(
            &mut downstream,
            &mut upstream,
            b"hello",
            b"",
            &usage,
            &metrics,
            Duration::from_secs(1),
        )
        .await;
        assert_eq!(result.unwrap_err().kind(), std::io::ErrorKind::BrokenPipe);
        assert_eq!(usage.counts(), (3, 0));
    }

    #[tokio::test]
    async fn tls_http_proxy_nodes_are_rejected_without_connecting() {
        let node = crate::model::ProxyNode::new(
            "127.0.0.1".to_string(),
            443,
            None,
            None,
            "US".to_string(),
            "datacenter".to_string(),
            "tls".to_string(),
            100,
        );
        let error = connect_http_upstream(
            &node,
            &parse_connect_authority("example.com:443").expect("target"),
        )
        .await
        .expect_err("TLS upstream unsupported");
        assert_eq!(error.0, 502);
        assert!(error.1.contains("TLS"));
    }
}
