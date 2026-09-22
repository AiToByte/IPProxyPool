//! GW-4 vendor SLA arbitrage: 60s audit over vendor×country, derate/restore.
//!
//! First-round vendors are all Mock (`mock-a/b/c`, plan: real keys land via
//! staging 1% gray release in GW-R2). [`arbitrage_action`] is the pure policy
//! core (<80 → weight 0, >95 → weight 100, else hold); the worker only maps it
//! onto [`RouterEngine::adjust_vendor_weight`].

use crate::analytics::{AnalyticsEngine, SLA_DERATE_BELOW, SLA_RESTORE_ABOVE};
use crate::router::RouterEngine;
use std::sync::Arc;
use std::time::Duration;

/// Audit period (plan: 60s).
pub const ARBITRAGE_INTERVAL: Duration = Duration::from_secs(60);
/// Weight applied on derate (cut) and restore (full).
pub const WEIGHT_CUT: u32 = 0;
pub const WEIGHT_FULL: u32 = 100;

/// Policy core: success rate → new weight (`None` = hold current weight).
pub fn arbitrage_action(success_rate: f64) -> Option<u32> {
    if success_rate < SLA_DERATE_BELOW {
        Some(WEIGHT_CUT)
    } else if success_rate > SLA_RESTORE_ABOVE {
        Some(WEIGHT_FULL)
    } else {
        None
    }
}

/// P3 免费独立套利：池级成功率 → 缩放因子（`None` = hold）。
/// 分档：小于 50 摘除为 0.0（TTL 到期＋复检通过即自愈，与 paid 小于 80 归 0 同族语义）；
/// 50~80 半权 0.5（free 上限本就远低于付费，半权即显著降载）；
/// 大于等于 80 则 hold（free 永不自动抬到付费量级；恢复走复检/health 路径，注释写明）。
/// 付费 `arbitrage_action` 80/95 冻结不动。
pub fn free_pool_action(success_rate: f64) -> Option<f64> {
    if success_rate < 50.0 {
        Some(0.0)
    } else if success_rate < 80.0 {
        Some(0.5)
    } else {
        None
    }
}

pub struct VendorArbitrageWorker {
    analytics: Arc<AnalyticsEngine>,
    router: Arc<RouterEngine>,
    vendors: Vec<String>,
    countries: Vec<String>,
    interval: Duration,
}

impl VendorArbitrageWorker {
    pub fn new(
        analytics: Arc<AnalyticsEngine>,
        router: Arc<RouterEngine>,
        vendors: Vec<String>,
        countries: Vec<String>,
    ) -> Self {
        Self {
            analytics,
            router,
            vendors,
            countries,
            interval: ARBITRAGE_INTERVAL,
        }
    }

    /// 注入审计间隔的 builder（R2-8 env 覆盖用；默认 60s）。
    pub fn with_interval(mut self, interval: Duration) -> Self {
        self.interval = interval;
        self
    }

    /// One audit pass over every vendor×country pair. CH errors are logged
    /// and skipped (degraded mode: weights simply hold).
    pub async fn audit_once(&self) {
        for country in &self.countries {
            for vendor in &self.vendors {
                match self.analytics.query_provider_sla(vendor, country).await {
                    Ok(rate) => {
                        log::info!(
                            "[Arbitrage] vendor={vendor} country={country} success={rate:.2}%"
                        );
                        if let Some(weight) = arbitrage_action(rate) {
                            self.router.adjust_vendor_weight(vendor, country, weight);
                            log::warn!(
                                "[Arbitrage] vendor={vendor} country={country} weight->{weight} (rate={rate:.2}%)"
                            );
                        }
                    }
                    Err(e) => {
                        log::error!(
                            "[Arbitrage] SLA query failed vendor={vendor} country={country}: {e:?} (weights hold)"
                        );
                    }
                }
            }
        }
        self.audit_free_once().await;
    }

