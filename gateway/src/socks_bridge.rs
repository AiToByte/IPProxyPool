//! P2 SOCKS 数据面翻译桥：显式 socks 请求的出站执行器。
//!
//! 网关 `proxy_upstream_filter` 短路分支调用（Pingora 0.6 无 SOCKS connector，
//! 不 fork——桥用 reqwest `socks` feature 出站，合成响应回下游）。
//! Client 按 `node.addr` 缓存复用（沿 prober OPT-5 模式＋TTL 淘汰；`Client` 克隆廉价内部 `Arc`）。

use crate::janitor;
use crate::model::{EgressProto, ProxyNode};
use crate::prober::CanaryProber;
use dashmap::DashMap;
use std::time::{Duration, Instant};

/// 逐跳头过滤表（小写比较；合成响应不得透传上游逐跳头，下游复用语义自己定）。
pub(crate) const HOP_HEADERS: [&str; 8] = [
    "connection",
    "transfer-encoding",
    "keep-alive",
    "proxy-authenticate",
    "proxy-authorization",
    "trailer",
    "upgrade",
    "te",
];

pub(crate) fn is_hop_header(name: &str) -> bool {
    HOP_HEADERS.contains(&name.to_ascii_lowercase().as_str())
}

/// A11：桥出站允许透传的下游头白名单（小写比较；逐跳头另由 `is_hop_header` 拦截，不在此列）。
pub(crate) const BRIDGE_FORWARD_HEADERS: [&str; 17] = [
    "host",
    "user-agent",
    "accept",
    "accept-language",
    "accept-encoding",
    "content-type",
    "content-length",
    "referer",
    "origin",
    "cookie",
    "authorization",
    "range",
    "if-modified-since",
    "if-none-match",
    "cache-control",
    "pragma",
    "x-requested-with",
];

/// A11：头名是否允许出站（白名单＋`x-` 业务扩展；`x-proxy-*` 网关控制头永不透传）。
pub(crate) fn is_allowed_bridge_header(name: &str) -> bool {
    let lower = name.to_ascii_lowercase();
    if lower.starts_with("x-proxy-") {
        return false;
    }
    if BRIDGE_FORWARD_HEADERS.contains(&lower.as_str()) {
        return true;
    }
    // 业务自定义头放行 `x-` 前缀（已过逐跳过滤＋HeaderName/Value 校验）。
    lower.starts_with("x-")
}

/// A11：出站头清洗（逐跳过滤＋白名单＋HeaderName/HeaderValue 校验；非法跳过记 debug）。
///
/// - 桥内无 metrics 句柄，按任务口径走 `debug!` 日志（调用方失败另有 bridge_errors 计数）；
/// - 返回可安全进 reqwest 的子集（调用方直接透传，不再二次校验）。
pub(crate) fn sanitize_bridge_headers(headers: &[(String, String)]) -> Vec<(String, String)> {
    let mut out = Vec::with_capacity(headers.len());
    for (k, v) in headers {
        if is_hop_header(k) {
            log::debug!("[SocksBridge] skip hop header {k}");
            continue;
        }
        if !is_allowed_bridge_header(k) {
            log::debug!("[SocksBridge] skip non-allowlisted header {k}");
            continue;
        }
        if k.parse::<http::HeaderName>().is_err() {
            log::debug!("[SocksBridge] skip illegal header name {k:?}");
            continue;
        }
        if v.parse::<http::HeaderValue>().is_err() {
            log::debug!("[SocksBridge] skip illegal header value for {k}");
            continue;
        }
        out.push((k.clone(), v.clone()));
    }
    out
}

/// 桥入参（网关 filter 把下游请求翻译成此形态；body 由 `read_request_body` 来）。
pub struct BridgeRequest {
    pub method: String,
    pub url: String,
    pub headers: Vec<(String, String)>,
    pub body: Option<bytes::Bytes>,
}

/// 桥出参（网关 filter 合成回下游；headers 已过滤逐跳头）。
pub struct BridgeResponse {
    pub status: u16,
    pub headers: Vec<(String, String)>,
    pub body: bytes::Bytes,
}

