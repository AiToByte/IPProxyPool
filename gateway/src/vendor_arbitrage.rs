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
use tokio::task::JoinSet;

/// Audit period (plan: 60s).
pub const ARBITRAGE_INTERVAL: Duration = Duration::from_secs(60);
/// Weight applied on derate (cut) and restore (full).
pub const WEIGHT_CUT: u32 = 0;
pub const WEIGHT_FULL: u32 = 100;
/// A10：单路 SLA 查询超时。CH 抖动/慢查询时该路 hold 权重，
/// 并发总量有界（≈单路超时），不拖长 60s 滴答。
pub const SLA_QUERY_TIMEOUT: Duration = Duration::from_secs(5);

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

    /// One audit pass over every vendor×country pair. A10：JoinSet 并发＋
    /// 单路超时；任一 slow/错只影响自己那一路。失败（查询错/超时/任务
    /// join 失败）一律 hold 权重——不下调、不摘除（degraded 语义）。
    pub async fn audit_once(&self) {
        let analytics = Arc::clone(&self.analytics);
        self.audit_once_with(
            move |vendor: String, country: String| {
                let analytics = Arc::clone(&analytics);
                async move {
                    analytics
                        .query_provider_sla(&vendor, &country)
                        .await
                        .map_err(|e| format!("{e:?}"))
                }
            },
            SLA_QUERY_TIMEOUT,
        )
        .await;
    }

    /// 可注入查询的审计入口（生产走 CH，单测注入慢/错查询）。
    /// 付费 vendor×country 与 free 对各跑一批并发查询，结果回主任务串行调权。
    async fn audit_once_with<Q, Fut, E>(&self, query: Q, per_query_timeout: Duration)
    where
        Q: Fn(String, String) -> Fut + Send + Clone + 'static,
        Fut: std::future::Future<Output = Result<f64, E>> + Send,
        E: std::fmt::Debug + Send,
    {
        let mut paid: Vec<(String, String)> = Vec::new();
        for country in &self.countries {
            for vendor in &self.vendors {
                paid.push((vendor.clone(), country.clone()));
            }
        }
        for (vendor, country, r) in sla_query_concurrent(paid, &query, per_query_timeout).await {
            match r {
                Ok(rate) => {
                    log::info!("[Arbitrage] vendor={vendor} country={country} success={rate:.2}%");
                    if let Some(weight) = arbitrage_action(rate) {
                        self.router.adjust_vendor_weight(&vendor, &country, weight);
                        log::warn!(
                            "[Arbitrage] vendor={vendor} country={country} weight->{weight} (rate={rate:.2}%)"
                        );
                    }
                }
                Err(e) => {
                    // hold：查询错/超时均不改权重（不下调、不摘除）。
                    log::error!(
                        "[Arbitrage] SLA query failed vendor={vendor} country={country}: {e} (weights hold)"
                    );
                }
            }
        }
        self.audit_free_with(query, per_query_timeout).await;
    }

    /// 免费分支的可注入查询版本：枚举池内 `free-*` provider×country 去重对 →
    /// 池级 SLA → 分档 scale（`free_pool_action`）；与付费循环解耦。
    /// 并发＋单路超时，失败 hold（不缩放、不摘除）；恢复走复检/health 路径
    /// （本函数永不抬权，见 `free_pool_action`）。
    async fn audit_free_with<Q, Fut, E>(&self, query: Q, per_query_timeout: Duration)
    where
        Q: Fn(String, String) -> Fut + Send + Clone + 'static,
        Fut: std::future::Future<Output = Result<f64, E>> + Send,
        E: std::fmt::Debug + Send,
    {
        use std::collections::BTreeSet;
        let pairs: BTreeSet<(String, String)> = self
            .router
            .snapshot_all()
            .iter()
            .filter(|n| n.provider.starts_with("free-"))
            .map(|n| (n.provider.clone(), n.country.clone()))
            .collect();
        for (vendor, country, r) in
            sla_query_concurrent(pairs.into_iter().collect(), &query, per_query_timeout).await
        {
            match r {
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
                    // hold：查询错/超时均不改权重（不缩放、不摘除）。
                    log::error!(
                        "[Arbitrage] free SLA query failed vendor={vendor} country={country}: {e} (weights hold)"
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

/// A10：并发跑一批 SLA 查询（JoinSet＋单路超时），返回与入参一一对应的 verdict。
/// 调用方按 `Ok(rate)`→调权 / `Err`→hold 处理；本函数永不抛错：
/// 超时、查询错、任务 join 失败全部折成 `Err`（hold），并记日志。
async fn sla_query_concurrent<Q, Fut, E>(
    pairs: Vec<(String, String)>,
    query: &Q,
    per_query_timeout: Duration,
) -> Vec<(String, String, Result<f64, String>)>
where
    Q: Fn(String, String) -> Fut + Send + Clone + 'static,
    Fut: std::future::Future<Output = Result<f64, E>> + Send,
    E: std::fmt::Debug + Send,
{
    if pairs.is_empty() {
        return Vec::new();
    }
    let mut set: JoinSet<(String, String, Result<f64, String>)> = JoinSet::new();
    for (vendor, country) in pairs {
        let q = query.clone();
        set.spawn(async move {
            let r =
                tokio::time::timeout(per_query_timeout, q(vendor.clone(), country.clone())).await;
            let mapped = match r {
                Ok(Ok(rate)) => Ok(rate),
                Ok(Err(e)) => Err(format!("sla query failed: {e:?}")),
                Err(_) => Err(format!("sla query timeout after {per_query_timeout:?}")),
            };
            (vendor, country, mapped)
        });
    }
    let mut out = Vec::with_capacity(set.len());
    while let Some(joined) = set.join_next().await {
        match joined {
            Ok(item) => out.push(item),
            // 任务 panic/abort：无 verdict 可应用，权重天然 hold，只记日志。
            Err(e) => {
                log::error!("[Arbitrage] sla query task join failed: {e} (weights hold)");
            }
        }
    }
    out
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

    // A10（S）：慢查询下 audit 有界完成（JoinSet 并发＋单路超时，不被串行拖长）。
    #[tokio::test]
    async fn slow_sla_queries_stay_bounded_and_hold_weights() {
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
                77,
            ),
            ProxyNode::new(
                "10.0.0.2".to_string(),
                8080,
                None,
                None,
                "JP".to_string(),
                "datacenter".to_string(),
                "mock-b".to_string(),
                77,
            ),
        ];
        let router = std::sync::Arc::new(RouterEngine::new(nodes));
        let analytics = std::sync::Arc::new(crate::analytics::AnalyticsEngine::new(
            "http://127.0.0.1:1",
            "u",
            "p",
            "db",
        ));
        let worker = VendorArbitrageWorker::new(
            analytics,
            router.clone(),
            vec!["mock-a".to_string(), "mock-b".to_string()],
            vec!["US".to_string(), "JP".to_string()],
        );
        // 每路都慢 5s（串行则 4 路=20s）；单路超时 50ms 下应有界完成。
        let slow = |_: String, _: String| async {
            tokio::time::sleep(std::time::Duration::from_secs(5)).await;
            Ok::<f64, String>(100.0)
        };
        let r = tokio::time::timeout(
            std::time::Duration::from_secs(2),
            worker.audit_once_with(slow, std::time::Duration::from_millis(50)),
        )
        .await;
        assert!(
            r.is_ok(),
            "慢查询下 audit 应有界完成（超时/并发缺失则挂起）"
        );
        // 超时路 hold 权重：77 不被 Ok(100) 恢复到 100。
        for n in router.snapshot_all() {
            assert_eq!(n.weight, 77, "超时失败应 hold 权重，不下调不恢复");
        }
    }

    // A10（S）：查询失败 hold 权重（不下调、不摘除）。
    #[tokio::test]
    async fn sla_error_holds_weight_no_derate() {
        use crate::model::ProxyNode;

        let nodes = vec![ProxyNode::new(
            "10.0.0.1".to_string(),
            8080,
            None,
            None,
            "US".to_string(),
            "residential".to_string(),
            "mock-a".to_string(),
            60,
        )];
        let router = std::sync::Arc::new(RouterEngine::new(nodes));
        let analytics = std::sync::Arc::new(crate::analytics::AnalyticsEngine::new(
            "http://127.0.0.1:1",
            "u",
            "p",
            "db",
        ));
        let worker = VendorArbitrageWorker::new(
            analytics,
            router.clone(),
            vec!["mock-a".to_string()],
            vec!["US".to_string()],
        );
        let failing = |_: String, _: String| async { Err::<f64, String>("boom".to_string()) };
        worker
            .audit_once_with(failing, std::time::Duration::from_millis(50))
            .await;
        let snap = router.snapshot_all();
        assert_eq!(snap.len(), 1);
        assert_eq!(snap[0].weight, 60, "失败路应 hold 权重，不下调、不摘除");
    }
}
