//! FreePool 第二供应线：公开免费源抓取 → 两级质检 → TTL 注册 → Router 合并。
//!
//! Phase 1 只收 HTTP(S)（零网关转发改动）；SOCKS 只解析标注、merge 过滤
//! （Phase 2 做 SOCKS egress）。默认关闭（`FREE_ENABLED=1` 开启）。
//! 免费线零信任（arXiv:2403.02445：16,923 篡改内容）：复检基址 https-only＋
//! canary 防篡改＋OPERATION 禁敏感流量。

use std::time::Duration;
use std::time::Instant;
/// 抓取协议（P2：全协议进池；出站形态见 `egress_of`，选路隔离见 `RouterEngine::matches`）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FreeProto {
    Http,
    Https,
    Socks4,
    Socks5,
}

impl FreeProto {
    fn from_token(tok: &str) -> Option<Self> {
        match tok.to_ascii_lowercase().as_str() {
            "http" => Some(FreeProto::Http),
            "https" => Some(FreeProto::Https),
            "socks4" => Some(FreeProto::Socks4),
            "socks5" => Some(FreeProto::Socks5),
            _ => None,
        }
    }
}

/// 源站吐出的原始节点（未质检）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RawNode {
    pub ip: String,
    pub port: u16,
    pub proto: FreeProto,
    pub country: Option<String>,
    pub source: String,
}

/// 抓取结果（v2：304 NotModified 显式信号，供 SourceGuard 区分“未变更”与“零产出”）。
#[derive(Debug)]
pub struct FetchOutcome {
    pub nodes: Vec<RawNode>,
    /// true＝源站明确表示未变更（GitHub ETag/304），调用方不得计零产出轮次。
    pub not_modified: bool,
}

impl FetchOutcome {
    pub fn nodes(nodes: Vec<RawNode>) -> Self {
        Self {
            nodes,
            not_modified: false,
        }
    }

    pub fn not_modified() -> Self {
        Self {
            nodes: Vec::new(),
            not_modified: true,
        }
    }
}

/// 抓取源插件接口（API / HTML / GitHub 各一实现；零新依赖，错误文案 String）。
#[async_trait::async_trait]
pub trait Source: Send + Sync {
    fn name(&self) -> &'static str;
    async fn fetch(&self, client: &reqwest::Client) -> Result<FetchOutcome, String>;
}

/// 条件 GET（R3-5 礼貌轮询共享件；GitHubSource 自有实现不动，存量冻结）。
/// 发 `If-None-Match`／`If-Modified-Since`（有缓存才带）；304→`Ok(None)`
/// （调用方包 `not_modified`，SourceGuard 不计数）；200→更新缓存并 `Ok(Some(body))`。
async fn conditional_get(
    client: &reqwest::Client,
    url: &str,
    etag: &parking_lot::Mutex<Option<String>>,
    last_modified: &parking_lot::Mutex<Option<String>>,
) -> Result<Option<String>, String> {
    let mut req = client.get(url);
    if let Some(e) = etag.lock().clone() {
        req = req.header("If-None-Match", e);
    }
    if let Some(m) = last_modified.lock().clone() {
        req = req.header("If-Modified-Since", m);
    }
    let resp = req
        .send()
        .await
        .map_err(|e| format!("GET {url} failed: {e}"))?;
    if resp.status() == reqwest::StatusCode::NOT_MODIFIED {
        return Ok(None);
    }
    if let Some(v) = resp.headers().get("etag").and_then(|h| h.to_str().ok()) {
        *etag.lock() = Some(v.to_string());
    }
    if let Some(v) = resp
        .headers()
        .get("last-modified")
        .and_then(|h| h.to_str().ok())
    {
        *last_modified.lock() = Some(v.to_string());
    }
    resp.text()
        .await
        .map(Some)
        .map_err(|e| format!("read {url} failed: {e}"))
}

/// JSON API 源（默认 Geonode；URL env 可配，见 Task 12）。
/// port 兼容字符串/数字双形态；缺 country 视为 None（后继标 ZZ）。
/// R3-5 礼貌轮询：ETag/Last-Modified 缓存＋304（对齐 GitHubSource）。
pub struct ApiSource {
    pub name: &'static str,
    pub url: String,
    etag: parking_lot::Mutex<Option<String>>,
    last_modified: parking_lot::Mutex<Option<String>>,
}

impl ApiSource {
    pub fn new(name: &'static str, url: String) -> Self {
        Self {
            name,
            url,
            etag: parking_lot::Mutex::new(None),
            last_modified: parking_lot::Mutex::new(None),
        }
    }
    pub fn parse(source: &str, body: &str) -> Vec<RawNode> {
        let v: serde_json::Value = match serde_json::from_str(body) {
            Ok(v) => v,
            Err(_) => return Vec::new(),
        };
        let arr = v.get("data").and_then(|d| d.as_array());
        let mut out = Vec::new();
        for item in arr.into_iter().flatten() {
            let ip = item.get("ip").and_then(|s| s.as_str()).unwrap_or("");
            if ip.is_empty() {
                continue;
            }
            let port: Option<u16> = match item.get("port") {
                Some(serde_json::Value::Number(n)) => {
                    n.as_u64().and_then(|p| u16::try_from(p).ok())
                }
                Some(serde_json::Value::String(s)) => s.parse::<u16>().ok(),
                _ => None,
            };
            let port = match port {
                Some(p) if p > 0 => p,
                _ => continue,
            };
            let proto = item
                .get("protocols")
                .and_then(|p| p.as_array())
                .and_then(|a| a.first())
                .and_then(|s| s.as_str())
                .and_then(FreeProto::from_token)
                .unwrap_or(FreeProto::Http);
            let country = item
                .get("country")
                .and_then(|c| c.as_str())
                .filter(|c| !c.is_empty())
                .map(|c| c.to_string());
            out.push(RawNode {
                ip: ip.to_string(),
                port,
                proto,
                country,
                source: source.to_string(),
            });
        }
        out
    }
}

#[async_trait::async_trait]
impl Source for ApiSource {
    fn name(&self) -> &'static str {
        self.name
    }

    async fn fetch(&self, client: &reqwest::Client) -> Result<FetchOutcome, String> {
        match conditional_get(client, &self.url, &self.etag, &self.last_modified).await? {
            Some(body) => Ok(FetchOutcome::nodes(Self::parse(self.name, &body))),
            None => Ok(FetchOutcome::not_modified()),
        }
    }
}

/// HTML 表格源（默认 free-proxy-list.net；URL env 可配）。
/// 无 HTML 解析依赖：字节扫描 `a.b.c.d[:port]|</td><td>port` 形态，octet≤255、
/// 端口 1..=65535；行内含 `socks4`/`socks5`（大小写不敏感）则标对应协议
///（Phase 1 Verifier 跳过）。
/// R3-5 礼貌轮询：ETag/Last-Modified 缓存＋304（对齐 GitHubSource）。
pub struct HtmlSource {
    pub name: &'static str,
    pub url: String,
    pub default_proto: FreeProto,
    etag: parking_lot::Mutex<Option<String>>,
    last_modified: parking_lot::Mutex<Option<String>>,
}

impl HtmlSource {
    pub fn new(name: &'static str, url: String, default_proto: FreeProto) -> Self {
        Self {
            name,
            url,
            default_proto,
            etag: parking_lot::Mutex::new(None),
            last_modified: parking_lot::Mutex::new(None),
        }
    }
    pub fn extract(source: &str, html: &str, default_proto: FreeProto) -> Vec<RawNode> {
        let bytes = html.as_bytes();
        let mut out = Vec::new();
        let mut i = 0;
        while i < bytes.len() {
            if !bytes[i].is_ascii_digit() {
                i += 1;
                continue;
            }
            // Token 边界：IP 起始前不得是数字或 `.`（否则 `999.1.1.1` 的子串
            // `99.1.1.1` 会被误判为合法 IP；非法 octet 整 token 跳过）。
            if i > 0 && (bytes[i - 1].is_ascii_digit() || bytes[i - 1] == b'.') {
                i += 1;
                continue;
            }
            if let Some((ip, port, consumed)) = Self::match_ip_port(&bytes[i..]) {
                // 行级协议嗅探：向前 200B 窗口找 socks 标记。
                let from = i.saturating_sub(200);
                let window = String::from_utf8_lossy(&bytes[from..i]).to_ascii_lowercase();
                let proto = if window.contains("socks5") {
                    FreeProto::Socks5
                } else if window.contains("socks4") {
                    FreeProto::Socks4
                } else {
                    default_proto
                };
                out.push(RawNode {
                    ip,
                    port,
                    proto,
                    country: None,
                    source: source.to_string(),
                });
                i += consumed;
            } else {
                i += 1;
            }
        }
        out
    }

    /// 在切片头部匹配 `a.b.c.d`＋可选 `:port`／紧随 `<…>port`；非法返回 None。
    fn match_ip_port(b: &[u8]) -> Option<(String, u16, usize)> {
        let mut octets = [0u32; 4];
        let mut pos = 0;
        for (k, octet) in octets.iter_mut().enumerate() {
            let start = pos;
            while pos < b.len() && b[pos].is_ascii_digit() {
                pos += 1;
            }
            if start == pos {
                return None;
            }
            *octet = std::str::from_utf8(&b[start..pos])
                .ok()?
                .parse::<u32>()
                .ok()?;
            if *octet > 255 {
                return None;
            }
            if k < 3 {
                if pos >= b.len() || b[pos] != b'.' {
                    return None;
                }
                pos += 1;
            }
        }
        // 端口：`:port` 或 `</td><td>` 类标签序列后的数字（多标签循环跳过）。
        let mut ppos = pos;
        if ppos < b.len() && b[ppos] == b':' {
            ppos += 1;
        } else {
            let mut p = ppos;
            loop {
                // 跳过标签间空白与已闭合的 `>`。
                while p < b.len()
                    && (b[p] == b' '
                        || b[p] == b'\t'
                        || b[p] == b'\r'
                        || b[p] == b'\n'
                        || b[p] == b'>')
                {
                    p += 1;
                }
                if p < b.len() && b[p].is_ascii_digit() {
                    ppos = p;
                    break;
                }
                if p < b.len() && b[p] == b'<' {
                    // 跳过一整个 `<…>` 标签后继续找数字。
                    while p < b.len() && b[p] != b'>' {
                        p += 1;
                    }
                    if p < b.len() {
                        p += 1; // 跳过 `>`
                        continue;
                    }
                }
                return None;
            }
        }
        let start = ppos;
        while ppos < b.len() && b[ppos].is_ascii_digit() {
            ppos += 1;
        }
        if start == ppos {
            return None;
        }
        let port: u16 = std::str::from_utf8(&b[start..ppos]).ok()?.parse().ok()?;
        if port == 0 {
            return None;
        }
        let ip = format!("{}.{}.{}.{}", octets[0], octets[1], octets[2], octets[3]);
        Some((ip, port, ppos))
    }
}

#[async_trait::async_trait]
impl Source for HtmlSource {
    fn name(&self) -> &'static str {
        self.name
    }

    async fn fetch(&self, client: &reqwest::Client) -> Result<FetchOutcome, String> {
        match conditional_get(client, &self.url, &self.etag, &self.last_modified).await? {
            Some(body) => Ok(FetchOutcome::nodes(Self::extract(
                self.name,
                &body,
                self.default_proto,
            ))),
            None => Ok(FetchOutcome::not_modified()),
        }
    }
}

/// GitHub raw 仓源（默认 clarketm/proxy-list raw；URL env 可配）。
/// 行格式：`ip:port`（`#` 开头与空行跳过；行尾 `socks4`/`socks5` 标记协议）。
/// 礼貌轮询：ETag 缓存＋If-None-Match，304 返回 `not_modified`（Task 10 不计零产出）。
pub struct GitHubSource {
    pub name: &'static str,
    pub url: String,
    etag: parking_lot::Mutex<Option<String>>,
}