    /// P3 免费分支：枚举池内 `free-*` provider×country 去重对 → 池级 SLA →
    /// 分档 scale（`free_pool_action`）。与付费循环解耦；CH 错 hold（同 degraded 语义）。
    /// 恢复走复检/health 路径（本函数永不抬权，见 `free_pool_action`）。
    pub async fn audit_free_once(&self) {
        use std::collections::BTreeSet;
        let pairs: BTreeSet<(String, String)> = self
            .router
            .snapshot_all()
            .iter()
            .filter(|n| n.provider.starts_with("free-"))
            .map(|n| (n.provider.clone(), n.country.clone()))
            .collect();
        for (vendor, country) in pairs {
            match self.analytics.query_provider_sla(&vendor, &country).await {
                Ok(rate) => {
                    log::info!(
                        "[Arbitrage] free vendor={vendor} country={country} success={rate:.2}%"
                    );
                    if let Some(factor) = free_pool_action(rate) {
                        self.router.scale_vendor_weights(&vendor, &country, factor);
                        log::warn!(
                            "[Arbitrage] free vendor={vendor} country={country} scale->{factor} (rate={rate:.2}%)"
                        );
                    }
                }
                Err(e) => {
                    log::error!(
                        "[Arbitrage] free SLA query failed vendor={vendor} country={country}: {e:?} (weights hold)"
                    );
                }
            }
        }
    }

    /// Background loop until process exit.
    pub async fn run(self) {
        let mut ticker = tokio::time::interval(self.interval);
        loop {
            ticker.tick().await;
            self.audit_once().await;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn policy_matches_plan_thresholds() {
        assert_eq!(arbitrage_action(79.9), Some(0));
        assert_eq!(arbitrage_action(0.0), Some(0));
        assert_eq!(arbitrage_action(80.0), None);
        assert_eq!(arbitrage_action(90.0), None);
        assert_eq!(arbitrage_action(95.0), None);
        assert_eq!(arbitrage_action(95.1), Some(100));
        assert_eq!(arbitrage_action(100.0), Some(100));
    }

    #[test]
    fn free_pool_action_thresholds() {
        // P3-2：池级成功率分档→缩放因子。<50 摘除（复检自愈）；50~80 半权；
        // ≥80 hold（free 永不自动抬权，恢复走复检/health 路径）；付费 80/95 冻结不动。
        assert_eq!(free_pool_action(0.0), Some(0.0));
        assert_eq!(free_pool_action(49.9), Some(0.0));
        assert_eq!(free_pool_action(50.0), Some(0.5));
        assert_eq!(free_pool_action(79.9), Some(0.5));
        assert_eq!(free_pool_action(80.0), None);
        assert_eq!(free_pool_action(100.0), None);
    }

    #[test]
    fn derate_removes_vendor_country_nodes() {
        use crate::model::ProxyNode;
        let nodes = vec![
            ProxyNode::new(
                "10.0.0.1".to_string(),
                8080,
                None,
                None,
                "US".to_string(),
                "residential".to_string(),
                "mock-a".to_string(),
                100,
            ),
            ProxyNode::new(
                "10.0.0.2".to_string(),
                8080,
                None,
                None,
                "US".to_string(),
                "datacenter".to_string(),
                "mock-b".to_string(),
                80,
            ),
        ];
        let router = RouterEngine::new(nodes);
        // Simulate a <80% audit verdict for mock-a/US.
        if let Some(w) = arbitrage_action(42.0) {
            router.adjust_vendor_weight("mock-a", "US", w);
        }
        let us = crate::model::RoutingSpec {
            country: Some("US".to_string()),
            session_id: None,
            tier: None,
            target_domain: "x.example".to_string(),
            proto: None,
        };
        let remaining = router.get_healthy_candidates(&us);
        assert_eq!(remaining.len(), 1);
        assert_eq!(remaining[0].provider, "mock-b");
        // A >95% verdict restores… (restore re-add is out of scope for the
        // GW-1 retain/remove shim; weight path is covered by adjust unit use).
        assert_eq!(arbitrage_action(99.0), Some(100));
    }
}