/// E6：单 chunk 接纳判定（纯函数；`true`＝接纳后仍在上限内，含等于边界）。
/// fetch 在 `extend` 前调用，超限 chunk 不进缓冲即早停（大文件不 transient 超占）；
/// 饱和加防极端长度溢出 wrap。
pub(crate) fn within_body_cap(current: usize, incoming: usize, max_body: u64) -> bool {
    (current as u64).saturating_add(incoming as u64) <= max_body
}

/// E6：Content-Length 声明预检（纯函数；`true`＝声明即超限，可零读取早停）。
/// 上游谎报偏大只会多一次换节点重试（调用方既有语义），不污染计量
/// （`transferred_bytes` 只在成功时赋值，见 gateway `serve_via_socks`）。
pub(crate) fn content_length_over_cap(content_length: Option<u64>, max_body: u64) -> bool {
    matches!(content_length, Some(n) if n > max_body)
}

/// 某节点的出站代理 URL（socks5 走远端解析 `socks5h`；账密编码复用 prober 单份实现）。
/// Http 节点桥不接 → None（防御分支，正常走不到：选路隔离已拦）。
pub(crate) fn proxy_url_for_node(node: &ProxyNode) -> Option<String> {
    let scheme = match node.proto {
        EgressProto::Socks5 => "socks5h",
        EgressProto::Socks4 => "socks4",
        EgressProto::Http => return None,
    };
    Some(match (&node.username, &node.password) {
        (Some(u), Some(p)) => format!(
            "{scheme}://{}:{}@{}:{}",
            CanaryProber::encode_userinfo(u),
            CanaryProber::encode_userinfo(p),
            node.ip,
            node.port
        ),
        _ => format!("{scheme}://{}:{}", node.ip, node.port),
    })
}

struct CachedClient {
    client: reqwest::Client,
    last_used: Instant,
}

/// Client 闲置 TTL（沿 prober `CLIENT_IDLE_TTL` 10min；60s 滴答淘汰）。
pub const BRIDGE_IDLE_TTL: Duration = Duration::from_secs(600);

/// P2 数据面桥（`Arc` 共享给网关；`fetch` 并发安全）。
pub struct SocksBridge {
    clients: DashMap<String, CachedClient>,
    timeout: Duration,
    max_body: u64,
}

impl SocksBridge {
    pub fn new(timeout: Duration, max_body: u64) -> Self {
        Self {
            clients: DashMap::new(),
            timeout,
            max_body: max_body.max(1),
        }
    }

    /// A9：单跳出站 timeout（整轮总预算＝此值＋8s 松弛，见 gateway `socks_overall_budget`）。
    pub fn timeout(&self) -> Duration {
        self.timeout
    }

    /// E6：出站 body 上限（`new` 第二参注入；main 经 `SOCKS_MAX_BODY_BYTES` 接 env，默认 10MB）。
    /// 仅单测读取生产值（生产侧直接用字段），故 cfg(test)。
    #[cfg(test)]
    pub fn max_body(&self) -> u64 {
        self.max_body
    }

    fn client_for(&self, node: &ProxyNode) -> Option<reqwest::Client> {
        if let Some(mut hit) = self.clients.get_mut(&node.addr) {
            hit.last_used = Instant::now();
            return Some(hit.client.clone());
        }
        let proxy = reqwest::Proxy::all(proxy_url_for_node(node)?).ok()?;
        let client = reqwest::Client::builder()
            .proxy(proxy)
            .timeout(self.timeout)
            .build()
            .ok()?;
        self.clients.insert(
            node.addr.clone(),
            CachedClient {
                client: client.clone(),
                last_used: Instant::now(),
            },
        );
        Some(client)
    }

