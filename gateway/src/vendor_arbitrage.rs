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
    fn derate_removes_vendor_country_nodes() {
        use crate::model::ProxyNode;
        let nodes = vec![
            ProxyNode {
                ip: "10.0.0.1".to_string(),
                port: 8080,
                username: None,
                password: None,
                country: "US".to_string(),
                tier: "residential".to_string(),
                provider: "mock-a".to_string(),
                weight: 100,
            },
            ProxyNode {
                ip: "10.0.0.2".to_string(),
                port: 8080,
                username: None,
                password: None,
                country: "US".to_string(),
                tier: "datacenter".to_string(),
                provider: "mock-b".to_string(),
                weight: 80,
            },
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
        };
        let remaining = router.get_healthy_candidates(&us);
        assert_eq!(remaining.len(), 1);
        assert_eq!(remaining[0].provider, "mock-b");
        // A >95% verdict restores… (restore re-add is out of scope for the
        // GW-1 retain/remove shim; weight path is covered by adjust unit use).
        assert_eq!(arbitrage_action(99.0), Some(100));
    }
}
