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
        // REVIEW-R2 Q6：误调（槽已空）不得 wrap 到 MAX（永久熔断该租户并发）；
        // 加回＋warn 可观测，正常单次释放行为不变。
        if tenant.in_flight.load(Ordering::Relaxed) == 0 {
            // 误调整单直接丢弃（首次释放已计量，重复计量即双重扣费）；只 warn 可观测。
            log::warn!(
                "[Tenant] release without slot tenant={} (double-release suspected, dropped)",
                tenant.tenant_id
            );
            return;
        }
        tenant.in_flight.fetch_sub(1, Ordering::Relaxed);
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
}