    /// 出站执行（单次调用＝一次出站尝试；失败 Err(String)，调用方记 failed_addrs＋换节点重试）。
    pub async fn fetch(
        &self,
        node: &ProxyNode,
        req: BridgeRequest,
    ) -> Result<BridgeResponse, String> {
        let client = self
            .client_for(node)
            .ok_or_else(|| format!("socks bridge: no client for {}", node.addr))?;
        let method = reqwest::Method::from_bytes(req.method.as_bytes())
            .map_err(|e| format!("socks bridge: bad method {}: {e}", req.method))?;
        let mut out = client.request(method, &req.url);
        // A11：下游头经白名单＋HeaderName/Value 校验后透传（非法跳过记 debug，防 reqwest 侧 panic／投毒上游）。
        for (k, v) in &sanitize_bridge_headers(&req.headers) {
            // 已校验合法，此处解析必成功；防御性兜底：万一失败则跳过（不 panic）。
            let (Ok(name), Ok(val)) = (
                k.parse::<http::HeaderName>(),
                v.parse::<http::HeaderValue>(),
            ) else {
                log::debug!("[SocksBridge] skip unverified header {k}");
                continue;
            };
            out = out.header(name, val);
        }
        if let Some(b) = req.body {
            out = out.body(b);
        }
        let resp = out
            .send()
            .await
            .map_err(|e| format!("socks bridge: fetch via {} failed: {e}", node.addr))?;
        let status = resp.status().as_u16();
        let mut headers = Vec::new();
        for (k, v) in resp.headers() {
            let name = k.as_str().to_string();
            if is_hop_header(&name) {
                continue;
            }
            if let Ok(val) = v.to_str() {
                headers.push((name, val.to_string()));
            }
        }
        // E6：Content-Length 声明预检（超限零读取早停；错误文案前缀与旧口径一致，
        // 调用方 gateway `serve_via_socks` 仍按失败计：bridge_errors＋换节点重试）。
        if content_length_over_cap(resp.content_length(), self.max_body) {
            return Err(format!(
                "socks bridge: body over cap {} bytes (content-length {})",
                self.max_body,
                resp.content_length().unwrap_or(0),
            ));
        }
        // E6：逐 chunk 累加＋上限（`extend` 前先判，超限 chunk 不进缓冲即早停，
        // 大文件不 transient 超占；调用方按失败计）。
        use futures::StreamExt;
        let mut body = Vec::new();
        let mut stream = resp.bytes_stream();
        while let Some(chunk) = stream.next().await {
            let c = chunk.map_err(|e| format!("socks bridge: body read failed: {e}"))?;
            if !within_body_cap(body.len(), c.len(), self.max_body) {
                return Err(format!(
                    "socks bridge: body over cap {} bytes",
                    self.max_body
                ));
            }
            body.extend_from_slice(&c);
        }
        Ok(BridgeResponse {
            status,
            headers,
            body: bytes::Bytes::from(body),
        })
    }

