//! 免费源抓取：JSON API / HTML 列表 / GitHub 裸列表三类源。
//!
//! 三者都实现 [`Source`]（定义在父模块）。共同点：礼貌轮询缓存（ETag /
//! Last-Modified + 304 不重复拉取）、分页上限、以及**只产出通过
//! `is_routable_ip` 的公网可回连地址**（bogon/私网/回环一律丢弃）。
//!
//! 与父模块的关系：`use super::*;` 继承 `Source`/`RawNode`/`FreeProto`/
//! `is_routable_ip`/`shared_client` 等父模块条目（私有项对后代可见）。

use super::*;

pub struct ApiSource {
    pub name: &'static str,
    pub url: String,
    /// NEXT-B2：并发抓取页数（1＝单页礼貌轮询；>1＝多页 plain-GET 合并，上限 5）。
    pub pages: usize,
    etag: parking_lot::Mutex<Option<String>>,
    last_modified: parking_lot::Mutex<Option<String>>,
}

/// NEXT-B2：API 分页上限（服务端负载礼貌；Geonode limit=100 时 3 页≈300 raw 足够）。
pub const API_MAX_PAGES: usize = 5;

impl ApiSource {
    pub fn new(name: &'static str, url: String) -> Self {
        Self {
            name,
            url,
            pages: 1,
            etag: parking_lot::Mutex::new(None),
            last_modified: parking_lot::Mutex::new(None),
        }
    }

    pub fn with_pages(mut self, pages: usize) -> Self {
        self.pages = pages.clamp(1, API_MAX_PAGES);
        self
    }

    /// 分页 URL：重写 `page` 查询参数（缺省补 `page=N`；非法 URL 回落原串）。
    pub(super) fn page_url(&self, page: usize) -> String {
        let mut url = match reqwest::Url::parse(&self.url) {
            Ok(u) => u,
            Err(_) => return self.url.clone(),
        };
        let kept: Vec<(String, String)> = url
            .query_pairs()
            .filter(|(k, _)| k != "page")
            .map(|(k, v)| (k.into_owned(), v.into_owned()))
            .collect();
        url.query_pairs_mut()
            .clear()
            .extend_pairs(kept)
            .append_pair("page", &page.to_string());
        url.to_string()
    }
    /// NEXT-B1：Geonode `anonymityLevel` 预筛——`transparent` 声明丢弃。
    /// 实测依据（2026-09-24 实样）：声明值仅 elite/anonymous/transparent 三档；
    /// 历史 pass 全为 Elite；`latency/responseTime` 量纲不明（elite 样样本 responseTime
    /// 反而最大），不做数值门。字段缺失 fail-open（他源/旧格式不受影响）。
    fn anonymity_passes(item: &serde_json::Value) -> bool {
        match item.get("anonymityLevel").and_then(|v| v.as_str()) {
            None => true,
            Some(level) => {
                let low = level.to_ascii_lowercase();
                low != "transparent"
            }
        }
    }