impl GitHubSource {
    pub fn new(name: &'static str, url: String) -> Self {
        Self {
            name,
            url,
            etag: parking_lot::Mutex::new(None),
        }
    }

    pub fn parse(source: &str, body: &str) -> Vec<RawNode> {
        let mut out = Vec::new();
        for line in body.lines() {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            let lower = line.to_ascii_lowercase();
            let proto = if lower.contains("socks5") {
                FreeProto::Socks5
            } else if lower.contains("socks4") {
                FreeProto::Socks4
            } else {
                FreeProto::Http
            };
            let addr = line.split_whitespace().next().unwrap_or("");
            let (ip, port) = match addr.rsplit_once(':') {
                Some((ip, port)) => (ip, port.parse::<u16>().ok()),
                None => continue,
            };
            let port = match port {
                Some(p) if p > 0 => p,
                _ => continue,
            };
            if ip.split('.').count() != 4 {
                continue;
            }
            out.push(RawNode {
                ip: ip.to_string(),
                port,
                proto,
                country: None,
                source: source.to_string(),
            });
        }
        out
    }
}

#[async_trait::async_trait]
impl Source for GitHubSource {
    fn name(&self) -> &'static str {
        self.name
    }

    async fn fetch(&self, client: &reqwest::Client) -> Result<FetchOutcome, String> {
        let mut req = client.get(&self.url);
        if let Some(etag) = self.etag.lock().clone() {
            req = req.header("If-None-Match", etag);
        }
        let resp = req
            .send()
            .await
            .map_err(|e| format!("GET {} failed: {e}", self.url))?;
        if resp.status() == reqwest::StatusCode::NOT_MODIFIED {
            return Ok(FetchOutcome::not_modified());
        }
        if let Some(v) = resp.headers().get("etag").and_then(|h| h.to_str().ok()) {
            *self.etag.lock() = Some(v.to_string());
        }
        let body = resp
            .text()
            .await
            .map_err(|e| format!("read {} failed: {e}", self.url))?;
        Ok(FetchOutcome::nodes(Self::parse(self.name, &body)))
    }
}

/// `FreeProto`（抓取形态）→`EgressProto`（出站形态）。
/// Http/Https 皆为 HTTP 正向代理（Phase 1 语义延续）；Socks4/5 直映（P2 走翻译桥）。
pub fn egress_of(proto: FreeProto) -> EgressProto {
    match proto {
        FreeProto::Http | FreeProto::Https => EgressProto::Http,
        FreeProto::Socks5 => EgressProto::Socks5,
        FreeProto::Socks4 => EgressProto::Socks4,
    }
}

/// `full_check_base`（`https://host[:port]`）→ socks CONNECT 验证目标。
/// 仅 http/https 基址合法（file/dict 等一律 None，沿 SSRF 护栏口径）；缺省端口 443/80。
pub fn socks_target_of_base(base: &str) -> Option<(String, u16)> {
    let rest = base
        .strip_prefix("https://")
        .map(|r| (r, 443u16))
        .or_else(|| base.strip_prefix("http://").map(|r| (r, 80u16)))?;
    let authority = rest.0.split('/').next().unwrap_or("");
    if authority.is_empty() {
        return None;
    }
    match authority.rsplit_once(':') {
        Some((h, p)) => {
            let port = p.parse::<u16>().ok()?;
            if h.is_empty() || port == 0 {
                return None;
            }
            Some((h.to_string(), port))
        }
        None => Some((authority.to_string(), rest.1)),
    }
}
/// 质检器：TCP 建链（超时）＋延迟门。
/// Http/Https 走 TCP 建链（原语义；通过只代表端口开放，匿名度由 FullChecker 判定）。
/// Socks4/5（P2）经 `socks_handshake::establish` 向 `socks_target` 做 CONNECT 验证，
/// 无 target 时 greeting-only 降级（prewarmer 同口径）。
/// 返回的延迟供 Registry 初值，权重主信号是 FullCheck 的转发延迟。
#[derive(Clone)]
pub struct Verifier {
    timeout: Duration,
    max_latency_ms: u64,
    socks_target: Option<(String, u16)>,
}

impl Verifier {
    pub fn new(timeout: Duration, max_latency_ms: u64) -> Self {
        Self {
            timeout,
            max_latency_ms,
            socks_target: None,
        }
    }

    /// P2：设置 socks CONNECT 验证目标（Worker 传入 `full_check_base` 解析出的 host:port）。
    pub fn with_socks_target(mut self, host: String, port: u16) -> Self {
        self.socks_target = Some((host, port));
        self
    }

    /// 通过返回质检耗时 ms；失败/超门返回 None。
    pub async fn verify(&self, raw: &RawNode) -> Option<u64> {
        match raw.proto {
            FreeProto::Http | FreeProto::Https => self.verify_tcp(raw).await,
            FreeProto::Socks5 | FreeProto::Socks4 => self.verify_socks(raw).await,
        }
    }

    async fn verify_tcp(&self, raw: &RawNode) -> Option<u64> {
        let start = std::time::Instant::now();
        let ok = tokio::time::timeout(
            self.timeout,
            tokio::net::TcpStream::connect((raw.ip.as_str(), raw.port)),
        )
        .await
        .is_ok_and(|r| r.is_ok());
        if !ok {
            return None;
        }
        let ms = start.elapsed().as_millis() as u64;
        if ms <= self.max_latency_ms {
            Some(ms)
        } else {
            log::debug!("[FreePool] slow {}:{} {ms}ms over gate", raw.ip, raw.port);
            None
        }
    }

    async fn verify_socks(&self, raw: &RawNode) -> Option<u64> {
        use crate::socks_handshake::{establish, greet_only};
        let start = std::time::Instant::now();
        let proto = egress_of(raw.proto);
        let done = match self.socks_target.clone() {
            Some((host, port)) => tokio::time::timeout(
                self.timeout,
                establish(&raw.ip, raw.port, None, None, proto, &host, port),
            )
            .await
            .is_ok_and(|r| r.is_ok()),
            // 降级：greeting-only（免费节点无账密字段，账密节点不在免费线出现，注释写明）。
            None => tokio::time::timeout(self.timeout, greet_only(&raw.ip, raw.port, proto))
                .await
                .is_ok_and(|r| r.is_ok()),
        };
        if !done {
            return None;
        }
        let ms = start.elapsed().as_millis() as u64;
        if ms <= self.max_latency_ms {
            Some(ms)
        } else {
            log::debug!(
                "[FreePool] slow socks {}:{} {ms}ms over gate",
                raw.ip,
                raw.port
            );
            None
        }
    }
}

/// 匿名度三级（openproxyhub/MiyaIP 定义）：Elite 最安全；Transparent 泄漏真实 IP，
/// 永不服务认证租户流量（merge 门＋OPERATION 硬规则）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AnonLevel {
    Elite,
    Anonymous,
    Transparent,
    Unknown,
}

/// 7 头披露检查表（MiyaIP 方法学；比较时全小写）。
pub const DISCLOSURE_HEADERS: [&str; 7] = [
    "forwarded",
    "x-forwarded-for",
    "x-real-ip",
    "client-ip",
    "via",
    "proxy-connection",
    "x-proxy-id",
];

/// 纯函数分级：基线（直连出口）vs 经代理观测（出口＋回显头）。
pub fn classify_anonymity(
    baseline_ip: &str,
    exit_ip: Option<&str>,
    echoed_headers: &std::collections::HashMap<String, String>,
) -> AnonLevel {
    let exit = match exit_ip {
        Some(e) => e,
        None => return AnonLevel::Unknown,
    };
    if exit.trim() == baseline_ip.trim() {
        return AnonLevel::Transparent;
    }
    let disclosed = echoed_headers
        .keys()
        .any(|k| DISCLOSURE_HEADERS.contains(&k.to_ascii_lowercase().as_str()));
    if disclosed {
        AnonLevel::Anonymous
    } else {
        AnonLevel::Elite
    }
}

/// canary 标记（`/anything/freepool-canary` 回显 url 须含此串，否则判篡改）。
pub const FULL_CHECK_MARKER: &str = "freepool-canary";

/// 实转复检结果（主健康信号）。
pub struct FullCheckResult {
    pub anon: AnonLevel,
    pub exit_ip: Option<String>,
    /// 经代理 GET 全程耗时（含代理转发；为主延迟信号，替代 TCP 建链延迟参与 EWMA）。
    pub fwd_latency_ms: u64,
}

/// 实转复检器：经候选代理 GET 基址三端点（`/ip` 出口＋`/headers` 回显＋
/// `/anything/{marker}` canary）。任一步失败/超门/canary 失配→None（失败）。
/// 非 HTTP(S) 直接 None（Phase 2 前 SOCKS 不复检）。
#[derive(Clone)]
pub struct FullChecker {
    base_url: String,
    timeout: Duration,
}

impl FullChecker {
    pub fn new(base_url: String, timeout: Duration) -> Self {
        Self {
            base_url: base_url.trim_end_matches('/').to_string(),
            timeout,
        }
    }

    pub async fn check(&self, raw: &RawNode, baseline_ip: &str) -> Option<FullCheckResult> {
        // P2：全协议复检（三端点逻辑零改动；socks 经 reqwest 原生 socks 代理，
        // socks5 走远端解析；匿名度/canary 同权）。
        let scheme = match raw.proto {
            FreeProto::Http | FreeProto::Https => "http",
            FreeProto::Socks5 => "socks5h",
            FreeProto::Socks4 => "socks4",
        };
        let proxy_url = format!("{scheme}://{}:{}", raw.ip, raw.port);
        let proxy = reqwest::Proxy::all(&proxy_url).ok()?;
        let client = reqwest::Client::builder()
            .proxy(proxy)
            .timeout(self.timeout)
            .build()
            .ok()?;
        let start = std::time::Instant::now();
        // 1. 出口。
        let ip_body: serde_json::Value = client
            .get(format!("{}/ip", self.base_url))
            .send()
            .await
            .ok()?
            .json()
            .await
            .ok()?;
        let exit = ip_body
            .get("origin")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string());
        // 2. 回显头。
        let h_body: serde_json::Value = client
            .get(format!("{}/headers", self.base_url))
            .send()
            .await
            .ok()?
            .json()
            .await
            .ok()?;
        let mut echoed = std::collections::HashMap::new();
        if let Some(map) = h_body.get("headers").and_then(|v| v.as_object()) {
            for (k, v) in map {
                if let Some(s) = v.as_str() {
                    echoed.insert(k.clone(), s.to_string());
                }
            }
        }
        // 3. canary（内容篡改检测；arXiv:2403.02445 16,923 篡改样本）。
        let c_body: serde_json::Value = client
            .get(format!("{}/anything/{}", self.base_url, FULL_CHECK_MARKER))
            .send()
            .await
            .ok()?
            .json()
            .await
            .ok()?;
        let canary_ok = c_body
            .get("url")
            .and_then(|v| v.as_str())
            .is_some_and(|u| u.contains(FULL_CHECK_MARKER));
        if !canary_ok {
            log::warn!(
                "[FreePool] canary mismatch {}:{} (tamper suspected)",
                raw.ip,
                raw.port
            );
            return None;
        }
        let anon = classify_anonymity(baseline_ip, exit.as_deref(), &echoed);
        Some(FullCheckResult {
            anon,
            exit_ip: exit,
            fwd_latency_ms: start.elapsed().as_millis() as u64,
        })
    }
}

