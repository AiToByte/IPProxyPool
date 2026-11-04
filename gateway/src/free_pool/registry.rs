//! 池状态：节点健康、TTL 注册表、源站熔断、intake 配额。
//!
//! [`Registry`] 是免费线的**唯一真相源**：条目按 TTL 过期、按容量淘汰、
//! 按 [`Health`] 的 EWMA 复合分决定留谁。声誉**不跨 TTL**——条目删除即清零，
//! IP 复用重新计（免费代理 IP 会被回收再分配，跨期继承声誉会把"上一个用户的
//! 表现"算到新用户头上）。
//!
//! intake 配额（`cap_intake`/`intake_cap_limit`）解决的是**源站回吐量失控**：
//! 某些源一次返回上万条，全量入库会让质检队列雪崩，故按 `max_nodes × factor`
//! 截断，并保留 Elite 地址（它们是稀缺品，不该被截断丢掉）。

use super::*;

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

pub(super) struct Entry {
    pub(super) node: ProxyNode,
    pub(super) health: Health,
    expires_at: Instant,
    /// OPT-R18 P2-3：**首次**进池时刻，只写一次，续期/复检都不改。
    ///
    /// 为什么不能用 `expires_at - ttl` 推：TTL 会随配置变化，且 `expires_at`
    /// 每次续命都被重写，推出来的"年龄"会永远等于当前 TTL——那是个看起来
    /// 合理、实际恒定的假数字。
    first_seen: Instant,
}

/// TTL 注册表：多源按 `addr` 去重（首见 source 获胜）；到期仅经复检续命；
/// 声誉不跨 TTL（条目删除即清零，IP 复用重计，防新旧污染双向错误）。
pub struct Registry {
    ttl: Duration,
    max_nodes: usize,
    pub(super) entries: HashMap<String, Entry>,
    /// OPT-R4 B5：未进池 addr 的失败指纹窗（addr→最近失败时刻；死 IP 在窗口内
    /// 跳过 TCP 初筛，防 9000 级 raw 洪峰重复建链；粗容量上限，满即清零重计）。
    fail_marks: HashMap<String, Instant>,
}

/// OPT-R4 B5：失败指纹粗容量（防 HashMap 无限膨胀；满清零后重计，正确性安全）。
const FAIL_MARKS_CAP: usize = 8192;

impl Registry {
    /// 默认容量构造（稳定 API：单测＋未来调用方；线上 Worker 走 `with_capacity`）。
    /// 按 `reload_nodes` 惯例放行 dead。
    #[allow(dead_code)]
    pub fn new(ttl: Duration) -> Self {
        Self {
            ttl,
            max_nodes: 2000,
            entries: HashMap::new(),
            fail_marks: HashMap::new(),
        }
    }

    pub fn with_capacity(ttl: Duration, max_nodes: usize) -> Self {
        Self {
            ttl,
            max_nodes,
            entries: HashMap::new(),
            fail_marks: HashMap::new(),
        }
    }

