//! GW-4 multi-tenant auth, zero-lock throttling, and usage metering.
//!
//! Data-plane checks are allocation-free: one DashMap lookup, one token-bucket
//! probe, one CAS slot grab. Pricing (plan): DC $0.2/GB, Residential $3/GB,
//! Mobile $15/GB.

use atomic_float::AtomicF64;
use dashmap::DashMap;
use governor::clock::DefaultClock;
use governor::state::{InMemoryState, NotKeyed};
use governor::{Quota, RateLimiter};
use std::num::NonZeroU32;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::Arc;

/// Egress $/GB price table.
pub const PRICE_DC_PER_GB: f64 = 0.2;
pub const PRICE_RESIDENTIAL_PER_GB: f64 = 3.0;
pub const PRICE_MOBILE_PER_GB: f64 = 15.0;
/// 免费线 $/GB 单价（R2-FreePool：免费节点仍计量字节，单价 0）。
pub const PRICE_FREE_PER_GB: f64 = 0.0;
const BYTES_PER_GB: f64 = 1024.0 * 1024.0 * 1024.0;

pub fn price_per_gb(tier: &str) -> f64 {
    match tier.to_ascii_lowercase().as_str() {
        "residential" | "res" => PRICE_RESIDENTIAL_PER_GB,
        "mobile" => PRICE_MOBILE_PER_GB,
        "free" => PRICE_FREE_PER_GB,
        _ => PRICE_DC_PER_GB,
    }
}

/// Tenant account with live throttle + metering state.
pub struct TenantAccount {
    pub tenant_id: String,
    /// Control-plane key id (map key); kept for billing joins (GW-R2).
    #[allow(dead_code)]
    pub api_key: String,
    pub is_active: AtomicBool,
    pub limiter: RateLimiter<NotKeyed, InMemoryState, DefaultClock>,
    pub in_flight: AtomicUsize,
    pub max_concurrency: usize,
    pub total_bytes: AtomicU64,
    pub balance_usd: AtomicF64,
}

pub struct TenantManager {
    tenants: DashMap<String, Arc<TenantAccount>>,
}

impl std::fmt::Debug for TenantAccount {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TenantAccount")
            .field("tenant_id", &self.tenant_id)
            .field("is_active", &self.is_active)
            .field("max_concurrency", &self.max_concurrency)
            .finish_non_exhaustive()
    }
}

// =====================================================================
// OPT-R12：鉴权密钥卫生（常量单一真源 ＋ 策略纯函数）
// =====================================================================
//
// # 为何把 `DEFAULT_API_KEY` 搬到 `tenant.rs`
//
// 它此前定义在 `gateway.rs`（**数据面**模块），却被 SDK／脚本／文档当**鉴权
// 公共约定**引用——结构性错位：改数据面的人会以为碰不到鉴权，改鉴权的人却要
// 去数据面找常量。鉴权常量的归属地就是租户模块（key 的持有者正是
// `TenantManager`）。**纯搬迁，行为不变。**（本仓惯例：新模块 2026-09-28
// 单独成文件而非塞进既有模块，故沿用。）
//
// # `REQUIRE_API_KEY=0` 的真实语义（此前**没有任何地方这样写**）
//
// 关掉鉴权门**不等于**"无鉴权"：无头请求会被
// `unwrap_or_else(|| DEFAULT_API_KEY.to_string())` **静默补成默认租户**，
// 而默认租户是 qps/并发 10000/10000。实测（release 二进制）：不带任何
// `X-API-Key` 的请求返回 **200** ＋ 完整代理服务。
//
// 也就是说 `REQUIRE_API_KEY=0` 的语义是"**全网共享一个满额身份**"，
// 而不是"没有鉴权"。这才是本项定级为 P1 的真正理由——**弱默认值的失败
// 模式没有被写下来**，运维读文档以为关掉了鉴权，实际是敞开。
//
// # 三条不变式（本组全部改动都必须保持）
//
// 1. `API_KEY` **未设置** ⇒ 仍注册 `default_key`（本机开发与本仓自检依赖它，
//    改动会让存量部署升级即挂）。
// 2. `API_KEY` **已设置** ⇒ 只注册它，`default_key` **立即失效**（请求 403）。
//    这是本轮用户明确选择的最强档，不再做双 key 并存过渡。
// 3. 任何情况下都不因配置而**拒绝启动**；风险一律以 `warn!` 表达
//    （fail-closed 只用于**误配**这一条，见 `resolve_api_key`）。