    /// 闲置 Client 淘汰（main 60s 滴答调用，沿 prober 节拍；返回删除个数）。
    pub fn evict_idle(&self) -> usize {
        let before = self.clients.len();
        self.clients
            .retain(|_, c| Instant::now().saturating_duration_since(c.last_used) < BRIDGE_IDLE_TTL);
        // OPT-R6 S2：Client 缓存正被数据面 `client_for` 并发写入（首次用到某 socks
        // 节点时建缓存），旧的 `before - len()` 会 debug panic / release 回绕。
        janitor::removed_count(before, self.clients.len())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn socks5_node(ip: &str, port: u16) -> ProxyNode {
        ProxyNode::new(
            ip.to_string(),
            port,
            None,
            None,
            "ZZ".to_string(),
            "free".to_string(),
            "free-socks".to_string(),
            10,
        )
        .with_proto(EgressProto::Socks5)
    }

    #[test]
    fn bridge_header_sanitize_drops_illegal_and_non_allowlisted() {
        // A11：非法头名/值不得进 reqwest（防 panic/投毒上游）；网关控制头与逐跳头不得透传。
        let input = vec![
            ("host".to_string(), "example.com".to_string()),
            ("x-custom".to_string(), "yes".to_string()),
            ("connection".to_string(), "close".to_string()),
            ("X-Proxy-Proto".to_string(), "socks5".to_string()),
            ("bad header".to_string(), "v".to_string()),
            ("x-ok".to_string(), "bad\nvalue".to_string()),
            ("via".to_string(), "1.1 proxy".to_string()),
        ];
        let out = sanitize_bridge_headers(&input);
        assert!(out.iter().any(|(k, v)| k == "host" && v == "example.com"));
        assert!(out.iter().any(|(k, v)| k == "x-custom" && v == "yes"));
        assert!(!out
            .iter()
            .any(|(k, _)| k.eq_ignore_ascii_case("connection")));
        assert!(!out
            .iter()
            .any(|(k, _)| k.eq_ignore_ascii_case("X-Proxy-Proto")));
        assert!(!out.iter().any(|(k, _)| k == "bad header"));
        assert!(!out.iter().any(|(k, _)| k == "x-ok"));
        assert!(!out.iter().any(|(k, _)| k.eq_ignore_ascii_case("via")));
    }

    #[test]
    fn hop_headers_stripped() {
        // 端到端逐跳头必须过滤（下游复用长连接语义，不得透传上游的逐跳头）；
        // 端到端业务头保留；比较大小写不敏感。
        for h in [
            "connection",
            "Transfer-Encoding",
            "KEEP-ALIVE",
            "proxy-authenticate",
            "Proxy-Authorization",
            "trailer",
            "upgrade",
            "te",
        ] {
            assert!(is_hop_header(h), "{h} must be stripped");
        }
        for h in ["x-custom", "content-type", "content-length", "server"] {
            assert!(!is_hop_header(h), "{h} must pass through");
        }
    }

    #[test]
    fn proxy_url_for_node_shape() {
        // socks5 走远端解析（socks5h）；账密编码复用 prober 单份实现；http 节点桥不接（None）。
        assert_eq!(
            proxy_url_for_node(&socks5_node("10.0.0.1", 1080)).as_deref(),
            Some("socks5h://10.0.0.1:1080")
        );
        let mut authed = socks5_node("10.0.0.2", 1080);
        authed.username = Some("u x".to_string());
        authed.password = Some("p@y".to_string());
        assert_eq!(
            proxy_url_for_node(&authed).as_deref(),
            Some("socks5h://u%20x:p%40y@10.0.0.2:1080")
        );
        let mut s4 = socks5_node("10.0.0.3", 1080);
        s4.proto = EgressProto::Socks4;
        assert_eq!(
            proxy_url_for_node(&s4).as_deref(),
            Some("socks4://10.0.0.3:1080")
        );
        let http = ProxyNode::new(
            "10.0.0.4".to_string(),
            8080,
            None,
            None,
            "US".to_string(),
            "residential".to_string(),
            "mock-a".to_string(),
            100,
        );
        assert_eq!(proxy_url_for_node(&http), None);
    }

    /// 单连接 relay 逻辑（SOCKS5 握手→CONNECT 解析目标→直连目标→双向管道），
    /// 即“最简 SOCKS5 出口”，专供桥透传断言（解密/改写一律不做）。
    async fn relay_once(mut s: tokio::net::TcpStream) {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        // greeting。
        let mut head = [0u8; 2];
        if s.read_exact(&mut head).await.is_err() {
            return;
        }
        let mut methods = vec![0u8; head[1] as usize];
        if s.read_exact(&mut methods).await.is_err() {
            return;
        }
        if s.write_all(&[0x05, 0x00]).await.is_err() {
            return;
        }
        // CONNECT：VER CMD RSV ATYP＋地址体。
        let mut req4 = [0u8; 4];
        if s.read_exact(&mut req4).await.is_err() {
            return;
        }
        let (host, port) = match req4[3] {
            0x01 => {
                let mut b = [0u8; 6];
                if s.read_exact(&mut b).await.is_err() {
                    return;
                }
                (
                    std::net::IpAddr::from([b[0], b[1], b[2], b[3]]).to_string(),
                    u16::from_be_bytes([b[4], b[5]]),
                )
            }
            0x03 => {
                let mut l = [0u8; 1];
                if s.read_exact(&mut l).await.is_err() {
                    return;
                }
                let mut b = vec![0u8; l[0] as usize + 2];
                if s.read_exact(&mut b).await.is_err() {
                    return;
                }
                let n = b.len();
                (
                    String::from_utf8_lossy(&b[..n - 2]).to_string(),
                    u16::from_be_bytes([b[n - 2], b[n - 1]]),
                )
            }
            _ => return,
        };
        let mut up = match tokio::net::TcpStream::connect((host.as_str(), port)).await {
            Ok(v) => v,
            Err(_) => {
                let _ = s
                    .write_all(&[0x05, 0x05, 0x00, 0x01, 0, 0, 0, 0, 0, 0])
                    .await;
                return;
            }
        };
        if s.write_all(&[0x05, 0x00, 0x00, 0x01, 0, 0, 0, 0, 0, 0])
            .await
            .is_err()
        {
            return;
        }
        let _ = tokio::io::copy_bidirectional(&mut s, &mut up).await;
    }

    /// 本地 relay-stub（单连接；存量透传断言用，语义与旧实现一致）。
    async fn spawn_relay_stub() -> u16 {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind");
        let port = listener.local_addr().expect("addr").port();
        tokio::spawn(async move {
            let (s, _) = listener.accept().await.expect("accept");
            relay_once(s).await;
        });
        port
    }

    /// E6：循环版 relay-stub（多连接；单测内多次 fetch 共用一个出口）。
    async fn spawn_relay_loop() -> u16 {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind");
        let port = listener.local_addr().expect("addr").port();
        tokio::spawn(async move {
            loop {
                let Ok((s, _)) = listener.accept().await else {
                    break;
                };
                tokio::spawn(relay_once(s));
            }
        });
        port
    }

    /// E6：按路径路由的 mock 源站（短连接；`/big` 诚实 256B，`/chunked` 分块 256B 无 CL，
    /// `/lying` 声明 100MB 实发 16B 后关连接——供零读取预检断言）。
    async fn serve_routed_mock() -> u16 {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind");
        let port = listener.local_addr().expect("addr").port();
        tokio::spawn(async move {
            loop {
                let Ok((mut s, _)) = listener.accept().await else {
                    break;
                };
                tokio::spawn(async move {
                    let mut buf = [0u8; 4096];
                    let n = s.read(&mut buf).await.unwrap_or(0);
                    let head = String::from_utf8_lossy(&buf[..n]).to_string();
                    if head.contains("/lying") {
                        // 声明 100MB 实发 16B 后直接关连接（旧实现读到断流报 read failed，
                        // 新实现 Content-Length 预检零读取即 over cap）。
                        let _ = s
                            .write_all(
                                b"HTTP/1.1 200 OK\r\ncontent-length: 104857600\r\nconnection: close\r\n\r\n0123456789abcdef",
                            )
                            .await;
                        let _ = s.shutdown().await;
                        return;
                    }
                    if head.contains("/chunked") {
                        // 无 Content-Length 的分块体（走逐 chunk 累加口径；256 == 0x100）。
                        let body = vec![b'x'; 256];
                        let _ = s
                            .write_all(
                                b"HTTP/1.1 200 OK\r\ntransfer-encoding: chunked\r\nconnection: close\r\n\r\n100\r\n",
                            )
                            .await;
                        let _ = s.write_all(&body).await;
                        let _ = s.write_all(b"\r\n0\r\n\r\n").await;
                        return;
                    }
                    let body = vec![b'x'; 256];
                    let _ = s
                        .write_all(
                            format!(
                                "HTTP/1.1 200 OK\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
                                body.len()
                            )
                            .as_bytes(),
                        )
                        .await;
                    let _ = s.write_all(&body).await;
                });
            }
        });
        port
    }

    #[tokio::test]
    async fn bridge_relays_through_stub_to_mock() {
        // mock HTTP 源站（沿 mock_upstream 手法换端口）。
        let mock = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind");
        let mock_port = mock.local_addr().expect("addr").port();
        tokio::spawn(async move {
            let (mut s, _) = mock.accept().await.expect("accept");
            use tokio::io::{AsyncReadExt, AsyncWriteExt};
            let mut buf = [0u8; 4096];
            let _ = s.read(&mut buf).await;
            let body = b"mock-via-socks";
            let _ = s
                .write_all(
                    format!(
                        "HTTP/1.1 200 OK\r\ncontent-length: {}\r\nx-custom: yes\r\nconnection: close\r\n\r\nmock-via-socks",
                        body.len()
                    )
                    .as_bytes(),
                )
                .await;
        });
        let relay = spawn_relay_stub().await;
        let bridge = SocksBridge::new(Duration::from_secs(10), 1024 * 1024);
        let node = socks5_node("127.0.0.1", relay);
        let res = bridge
            .fetch(
                &node,
                BridgeRequest {
                    method: "GET".to_string(),
                    url: format!("http://127.0.0.1:{mock_port}/"),
                    headers: vec![("host".to_string(), format!("127.0.0.1:{mock_port}"))],
                    body: None,
                },
            )
            .await
            .expect("bridge fetch");
        assert_eq!(res.status, 200);
        assert_eq!(res.body.as_ref(), b"mock-via-socks");
        // 端到端头透回，逐跳头（connection）被过滤。
        assert!(res
            .headers
            .iter()
            .any(|(k, v)| k == "x-custom" && v == "yes"));
        assert!(!res.headers.iter().any(|(k, _)| k == "connection"));
    }

    #[test]
    fn bridge_body_cap_checker_blocks_over_cap_chunk_before_buffering() {
        // E6：超限 chunk 在 extend 前即判停（含等于边界放行；饱和加防极端长度溢出 wrap）。
        assert!(within_body_cap(0, 10, 10));
        assert!(within_body_cap(6, 4, 10));
        assert!(!within_body_cap(6, 5, 10));
        assert!(!within_body_cap(0, 11, 10));
        assert!(within_body_cap(usize::MAX, 0, u64::MAX));
        // 饱和到 u64::MAX 后仍大于 `u64::MAX - 1` 上限（无 wrap 回绕误放行）。
        assert!(!within_body_cap(usize::MAX, 1, u64::MAX - 1));
    }

    #[test]
    fn bridge_content_length_precheck_trips_without_reading() {
        // E6：声明超限零读取早停（未知长度不拦；等于边界放行，由逐 chunk 口径兜底）。
        assert!(!content_length_over_cap(None, 16));
        assert!(!content_length_over_cap(Some(16), 16));
        assert!(content_length_over_cap(Some(17), 16));
        assert!(content_length_over_cap(Some(100 * 1024 * 1024), 16));
    }

    #[test]
    fn bridge_max_body_configurable_via_new() {
        // E6：上限可配——同 chunk 序列在不同 max_body 下判定不同（经 new 第二参注入）。
        let tiny = SocksBridge::new(Duration::from_secs(10), 16);
        let big = SocksBridge::new(Duration::from_secs(10), 1024 * 1024);
        assert_eq!(tiny.max_body(), 16);
        assert_eq!(big.max_body(), 1024 * 1024);
        assert!(!within_body_cap(0, 256, tiny.max_body()));
        assert!(within_body_cap(0, 256, big.max_body()));
        // 下限钳制（0 → 1，沿既有 `.max(1)` 语义）。
        assert_eq!(SocksBridge::new(Duration::from_secs(10), 0).max_body(), 1);
    }

    #[tokio::test]
    async fn bridge_fetch_stops_early_on_over_cap_body() {
        // E6 超限早停＋上限可配（同上游不同 max_body 行为不同；错误口径与 gateway
        // 调用方兼容：Err(String) 含 "over cap"，调用方记 bridge_errors＋换节点重试）。
        let mock_port = serve_routed_mock().await;
        let relay = spawn_relay_loop().await;
        let node = socks5_node("127.0.0.1", relay);
        let tiny = SocksBridge::new(Duration::from_secs(10), 16);
        let big = SocksBridge::new(Duration::from_secs(10), 1024 * 1024);
        let get = |path: &str| BridgeRequest {
            method: "GET".to_string(),
            url: format!("http://127.0.0.1:{mock_port}{path}"),
            headers: vec![("host".to_string(), format!("127.0.0.1:{mock_port}"))],
            body: None,
        };
        // 诚实 256B：小上限 Content-Length 预检即停（Err），大上限全量成功。
        // 注：不用 `expect_err`（`BridgeResponse` 无 Debug，不为单测加 derive 污染出参结构）。
        let err = match tiny.fetch(&node, get("/big")).await {
            Ok(_) => panic!("tiny cap must reject 256B"),
            Err(e) => e,
        };
        assert!(err.contains("over cap"), "unexpected: {err}");
        let ok = big
            .fetch(&node, get("/big"))
            .await
            .expect("big cap must pass 256B");
        assert_eq!(ok.body.len(), 256);
        // 无 CL 分块体：走逐 chunk 累加口径，同样早停。
        let err_chunked = match tiny.fetch(&node, get("/chunked")).await {
            Ok(_) => panic!("tiny cap must reject chunked 256B"),
            Err(e) => e,
        };
        assert!(
            err_chunked.contains("over cap"),
            "unexpected: {err_chunked}"
        );
        // 谎报 100MB：零读取预检即 over cap（旧实现会先读 16B 再断流报 read failed）。
        let err_lying = match tiny.fetch(&node, get("/lying")).await {
            Ok(_) => panic!("lying CL must trip precheck"),
            Err(e) => e,
        };
        assert!(err_lying.contains("over cap"), "unexpected: {err_lying}");
    }
}
