//! P2 SOCKS 数据面翻译桥：显式 socks 请求的出站执行器。
//!
//! 网关 `proxy_upstream_filter` 短路分支调用（Pingora 0.6 无 SOCKS connector，
//! 不 fork——桥用 reqwest `socks` feature 出站，合成响应回下游）。
//! Client 按 `node.addr` 缓存复用（沿 prober OPT-5 模式＋TTL 淘汰；`Client` 克隆廉价内部 `Arc`）。

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
        for (k, v) in &req.headers {
            if is_hop_header(k) {
                continue;
            }
            out = out.header(k.as_str(), v.as_str());
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
        // 逐 chunk 累加＋上限（超限即 Err，防内存爆；调用方按失败计）。
        use futures::StreamExt;
        let mut body = Vec::new();
        let mut stream = resp.bytes_stream();
        while let Some(chunk) = stream.next().await {
            let c = chunk.map_err(|e| format!("socks bridge: body read failed: {e}"))?;
            body.extend_from_slice(&c);
            if body.len() as u64 > self.max_body {
                return Err(format!(
                    "socks bridge: body over cap {} bytes",
                    self.max_body
                ));
            }
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
        before - self.clients.len()
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

    /// 本地 relay-stub（~50 行）：SOCKS5 握手→CONNECT 解析目标→直连目标→双向管道。
    /// 即“最简 SOCKS5 出口”，专供桥透传断言（解密/改写一律不做）。
    async fn spawn_relay_stub() -> u16 {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind");
        let port = listener.local_addr().expect("addr").port();
        tokio::spawn(async move {
            let (mut s, _) = listener.accept().await.expect("accept");
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
}