/// 兜底 API Key（**仅本机开发/自检**）。生产必须用 `API_KEY` 覆盖。
pub const DEFAULT_API_KEY: &str = "default_key";

/// 默认租户的哨兵配额（qps 与并发同值）。用于识别"满配额"。
///
/// 单独成为常量而非内联 `10_000`：A1 的告警判定必须引用**同一份**数值，
/// 否则改了注册处的配额而忘了改判定，告警就成了永远不触发的死代码。
pub const TENANT_SENTINEL_QUOTA: u32 = 10_000;

/// 从 `API_KEY` env 解析生效的默认租户 key。
///
/// # 语义
///
/// - `Ok(None)` ⇒ 未设置 ⇒ 调用方回退 [`DEFAULT_API_KEY`]（开发默认）。
/// - `Ok(Some(k))` ⇒ 使用 `k`，且**此时 `default_key` 失效**。
/// - `Err(_)` ⇒ 误配（空串／纯空白／显式等于 `default_key`）⇒ **fail-closed**：
///   宁可不注册任何默认租户（全部 403），也不接受一个等于"没配"的 key——
///   后者会让运维以为配好了，实际仍在用公开的弱默认值。
///
/// 纯函数（不读 env 本身，只吃入参）以便单测穷举；env 读取在 `main.rs`。
pub fn resolve_api_key(raw: Option<&str>) -> Result<Option<String>, &'static str> {
    match raw {
        None => Ok(None),
        Some(s) => {
            let t = s.trim();
            if t.is_empty() {
                return Err("API_KEY is set but empty/whitespace; refusing to fall back to the public default key");
            }
            if t == DEFAULT_API_KEY {
                return Err(
                    "API_KEY is set to the public default value; refusing — set a strong random value instead",
                );
            }
            Ok(Some(t.to_string()))
        }
    }
}

/// A1：判定当前默认租户配置是否属于「危险组合」并给出告警文本。
///
/// 危险 = **同时**满足：弱默认值 ＋ 满配额 ＋ 非回环监听。三者缺一不告警——
/// 这是"防噪音"的关键：本机开发（弱默认值 ＋ 满配额 ＋ **回环**）与
/// 生产正确配置（强 key ＋ 满配额 ＋ 非回环）都**不该**被这条告警打扰。
///
/// # 为何只是 `warn!` 而非拒绝启动
///
/// 满足三条件确实是真实事故组合，但直接 panic 会让存量部署**升级即挂**，
/// 违反本轮"存量行为零变化"铁律。`warn!` 能在零行为变更前提下把"静默敞开"
/// 变成"日志里明写敞开"，并逐条给出整改动作。
///
/// 返回 `None` 表示无需告警（**不要**返回空串占位——那会让调用方难以区分
/// "不告警"与"告警但无内容"）。
pub fn weak_auth_warning(
    key_is_default: bool,
    qps: u32,
    max_concurrency: usize,
    listen: &str,
) -> Option<String> {
    if !key_is_default {
        return None; // 已用 API_KEY 覆盖，不是弱默认值。
    }
    let full_quota =
        qps >= TENANT_SENTINEL_QUOTA && max_concurrency >= TENANT_SENTINEL_QUOTA as usize;
    if !full_quota {
        return None; // 配额已收窄，风险已被限制。
    }
    if is_loopback_listen(listen) {
        return None; // 只监听回环 ⇒ 仅本机可达，外部无法滥用。
    }
    Some(format!(
        "INSECURE DEFAULT AUTH: tenant 'default' is using the public key {DEFAULT_API_KEY:?} \
         with sentinel quota {qps} qps / {max_concurrency} concurrent, listening on {listen} \
         (non-loopback). Anyone who can reach this port shares ONE full-rate identity. \
         Remediation (do at least 1): 1) set API_KEY to a strong random value \
         (default_key then stops working); 2) narrow the quota below {TENANT_SENTINEL_QUOTA}; \
         3) bind loopback (GATEWAY_ADDR=127.0.0.1:8916) and front the port with a WAF/mTLS proxy."
    ))
}

