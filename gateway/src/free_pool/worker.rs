//! 后台 worker 与配置：env 组装 → 周期抓取 → 质检 → geo 裁决 → Router 合并。
//!
//! 这是免费线的**驱动层**，不持有池状态（状态在 `registry::Registry`）。
//! 每个 tick 的流程：抓取各源 → 去重 → intake 配额截断 → 并发质检 →
//! 写入 Registry → 把存活节点合并进 `RouterEngine`（免费档权重见
//! `FREE_POOL_WEIGHT`，永远远低于付费线，避免免费淹没付费）。

use super::registry::{baseline_bases, cap_intake, intake_cap_limit};
use super::sources::fetch_all;
use super::*;

/// Worker 配置（main 从 env 组装；默认值见计划 §2 Env 总表）。
#[derive(Debug, Clone)]
pub struct FreePoolConfig {
    pub api_urls: Vec<String>,
    /// NEXT-B2：API 分页数（1＝单页礼貌轮询；>1＝多页合并，上限见 `API_MAX_PAGES`）。
    pub api_pages: usize,
    pub html_urls: Vec<String>,
    pub github_urls: Vec<String>,
    /// Strict proxy URI lists (one `scheme://IPv4:port` per line).
    pub uri_urls: Vec<String>,
    pub fetch_interval: Duration,
    pub ttl: Duration,
    pub verify_timeout: Duration,
    /// OPT-R4 B3：单源抓取超时（默认 15s；干旱可压小、洪峰可放大，无需改代码）。
    pub fetch_timeout: Duration,
    /// OPT-R4 B3：intake 上限因子（cap＝max_nodes×factor，默认 2；洪峰可压小）。
    pub intake_factor: usize,
    pub max_latency_ms: u64,
    pub max_concurrent: usize,
    pub full_concurrent: usize,
    pub max_nodes: usize,
    pub full_check_base: String,
    /// NEXT-B4：备用复检基址（首基址抖动时轮询；空即单基址；https-only）。
    pub full_check_bases: Vec<String>,
    pub require_elite: bool,
    pub max_zero_cycles: u32,
    pub suspend_retry_every: u64,
    /// P4-1 mismatch 执法开关（`GEOIP_ENFORCE_MISMATCH=1`；默认 false＝只观察）。
    pub geo_enforce: bool,
}