    /// 质检通过即插入/刷新（已存在 addr：续期＋health 更新，不覆盖 source；首见获胜）。
    /// P2：全协议进池（Http/Https 走经典转发，Socks4/5 走翻译桥；出站形态存 `node.proto`，
    /// 选路隔离见 `RouterEngine::matches`）。
    /// v1 `upsert(raw, latency, now)` 语义由本函数替代（TCP-only 降级时 anon=Unknown/lat=tcp）。
    /// OPT-R4 B9：返回本轮连带淘汰条数（调用方记 `free_pool_evicted_total`）。
    pub fn upsert_full(
        &mut self,
        raw: &RawNode,
        fwd_latency_ms: u64,
        anon: AnonLevel,
        exit_ip: Option<String>,
        now: Instant,
    ) -> usize {
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
            return 0;
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
                first_seen: now,
            },
        );
        self.evict_if_over_capacity()
    }

    /// REVIEW-R2 Q3：基线缺失降级续命——只续 TTL，不碰 anon/exit_ip/权重/EWMA。
    /// TCP 建链成功≠转发成功，按整成功加权属虚增；匿名度以最近一次 FullCheck 为准。
    pub fn renew_ttl(&mut self, addr: &str, now: Instant) {
        if let Some(e) = self.entries.get_mut(addr) {
            e.expires_at = now + self.ttl;
        }
    }

    /// 复检失败（TCP/Full 任一）：EWMA 记失败＋backoff，TTL 内保留（自动恢复）。
    /// OPT-R4 B5：未进池 addr 同样留失败指纹（窗口内跳过初筛，省重复 TCP 建链）。
    pub fn note_verify_failed(&mut self, addr: &str, now: Instant) {
        if let Some(e) = self.entries.get_mut(addr) {
            e.health.note_failure(now);
            e.node.weight = e.health.weight();
        }
        if self.fail_marks.len() >= FAIL_MARKS_CAP {
            self.fail_marks.clear();
        }
        self.fail_marks.insert(addr.to_string(), now);
    }

    /// OPT-R4 B5：addr 在窗口内失败过即跳过本轮初筛（调用方：TCP spawn 前 retain）。
    pub fn failed_recently(&self, addr: &str, now: Instant, window: Duration) -> bool {
        self.fail_marks
            .get(addr)
            .is_some_and(|t| now.saturating_duration_since(*t) < window)
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
    /// OPT-R4 B9：返回本轮淘汰条数（调用方记 `free_pool_evicted_total`）。
    fn evict_if_over_capacity(&mut self) -> usize {
        let now = Instant::now();
        let mut removed = 0usize;
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
                    removed += 1;
                }
                None => break,
            }
        }
        removed
    }

    /// 可进池快照：未过期＋非 backoff＋匿名度门。
    /// `require_elite=true` 时仅 Elite（FREE_REQUIRE_ELITE=1）。
    pub fn snapshot(&self, now: Instant, require_elite: bool) -> Vec<ProxyNode> {
        self.entries
            .values()
            .filter(|e| Self::is_pooled(e, now, require_elite))
            .map(|e| e.node.clone())
            .collect()
    }

    /// "该条目此刻是否算在池内"——**唯一判定口径**。
    ///
    /// `snapshot()` 与 `median_age_secs()` 必须用同一个谓词：两处各写一遍
    /// 过滤条件，就会出现"节点数按 A 口径、年龄按 B 口径"的数字，而两个数字
    /// 放在同一张仪表盘上时没人能看出它们描述的不是同一个集合。
    fn is_pooled(e: &Entry, now: Instant, require_elite: bool) -> bool {
        e.expires_at > now
            && !e.health.backed_off(now)
            && (!require_elite || e.health.anon == AnonLevel::Elite)
    }

    /// OPT-R18 P2-3：池内节点年龄**中位数**（秒）。
    ///
    /// 为什么是中位数而不是均值：免费代理池的年龄分布是长尾的——大多数节点
    /// 几分钟就死，少数活几小时。均值会被长尾拖高，读起来像"池子很健康"；
    /// 中位数才回答"典型节点已经活了多久"。
    ///
    /// 返回 `None` 表示池为空（调用方渲染 NaN，不能渲染 0）。
    pub fn median_age_secs(&self, now: Instant, require_elite: bool) -> Option<f64> {
        let mut ages: Vec<f64> = self
            .entries
            .values()
            .filter(|e| Self::is_pooled(e, now, require_elite))
            .map(|e| now.saturating_duration_since(e.first_seen).as_secs_f64())
            .collect();
        if ages.is_empty() {
            return None;
        }
        ages.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
        let mid = ages.len() / 2;
        Some(if ages.len().is_multiple_of(2) {
            (ages[mid - 1] + ages[mid]) / 2.0
        } else {
            ages[mid]
        })
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

/// OPT-R4 B3：intake 上限公式（max_nodes×factor，保底 100；factor 由
/// `FREE_INTAKE_FACTOR` 入，调用方保证 factor≥1，内部再 max(1) 兜底）。
pub(super) fn intake_cap_limit(max_nodes: usize, factor: usize) -> usize {
    max_nodes.saturating_mul(factor.max(1)).max(100)
}

/// OPT-R4 B7：基线轮询表（主基址＋fallback 去重保序；https-only 由配置层保证）。
pub(super) fn baseline_bases(primary: &str, fallbacks: &[String]) -> Vec<String> {
    let mut bases = vec![primary.to_string()];
    for b in fallbacks {
        if !bases.contains(b) {
            bases.push(b.clone());
        }
    }
    bases
}

/// OPT-R4 B8：加权截断——Elite 历史优先，其次源序（fetch_all 已按
/// api>html>github 拼接，首次出现序即优先级），同级内稳定排序保原始相对顺序.
/// 确定性（无 shuffle）：落选者下轮重抓仍有机会；调用方保证 cap>0。
pub(super) fn cap_intake(
    mut raws: Vec<RawNode>,
    cap: usize,
    elite_addrs: &HashSet<String>,
) -> Vec<RawNode> {
    if raws.len() <= cap {
        return raws;
    }
    let mut order: HashMap<String, usize> = HashMap::new();
    for r in &raws {
        let n = order.len();
        order.entry(r.source.clone()).or_insert(n);
    }
    raws.sort_by_key(|r| {
        let elite = elite_addrs.contains(&format!("{}:{}", r.ip, r.port));
        (!elite, *order.get(&r.source).unwrap_or(&usize::MAX))
    });
    raws.truncate(cap);
    raws
}