/// 监听地址是否回环（`127.0.0.0/8`、`::1`、`localhost`）。
///
/// 保守取向：**只**认这些明确的回环写法；`0.0.0.0`／`::`／空串／任何无法
/// 解析出 ip 的字符串都**不**算回环（宁可多告警一次，也不要漏掉真实暴露）。
pub fn is_loopback_listen(listen: &str) -> bool {
    let s = listen.trim();
    // 先尝试把**整串**直接当 IP 解析——这是裸 IPv6（如 `::1`）唯一正确的读法。
    //
    // 【踩坑记录】首版直接 `rsplit_once(':')` 剥端口，于是 `::1` 被切成
    // host=`:` / port=`1`，解析失败 ⇒ 判成"非回环" ⇒ 对**合法的回环绑定**
    // 误发告警。而本函数的价值恰恰在于"不该响的时候不响"，被自己的误判
    // 破掉就等于噪音门。故必须先试整体解析。
    if let Ok(ip) = s.parse::<std::net::IpAddr>() {
        return ip.is_loopback();
    }
    // 带端口（或带方括号）的写法：剥掉端口再解析。
    let host = match s.rsplit_once(':') {
        Some((h, _port)) => h,
        None => s,
    };
    let host = host.trim().trim_matches('[').trim_matches(']');
    if host.eq_ignore_ascii_case("localhost") {
        return true;
    }
    match host.parse::<std::net::IpAddr>() {
        Ok(ip) => ip.is_loopback(),
        Err(_) => false,
    }
}

impl TenantManager {
    pub fn new() -> Self {
        Self {
            tenants: DashMap::new(),
        }
    }

    /// R2-3 默认 burst：`qps/10`（至少 1）。突发容忍与限流强度解耦，
    /// 老调用按此换算后行为不变（`qps=1 → burst=1`，存量单测语义冻结）。
    pub fn default_burst_for_qps(qps: u32) -> u32 {
        (qps / 10).max(1)
    }

    pub fn register_tenant(
        &self,
        tenant_id: &str,
        api_key: &str,
        qps: u32,
        max_concurrency: usize,
        burst: u32,
    ) {
        let quota = Quota::per_second(NonZeroU32::new(qps.max(1)).expect("qps must be non-zero"))
            .allow_burst(NonZeroU32::new(burst.max(1)).expect("burst must be non-zero"));
        let account = Arc::new(TenantAccount {
            tenant_id: tenant_id.to_string(),
            api_key: api_key.to_string(),
            is_active: AtomicBool::new(true),
            limiter: RateLimiter::direct(quota),
            in_flight: AtomicUsize::new(0),
            max_concurrency,
            total_bytes: AtomicU64::new(0),
            balance_usd: AtomicF64::new(100.0),
        });
        self.tenants.insert(api_key.to_string(), account);
    }

    /// Deactivate without dropping live references (billing holds `Arc`s).
    /// Control-plane op (covered by unit tests; live use lands in GW-R2).
    #[allow(dead_code)]
    pub fn set_active(&self, api_key: &str, active: bool) -> bool {
        match self.tenants.get(api_key) {
            Some(entry) => {
                entry.value().is_active.store(active, Ordering::Relaxed);
                true
            }
            None => false,
        }
    }