/// 免费线池权重上限（vs 付费 60~100，加权混合中占少数；bandit free cost 0 不再叠加）。
/// v2：10 为非 trusted 上限，实际权重由 Health 连续映射 1..=上限。
pub const FREE_POOL_WEIGHT: u32 = 10;
/// v2 trusted 加成后硬上限（仍远低于付费线，避免免费淹没付费）。
pub const FREE_POOL_WEIGHT_TRUSTED_MAX: u32 = 20;
/// 无归属国家的缺省标记（只服务无 country 要求的流量，见 RouterEngine::matches）。
pub const FREE_UNKNOWN_COUNTRY: &str = "ZZ";
/// EWMA 衰减（proxyhive 同款 α=0.3；成功率与延迟共用）。
pub const HEALTH_EWMA_ALPHA: f64 = 0.3;
/// 延迟中性点（== FREE_MAX_LATENCY_MS 默认 3000；ewma_latency 高于此则惩罚<1）。
pub const HEALTH_NEUTRAL_LATENCY_MS: f64 = 3000.0;
/// trusted 门：streak≥3＋Elite＋ewma 延迟<1500ms（Thordata top-trusted 思想）。
pub const TRUSTED_MIN_STREAK: u32 = 3;
pub const TRUSTED_MAX_LATENCY_MS: f64 = 1500.0;
/// backoff：60s 起指数增长，封顶 1h。
pub const BACKOFF_BASE_SECS: u64 = 60;
pub const BACKOFF_MAX_SECS: u64 = 3600;

use crate::model::EgressProto;
use crate::model::ProxyNode;
use crate::router::RouterEngine;
use std::collections::HashMap;
use std::collections::HashSet;
use std::sync::Arc;

/// 单节点健康（EWMA 成功率×延迟惩罚；声誉不跨 TTL：条目删除即清零，IP 复用重计）。
pub struct Health {
    pub ewma_success: f64,
    pub ewma_latency_ms: f64,
    pub streak: u32,
    pub fail_streak: u32,
    pub backoff_until: Option<Instant>,
    pub anon: AnonLevel,
    pub exit_ip: Option<String>,
}

impl Health {
    pub fn fresh() -> Self {
        Self {
            ewma_success: 0.5,
            ewma_latency_ms: HEALTH_NEUTRAL_LATENCY_MS,
            streak: 0,
            fail_streak: 0,
            backoff_until: None,
            anon: AnonLevel::Unknown,
            exit_ip: None,
        }
    }

    pub fn note_success(&mut self, fwd_latency_ms: u64) {
        self.ewma_success += HEALTH_EWMA_ALPHA * (1.0 - self.ewma_success);
        self.ewma_latency_ms += HEALTH_EWMA_ALPHA * (fwd_latency_ms as f64 - self.ewma_latency_ms);
        self.streak += 1;
        self.fail_streak = 0;
        self.backoff_until = None;
    }

    pub fn note_failure(&mut self, now: Instant) {
        self.ewma_success += HEALTH_EWMA_ALPHA * (0.0 - self.ewma_success);
        self.streak = 0;
        self.fail_streak += 1;
        // fail_streak≥1 恒成立（先自增），min(7)-1 无下溢；60s 起指数增长，封顶 1h。
        let secs =
            (BACKOFF_BASE_SECS * 2u64.pow(self.fail_streak.min(7) - 1)).min(BACKOFF_MAX_SECS);
        self.backoff_until = Some(now + Duration::from_secs(secs));
    }

    /// 成功率主项×延迟惩罚（可解释双因子；延迟惩罚＝中性点/(中性点+超额)，超额≤0 时为 1）。
    /// P3 起本体冻结（`health_score_math` 逐字守护）；权重走 [`Health::composite`]。
    pub fn score(&self) -> f64 {
        let over = (self.ewma_latency_ms - HEALTH_NEUTRAL_LATENCY_MS).max(0.0);
        self.ewma_success * (HEALTH_NEUTRAL_LATENCY_MS / (HEALTH_NEUTRAL_LATENCY_MS + over))
    }

    /// P3 复合分（ProxyStats 方法学可落地子集）：`score()` × 留存因子。
    /// 留存因子＝0.5＋0.5×min(1, streak/3)：连续存活（survival streak，Thordata top-trusted
    /// 同族思想）满 3 即满权；新节点半权起步（不搞 0/1 硬阈值抖动，不断流只降权）。
    pub fn composite(&self) -> f64 {
        let retention = 0.5 + 0.5 * ((self.streak as f64) / (TRUSTED_MIN_STREAK as f64)).min(1.0);
        self.score() * retention
    }

    pub fn trusted(&self) -> bool {
        self.anon == AnonLevel::Elite
            && self.streak >= TRUSTED_MIN_STREAK
            && self.ewma_latency_ms < TRUSTED_MAX_LATENCY_MS
    }

    /// 连续权重 1..=上限（低分保底 1 不断流；trusted 封顶 TRUSTED_MAX）。
    /// P3 起走 composite（score 本体冻结；存量四断言复算守护，见 `health_score_math`）。
    pub fn weight(&self) -> u32 {
        let cap = if self.trusted() {
            FREE_POOL_WEIGHT_TRUSTED_MAX
        } else {
            FREE_POOL_WEIGHT
        };
        ((self.composite() * cap as f64).round() as u32).clamp(1, cap)
    }

    pub fn backed_off(&self, now: Instant) -> bool {
        self.backoff_until.is_some_and(|t| t > now)
    }
}

struct Entry {
    node: ProxyNode,
    health: Health,
    expires_at: Instant,
}

/// TTL 注册表：多源按 `addr` 去重（首见 source 获胜）；到期仅经复检续命；
/// 声誉不跨 TTL（条目删除即清零，IP 复用重计，防新旧污染双向错误）。
pub struct Registry {
    ttl: Duration,
    max_nodes: usize,
    entries: HashMap<String, Entry>,
}

impl Registry {
    /// 默认容量构造（稳定 API：单测＋未来调用方；线上 Worker 走 `with_capacity`）。
    /// 按 `reload_nodes` 惯例放行 dead。
    #[allow(dead_code)]
    pub fn new(ttl: Duration) -> Self {
        Self {
            ttl,
            max_nodes: 2000,
            entries: HashMap::new(),
        }
    }

    pub fn with_capacity(ttl: Duration, max_nodes: usize) -> Self {
        Self {
            ttl,
            max_nodes,
            entries: HashMap::new(),
        }
    }

    /// 质检通过即插入/刷新（已存在 addr：续期＋health 更新，不覆盖 source；首见获胜）。
    /// P2：全协议进池（Http/Https 走经典转发，Socks4/5 走翻译桥；出站形态存 `node.proto`，
    /// 选路隔离见 `RouterEngine::matches`）。
    /// v1 `upsert(raw, latency, now)` 语义由本函数替代（TCP-only 降级时 anon=Unknown/lat=tcp）。
    pub fn upsert_full(
        &mut self,
        raw: &RawNode,
        fwd_latency_ms: u64,
        anon: AnonLevel,
        exit_ip: Option<String>,
        now: Instant,
    ) {
        let addr = format!("{}:{}", raw.ip, raw.port);
        if let Some(e) = self.entries.get_mut(&addr) {
            e.expires_at = now + self.ttl;
            e.health.note_success(fwd_latency_ms);
            e.health.anon = anon;
            e.health.exit_ip = exit_ip.clone();
            // R3-2：快照节点同步真 egress（exit 轮转即更新；遥测 out_ip 与隔离消费）。
            e.node.exit_ip = exit_ip;
            e.node.weight = e.health.weight();
            log::debug!(
                "[FreePool] upsert {addr} anon={:?} exit={:?} score={:.3} weight={}",
                e.health.anon,
                e.health.exit_ip,
                e.health.score(),
                e.node.weight
            );
            return;
        }
        let mut health = Health::fresh();
        health.note_success(fwd_latency_ms);
        health.anon = anon;
        health.exit_ip = exit_ip.clone();
        let node = ProxyNode::new(
            raw.ip.clone(),
            raw.port,
            None,
            None,
            raw.country
                .clone()
                .unwrap_or_else(|| FREE_UNKNOWN_COUNTRY.to_string()),
            "free".to_string(),
            format!("free-{}", raw.source),
            health.weight(),
        )
        .with_proto(egress_of(raw.proto))
        .with_exit_ip(exit_ip);
        self.entries.insert(
            addr,
            Entry {
                node,
                health,
                expires_at: now + self.ttl,
            },
        );
        self.evict_if_over_capacity();
    }

    /// REVIEW-R2 Q3：基线缺失降级续命——只续 TTL，不碰 anon/exit_ip/权重/EWMA。
    /// TCP 建链成功≠转发成功，按整成功加权属虚增；匿名度以最近一次 FullCheck 为准。
    pub fn renew_ttl(&mut self, addr: &str, now: Instant) {
        if let Some(e) = self.entries.get_mut(addr) {
            e.expires_at = now + self.ttl;
        }
    }

    /// 复检失败（TCP/Full 任一）：EWMA 记失败＋backoff，TTL 内保留（自动恢复）。
    pub fn note_verify_failed(&mut self, addr: &str, now: Instant) {
        if let Some(e) = self.entries.get_mut(addr) {
            e.health.note_failure(now);
            e.node.weight = e.health.weight();
        }
    }

    /// 复检：通过由 `upsert_full` 续期；失败记 backoff（TTL 内保留，而非删除）。
    /// 当前 Worker 通过路径走 `upsert_full`（附带健康更新），本函数为稳定 API
    /// （轻量续命入口，供运维/Phase 3 调用），按 `reload_nodes` 惯例放行 dead。
    #[allow(dead_code)]
    pub fn reverify(&mut self, addr: &str, passed: bool, now: Instant) {
        if passed {
            if let Some(e) = self.entries.get_mut(addr) {
                e.expires_at = now + self.ttl;
            }
        } else {
            self.note_verify_failed(addr, now);
        }
    }

    /// 容量淘汰：先逐 backoff 条目（其中最低分），再逐全局最低分。
    /// REVIEW-R2 Q7：`now` 外提（每轮重取无意义）；排名依据与权重同基的
    /// `composite()`（原 `score()` 与 `weight()` 脱钩）；平局按 addr 终裁
    /// （HashMap 迭代随机，不得决定受害者）；分数非负故 `to_bits` 保序。
    fn evict_if_over_capacity(&mut self) {
        let now = Instant::now();
        while self.entries.len() > self.max_nodes {
            let victim = self
                .entries
                .iter()
                .filter(|(_, e)| e.health.backed_off(now))
                .min_by(|a, b| eviction_rank(a.0, a.1).cmp(&eviction_rank(b.0, b.1)))
                .or_else(|| {
                    self.entries
                        .iter()
                        .min_by(|a, b| eviction_rank(a.0, a.1).cmp(&eviction_rank(b.0, b.1)))
                })
                .map(|(k, _)| k.clone());
            match victim {
                Some(k) => {
                    self.entries.remove(&k);
                }
                None => break,
            }
        }
    }

    /// 可进池快照：未过期＋非 backoff＋匿名度门。
    /// `require_elite=true` 时仅 Elite（FREE_REQUIRE_ELITE=1）。
    pub fn snapshot(&self, now: Instant, require_elite: bool) -> Vec<ProxyNode> {
        self.entries
            .values()
            .filter(|e| e.expires_at > now)
            .filter(|e| !e.health.backed_off(now))
            .filter(|e| !require_elite || e.health.anon == AnonLevel::Elite)
            .map(|e| e.node.clone())
            .collect()
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }
}

/// REVIEW-R2 Q7：淘汰排名键（composite 分＋addr 终裁，见 `evict_if_over_capacity`）。
fn eviction_rank<'a>(addr: &'a str, e: &Entry) -> (u64, &'a str) {
    (e.health.composite().to_bits(), addr)
}

/// 源站零产出熔断（连续 MAX_ZERO_CYCLES 轮零产出→暂停；304 不计；每 N tick 试探）。
pub struct SourceGuard {
    max_zero: u32,
    retry_every: u64,
    zero_cycles: u32,
    suspended_at_tick: Option<u64>,
}

impl SourceGuard {
    pub fn new(max_zero: u32, retry_every: u64) -> Self {
        Self {
            max_zero: max_zero.max(1),
            retry_every: retry_every.max(1),
            zero_cycles: 0,
            suspended_at_tick: None,
        }
    }

    pub fn should_fetch(&self, tick: u64) -> bool {
        match self.suspended_at_tick {
            None => true,
            Some(t) => tick >= t && (tick - t).is_multiple_of(self.retry_every),
        }
    }