/// Source 默认 URL（全 env 可覆盖；单源挂了只 hold 旧集，不清空池）。
pub const DEFAULT_API_URL: &str = "https://proxylist.geonode.com/api/proxy-list?limit=100&page=1&sort_by=lastChecked&sort_type=desc";
pub const DEFAULT_HTML_URL: &str = "https://free-proxy-list.net/";
/// NEXT-B3：raw 行列表默认三源（逗号分隔，`split_env_list` 切分）。
/// clarketm 在部分网络不可达（hold 旧集＋熔断，不阻塞他源）；
/// openproxylist http/socks5 双文件 2026-09-24 实测 200（行内无标记，按 URL 嗅探协议）。
/// OPT-R4 B12：第 4 源 monosans（在维护，日更；http＋socks5 双文件，2026-09-25
/// 经 VPN 路径实测 200：http 393 行／socks5 444 行，`ip:port` 行形态复用 parse_with）。
/// OPT-R5 E3：第 5 源 relayglass（5min 活检；jsdelivr 双路实测 200：http 152 行／
/// socks5 114 行，裸 `ip:port` 复用 parse_with，首行见 `e3_relayglass_fixture_parses`）。
/// 落选：TheSpeedX/SOCKS-List（延续备份不接）；proxyscrape-v2（py200形态对但curl直连000超时）／
/// spys.me（proxy.txt-403/socks.txt 头文本非ip:port）／openproxy.space（py403/curl521）／
/// freeproxyupdate（无raw只有HTML）／proxy-list.download（双路502）／proxyscrape-v4＋proxifly系
/// （`scheme://ip:port`形态不兼容parse_with）／proxyspace.pro（双路200＋3901/3232行形态对但首行
/// 0.0.0.0污染＋体量大记备份）／hproxy（双路200＋43k行形态对但淹没intake记备份）。
pub const DEFAULT_GITHUB_URL: &str = "https://raw.githubusercontent.com/clarketm/proxy-list/master/proxy-list-raw.txt,https://api.openproxylist.xyz/http.txt,https://api.openproxylist.xyz/socks5.txt,https://raw.githubusercontent.com/monosans/proxy-list/main/proxies/http.txt,https://raw.githubusercontent.com/monosans/proxy-list/main/proxies/socks5.txt,https://cdn.jsdelivr.net/gh/relayglass/free-proxy-list@main/protocol/http/http.txt,https://cdn.jsdelivr.net/gh/relayglass/free-proxy-list@main/protocol/socks5/socks5.txt";
pub const DEFAULT_URI_URL: &str = "";
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
pub(super) fn apply_geo_verdict(
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
        let n_sources = (config.api_urls.len()
            + config.html_urls.len()
            + config.github_urls.len()
            + config.uri_urls.len())
        .max(1);
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

    /// Assemble API, HTML, raw and opt-in URI sources. Each source has its own breaker.
    fn build_sources(&mut self) -> Vec<Box<dyn Source>> {
        // 先克隆 URL 表（避免 config 不可变借用与 intern_name 可变借用冲突）。
        let api_urls = self.config.api_urls.clone();
        let api_pages = self.config.api_pages;
        let html_urls = self.config.html_urls.clone();
        let github_urls = self.config.github_urls.clone();
        let uri_urls = self.config.uri_urls.clone();
        let mut sources: Vec<Box<dyn Source>> = Vec::new();
        for (i, url) in api_urls.iter().enumerate() {
            let name = self.intern_name(format!("api{i}"));
            sources.push(Box::new(
                ApiSource::new(name, url.clone()).with_pages(api_pages),
            ));
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
            // NEXT-B3：按 URL 嗅探文件协议（openproxylist 按文件切分，无行内标记）。
            let lower = url.to_ascii_lowercase();
            let default_proto = if lower.contains("socks5") {
                FreeProto::Socks5
            } else if lower.contains("socks4") {
                FreeProto::Socks4
            } else {
                FreeProto::Http
            };
            sources.push(Box::new(GitHubSource::new(
                name,
                url.clone(),
                default_proto,
            )));
        }
        for (i, url) in uri_urls.iter().enumerate() {
            let name = self.intern_name(format!("uri{i}"));
            sources.push(Box::new(UriSource::new(name, url.clone())));
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
        let mut raws = fetch_all(
            &sources,
            &mut self.guards,
            client,
            tick,
            self.config.fetch_timeout,
            Some(self.metrics.as_ref()),
        )
        .await;
        // OPT-R4 B3/B8：intake 上限＝max_nodes×factor（默认 2；洪峰可压小运维可调）。
        // 加权截断（Elite 历史优先＋源序；调用方保证 cap>0）。
        let intake_cap = intake_cap_limit(self.config.max_nodes, self.config.intake_factor);
        if raws.len() > intake_cap {
            let before = raws.len();
            let elite: HashSet<String> = self
                .registry
                .entries
                .iter()
                .filter(|(_, e)| e.health.anon == AnonLevel::Elite)
                .map(|(k, _)| k.clone())
                .collect();
            raws = cap_intake(raws, intake_cap, &elite);
            self.metrics
                .note_free_intake_capped((before - intake_cap) as u64);
            log::info!("[FreePool] intake capped {before}->{intake_cap} (elite-weighted)");
        }
        // OPT-R4 B5：失败指纹窗内 addr 跳过本轮 TCP 初筛（省重复建链；窗口＝2×抓取间隔）。
        {
            let now = Instant::now();
            let window = self.config.fetch_interval.saturating_mul(2);
            let before = raws.len();
            raws.retain(|r| {
                !self
                    .registry
                    .failed_recently(&format!("{}:{}", r.ip, r.port), now, window)
            });
            let skipped = before - raws.len();
            if skipped > 0 {
                log::debug!("[FreePool] pre-TCP skipped {skipped} recently-failed addrs");
            }
        }
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
        // 3. 直连基线：轮询基址表取首个成功 origin（5s 单基址超时；全失败→None＝降级 TCP-only）。
        // OPT-R4 B7：主基址＋full_check_bases fallback（首基址抖动不再整轮降级）。
        // 基线获取失败不计节点失败（源站侧问题，非节点问题）。
        let baseline: Option<String> = {
            let bases = baseline_bases(&self.config.full_check_base, &self.config.full_check_bases);
            let mut found = None;
            for base in &bases {
                let r = tokio::time::timeout(
                    Duration::from_secs(5),
                    client.get(format!("{base}/ip")).send(),
                )
                .await;
                match r {
                    Ok(Ok(resp)) => {
                        found = resp.json::<serde_json::Value>().await.ok().and_then(|v| {
                            v.get("origin")
                                .and_then(|o| o.as_str())
                                .map(|s| s.to_string())
                        });
                        if found.is_some() {
                            break;
                        }
                    }
                    _ => continue,
                }
            }
            found
        };
        if baseline.is_none() {
            // OPT-R4 B9：基线全灭记一笔（降级轮可观测）。
            self.metrics.note_free_baseline_fail();
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
        )
        .with_fallback_bases(self.config.full_check_bases.clone());
        let now = Instant::now();
        let mut fset = tokio::task::JoinSet::new();
        // OPT-R4 B9：本轮容量淘汰累计（upsert 连带）。
        let mut evicted_total: u64 = 0;
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
                    // OPT-R4 B9：连带淘汰量累积（循环后一次记入 evicted）。
                    evicted_total += self.registry.upsert_full(
                        &raw,
                        res.fwd_latency_ms,
                        res.anon,
                        res.exit_ip.clone(),
                        now,
                    ) as u64;
                    self.metrics.note_free_verify("pass");
                    self.metrics.note_free_anonymity(match res.anon {
                        AnonLevel::Elite => "elite",
                        AnonLevel::Anonymous => "anonymous",
                        AnonLevel::Transparent => "transparent",
                        AnonLevel::Unknown => "unknown",
                    });
                    // NEXT-B5：Elite 记来源（回答哪个源真出货）。
                    if res.anon == AnonLevel::Elite {
                        self.metrics.note_free_source_elite(&raw.source);
                    }
                    // OPT-R5 E5：FullCheck 成功路径记转发延迟＋出口（轮转由 PromQL 看）。
                    self.metrics.note_free_fwd_latency(res.fwd_latency_ms);
                    if let Some(ref exit) = res.exit_ip {
                        self.metrics.note_free_exit(exit);
                    }
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
        if evicted_total > 0 {
            self.metrics.note_free_evicted(evicted_total);
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
        let mut snap = registry.snapshot(now, require_elite);
        // NEXT-B6：合并时应用审计因子（存量快照权重是复检健康值；因子表在 Router 侧，
        // 跨 tick 持久；下限语义沿 Q5：factor<=0 摘除，>0 保底 1）。
        for n in &mut snap {
            let f = router.free_factor(&n.provider, &n.country);
            if (f - 1.0).abs() > f64::EPSILON {
                n.weight = if f <= 0.0 {
                    0
                } else {
                    ((n.weight as f64 * f).round() as u32).max(1)
                };
            }
        }
        metrics.set_free_pool_nodes(snap.len() as u64);
        // OPT-R18 P2-3：供给 SLO —— 出口多样性 + 年龄中位数。
        //
        // 为什么在这里算而不是在 metrics 里算：出口身份来自 `ProxyNode`
        // （`exit_ip` 优先，未知回落 `ip`），只有合并这一步同时持有完整快照
        // 与 registry 的年龄信息。
        let distinct_exits = {
            let mut seen: std::collections::HashSet<&str> =
                std::collections::HashSet::with_capacity(snap.len());
            for n in &snap {
                seen.insert(n.exit_ip.as_deref().unwrap_or(n.ip.as_str()));
            }
            seen.len() as u64
        };
        let median_age = registry
            .median_age_secs(now, require_elite)
            .map(std::time::Duration::from_secs_f64);
        metrics.set_free_pool_supply(distinct_exits, median_age);
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