    /// Auth + balance + QPS + concurrency gate. On success the caller owns one
    /// in-flight slot and MUST call [`Self::release_and_meter`].
    ///
    /// R2-3：余额门置于 QPS/并发之前——欠费租户快速失败，不烧 rate 令牌、
    /// 不占并发槽（ shaping 顺序：身份 → 开关 → 余额 → 速率 → 并发）。
    pub fn authenticate_and_throttle(
        &self,
        api_key: &str,
    ) -> Result<Arc<TenantAccount>, &'static str> {
        let tenant = self.tenants.get(api_key).ok_or("Invalid API Key")?;
        if !tenant.is_active.load(Ordering::Relaxed) {
            return Err("Tenant is disabled");
        }
        if tenant.balance_usd.load(Ordering::Relaxed) <= 0.0 {
            return Err("Insufficient balance");
        }
        if tenant.limiter.check().is_err() {
            return Err("Rate limit exceeded (QPS)");
        }
        let current = tenant.in_flight.fetch_add(1, Ordering::Relaxed);
        if current >= tenant.max_concurrency {
            tenant.in_flight.fetch_sub(1, Ordering::Relaxed);
            return Err("Max concurrency limit reached");
        }
        Ok(tenant.clone())
    }

    /// Release the slot, add bytes, and deduct the tier price.
    ///
    /// R2-3：允许一次扣成负数（当次流量不断流），欠费由下次 `authenticate` 拦截。
    pub fn release_and_meter(&self, tenant: &TenantAccount, bytes: u64, tier: &str) {
        // OPT-R4 A1：旧 `load==0` 检查与 `fetch_sub` 非原子，并发双释放可同时过检
        // 把 in_flight 绕回 usize::MAX（永久熔断该租户并发）；改 CAS 循环原子扣槽
        //（不用 `fetch_sub` 返回值判定：零槽先减后加会瞬时暴露 MAX，可被并发
        // authenticate 观测到）。零槽误调直接丢弃＋warn。
        // 可观测说明：MetricsRegistry 暂无租户级 double-release 计数器，不新增指标，
        // 沿 REVIEW-R2 Q6 口径只打 warn 日志（正常单次释放行为不变）。
        let mut cur = tenant.in_flight.load(Ordering::Relaxed);
        loop {
            if cur == 0 {
                // 误调整单直接丢弃（首次释放已计量，重复计量即双重扣费）；只 warn 可观测。
                log::warn!(
                    "[Tenant] release without slot tenant={} (double-release suspected, dropped)",
                    tenant.tenant_id
                );
                return;
            }
            match tenant.in_flight.compare_exchange_weak(
                cur,
                cur - 1,
                Ordering::Relaxed,
                Ordering::Relaxed,
            ) {
                Ok(_) => break,
                Err(actual) => cur = actual,
            }
        }
        tenant.total_bytes.fetch_add(bytes, Ordering::Relaxed);
        let cost = (bytes as f64 / BYTES_PER_GB) * price_per_gb(tier);
        tenant.balance_usd.fetch_sub(cost, Ordering::Relaxed);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn manager_with(api_key: &str, qps: u32, max_c: usize) -> TenantManager {
        let m = TenantManager::new();
        // 存量 helper 走默认 burst（qps/10≥1），语义与 R2-3 前一致。
        m.register_tenant(
            "t-1",
            api_key,
            qps,
            max_c,
            TenantManager::default_burst_for_qps(qps),
        );
        m
    }

    #[test]
    fn price_table_matches_plan() {
        assert_eq!(price_per_gb("datacenter"), 0.2);
        assert_eq!(price_per_gb("dc"), 0.2);
        assert_eq!(price_per_gb("residential"), 3.0);
        assert_eq!(price_per_gb("mobile"), 15.0);
        assert_eq!(price_per_gb("weird"), 0.2);
        assert_eq!(price_per_gb("free"), 0.0);
    }

    #[test]
    fn qps_overflow_rejected() {
        let m = manager_with("k1", 1, 100);
        let a = m.authenticate_and_throttle("k1").expect("first");
        // Second immediate check exhausts the 1/s bucket.
        let err = m.authenticate_and_throttle("k1").unwrap_err();
        assert!(err.contains("Rate limit"), "{err}");
        m.release_and_meter(&a, 0, "datacenter");
    }

    #[test]
    fn concurrency_cap_rejected_and_slot_returned() {
        let m = manager_with("k2", 10_000, 1);
        let a = m.authenticate_and_throttle("k2").expect("slot 1");
        let err = m.authenticate_and_throttle("k2").unwrap_err();
        assert!(err.contains("concurrency"), "{err}");
        // Rejected attempt must not leak the slot.
        assert_eq!(a.in_flight.load(Ordering::Relaxed), 1);
        m.release_and_meter(&a, 512, "residential");
        assert_eq!(a.in_flight.load(Ordering::Relaxed), 0);
        assert_eq!(a.total_bytes.load(Ordering::Relaxed), 512);
    }

    #[test]
    fn double_release_does_not_wrap() {
        // REVIEW-R2 Q6：误调第二次 release 不得 wrap 到 MAX（永久熔断并发）；槽回 0。
        let m = manager_with("k-double", 10_000, 100);
        let a = m.authenticate_and_throttle("k-double").expect("auth");
        m.release_and_meter(&a, 0, "datacenter");
        assert_eq!(a.in_flight.load(Ordering::Relaxed), 0);
        m.release_and_meter(&a, 0, "datacenter");
        assert_eq!(
            a.in_flight.load(Ordering::Relaxed),
            0,
            "double release must not wrap in_flight"
        );
    }

    #[test]
    fn concurrent_double_release_no_wraparound() {
        // OPT-R4 A1：`load==0` 检查与 `fetch_sub` 非原子，并发双释放可同时过检
        // 把 in_flight 绕回 usize::MAX（永久熔断该租户并发）。
        // Barrier 对齐 8 线程同抢 1 个槽位：旧写法高概率回绕（红），CAS 修复后恒为 0（绿）。
        // bytes=0 使计量无副作用，只验槽位有界。
        let m = manager_with("k-a1-race", 10_000, 100);
        let a = m.authenticate_and_throttle("k-a1-race").expect("auth");
        assert_eq!(a.in_flight.load(Ordering::Relaxed), 1);
        for _ in 0..200 {
            a.in_flight.store(1, Ordering::Relaxed);
            let barrier = Arc::new(std::sync::Barrier::new(8));
            let mgr = &m;
            std::thread::scope(|s| {
                for _ in 0..8 {
                    let b = barrier.clone();
                    let acc = a.clone();
                    s.spawn(move || {
                        b.wait();
                        mgr.release_and_meter(&acc, 0, "datacenter");
                    });
                }
            });
            let v = a.in_flight.load(Ordering::Relaxed);
            assert!(
                v <= 1,
                "concurrent double-release must not wrap in_flight, got {v}"
            );
            assert_eq!(v, 0, "8 releases on 1 slot must drain to 0, got {v}");
        }
        // 计量无副作用（bytes=0）：total_bytes 保持 0，balance 保持 100。
        assert_eq!(a.total_bytes.load(Ordering::Relaxed), 0);
        assert!((a.balance_usd.load(Ordering::Relaxed) - 100.0).abs() < 1e-9);
    }

    #[test]
    fn metering_deducts_tier_price() {
        let m = manager_with("k3", 10_000, 100);
        let a = m.authenticate_and_throttle("k3").expect("auth");
        let one_gb = 1024 * 1024 * 1024u64;
        m.release_and_meter(&a, one_gb, "residential");
        assert!((a.balance_usd.load(Ordering::Relaxed) - 97.0).abs() < 1e-9);
        let b = m.authenticate_and_throttle("k3").expect("auth");
        m.release_and_meter(&b, one_gb, "mobile");
        assert!((b.balance_usd.load(Ordering::Relaxed) - 82.0).abs() < 1e-9);
    }

    #[test]
    fn unknown_and_disabled_keys_rejected() {
        let m = manager_with("k4", 10_000, 100);
        assert_eq!(
            m.authenticate_and_throttle("nope").unwrap_err(),
            "Invalid API Key"
        );
        assert!(m.set_active("k4", false));
        assert_eq!(
            m.authenticate_and_throttle("k4").unwrap_err(),
            "Tenant is disabled"
        );
        assert!(!m.set_active("ghost", false));
    }

    #[test]
    fn default_burst_scales_with_qps() {
        // R2-3：qps/10，至少 1（qps=1 老行为 burst=1 不变）。
        assert_eq!(TenantManager::default_burst_for_qps(1), 1);
        assert_eq!(TenantManager::default_burst_for_qps(9), 1);
        assert_eq!(TenantManager::default_burst_for_qps(50), 5);
        assert_eq!(TenantManager::default_burst_for_qps(10_000), 1_000);
    }

    #[test]
    fn burst_allows_short_burst_then_shapes() {
        // R2-3：burst=5 时 5 连击全过、第 6 次被整形；拒绝不占槽。
        let m = TenantManager::new();
        m.register_tenant("t-b", "kb", 5, 10, 5);
        let mut held = Vec::new();
        for _ in 0..5 {
            held.push(m.authenticate_and_throttle("kb").expect("burst slot"));
        }
        let err = m.authenticate_and_throttle("kb").unwrap_err();
        assert!(err.contains("Rate limit"), "{err}");
        assert_eq!(held[0].in_flight.load(Ordering::Relaxed), 5);
        for a in &held {
            m.release_and_meter(a, 0, "datacenter");
        }
        assert_eq!(held[0].in_flight.load(Ordering::Relaxed), 0);
    }

    #[test]
    fn insufficient_balance_rejected_without_slot() {
        // R2-3：一次性扣穿（1TB residential ≈ $3072 > $100 起步余额），
        // 当次不断流（release 照常），下次鉴权拦截且不占槽。
        let m = manager_with("k5", 10_000, 100);
        let a = m.authenticate_and_throttle("k5").expect("auth");
        let one_tb = 1024u64 * 1024 * 1024 * 1024;
        m.release_and_meter(&a, one_tb, "residential");
        assert!(a.balance_usd.load(Ordering::Relaxed) < 0.0);
        let before = a.in_flight.load(Ordering::Relaxed);
        let err = m.authenticate_and_throttle("k5").unwrap_err();
        assert_eq!(err, "Insufficient balance");
        // 拒绝路径不取槽。
        assert_eq!(a.in_flight.load(Ordering::Relaxed), before);
    }

    // ---- OPT-R12：鉴权密钥卫生 ----

    /// B1：`API_KEY` 解析三态，且**误配 fail-closed**。
    #[test]
    fn opt_r12_resolve_api_key_states() {
        use super::{resolve_api_key, DEFAULT_API_KEY};
        // 1) 未设置 → None（调用方回退开发默认）。
        assert_eq!(resolve_api_key(None).unwrap(), None);
        // 2) 设置为强值 → 使用它，且不得等于默认值。
        let k = resolve_api_key(Some("  s3cret-value  ")).unwrap().unwrap();
        assert_eq!(k, "s3cret-value", "应 trim");
        assert_ne!(k, DEFAULT_API_KEY);
        // 3) 误配：空串 / 纯空白 / 显式等于 default_key → 全部 Err（不回退）。
        assert!(
            resolve_api_key(Some("")).is_err(),
            "空串必须拒（否则等于没配）"
        );
        assert!(resolve_api_key(Some("   \t\n")).is_err(), "纯空白必须拒");
        assert!(
            resolve_api_key(Some(DEFAULT_API_KEY)).is_err(),
            "显式设为 default_key 必须拒（否则运维不变且误以为已配）"
        );
    }

    /// A1：危险组合判定——**三条同时才响**。
    ///
    /// 重点是「**不该响的不响**」：本机开发与生产正确配置
    /// 都不应被这条告警扰动——否则它会被当成噪声而弃用。
    #[test]
    fn opt_r12_weak_auth_warning_only_for_genuine_risk() {
        use super::{weak_auth_warning, DEFAULT_API_KEY, TENANT_SENTINEL_QUOTA};
        let q = TENANT_SENTINEL_QUOTA;
        let conc = TENANT_SENTINEL_QUOTA as usize;

        // 应响：弱默认 key 且满配额且**非回环**监听。
        let w = weak_auth_warning(true, q, conc, "0.0.0.0:8916");
        assert!(w.is_some(), "0.0.0.0 上的弱默认 key 必须告警");
        let msg = w.unwrap();
        assert!(msg.contains("API_KEY"), "告警必须给出整改动作（API_KEY）");
        assert!(
            msg.contains("loopback") || msg.contains("127.0.0.1"),
            "告警必须给出绑回环的动作"
        );
        assert!(
            msg.contains(DEFAULT_API_KEY),
            "告警必须明说用的是公开默认值"
        );

        // 不应响 1：本机开发（弱默认 但回环监听）。
        assert!(
            weak_auth_warning(true, q, conc, "127.0.0.1:8916").is_none(),
            "回环监听时不应告警（本机开发场景）"
        );
        // 不应响 2：已用 API_KEY 覆盖（非弱默认）。
        assert!(
            weak_auth_warning(false, q, conc, "0.0.0.0:8916").is_none(),
            "已用 API_KEY 时不应告警"
        );
        // 不应响 3：配额已收窄（危险已被限制）。
        assert!(
            weak_auth_warning(true, 100, 100, "0.0.0.0:8916").is_none(),
            "配额低于哨兵时不应告警"
        );
    }

    /// 回环判定：只认明确回环写法，**保守取向**。
    #[test]
    fn opt_r12_is_loopback_listen_conservative() {
        use super::is_loopback_listen;
        for yes in [
            "127.0.0.1:8916",
            "127.0.0.1",
            " 127.0.0.1:1 ",
            "localhost:8916",
            "LOCALHOST:8916",
            "127.5.5.5:8916",
            "[::1]:8916",
            "::1",
        ] {
            assert!(is_loopback_listen(yes), "应判为回环: {yes:?}");
        }
        for no in [
            "0.0.0.0:8916",
            "0.0.0.0",
            "::",
            "[::]:8916",
            "10.0.0.5:8916",
            "192.168.1.9:8916",
            "example.com:8916",
            "",
            "   ",
        ] {
            assert!(!is_loopback_listen(no), "不应判为回环: {no:?}");
        }
    }
}
