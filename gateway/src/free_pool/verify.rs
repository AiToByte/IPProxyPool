//! 两级质检与匿名度分级。
//!
//! * [`Verifier`]：轻量级——只验"这个地址此刻能不能作为代理转发一次"（TCP 连通 +
//!   协议握手），廉价、可高频跑。
//! * [`FullChecker`]：全量——经代理打 canary（[`FULL_CHECK_MARKER`]），校验响应
//!   体与出口 IP，并识别 7 个披露头（[`DISCLOSURE_HEADERS`]）判定匿名度。
//!   免费线零信任的前提：**复检基址必须 https**，否则中间人可伪造 canary。
//!
//! `egress_of` / `socks_target_of_base` 是抓取形态 → 出站形态的映射（Phase 1
//! 只收 HTTP(S)，SOCKS 仅标注）。

use super::*;

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
    // OPT-R4 B2：披露头必须值含基线 IP 才算暴露——仅键名（如无害 Via）不验值
    // 会误杀 Elite。基线为空时 `contains("")` 恒真，退化为旧键名口径（保守）。
    let base = baseline_ip.trim();
    let disclosed = echoed_headers.iter().any(|(k, v)| {
        DISCLOSURE_HEADERS.contains(&k.to_ascii_lowercase().as_str()) && v.contains(base)
    });
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
    bases: Vec<String>,
    timeout: Duration,
    /// NEXT-B4：per-代理 Client 缓存（逐节点重建浪费连接池；沿 prober OPT-5）。
    /// OPT-R4 B6：附 last_used，超 512 项时淘汰 10min 未用（防常开免费线内存泄漏；
    /// 淘汰后按需重建，正确性安全，只付重建成本）。
    clients: std::sync::Arc<
        parking_lot::Mutex<std::collections::HashMap<String, (reqwest::Client, Instant)>>,
    >,
}

/// OPT-R4 B6：Client 缓存上限＋空闲 TTL（与 `client_for` 内联淘汰配套）。
pub(super) const CHECKER_CLIENT_CAP: usize = 512;
const CHECKER_CLIENT_IDLE: Duration = Duration::from_secs(600);

impl FullChecker {
    pub fn new(base_url: String, timeout: Duration) -> Self {
        Self {
            bases: vec![base_url.trim_end_matches('/').to_string()],
            timeout,
            clients: std::sync::Arc::new(parking_lot::Mutex::new(std::collections::HashMap::new())),
        }
    }

    /// NEXT-B4：追加备用基址（首基址抖动不再判整节点失败；去重＋保序）。
    pub fn with_fallback_bases(mut self, bases: Vec<String>) -> Self {
        for b in bases {
            let t = b.trim_end_matches('/').to_string();
            if !self.bases.contains(&t) {
                self.bases.push(t);
            }
        }
        self
    }

    pub(super) fn client_for(&self, proxy_url: &str) -> Option<reqwest::Client> {
        let mut map = self.clients.lock();
        if let Some((c, last)) = map.get_mut(proxy_url) {
            *last = Instant::now();
            return Some(c.clone());
        }
        let proxy = reqwest::Proxy::all(proxy_url).ok()?;
        let client = reqwest::Client::builder()
            .proxy(proxy)
            .timeout(self.timeout)
            .build()
            .ok()?;
        map.insert(proxy_url.to_string(), (client.clone(), Instant::now()));
        // OPT-R4 B6：超限淘汰 10min 未用（仍超则整清重建，正确性安全）。
        // OPT-R15 修 bug：原写法 `Instant::now().checked_sub(IDLE).unwrap_or(Instant::now())`
        // 在 Windows 上有致命缺陷——`Instant` 基准是系统/会话启动时刻，若开机不足 IDLE，
        // `checked_sub` 返回 None，回退成 `cutoff == now`，`retain` 会把**全部**条目清空，
        // 客户端缓存命中率归零、每节点重建 http 连接。改为 `is_idle_over`，只比较
        // 「已空闲时长」这个相对量，不再依赖 Instant 基准距今多远。
        if map.len() > CHECKER_CLIENT_CAP {
            let now = Instant::now();
            map.retain(|_, (_, t)| !is_idle_over(*t, now, CHECKER_CLIENT_IDLE));
            if map.len() > CHECKER_CLIENT_CAP * 2 {
                map.clear();
            }
        }
        Some(client)
    }

    #[cfg(test)]
    pub(super) fn client_count(&self) -> usize {
        self.clients.lock().len()
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
        let client = self.client_for(&proxy_url)?;
        let start = std::time::Instant::now();
        for base in &self.bases {
            if let Some(res) = self.check_one(&client, base, raw, baseline_ip, start).await {
                return Some(res);
            }
        }
        None
    }

    /// NEXT-B4：单基址三端点并发（`join!`；单端点超时沿用 client 级 `timeout`，
    /// 判定口径与串行一致，墙钟≈最慢端点而非三者之和）。
    async fn check_one(
        &self,
        client: &reqwest::Client,
        base: &str,
        raw: &RawNode,
        baseline_ip: &str,
        start: std::time::Instant,
    ) -> Option<FullCheckResult> {
        let (ip_r, h_r, c_r) = futures::future::join3(
            client.get(format!("{base}/ip")).send(),
            client.get(format!("{base}/headers")).send(),
            client
                .get(format!("{base}/anything/{FULL_CHECK_MARKER}"))
                .send(),
        )
        .await;
        // 1. 出口。
        let ip_body: serde_json::Value = ip_r.ok()?.json().await.ok()?;
        let exit = ip_body
            .get("origin")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string());
        // 2. 回显头。
        let h_body: serde_json::Value = h_r.ok()?.json().await.ok()?;
        let mut echoed = std::collections::HashMap::new();
        if let Some(map) = h_body.get("headers").and_then(|v| v.as_object()) {
            for (k, v) in map {
                if let Some(s) = v.as_str() {
                    echoed.insert(k.clone(), s.to_string());
                }
            }
        }
        // 3. canary（内容篡改检测；arXiv:2403.02445 16,923 篡改样本）。
        let c_body: serde_json::Value = c_r.ok()?.json().await.ok()?;
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