    /// 有产出/304→恢复或维持；零产出→计数，达阈值熔断（记录熔断 tick）。
    pub fn note_outcome(&mut self, outcome: &FetchOutcome, tick: u64) {
        if outcome.not_modified || !outcome.nodes.is_empty() {
            self.zero_cycles = 0;
            self.suspended_at_tick = None;
            return;
        }
        self.zero_cycles += 1;
        if self.zero_cycles >= self.max_zero {
            if self.suspended_at_tick.is_none() {
                log::warn!(
                    "[FreePool] source suspended after {} zero-yield cycles",
                    self.zero_cycles
                );
            }
            self.suspended_at_tick.get_or_insert(tick);
        }
    }

    pub fn suspended(&self) -> bool {
        self.suspended_at_tick.is_some()
    }
}

/// 并发抓取全源（`join_all` 按源序并发＋per-source 15s 超时；输出与输入同序，
/// 保证去重“首见获胜”确定性：调用方保证 `sources` 配置序＝优先级序）。
/// 暂停源跳过（其 guard 不计数，保持旧集）；超时/失败 hold 旧集（registry 不动）。
pub async fn fetch_all(
    sources: &[Box<dyn Source>],
    guards: &mut [SourceGuard],
    client: &reqwest::Client,
    tick: u64,
) -> Vec<RawNode> {
    let futs: Vec<_> = sources
        .iter()
        .enumerate()
        .filter(|(i, _)| guards[*i].should_fetch(tick))
        .map(|(i, s)| async move {
            let r = tokio::time::timeout(Duration::from_secs(15), s.fetch(client)).await;
            (i, r)
        })
        .collect();
    let results = futures::future::join_all(futs).await; // 输出与输入同序
    let mut raws = Vec::new();
    for (i, r) in results {
        match r {
            Ok(Ok(outcome)) => {
                guards[i].note_outcome(&outcome, tick);
                if !outcome.not_modified {
                    raws.extend(outcome.nodes);
                }
            }
            Ok(Err(e)) => {
                // REVIEW-R2 Q4：传输失败视同零产出计入熔断（否则恒错源每轮空耗 15s 超时）。
                // 304/有产出仍走恢复路径（`note_outcome` 内豁免），语义不变。
                guards[i].note_outcome(&FetchOutcome::nodes(Vec::new()), tick);
                log::warn!("[FreePool] source fetch failed: {e} (holding last good set)")
            }
            Err(_) => {
                guards[i].note_outcome(&FetchOutcome::nodes(Vec::new()), tick);
                log::warn!("[FreePool] source fetch timed out (15s, holding last good set)")
            }
        }
    }
    // 去重（首见获胜；源序＝配置序：api > html > github，调用方保证 sources 顺序）。
    let mut seen = HashSet::new();
    raws.retain(|r| seen.insert(format!("{}:{}", r.ip, r.port)));
    raws
}

/// Worker 配置（main 从 env 组装；默认值见计划 §2 Env 总表）。
#[derive(Debug, Clone)]
pub struct FreePoolConfig {
    pub api_urls: Vec<String>,
    pub html_urls: Vec<String>,
    pub github_urls: Vec<String>,
    pub fetch_interval: Duration,
    pub ttl: Duration,
    pub verify_timeout: Duration,
    pub max_latency_ms: u64,
    pub max_concurrent: usize,
    pub full_concurrent: usize,
    pub max_nodes: usize,
    pub full_check_base: String,
    pub require_elite: bool,
    pub max_zero_cycles: u32,
    pub suspend_retry_every: u64,
    /// P4-1 mismatch 执法开关（`GEOIP_ENFORCE_MISMATCH=1`；默认 false＝只观察）。
    pub geo_enforce: bool,
}

/// Source 默认 URL（全 env 可覆盖；单源挂了只 hold 旧集，不清空池）。
pub const DEFAULT_API_URL: &str = "https://proxylist.geonode.com/api/proxy-list?limit=100&page=1&sort_by=lastChecked&sort_type=desc";
pub const DEFAULT_HTML_URL: &str = "https://free-proxy-list.net/";
pub const DEFAULT_GITHUB_URL: &str =
    "https://raw.githubusercontent.com/clarketm/proxy-list/master/proxy-list-raw.txt";
pub const DEFAULT_FULL_CHECK_BASE: &str = "https://httpbin.org";

/// FullCheck 任务装配 helper（许可拿不到按失败计，沿 R2-7 `?`-in-bool 教训）。
fn spawn_full_check(
    set: &mut tokio::task::JoinSet<(RawNode, Option<FullCheckResult>)>,
    sem: Arc<tokio::sync::Semaphore>,
    checker: FullChecker,
    raw: RawNode,
    baseline: String,
) {
    set.spawn(async move {
        // REVIEW-R2 Q1：许可必须持有跨过 `check` await（临时值即时释放→并发无上限）。
        // 拿不到许可按失败计（沿 R2-7 `?`-in-bool 教训语义）。
        let Ok(_permit) = sem.acquire_owned().await else {
            return (raw, None);
        };
        let res = checker.check(&raw, &baseline).await;
        (raw, res)
    });
}

/// P4-1 执法映射（worker 与单测共用）：Mismatch 且开关开→按复检失败计
/// （backoff＋`geo_fail` 指标，TTL 内保留＋自动恢复，沿 `note_verify_failed` 语义）；
/// 其余一律放过（默认关＝Phase 3 逐行等价；miss/disabled 永不执法）。
fn apply_geo_verdict(
    registry: &mut Registry,
    metrics: &crate::metrics::MetricsRegistry,
    addr: &str,
    verdict: crate::geo::GeoVerdict,
    enforce: bool,
) {
    use crate::geo::GeoVerdict;
    if verdict == GeoVerdict::Mismatch && enforce {
        registry.note_verify_failed(addr, Instant::now());
        metrics.note_free_verify("geo_fail");
        log::warn!("[GeoIP] enforced mismatch on {addr} (backoff, auto-recover via reverify)");
    }
}

pub struct FreePoolWorker {
    router: Arc<RouterEngine>,
    metrics: Arc<crate::metrics::MetricsRegistry>,
    config: FreePoolConfig,
    registry: Registry,
    guards: Vec<SourceGuard>,
    tick: u64,
    /// 已泄漏源名 intern 池（URL 稳定时零增长；防每轮 `Box::leak` 微泄漏）。
    leaked_names: HashSet<&'static str>,
    /// P3 exit-IP 画像库（None＝Disabled 快捷；main 有 path 才建 Live，见 P3-4）。
    geo: Option<Arc<crate::geo::GeoDb>>,
}

impl FreePoolWorker {
    pub fn new(
        router: Arc<RouterEngine>,
        metrics: Arc<crate::metrics::MetricsRegistry>,
        config: FreePoolConfig,
    ) -> Self {
        let ttl = config.ttl;
        let max_nodes = config.max_nodes;
        let n_sources =
            (config.api_urls.len() + config.html_urls.len() + config.github_urls.len()).max(1);
        let guards = (0..n_sources)
            .map(|_| SourceGuard::new(config.max_zero_cycles, config.suspend_retry_every))
            .collect();
        Self {
            router,
            metrics,
            config,
            registry: Registry::with_capacity(ttl, max_nodes),
            guards,
            tick: 0,
            leaked_names: HashSet::new(),
            geo: None,
        }
    }

    /// P3：装配画像库（main 在 `GEOIP_MMDB_PATH` 可用时调用；单测默认 None）。
    pub fn with_geo(mut self, geo: Arc<crate::geo::GeoDb>) -> Self {
        self.geo = Some(geo);
        self
    }

    /// P3 geo 观察（P4 起返回判定供执法映射）：FullCheck 成功且有 exit_ip 时比对声明国家。
    /// mismatch 记指标＋debug；执法与否由调用方按 `config.geo_enforce` 决定（默认只观察）。
    /// 注：Disabled 时不注记（main 启动行 `[GeoIP] disabled` 已覆盖可观测；
    /// 首 tick 注记在 supervisor 重启 Workers 时会重复刷计数器，R3-3 已删）。
    fn observe_geo(&self, raw: &RawNode, exit_ip: Option<&str>) -> crate::geo::GeoVerdict {
        use crate::geo::{geo_verdict, GeoVerdict};
        let Some(geo) = self.geo.as_ref() else {
            return GeoVerdict::Skipped;
        };
        if !geo.enabled() {
            return GeoVerdict::Skipped;
        }
        let Some(exit) = exit_ip else {
            return GeoVerdict::Skipped; // 无 exit（降级分支）不查
        };
        match geo.country(exit) {
            Some(code) => {
                self.metrics.note_geo_lookup("hit");
                let verdict = geo_verdict(raw.country.as_deref(), Some(&code));
                if verdict == GeoVerdict::Mismatch {
                    self.metrics.note_geo_mismatch();
                    log::debug!(
                        "[GeoIP] mismatch {}:{} declared={:?} exit={exit} looked_up={code}",
                        raw.ip,
                        raw.port,
                        raw.country
                    );
                }
                verdict
            }
            None => {
                self.metrics.note_geo_lookup("miss");
                GeoVerdict::Skipped
            }
        }
    }