    pub fn parse(source: &str, body: &str) -> Vec<RawNode> {
        let v: serde_json::Value = match serde_json::from_str(body) {
            Ok(v) => v,
            Err(_) => return Vec::new(),
        };
        let arr = v.get("data").and_then(|d| d.as_array());
        let mut out = Vec::new();
        let mut bogon = 0u32;
        for item in arr.into_iter().flatten() {
            let ip = item.get("ip").and_then(|s| s.as_str()).unwrap_or("");
            if ip.is_empty() {
                continue;
            }
            // OPT-R4 B1：私网/bogon 不进候选（省 TCP 并发槽）。
            if !is_routable_ip(ip) {
                bogon += 1;
                continue;
            }
            if !Self::anonymity_passes(item) {
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
        if bogon > 0 {
            log::debug!("[FreePool] api {source} dropped {bogon} bogon ips pre-TCP");
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
        if self.pages <= 1 {
            match conditional_get(client, &self.url, &self.etag, &self.last_modified).await? {
                Some(body) => Ok(FetchOutcome::nodes(Self::parse(self.name, &body))),
                None => Ok(FetchOutcome::not_modified()),
            }
        } else {
            self.fetch_pages(client).await
        }
    }
}

impl ApiSource {
    /// NEXT-B2：多页并发抓取（plain-GET；ETag 只适用于单页礼貌轮询，多页礼貌靠
    /// 上限 5 页＋外层 15s 超时）。单页失败容忍（其余页照常合并）；全失败才 Err。
    /// OPT-R4 B10：首页走 conditional_get（复用源级 etag/last_modified；304 即
    /// 本页无变更，不计失败；etag 按源共享的近似语义见注释）。
    async fn fetch_pages(&self, client: &reqwest::Client) -> Result<FetchOutcome, String> {
        // 首页条件 GET（与单页路径共享 etag 存储：Geonode ETag 按查询全局，
        // 页间复用近似成立；不匹配时服务端回 200 全量，正确性安全）。
        let first =
            conditional_get(client, &self.page_url(1), &self.etag, &self.last_modified).await;
        let mut nodes = Vec::new();
        let mut ok_pages = 0u32;
        let mut not_modified_pages = 0u32;
        match first {
            Ok(Some(body)) => {
                ok_pages += 1;
                nodes.extend(Self::parse(self.name, &body));
            }
            Ok(None) => {
                not_modified_pages += 1;
            }
            Err(e) => log::warn!("[FreePool] api page 1 fetch failed: {e}"),
        }
        let futs: Vec<_> = (2..=self.pages)
            .map(|p| {
                let url = self.page_url(p);
                async move {
                    let r = client.get(&url).send().await;
                    (p, r)
                }
            })
            .collect();
        let results = futures::future::join_all(futs).await;
        for (p, r) in results {
            match r {
                Ok(resp) => match read_source_body(resp).await {
                    Ok(body) => {
                        ok_pages += 1;
                        nodes.extend(Self::parse(self.name, &body));
                    }
                    Err(e) => log::warn!("[FreePool] api page {p} read failed: {e}"),
                },
                Err(e) => log::warn!("[FreePool] api page {p} fetch failed: {e}"),
            }
        }
        if ok_pages == 0 {
            if not_modified_pages > 0 {
                // 全页 304：源未变更（SourceGuard 豁免，不计熔断）。
                return Ok(FetchOutcome::not_modified());
            }
            return Err(format!("all {} api pages failed", self.pages));
        }
        Ok(FetchOutcome::nodes(nodes))
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
        let mut bogon = 0u32;
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
                // OPT-R4 B1：HTML 嗅探到的私网/bogon 同样不进候选。
                if !is_routable_ip(&ip) {
                    bogon += 1;
                    i += consumed;
                    continue;
                }
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
        if bogon > 0 {
            log::debug!("[FreePool] html {source} dropped {bogon} bogon ips pre-TCP");
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

/// URI-formatted plain-text source: one strict `scheme://IPv4:port` per line.
pub struct UriSource {
    pub name: &'static str,
    pub url: String,
    etag: parking_lot::Mutex<Option<String>>,
}

impl UriSource {
    pub fn new(name: &'static str, url: String) -> Self {
        Self {
            name,
            url,
            etag: parking_lot::Mutex::new(None),
        }
    }

    pub fn parse(source: &str, body: &str) -> Vec<RawNode> {
        body.lines()
            .filter_map(|line| Self::parse_line(source, line.trim()))
            .collect()
    }

    fn parse_line(source: &str, line: &str) -> Option<RawNode> {
        if line.is_empty() || line.starts_with('#') || line.bytes().any(|b| b.is_ascii_control()) {
            return None;
        }
        let (scheme, authority) = line.split_once("://")?;
        let proto = match scheme.to_ascii_lowercase().as_str() {
            "http" => FreeProto::Http,
            "socks4" => FreeProto::Socks4,
            "socks5" => FreeProto::Socks5,
            _ => return None,
        };
        if authority.is_empty()
            || authority.chars().any(|ch| {
                matches!(ch, '/' | '?' | '#' | '@' | '%' | '\\' | ' ') || ch.is_ascii_control()
            })
        {
            return None;
        }
        let (host, port) = authority.rsplit_once(':')?;
        if host.contains(':') || host.is_empty() {
            return None;
        }
        let ip = host.parse::<std::net::Ipv4Addr>().ok()?.to_string();
        let port = port.parse::<u16>().ok().filter(|port| *port != 0)?;
        if !is_routable_ip(&ip) {
            return None;
        }
        Some(RawNode {
            ip,
            port,
            proto,
            country: None,
            source: source.to_string(),
        })
    }
}

#[async_trait::async_trait]
impl Source for UriSource {
    fn name(&self) -> &'static str {
        self.name
    }

    async fn fetch(&self, client: &reqwest::Client) -> Result<FetchOutcome, String> {
        let mut request = client.get(&self.url);
        if let Some(etag) = self.etag.lock().clone() {
            request = request.header("If-None-Match", etag);
        }
        let response = request
            .send()
            .await
            .map_err(|_| "source request failed".to_string())?;
        if response.status() == reqwest::StatusCode::NOT_MODIFIED {
            return Ok(FetchOutcome::not_modified());
        }
        let etag = response
            .headers()
            .get("etag")
            .and_then(|value| value.to_str().ok())
            .map(str::to_owned);
        let body = read_source_body(response).await?;
        if let Some(etag) = etag {
            *self.etag.lock() = Some(etag);
        }
        Ok(FetchOutcome::nodes(Self::parse(self.name, &body)))
    }
}

/// raw 行列表源（默认 clarketm＋openproxylist；URL env 可配）。
/// 行格式：`ip:port`（`#` 开头与空行跳过；行内 `socks4`/`socks5` 标记优先，
/// 无标记落 `default_proto`，供 openproxylist 按文件切分协议）。
/// 礼貌轮询：ETag 缓存＋If-None-Match，304 返回 `not_modified`（Task 10 不计零产出）。
pub struct GitHubSource {
    pub name: &'static str,
    pub url: String,
    pub default_proto: FreeProto,
    etag: parking_lot::Mutex<Option<String>>,
}

impl GitHubSource {
    pub fn new(name: &'static str, url: String, default_proto: FreeProto) -> Self {
        Self {
            name,
            url,
            default_proto,
            etag: parking_lot::Mutex::new(None),
        }
    }

    /// NEXT-B3：`default_proto` 供无行内标记的裸 `ip:port` 文件
    /// （openproxylist 按文件切分协议）；行内标记优先；octet 逐段校验＋
    /// 拒绝全零地址（openproxylist 首行 `0.0.0.0:80` 实锤）。
    pub fn parse_with(source: &str, body: &str, default_proto: FreeProto) -> Vec<RawNode> {
        let mut out = Vec::new();
        let mut bogon = 0u32;
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
                default_proto
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
            let octets: Vec<&str> = ip.split('.').collect();
            if octets.len() != 4
                || octets.iter().any(|o| o.parse::<u8>().is_err())
                || octets
                    .iter()
                    .all(|o| *o == "0" || *o == "00" || *o == "000")
            {
                continue;
            }
            // OPT-R4 B1：私网/bogon 不进候选（全零已在上分支丢弃，此处补 RFC1918 等）。
            if !is_routable_ip(ip) {
                bogon += 1;
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
        if bogon > 0 {
            log::debug!("[FreePool] raw {source} dropped {bogon} bogon ips pre-TCP");
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
        let etag = resp
            .headers()
            .get("etag")
            .and_then(|header| header.to_str().ok())
            .map(str::to_owned);
        let body = read_source_body(resp).await?;
        if let Some(etag) = etag {
            *self.etag.lock() = Some(etag);
        }
        Ok(FetchOutcome::nodes(Self::parse_with(
            self.name,
            &body,
            self.default_proto,
        )))
    }
}

/// 并发抓取全源（`join_all` 按源序并发＋per-source 超时；输出与输入同序，
/// 保证去重“首见获胜”确定性：调用方保证 `sources` 配置序＝优先级序）。
/// 暂停源跳过（其 guard 不计数，保持旧集）；超时/失败 hold 旧集（registry 不动）。
/// OPT-R4 B3：超时由调用方按 `FREE_FETCH_TIMEOUT_SECS` 传入（默认 15s）。
/// OPT-R4 B9：每源每轮耗时必记（含超时/失败轮，分母完整，供均值口径）。
pub async fn fetch_all(
    sources: &[Box<dyn Source>],
    guards: &mut [SourceGuard],
    client: &reqwest::Client,
    tick: u64,
    fetch_timeout: Duration,
    metrics: Option<&crate::metrics::MetricsRegistry>,
) -> Vec<RawNode> {
    let futs: Vec<_> = sources
        .iter()
        .enumerate()
        .filter(|(i, _)| guards[*i].should_fetch(tick))
        .map(|(i, s)| {
            let nm = s.name();
            async move {
                let start = Instant::now();
                let r = tokio::time::timeout(fetch_timeout, s.fetch(client)).await;
                (i, nm, start.elapsed(), r)
            }
        })
        .collect();
    let results = futures::future::join_all(futs).await; // 输出与输入同序
    let mut raws = Vec::new();
    for (i, nm, elapsed, r) in results {
        if let Some(m) = metrics {
            m.note_free_source_fetch(nm, elapsed);
        }
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
                log::warn!(
                    "[FreePool] source fetch timed out ({}s, holding last good set)",
                    fetch_timeout.as_secs()
                )
            }
        }
    }
    // 去重（首见获胜；源序＝配置序：api > html > github，调用方保证 sources 顺序）。
    // OPT-R4 B4：去重键含协议（同址 http/socks5 互挤会丢 socks5 Elite）。
    let mut seen = HashSet::new();
    raws.retain(|r| seen.insert(format!("{}:{}:{:?}", r.ip, r.port, r.proto)));
    raws
}