    /// 源名 intern（`Source::name` 需 `'static`；URL 稳定时集合大小恒定）。
    fn intern_name(&mut self, s: String) -> &'static str {
        if let Some(existing) = self.leaked_names.get(s.as_str()).copied() {
            return existing;
        }
        let leaked: &'static str = Box::leak(s.into_boxed_str());
        self.leaked_names.insert(leaked);
        leaked
    }

    /// 组装三类源（配置序＝去重优先级：api > html > github；URL 集合变化时重建
    /// guards 以对齐下标）。
    fn build_sources(&mut self) -> Vec<Box<dyn Source>> {
        // 先克隆 URL 表（避免 config 不可变借用与 intern_name 可变借用冲突）。
        let api_urls = self.config.api_urls.clone();
        let html_urls = self.config.html_urls.clone();
        let github_urls = self.config.github_urls.clone();
        let mut sources: Vec<Box<dyn Source>> = Vec::new();
        for (i, url) in api_urls.iter().enumerate() {
            let name = self.intern_name(format!("api{i}"));
            sources.push(Box::new(ApiSource::new(name, url.clone())));
        }
        for (i, url) in html_urls.iter().enumerate() {
            let name = self.intern_name(format!("html{i}"));
            sources.push(Box::new(HtmlSource::new(
                name,
                url.clone(),
                FreeProto::Http,
            )));
        }
        for (i, url) in github_urls.iter().enumerate() {
            let name = self.intern_name(format!("gh{i}"));
            sources.push(Box::new(GitHubSource::new(name, url.clone())));
        }
        if self.guards.len() != sources.len() {
            self.guards = (0..sources.len())
                .map(|_| {
                    SourceGuard::new(self.config.max_zero_cycles, self.config.suspend_retry_every)
                })
                .collect();
        }
        sources
    }

    /// 单轮：并发抓取 → TCP 初筛（信号量封顶）→ FullCheck 复检（信号量 20，
    /// 基址探活失败则降级 TCP-only，anon=Unknown）→ upsert/失败记 backoff →
    /// 过期自然掉出 → merge（require_elite 门）→ 水位计＋扩展指标。
    pub async fn run_once(&mut self, client: &reqwest::Client) {
        self.tick += 1;
        let tick = self.tick;
        // 1-2. 组装＋并发抓取（失败 hold 旧集）＋ suspend 指标同步。
        let sources = self.build_sources();
        let raws = fetch_all(&sources, &mut self.guards, client, tick).await;
        for (i, s) in sources.iter().enumerate() {
            self.metrics
                .set_free_source_suspended(s.name(), self.guards[i].suspended());
        }
        // yield 按源聚合（>0 才记行；零产出源由 suspend gauge 覆盖）。
        {
            let mut per_source: HashMap<&str, u64> = HashMap::new();
            for r in &raws {
                *per_source.entry(r.source.as_str()).or_insert(0) += 1;
            }
            for (s, n) in per_source {
                self.metrics.note_free_source_yield(s, n);
            }
        }
        // 3. 直连基线：GET {base}/ip → origin（5s 超时；失败→None＝降级 TCP-only）。
        // 基线获取失败不计节点失败（源站侧问题，非节点问题）。
        let baseline: Option<String> = {
            let r = tokio::time::timeout(
                Duration::from_secs(5),
                client
                    .get(format!("{}/ip", self.config.full_check_base))
                    .send(),
            )
            .await;
            match r {
                Ok(Ok(resp)) => resp.json::<serde_json::Value>().await.ok().and_then(|v| {
                    v.get("origin")
                        .and_then(|o| o.as_str())
                        .map(|s| s.to_string())
                }),
                _ => None,
            }
        };
        if baseline.is_none() {
            log::warn!("[FreePool] baseline unreachable, degrading to TCP-only this tick");
        }
        // 4a. 初筛（信号量 max_concurrent）：Http/Https 走 TCP 建链，
        // Socks4/5 走握手/CONNECT 验证（目标＝复检基址 host，见 with_socks_target）。
        let mut verifier = Verifier::new(self.config.verify_timeout, self.config.max_latency_ms);
        if let Some((host, port)) = socks_target_of_base(&self.config.full_check_base) {
            verifier = verifier.with_socks_target(host, port);
        }
        let tcp_sem = Arc::new(tokio::sync::Semaphore::new(
            self.config.max_concurrent.max(1),
        ));
        let mut set = tokio::task::JoinSet::new();
        for raw in raws {
            let sem = tcp_sem.clone();
            let vf = verifier.clone();
            set.spawn(async move {
                // REVIEW-R2 Q1：同上，许可持有跨过 `verify` await（`pool.rs:92` 同形态）。
                let Ok(_permit) = sem.acquire_owned().await else {
                    return (raw, None);
                };
                let latency = vf.verify(&raw).await;
                (raw, latency)
            });
        }
        // 4b. 通过 TCP 者→FullCheck（信号量 full_concurrent；基线缺失则跳过，
        // anon=Unknown＋tcp 延迟；REQUIRE_ELITE=1 时此类条目被 merge 门过滤，语义自洽）。
        let full_sem = Arc::new(tokio::sync::Semaphore::new(
            self.config.full_concurrent.max(1),
        ));
        let checker = FullChecker::new(
            self.config.full_check_base.clone(),
            self.config.verify_timeout,
        );
        let now = Instant::now();
        let mut fset = tokio::task::JoinSet::new();
        while let Some(joined) = set.join_next().await {
            match joined {
                Ok((raw, Some(_))) => {
                    if let Some(ref base) = baseline {
                        spawn_full_check(
                            &mut fset,
                            full_sem.clone(),
                            checker.clone(),
                            raw,
                            base.clone(),
                        );
                    } else {
                        // REVIEW-R2 Q3：降级只续命（存量条目续 TTL；新 addr 本轮不进池，
                        // 下轮基址恢复后走正常 FullCheck，避免 Unknown 洗掉 Elite）。
                        self.registry
                            .renew_ttl(&format!("{}:{}", raw.ip, raw.port), now);
                        self.metrics.note_free_verify("pass");
                        self.metrics.note_free_anonymity("unknown");
                    }
                }
                Ok((raw, None)) => {
                    self.metrics.note_free_verify("tcp_fail");
                    self.registry
                        .note_verify_failed(&format!("{}:{}", raw.ip, raw.port), now);
                }
                Err(e) => log::warn!("[FreePool] tcp task join failed: {e:?}"),
            }
        }
        while let Some(joined) = fset.join_next().await {
            match joined {
                Ok((raw, Some(res))) => {
                    self.registry.upsert_full(
                        &raw,
                        res.fwd_latency_ms,
                        res.anon,
                        res.exit_ip.clone(),
                        now,
                    );
                    self.metrics.note_free_verify("pass");
                    self.metrics.note_free_anonymity(match res.anon {
                        AnonLevel::Elite => "elite",
                        AnonLevel::Anonymous => "anonymous",
                        AnonLevel::Transparent => "transparent",
                        AnonLevel::Unknown => "unknown",
                    });
                    // P4-1 geo 观察＋执法映射（默认只观察；开关开且实锤分歧→复检失败）。
                    let verdict = self.observe_geo(&raw, res.exit_ip.as_deref());
                    if self.config.geo_enforce {
                        apply_geo_verdict(
                            &mut self.registry,
                            &self.metrics,
                            &format!("{}:{}", raw.ip, raw.port),
                            verdict,
                            true,
                        );
                    }
                }
                Ok((raw, None)) => {
                    self.metrics.note_free_verify("full_fail");
                    self.registry
                        .note_verify_failed(&format!("{}:{}", raw.ip, raw.port), now);
                }
                Err(e) => log::warn!("[FreePool] full task join failed: {e:?}"),
            }
        }
        // 5. 合并（过期/backoff 条目自然掉出快照）＋ tick 日志。
        Self::merge_once(
            &self.router,
            &self.metrics,
            &self.registry,
            self.config.require_elite,
        );
        log::info!(
            "[FreePool] tick={tick} pool={} interval={:?}",
            self.registry.len(),
            self.config.fetch_interval
        );
    }

    /// 纯合并步（可单测）：快照 → 路由 → 水位计（含按协议水位）。
    pub fn merge_once(
        router: &Arc<RouterEngine>,
        metrics: &Arc<crate::metrics::MetricsRegistry>,
        registry: &Registry,
        require_elite: bool,
    ) {
        let now = Instant::now();
        let snap = registry.snapshot(now, require_elite);
        metrics.set_free_pool_nodes(snap.len() as u64);
        // 按出站协议聚合水位（三档恒设 0/数值，仪表盘行稳定；P2-5）。
        let (mut http, mut s5, mut s4) = (0u64, 0u64, 0u64);
        for n in &snap {
            match n.proto {
                EgressProto::Http => http += 1,
                EgressProto::Socks5 => s5 += 1,
                EgressProto::Socks4 => s4 += 1,
            }
        }
        metrics.set_free_pool_nodes_proto("http", http);
        metrics.set_free_pool_nodes_proto("socks5", s5);
        metrics.set_free_pool_nodes_proto("socks4", s4);
        router.replace_vendor_nodes("free-", snap);
    }

    /// 常驻循环（supervisor 托管；单轮 panic 由 supervisor 捕获重启）。
    pub async fn run(mut self, client: reqwest::Client) {
        loop {
            self.run_once(&client).await;
            tokio::time::sleep(self.config.fetch_interval).await;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn api_source_parses_geonode_shape() {
        let body = r#"{"data":[
            {"ip":"203.0.113.7","port":"8080","protocols":["http"],"country":"US"},
            {"ip":"198.51.100.9","port":3128,"protocols":["https"],"country":"DE"},
            {"ip":"bad","port":"x","protocols":["http"],"country":"US"},
            {"ip":"192.0.2.1","port":"1080","protocols":["socks5"],"country":"US"}
        ]}"#;
        let nodes = ApiSource::parse("geonode", body);
        // 坏行丢弃；socks5 解析保留（Phase 1 由 Verifier 跳过，不在此处丢）。
        assert_eq!(nodes.len(), 3);
        assert_eq!(nodes[0].port, 8080);
        assert_eq!(nodes[0].proto, FreeProto::Http);
        assert_eq!(nodes[1].proto, FreeProto::Https);
        assert_eq!(nodes[2].proto, FreeProto::Socks5);
        assert_eq!(nodes[0].country.as_deref(), Some("US"));
    }

    #[test]
    fn html_source_extracts_ip_ports() {
        let html = r#"<table><tr><td>203.0.113.7</td><td>8080</td><td>yes</td></tr>
            <tr><td>999.1.1.1</td><td>80</td></tr>
            <tr><td>198.51.100.9:3128</td></tr>
            <tr><td>10.0.0.1</td><td>70000</td></tr></table>"#;
        let nodes = HtmlSource::extract("fpl", html, FreeProto::Http);
        // 非法 octet/超范围端口丢弃；`ip:port` 紧凑形态同样识别。
        assert_eq!(nodes.len(), 2);
        assert_eq!((nodes[0].ip.as_str(), nodes[0].port), ("203.0.113.7", 8080));
        assert_eq!(
            (nodes[1].ip.as_str(), nodes[1].port),
            ("198.51.100.9", 3128)
        );
        assert!(nodes
            .iter()
            .all(|n| n.proto == FreeProto::Http && n.source == "fpl"));
    }

    #[test]
    fn github_source_parses_line_list() {
        let body =
            "203.0.113.7:8080\n\n# comment\n198.51.100.9:3128 socks5\nbad-line\n10.0.0.1:0\n";
        let nodes = GitHubSource::parse("gh", body);
        assert_eq!(nodes.len(), 2);
        assert_eq!(nodes[0].proto, FreeProto::Http);
        assert_eq!(nodes[1].proto, FreeProto::Socks5);
    }

    #[tokio::test]
    async fn github_source_fetches_from_local_server() {
        // 零外部依赖：本地临时 HTTP 服务冒充 raw 仓（沿用 metrics 单测手法换端口）。
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind");
        let port = listener.local_addr().expect("addr").port();
        tokio::spawn(async move {
            let (mut s, _) = listener.accept().await.expect("accept");
            use tokio::io::{AsyncReadExt, AsyncWriteExt};
            let mut buf = [0u8; 1024];
            let _ = s.read(&mut buf).await;
            let body = "203.0.113.7:8080\n";
            let _ = s
                .write_all(
                    format!(
                        "HTTP/1.1 200 OK\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                        body.len()
                    )
                    .as_bytes(),
                )
                .await;
        });
        let client = reqwest::Client::new();
        let src = GitHubSource::new("gh", format!("http://127.0.0.1:{port}/list.txt"));
        let outcome = src.fetch(&client).await.expect("fetch");
        assert!(!outcome.not_modified);
        assert_eq!(outcome.nodes.len(), 1);
        assert_eq!(outcome.nodes[0].ip, "203.0.113.7");
    }

    #[tokio::test]
    async fn github_source_etag_not_modified() {
        // ETag 礼貌轮询：首轮存 ETag；次轮带 If-None-Match，被 304 后 not_modified=true
        // 且调用方（SourceGuard）不得计零产出（见 Task 10 单测联动）。
        use std::sync::{Arc, Mutex as StdMutex};
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let seen_if_none_match: Arc<StdMutex<Vec<String>>> = Arc::new(StdMutex::new(Vec::new()));
        let seen = seen_if_none_match.clone();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind");
        let port = listener.local_addr().expect("addr").port();
        tokio::spawn(async move {
            for _ in 0..2 {
                let (mut s, _) = listener.accept().await.expect("accept");
                let mut buf = [0u8; 2048];
                let n = s.read(&mut buf).await.unwrap_or(0);
                let req = String::from_utf8_lossy(&buf[..n]).to_string();
                let inm = req
                    .lines()
                    .find(|l| l.to_ascii_lowercase().starts_with("if-none-match:"))
                    .map(|l| {
                        l.split_once(':')
                            .map(|(_, v)| v.trim().to_string())
                            .unwrap_or_default()
                    })
                    .unwrap_or_default();
                seen.lock().expect("lock").push(inm.clone());
                if inm == "\"v1\"" {
                    let _ = s
                        .write_all(b"HTTP/1.1 304 Not Modified\r\nconnection: close\r\n\r\n")
                        .await;
                } else {
                    let body = "203.0.113.7:8080\n";
                    let _ = s
                        .write_all(
                            format!(
                                "HTTP/1.1 200 OK\r\nETag: \"v1\"\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                                body.len()
                            )
                            .as_bytes(),
                        )
                        .await;
                }
            }
        });
        let client = reqwest::Client::new();
        let src = GitHubSource::new("gh", format!("http://127.0.0.1:{port}/list.txt"));
        let first = src.fetch(&client).await.expect("first");
        assert_eq!(first.nodes.len(), 1);
        assert!(!first.not_modified);
        let second = src.fetch(&client).await.expect("second");
        assert!(second.not_modified);
        assert!(second.nodes.is_empty());
        let seen = seen_if_none_match.lock().expect("lock");
        assert_eq!(seen.len(), 2);
        assert!(
            seen[1].contains("v1"),
            "second request must carry If-None-Match, got {seen:?}"
        );
    }

    /// 通用 ETag stub：首轮 200＋ETag＋给定 body；次轮见 If-None-Match 含 v1 即 304。
    /// 返回（端口，收到的 If-None-Match 记录）。R3-5 供 Api/Html 礼貌轮询断言。
    async fn spawn_etag_stub(
        body_200: &'static str,
    ) -> (u16, std::sync::Arc<std::sync::Mutex<Vec<String>>>) {
        use std::sync::{Arc, Mutex as StdMutex};
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let seen: Arc<StdMutex<Vec<String>>> = Arc::new(StdMutex::new(Vec::new()));
        let seen2 = seen.clone();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind");
        let port = listener.local_addr().expect("addr").port();
        tokio::spawn(async move {
            for _ in 0..2 {
                let (mut s, _) = listener.accept().await.expect("accept");
                let mut buf = [0u8; 4096];
                let n = s.read(&mut buf).await.unwrap_or(0);
                let req = String::from_utf8_lossy(&buf[..n]).to_string();
                let inm = req
                    .lines()
                    .find(|l| l.to_ascii_lowercase().starts_with("if-none-match:"))
                    .map(|l| {
                        l.split_once(':')
                            .map(|(_, v)| v.trim().to_string())
                            .unwrap_or_default()
                    })
                    .unwrap_or_default();
                seen2.lock().expect("lock").push(inm.clone());
                if inm.contains("v1") {
                    let _ = s
                        .write_all(b"HTTP/1.1 304 Not Modified\r\nconnection: close\r\n\r\n")
                        .await;
                } else {
                    let _ = s
                        .write_all(
                            format!(
                                "HTTP/1.1 200 OK\r\nETag: \"v1\"\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body_200}",
                                body_200.len()
                            )
                            .as_bytes(),
                        )
                        .await;
                }
            }
        });
        (port, seen)
    }

    #[tokio::test]
    async fn api_source_etag_not_modified() {
        // R3-5：ApiSource 礼貌轮询（对齐 GitHub）：首轮存 ETag，次轮 304→not_modified。
        let body =
            r#"{"data":[{"ip":"203.0.113.7","port":"8080","protocols":["http"],"country":"US"}]}"#;
        let (port, seen) = spawn_etag_stub(body).await;
        let client = reqwest::Client::new();
        let src = ApiSource::new("api", format!("http://127.0.0.1:{port}/api"));
        let first = src.fetch(&client).await.expect("first");
        assert_eq!(first.nodes.len(), 1);
        assert!(!first.not_modified);
        let second = src.fetch(&client).await.expect("second");
        assert!(second.not_modified);
        assert!(second.nodes.is_empty());
        let seen = seen.lock().expect("lock");
        assert_eq!(seen.len(), 2);
        assert!(seen[1].contains("v1"));
    }

    #[tokio::test]
    async fn html_source_etag_not_modified() {
        // R3-5：HtmlSource 礼貌轮询同理。
        let body = "<table><tr><td>203.0.113.7</td><td>8080</td></tr></table>";
        let (port, seen) = spawn_etag_stub(body).await;
        let client = reqwest::Client::new();
        let src = HtmlSource::new(
            "html",
            format!("http://127.0.0.1:{port}/list"),
            FreeProto::Http,
        );
        let first = src.fetch(&client).await.expect("first");
        assert_eq!(first.nodes.len(), 1);
        assert!(!first.not_modified);
        let second = src.fetch(&client).await.expect("second");
        assert!(second.not_modified);
        assert!(second.nodes.is_empty());
        let seen = seen.lock().expect("lock");
        assert_eq!(seen.len(), 2);
        assert!(seen[1].contains("v1"));
    }

    #[tokio::test]
    async fn verifier_admits_fast_listener() {
        // 本地 listener 必连上（OPT-6 手法），延迟门内放行。
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind");
        let port = listener.local_addr().expect("addr").port();
        let v = Verifier::new(Duration::from_secs(3), 3000);
        let raw = RawNode {
            ip: "127.0.0.1".to_string(),
            port,
            proto: FreeProto::Http,
            country: None,
            source: "t".to_string(),
        };
        let ok = v.verify(&raw).await;
        assert!(ok.is_some());
        assert!(ok.expect("latency") < 3000);
        drop(listener);
    }

    #[tokio::test]
    async fn verifier_rejects_refused_and_slow() {
        // 拒连端口（127.0.0.1:1）必失败；socks 协议 Phase 1 跳过（None）。
        let v = Verifier::new(Duration::from_secs(3), 3000);
        let refused = RawNode {
            ip: "127.0.0.1".to_string(),
            port: 1,
            proto: FreeProto::Http,
            country: None,
            source: "t".to_string(),
        };
        assert!(v.verify(&refused).await.is_none());
        let socks = RawNode {
            ip: "127.0.0.1".to_string(),
            port: 1,
            proto: FreeProto::Socks5,
            country: None,
            source: "t".to_string(),
        };
        assert!(v.verify(&socks).await.is_none());
    }

    #[test]
    fn anonymity_classification_matrix() {
        // 三级矩阵（openproxyhub 定义）：出口==基线→Transparent；否则 7 头有披露→Anonymous；无→Elite。
        use std::collections::HashMap;
        let empty: HashMap<String, String> = HashMap::new();
        assert_eq!(
            classify_anonymity("1.1.1.1", Some("1.1.1.1"), &empty),
            AnonLevel::Transparent
        );
        assert_eq!(
            classify_anonymity("1.1.1.1", Some("9.9.9.9"), &empty),
            AnonLevel::Elite
        );
        let mut disclosed = HashMap::new();
        disclosed.insert("via".to_string(), "1.0 proxy".to_string());
        assert_eq!(
            classify_anonymity("1.1.1.1", Some("9.9.9.9"), &disclosed),
            AnonLevel::Anonymous
        );
        let mut upper = HashMap::new();
        upper.insert("X-Forwarded-For".to_string(), "1.1.1.1".to_string());
        assert_eq!(
            classify_anonymity("1.1.1.1", Some("9.9.9.9"), &upper),
            AnonLevel::Anonymous
        );
        // 出口未知（代理失败）→Unknown，调用方按失败计。
        assert_eq!(
            classify_anonymity("1.1.1.1", None, &empty),
            AnonLevel::Unknown
        );
    }

    #[tokio::test]
    async fn checker_rejects_refused_proxy() {
        // 拒连代理（127.0.0.1:1）→ None（失败），不 panic。
        let c = FullChecker::new("http://127.0.0.1:1/".to_string(), Duration::from_secs(3));
        let raw = RawNode {
            ip: "127.0.0.1".to_string(),
            port: 1,
            proto: FreeProto::Http,
            country: None,
            source: "t".to_string(),
        };
        assert!(c.check(&raw, "9.9.9.9").await.is_none());
    }

    /// 本地微型 HTTP 代理 stub（单测内联 ~40 行）：读绝对 URI 首行，按 path
    /// 返回固定 JSON（`/ip` 出口／`/headers` 回显空头／`/anything` canary）。
    /// `tamper=true` 时 canary 的 url 缺 marker（模拟内容篡改）。
    async fn spawn_stub_proxy(tamper: bool) -> u16 {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind");
        let port = listener.local_addr().expect("addr").port();
        tokio::spawn(async move {
            // 一次 check 发 3 请求（server 回 connection:close，故 3 连接）。
            for _ in 0..3 {
                let (mut s, _) = match listener.accept().await {
                    Ok(v) => v,
                    Err(_) => return,
                };
                let mut buf = [0u8; 4096];
                let n = s.read(&mut buf).await.unwrap_or(0);
                let req = String::from_utf8_lossy(&buf[..n]).to_string();
                let path = req
                    .lines()
                    .next()
                    .unwrap_or("")
                    .split_whitespace()
                    .nth(1)
                    .unwrap_or("/")
                    .to_string();
                // 绝对 URI（经代理）与源形态（直连）双兼容：截 path。
                let path = path
                    .split_once("://")
                    .map(|(_, rest)| rest.find('/').map(|i| &rest[i..]).unwrap_or("/"))
                    .unwrap_or(&path)
                    .to_string();
                let body = if path.starts_with("/ip") {
                    r#"{"origin":"10.9.9.9"}"#.to_string()
                } else if path.starts_with("/headers") {
                    r#"{"headers":{}}"#.to_string()
                } else if path.contains(FULL_CHECK_MARKER) {
                    if tamper {
                        r#"{"url":"tampered"}"#.to_string()
                    } else {
                        // 显式位置参数（不用内联作用域捕获：raw 字符串大括号＋捕获式混写
                        // 在部分 rust-analyzer 版本误报，见硬化记录）。
                        format!(r#"{{"url":"http://x/anything/{}"}}"#, FULL_CHECK_MARKER)
                    }
                } else {
                    r#"{}"#.to_string()
                };
                let _ = s
                    .write_all(
                        format!(
                            "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                            body.len()
                        )
                        .as_bytes(),
                    )
                    .await;
            }
        });
        port
    }

    #[tokio::test]
    async fn checker_detects_tampered_canary() {
        // 篡改 stub（canary url 缺 marker）→ check 判失败（None）。
        let stub = spawn_stub_proxy(true).await;
        let c = FullChecker::new(format!("http://127.0.0.1:{stub}"), Duration::from_secs(3));
        let raw = RawNode {
            ip: "127.0.0.1".to_string(),
            port: stub,
            proto: FreeProto::Http,
            country: None,
            source: "t".to_string(),
        };
        assert!(c.check(&raw, "1.2.3.4").await.is_none());
    }

    #[tokio::test]
    async fn checker_records_forward_latency() {
        // 正常 stub：出口 10.9.9.9 ≠ 基线 → Elite；转发延迟 < 3000ms。
        let stub = spawn_stub_proxy(false).await;
        let c = FullChecker::new(format!("http://127.0.0.1:{stub}"), Duration::from_secs(3));
        let raw = RawNode {
            ip: "127.0.0.1".to_string(),
            port: stub,
            proto: FreeProto::Http,
            country: None,
            source: "t".to_string(),
        };
        let res = c.check(&raw, "1.2.3.4").await.expect("pass");
        assert_eq!(res.anon, AnonLevel::Elite);
        assert_eq!(res.exit_ip.as_deref(), Some("10.9.9.9"));
        assert!(res.fwd_latency_ms < 3000);
    }

    fn raw(ip: &str, source: &str) -> RawNode {
        RawNode {
            ip: ip.to_string(),
            port: 8080,
            proto: FreeProto::Http,
            country: Some("US".to_string()),
            source: source.to_string(),
        }
    }

    #[test]
    fn registry_ttl_expiry() {
        // TTL 到即快照不可见（可测版本注入未来时间，沿用 sweep_expired_at 手法）。
        let mut reg = Registry::new(Duration::from_secs(1800));
        reg.upsert_full(
            &raw("10.0.0.1", "a"),
            50,
            AnonLevel::Elite,
            None,
            Instant::now(),
        );
        assert_eq!(reg.snapshot(Instant::now(), false).len(), 1);
        assert!(reg
            .snapshot(Instant::now() + Duration::from_secs(1801), false)
            .is_empty());
    }

    #[test]
    fn registry_stores_exit_ip() {
        // R3-2：upsert 的 exit_ip 落到快照节点（遥测 out_ip 用）；刷新即更新（exit 轮转不保留旧值）。
        let mut reg = Registry::new(Duration::from_secs(1800));
        let now = Instant::now();
        reg.upsert_full(
            &raw("10.0.0.1", "a"),
            50,
            AnonLevel::Elite,
            Some("10.9.9.9".to_string()),
            now,
        );
        let snap = reg.snapshot(now, false);
        assert_eq!(snap[0].exit_ip.as_deref(), Some("10.9.9.9"));
        reg.upsert_full(
            &raw("10.0.0.1", "a"),
            60,
            AnonLevel::Elite,
            Some("10.9.9.10".to_string()),
            now,
        );
        let snap2 = reg.snapshot(now, false);
        assert_eq!(snap2.len(), 1);
        assert_eq!(snap2[0].exit_ip.as_deref(), Some("10.9.9.10"));
    }

    #[test]
    fn enforce_maps_mismatch_to_failure() {
        // P4-1：enforce 开＋Mismatch→backoff（快照不可见但条目保留，沿 note_verify_failed 语义，
        // TTL 内自动恢复）；enforce 关＋Mismatch→仍在池（只记指标，Phase 3 行为）。
        use crate::geo::GeoVerdict;
        use crate::metrics::MetricsRegistry;
        let now = Instant::now();
        let m = MetricsRegistry::new();
        let mut reg = Registry::new(Duration::from_secs(1800));
        reg.upsert_full(
            &raw("10.0.0.1", "a"),
            50,
            AnonLevel::Elite,
            Some("10.9.9.9".to_string()),
            now,
        );
        apply_geo_verdict(&mut reg, &m, "10.0.0.1:8080", GeoVerdict::Mismatch, true);
        assert!(reg.snapshot(now, false).is_empty());
        assert_eq!(reg.len(), 1);
        assert!(m
            .render()
            .contains("free_pool_verify_total{result=\"geo_fail\"} 1"));
        let mut reg2 = Registry::new(Duration::from_secs(1800));
        reg2.upsert_full(
            &raw("10.0.0.2", "a"),
            50,
            AnonLevel::Elite,
            Some("10.9.9.9".to_string()),
            now,
        );
        apply_geo_verdict(&mut reg2, &m, "10.0.0.2:8080", GeoVerdict::Mismatch, false);
        assert_eq!(reg2.snapshot(now, false).len(), 1);
    }

    #[test]
    fn registry_reverify_renews() {
        // 复检：失败记 backoff（TTL 内保留），成功路径由 upsert_full 续期。
        let mut reg = Registry::new(Duration::from_secs(1800));
        let now = Instant::now();
        reg.upsert_full(&raw("10.0.0.1", "a"), 50, AnonLevel::Elite, None, now);
        // TTL 1800s：2000s 处已过期不可见；1000s 处（TTL 内）可见。
        assert_eq!(
            reg.snapshot(now + Duration::from_secs(2000), false).len(),
            0
        );
        // 新鲜条目在 TTL 内可见。
        assert_eq!(
            reg.snapshot(now + Duration::from_secs(1000), false).len(),
            1
        );
        // 失败→backoff：快照不可见但条目保留（len 不变）。
        reg.reverify("10.0.0.1:8080", false, now + Duration::from_secs(1000));
        assert!(reg
            .snapshot(now + Duration::from_secs(1000), false)
            .is_empty());
        assert_eq!(reg.len(), 1);
    }

    #[test]
    fn registry_dedupes_across_sources() {
        // 多源同 addr 只留首见 source；快照转 ProxyNode（provider free-{source}/tier free/weight/country 缺省 ZZ）。
        let mut reg = Registry::new(Duration::from_secs(1800));
        let now = Instant::now();
        reg.upsert_full(&raw("10.0.0.1", "a"), 50, AnonLevel::Elite, None, now);
        reg.upsert_full(&raw("10.0.0.1", "b"), 60, AnonLevel::Elite, None, now);
        let snap = reg.snapshot(now, false);
        assert_eq!(snap.len(), 1);
        assert_eq!(snap[0].provider, "free-a");
        assert_eq!(snap[0].tier, "free");
        assert_eq!(snap[0].addr, "10.0.0.1:8080");
        let mut no_country = raw("10.0.0.2", "a");
        no_country.country = None;
        reg.upsert_full(&no_country, 50, AnonLevel::Elite, None, now);
        // 快照为 HashMap 迭代序（不定），按 addr 查找断言 ZZ 缺省。
        let snap = reg.snapshot(now, false);
        assert_eq!(snap.len(), 2);
        let zz = snap
            .iter()
            .find(|n| n.addr == "10.0.0.2:8080")
            .expect("zz node");
        assert_eq!(zz.country, "ZZ");
    }

    #[test]
    fn health_score_math() {
        // 健康分＝EWMA成功率×延迟惩罚；权重 1..20 连续映射；trusted 加成封顶 20。
        let mut h = Health::fresh();
        assert!((h.score() - 0.5).abs() < 1e-9);
        // Elite＋快＋全成功→trusted→封顶 20。
        h.anon = AnonLevel::Elite;
        for _ in 0..50 {
            h.note_success(100);
        }
        assert!(h.ewma_success > 0.99);
        assert!(h.trusted());
        assert_eq!(h.weight(), 20);
        // 高延迟→惩罚生效（Unknown 非 trusted，cap 10）。
        let mut slow = Health::fresh();
        for _ in 0..50 {
            slow.note_success(9000);
        }
        assert!(slow.score() < h.score(), "latency penalty must bite");
        assert!(slow.weight() < 20);
        // 连败→保底 1（不断流，只降权）。
        let mut bad = Health::fresh();
        for _ in 0..10 {
            bad.note_failure(Instant::now());
        }
        assert_eq!(bad.weight(), 1);
    }

    #[test]
    fn composite_rewards_streak() {
        // P3-3：composite＝score×(0.5＋0.5×min(1,streak/3))。
        // fresh（streak 0）→ 0.25（score 本体 0.5 冻结）；50 连成功→factor 1→composite==score；
        // 单次成功（streak 1）介于两者之间；连败比同 score 更低（方向不断精确值）。
        let fresh = Health::fresh();
        assert!((fresh.composite() - 0.25).abs() < 1e-9);
        let mut hot = Health::fresh();
        hot.anon = AnonLevel::Elite;
        for _ in 0..50 {
            hot.note_success(100);
        }
        assert!((hot.composite() - hot.score()).abs() < 1e-9);
        let mut warm = Health::fresh();
        warm.note_success(100);
        assert!(warm.composite() > fresh.composite());
        assert!(warm.composite() < warm.score());
        assert!(warm.composite() < hot.composite());
    }

    #[test]
    fn backoff_and_capacity() {
        // backoff：连续失败→merge 不可见；TTL 内保留；成功恢复。
        // 容量：超量逐最低分淘汰（backoff 优先）。
        let mut reg = Registry::with_capacity(Duration::from_secs(1800), 2);
        let now = Instant::now();
        reg.upsert_full(&raw("10.0.0.1", "a"), 50, AnonLevel::Elite, None, now);
        reg.upsert_full(&raw("10.0.0.2", "a"), 50, AnonLevel::Elite, None, now);
        // 灌第三个→淘汰最低分之一（初分相同，允许淘汰任一，但总数恒 2）。
        reg.upsert_full(&raw("10.0.0.3", "a"), 50, AnonLevel::Elite, None, now);
        assert_eq!(reg.snapshot(now, false).len(), 2);
        // backoff：连 fail 3 次→快照不可见但 len 仍计入（TTL 内保留）。
        // 注：容量淘汰后 10.0.0.2 可能已被逐出；取快照现存任一节点做 backoff。
        let victim = reg.snapshot(now, false)[0].addr.clone();
        for _ in 0..3 {
            reg.note_verify_failed(&victim, now);
        }
        let snap = reg.snapshot(now, false);
        assert!(snap.iter().all(|n| n.addr != victim));
        assert_eq!(reg.len(), 2);
    }

    #[test]
    fn source_guard_trips_and_recovers() {
        // 连续 3 轮零产出→暂停；304 不计数；暂停后每 3 tick 试探一次；有产出即恢复。
        let mut g = SourceGuard::new(3, 3);
        assert!(g.should_fetch(0));
        g.note_outcome(&FetchOutcome::nodes(vec![]), 0);
        g.note_outcome(&FetchOutcome::nodes(vec![]), 1);
        assert!(g.should_fetch(2));
        g.note_outcome(&FetchOutcome::nodes(vec![]), 2); // 第 3 轮零产出→熔断
        assert!(!g.should_fetch(3), "suspended");
        assert!(!g.should_fetch(4));
        assert!(g.should_fetch(5), "probe every 3rd tick"); // tick 2 熔断→5 试探（2+3）
                                                            // 304 不计轮次。
        let mut g2 = SourceGuard::new(3, 3);
        g2.note_outcome(&FetchOutcome::not_modified(), 0);
        g2.note_outcome(&FetchOutcome::nodes(vec![]), 1);
        assert!(g2.should_fetch(2), "304 must not count as zero-yield");
        // 有产出清零。
        g.note_outcome(&FetchOutcome::nodes(vec![raw("10.0.0.1", "a")]), 5);
        assert!(g.should_fetch(6));
    }

    #[test]
    fn degraded_baseline_keeps_anonymity() {
        // REVIEW-R2 Q3：基线缺失降级续命只续 TTL，不得把存量 Elite 洗成 Unknown、
        // exit_ip 洗成 None、权重打回初值（同文件子模块，直读 entries 断言）。
        let mut reg = Registry::with_capacity(Duration::from_secs(1800), 10);
        let now = Instant::now();
        let raw9 = RawNode {
            ip: "10.0.0.9".to_string(),
            port: 8080,
            proto: FreeProto::Http,
            country: Some("US".to_string()),
            source: "a".to_string(),
        };
        reg.upsert_full(
            &raw9,
            120,
            AnonLevel::Elite,
            Some("9.9.9.9".to_string()),
            now,
        );
        let addr = "10.0.0.9:8080".to_string();
        let w0 = reg.snapshot(now, false)[0].weight;
        reg.renew_ttl(&addr, now);
        let e = reg.entries.get(&addr).expect("entry kept");
        assert_eq!(e.health.anon, AnonLevel::Elite);
        assert_eq!(e.health.exit_ip.as_deref(), Some("9.9.9.9"));
        assert_eq!(e.node.exit_ip.as_deref(), Some("9.9.9.9"));
        assert_eq!(reg.snapshot(now, false)[0].weight, w0);
    }

    #[test]
    fn eviction_is_deterministic() {
        // REVIEW-R2 Q7：同分平局一律淘汰 addr 最小者（HashMap 迭代随机，不得决定受害者；
        // 排名依据与权重同基 composite，原 score() 与 weight() 脱钩一并对齐）。
        // 逐个插入时溢出点集合为 {.3,.1,.2} 全平局→确定淘汰 .1，幸存 {.2,.3}；
        // 10 轮独立注册表同断言（修前随机受害者，10 轮全中概率 (1/3)^10≈0，几乎必红）。
        for _ in 0..10 {
            let mut reg = Registry::with_capacity(Duration::from_secs(1800), 2);
            let now = Instant::now();
            for ip in ["10.0.0.3", "10.0.0.1", "10.0.0.2"] {
                reg.upsert_full(&raw(ip, "a"), 50, AnonLevel::Elite, None, now);
            }
            let mut addrs: Vec<String> = reg
                .snapshot(now, false)
                .iter()
                .map(|n| n.addr.clone())
                .collect();
            addrs.sort();
            assert_eq!(
                addrs,
                vec!["10.0.0.2:8080".to_string(), "10.0.0.3:8080".to_string()]
            );
        }
    }

    struct FailSource {
        name: &'static str,
    }

    #[async_trait::async_trait]
    impl Source for FailSource {
        fn name(&self) -> &'static str {
            self.name
        }

        async fn fetch(&self, _client: &reqwest::Client) -> Result<FetchOutcome, String> {
            Err("boom".to_string())
        }
    }

    #[tokio::test]
    async fn failed_source_trips_guard() {
        // REVIEW-R2 Q4：传输失败（Err/超时）视同零产出计入熔断；连续 3 轮即暂停。
        let bad: Box<dyn Source> = Box::new(FailSource { name: "bad" });
        let sources = vec![bad];
        let mut guards = vec![SourceGuard::new(3, 3)];
        let client = reqwest::Client::new();
        for tick in 0..3 {
            let raws = fetch_all(&sources, &mut guards, &client, tick).await;
            assert!(raws.is_empty());
        }
        assert!(
            !guards[0].should_fetch(3),
            "persistently failing source must suspend"
        );
    }

    struct StubSource {
        name: &'static str,
        nodes: Vec<RawNode>,
        delay_ms: u64,
    }

    #[async_trait::async_trait]
    impl Source for StubSource {
        fn name(&self) -> &'static str {
            self.name
        }

        async fn fetch(&self, _client: &reqwest::Client) -> Result<FetchOutcome, String> {
            tokio::time::sleep(Duration::from_millis(self.delay_ms)).await;
            Ok(FetchOutcome::nodes(self.nodes.clone()))
        }
    }

    #[tokio::test]
    async fn fetch_all_preserves_source_order() {
        // 并发抓取但结果按源序拼接（去重“首见获胜”确定性；G9）。
        // 慢源（200ms）配置在前，快源（0ms）在后：慢源条目恒在前。
        let slow: Box<dyn Source> = Box::new(StubSource {
            name: "slow",
            nodes: vec![raw("10.0.0.1", "slow")],
            delay_ms: 200,
        });
        let fast: Box<dyn Source> = Box::new(StubSource {
            name: "fast",
            nodes: vec![raw("10.0.0.2", "fast")],
            delay_ms: 0,
        });
        let sources = vec![slow, fast];
        let mut guards = vec![SourceGuard::new(3, 3), SourceGuard::new(3, 3)];
        let client = reqwest::Client::new();
        let raws = fetch_all(&sources, &mut guards, &client, 0).await;
        assert_eq!(raws.len(), 2);
        assert_eq!(raws[0].source, "slow");
        assert_eq!(raws[1].source, "fast");
    }

    #[tokio::test]
    async fn worker_merge_pushes_snapshot_to_router() {
        // Worker 合并语义：registry 快照经 replace_vendor_nodes 进池；水位计同步。
        use crate::metrics::MetricsRegistry;
        use crate::router::RouterEngine;
        use std::sync::Arc;
        let router = Arc::new(RouterEngine::new(vec![]));
        let metrics = Arc::new(MetricsRegistry::new());
        let mut reg = Registry::new(Duration::from_secs(1800));
        reg.upsert_full(
            &raw("10.0.0.1", "a"),
            50,
            AnonLevel::Elite,
            None,
            Instant::now(),
        );
        FreePoolWorker::merge_once(&router, &metrics, &reg, false);
        assert_eq!(router.snapshot_all().len(), 1);
        assert!(metrics.render().contains("free_pool_nodes_total 1"));
    }

    #[tokio::test]
    async fn semaphore_caps_concurrent_verify() {
        // REVIEW-R2 Q1：spawn 内许可必须持有跨过 await（`pool.rs:92` 形态）；
        // 丢弃写法（`if acquire_owned().await.is_err()` 临时值）即时释放，并发无上限。
        use std::sync::atomic::{AtomicUsize, Ordering};
        use std::sync::Arc;
        let sem = Arc::new(tokio::sync::Semaphore::new(2));
        let live = Arc::new(AtomicUsize::new(0));
        let high = Arc::new(AtomicUsize::new(0));
        let mut set = tokio::task::JoinSet::new();
        for _ in 0..8 {
            let (s, l, h) = (sem.clone(), live.clone(), high.clone());
            set.spawn(async move {
                // 许可持有跨过 await（丢弃写法即时释放，高水位==任务数，见本单测红灯记录）。
                let Ok(_permit) = s.acquire_owned().await else {
                    return;
                };
                let n = l.fetch_add(1, Ordering::SeqCst) + 1;
                h.fetch_max(n, Ordering::SeqCst);
                tokio::time::sleep(std::time::Duration::from_millis(20)).await;
                l.fetch_sub(1, Ordering::SeqCst);
            });
        }
        while set.join_next().await.is_some() {}
        assert!(
            high.load(Ordering::SeqCst) <= 2,
            "concurrency escaped cap: high={}",
            high.load(Ordering::SeqCst)
        );
    }

    fn socks_raw(ip: &str, source: &str) -> RawNode {
        RawNode {
            ip: ip.to_string(),
            port: 1080,
            proto: FreeProto::Socks5,
            country: None,
            source: source.to_string(),
        }
    }

    #[test]
    fn registry_stores_socks_proto() {
        // P2-5：socks 节点正常进池（poolable 门已拆），快照携带 proto＝Socks5。
        use crate::model::EgressProto;
        let mut reg = Registry::new(Duration::from_secs(1800));
        let now = Instant::now();
        reg.upsert_full(&socks_raw("9.9.9.9", "gh"), 50, AnonLevel::Elite, None, now);
        let snap = reg.snapshot(now, false);
        assert_eq!(snap.len(), 1);
        assert_eq!(snap[0].proto, EgressProto::Socks5);
        assert_eq!(snap[0].tier, "free");
    }

    /// CONNECT 常成功 stub（greeting→CONNECT→回成功；再读带 500ms 超时以兼容 greeting-only）。
    async fn spawn_connect_ok_stub() -> u16 {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind");
        let port = listener.local_addr().expect("addr").port();
        tokio::spawn(async move {
            let (mut s, _) = match listener.accept().await {
                Ok(v) => v,
                Err(_) => return,
            };
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
            // CONNECT 头 4 字节（500ms 读不到＝greeting-only，正常关闭）。
            let mut req4 = [0u8; 4];
            let got =
                tokio::time::timeout(Duration::from_millis(500), s.read_exact(&mut req4)).await;
            if got.is_err() {
                return;
            }
            let rest_len = match req4[3] {
                0x01 => 4 + 2,
                0x03 => {
                    let mut l = [0u8; 1];
                    if s.read_exact(&mut l).await.is_err() {
                        return;
                    }
                    l[0] as usize + 2
                }
                _ => return,
            };
            let mut rest = vec![0u8; rest_len];
            if s.read_exact(&mut rest).await.is_err() {
                return;
            }
            let _ = s
                .write_all(&[0x05, 0x00, 0x00, 0x01, 0, 0, 0, 0, 0, 0])
                .await;
        });
        port
    }

    #[tokio::test]
    async fn verifier_socks_handshake_ok_and_refused() {
        // 有 target→CONNECT 验证 Some；无 target→greeting-only 降级 Some；拒连 None。
        let stub = spawn_connect_ok_stub().await;
        let raw = socks_raw("127.0.0.1", "t");
        let raw = RawNode { port: stub, ..raw };
        let v = Verifier::new(Duration::from_secs(3), 3000)
            .with_socks_target("127.0.0.1".to_string(), stub);
        assert!(v.verify(&raw).await.is_some());
        let stub2 = spawn_connect_ok_stub().await;
        let raw2 = RawNode {
            port: stub2,
            ..socks_raw("127.0.0.1", "t")
        };
        let v2 = Verifier::new(Duration::from_secs(3), 3000);
        assert!(v2.verify(&raw2).await.is_some());
        let refused = RawNode {
            port: 1,
            ..socks_raw("127.0.0.1", "t")
        };
        assert!(v.verify(&refused).await.is_none());
    }

    #[test]
    fn check_target_parses_base() {
        // full_check_base → CONNECT 验证目标（host＋端口，缺省 443/80）。
        assert_eq!(
            socks_target_of_base("https://httpbin.org"),
            Some(("httpbin.org".to_string(), 443))
        );
        assert_eq!(
            socks_target_of_base("http://127.0.0.1:18080/"),
            Some(("127.0.0.1".to_string(), 18080))
        );
        assert_eq!(socks_target_of_base("gopher://x/"), None);
        assert_eq!(socks_target_of_base(""), None);
    }

    #[tokio::test]
    async fn fullcheck_via_socks_relay_reports_elite() {
        // 本地 echo 基址（/ip＋/headers＋/anything 三连 accept）＋relay-stub
        // （CONNECT 到 echo 端口后双向管道）→经 socks 复检 Some＋Elite。
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let echo = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind");
        let echo_port = echo.local_addr().expect("addr").port();
        tokio::spawn(async move {
            for _ in 0..3 {
                let (mut s, _) = match echo.accept().await {
                    Ok(v) => v,
                    Err(_) => return,
                };
                let mut buf = [0u8; 4096];
                let n = s.read(&mut buf).await.unwrap_or(0);
                let req = String::from_utf8_lossy(&buf[..n]).to_string();
                let path = req
                    .lines()
                    .next()
                    .unwrap_or("")
                    .split_whitespace()
                    .nth(1)
                    .unwrap_or("/")
                    .to_string();
                let body = if path.starts_with("/ip") {
                    r#"{"origin":"10.9.9.9"}"#.to_string()
                } else if path.starts_with("/headers") {
                    r#"{"headers":{}}"#.to_string()
                } else {
                    // 同上：显式位置参数。
                    format!(r#"{{"url":"http://x/anything/{}"}}"#, FULL_CHECK_MARKER)
                };
                let _ = s
                    .write_all(
                        format!(
                            "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                            body.len()
                        )
                        .as_bytes(),
                    )
                    .await;
            }
        });
        let relay = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind");
        let relay_port = relay.local_addr().expect("addr").port();
        tokio::spawn(async move {
            for _ in 0..3 {
                let (mut s, _) = match relay.accept().await {
                    Ok(v) => v,
                    Err(_) => return,
                };
                let mut head = [0u8; 2];
                if s.read_exact(&mut head).await.is_err() {
                    continue;
                }
                let mut methods = vec![0u8; head[1] as usize];
                if s.read_exact(&mut methods).await.is_err() {
                    continue;
                }
                if s.write_all(&[0x05, 0x00]).await.is_err() {
                    continue;
                }
                let mut req4 = [0u8; 4];
                if s.read_exact(&mut req4).await.is_err() {
                    continue;
                }
                let (host, port) = match req4[3] {
                    0x01 => {
                        let mut b = [0u8; 6];
                        if s.read_exact(&mut b).await.is_err() {
                            continue;
                        }
                        (
                            std::net::IpAddr::from([b[0], b[1], b[2], b[3]]).to_string(),
                            u16::from_be_bytes([b[4], b[5]]),
                        )
                    }
                    0x03 => {
                        let mut l = [0u8; 1];
                        if s.read_exact(&mut l).await.is_err() {
                            continue;
                        }
                        let mut b = vec![0u8; l[0] as usize + 2];
                        if s.read_exact(&mut b).await.is_err() {
                            continue;
                        }
                        let n = b.len();
                        (
                            String::from_utf8_lossy(&b[..n - 2]).to_string(),
                            u16::from_be_bytes([b[n - 2], b[n - 1]]),
                        )
                    }
                    _ => continue,
                };
                let mut up = match tokio::net::TcpStream::connect((host.as_str(), port)).await {
                    Ok(v) => v,
                    Err(_) => continue,
                };
                if s.write_all(&[0x05, 0x00, 0x00, 0x01, 0, 0, 0, 0, 0, 0])
                    .await
                    .is_err()
                {
                    continue;
                }
                let _ = tokio::io::copy_bidirectional(&mut s, &mut up).await;
            }
        });
        let checker = FullChecker::new(
            format!("http://127.0.0.1:{echo_port}"),
            Duration::from_secs(5),
        );
        let raw = RawNode {
            port: relay_port,
            ..socks_raw("127.0.0.1", "t")
        };
        let res = checker.check(&raw, "1.2.3.4").await.expect("pass");
        assert_eq!(res.anon, AnonLevel::Elite);
        assert_eq!(res.exit_ip.as_deref(), Some("10.9.9.9"));
    }
}
