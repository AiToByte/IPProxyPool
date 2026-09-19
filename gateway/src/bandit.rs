//! GW-3 contextual LinUCB adaptive routing (d=4).
//!
//! Feature vector: `[DomainRisk, Bias, CosTime, LatencyPrior]`. Covariance
//! inverse is updated in place with Sherman-Morrison (O(d²), no matrix
//! inversion on the data plane). Arms are keyed by `node.addr()`
//! (`ip:port`), **not** bare IP, because mock/egress nodes may share an IP.
//!
//! Deviation from manual-A3: `extract_context` computes a real day-cycle
//! cosine instead of the hardcoded `0.5`, so time actually informs explore.

use nalgebra::{SMatrix, SVector};
use parking_lot::RwLock;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// Context dimension: DomainRisk / Bias / CosTime / LatencyPrior.
pub const CONTEXT_DIM: usize = 4;
/// Exploration factor at boot (plan: alpha 0.4 and up).
pub const DEFAULT_ALPHA: f64 = 0.4;
/// Tier cost weights (plan: DC 0.1 / Res 1.0 / Mobile 3.0).
pub const COST_DC: f64 = 0.1;
pub const COST_RESIDENTIAL: f64 = 1.0;
pub const COST_MOBILE: f64 = 3.0;

pub type VectorD = SVector<f64, CONTEXT_DIM>;
pub type MatrixD = SMatrix<f64, CONTEXT_DIM, CONTEXT_DIM>;

/// Cost weight for an egress tier.
pub fn cost_weight_for_tier(tier: &str) -> f64 {
    match tier.to_ascii_lowercase().as_str() {
        "datacenter" | "dc" => COST_DC,
        "mobile" => COST_MOBILE,
        _ => COST_RESIDENTIAL,
    }
}

/// Online reward in [0,1] from an observed response.
pub fn compute_reward(status_code: u16, latency: Duration) -> f64 {
    match status_code {
        200..=299 => 1.0 - (latency.as_millis() as f64 / 2000.0).min(0.5),
        403 | 429 => 0.0,
        _ => 0.2,
    }
}

/// One egress channel's LinUCB state.
pub struct BanditArm {
    /// Arm key (`ip:port`); doubles as the routing lookup key.
    pub key: String,
    /// Egress tier (kept for GW-4 arbitrage/analytics; cost folded into `cost_weight`).
    #[allow(dead_code)]
    pub tier: String,
    /// Single lock for the (matrix, vector) pair: one acquire per score.
    pub state: RwLock<ArmState>,
    pub cost_weight: f64,
}

/// Covariance inverse + bias vector, locked and updated together.
#[derive(Debug, Clone)]
pub struct ArmState {
    /// Inverse covariance `A_inv` (d×d), starts at identity.
    pub a_inv: MatrixD,
    /// Bias reward vector `b` (d×1), starts at zero.
    pub b: VectorD,
}

impl BanditArm {
    pub fn new(key: String, tier: String) -> Self {
        let cost_weight = cost_weight_for_tier(&tier);
        Self {
            key,
            tier,
            state: RwLock::new(ArmState {
                a_inv: MatrixD::identity(),
                b: VectorD::zeros(),
            }),
            cost_weight,
        }
    }

    /// Upper-confidence-bound score: predicted pass rate + explore bonus.
    ///
    /// Fast path: one lock acquire, one mat-vec for `theta`, one mat-vec for
    /// the quadratic form `xᵀA⁻¹x` (no intermediate matrix products).
    #[inline]
    pub fn compute_ucb_score(&self, context: &VectorD, alpha: f64) -> f64 {
        let state = self.state.read();
        // Ridge estimate theta = A_inv * b.
        let theta = state.a_inv * state.b;
        let expected_reward = theta.dot(context);
        // Uncertainty bound sqrt(xᵀ A_inv x), clamped at 0 for fp noise.
        let a_inv_x = state.a_inv * context;
        let variance = context.dot(&a_inv_x).max(0.0).sqrt();
        expected_reward + alpha * variance - 0.05 * self.cost_weight
    }

    /// Online update after passive feedback (Sherman-Morrison, O(d²)).
    pub fn update(&self, context: &VectorD, reward: f64) {
        let mut state = self.state.write();
        state.b += reward * context;
        let a_inv_x = state.a_inv * context;
        let denominator = 1.0 + context.dot(&a_inv_x);
        let numerator = a_inv_x * a_inv_x.transpose();
        state.a_inv -= numerator / denominator;
    }
}

/// LinUCB dispatch engine (stateless; arm state lives in `BanditArm`s).
pub struct LinUCBEngine {
    pub alpha: f64,
}

impl LinUCBEngine {
    pub fn new(alpha: f64) -> Self {
        Self { alpha }
    }

    /// Build the context vector for a target domain.
    #[inline]
    pub fn extract_context(&self, target_domain: &str) -> VectorD {
        VectorD::new(domain_risk(target_domain), 1.0, cos_time_prior(), 0.2)
    }

    /// Pick the highest-UCB arm (None when the pool is empty).
    ///
    /// Each arm is scored exactly once per call (not pairwise), so select
    /// cost scales as O(n·d²) with a single pass.
    pub fn select_best_arm<'a>(
        &self,
        arms: &'a [Arc<BanditArm>],
        context: &VectorD,
    ) -> Option<&'a Arc<BanditArm>> {
        let mut best: Option<&'a Arc<BanditArm>> = None;
        let mut best_score = f64::NEG_INFINITY;
        for arm in arms {
            let score = arm.compute_ucb_score(context, self.alpha);
            if score > best_score {
                best_score = score;
                best = Some(arm);
            }
        }
        best
    }
}

/// WAF-grade domain risk prior.
fn domain_risk(target_domain: &str) -> f64 {
    let d = target_domain.to_ascii_lowercase();
    if d.contains("cloudflare") || d.contains("turnstile") {
        0.9
    } else if d.contains("akamai") || d.contains("datadome") {
        0.85
    } else {
        0.3
    }
}

/// Time-of-day prior: cosine of the UTC day fraction, normalized to [0,1].
fn cos_time_prior() -> f64 {
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() % 86_400)
        .unwrap_or(0) as f64;
    ((secs / 86_400.0) * std::f64::consts::TAU).cos() * 0.5 + 0.5
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Instant;

    fn fixed_context() -> VectorD {
        VectorD::new(0.3, 1.0, 0.5, 0.2)
    }

    #[test]
    fn cost_mapping_matches_plan() {
        assert_eq!(cost_weight_for_tier("datacenter"), 0.1);
        assert_eq!(cost_weight_for_tier("dc"), 0.1);
        assert_eq!(cost_weight_for_tier("residential"), 1.0);
        assert_eq!(cost_weight_for_tier("mobile"), 3.0);
        assert_eq!(cost_weight_for_tier("unknown"), 1.0);
    }

    #[test]
    fn context_domain_risk_mapping() {
        let e = LinUCBEngine::new(DEFAULT_ALPHA);
        assert_eq!(e.extract_context("x.cloudflare.com")[0], 0.9);
        assert_eq!(e.extract_context("cdn.akamai.net")[0], 0.85);
        assert_eq!(e.extract_context("plain.example")[0], 0.3);
        assert_eq!(e.extract_context("plain.example")[1], 1.0);
        assert_eq!(e.extract_context("plain.example")[3], 0.2);
        let cos = e.extract_context("plain.example")[2];
        assert!((0.0..=1.0).contains(&cos));
    }

    #[test]
    fn compute_ucb_deterministic_on_fresh_arm() {
        // Fresh arm: A_inv=I, b=0 → score = alpha*|x| - 0.05*cost.
        let arm = BanditArm::new("10.0.0.1:8080".to_string(), "residential".to_string());
        let x = fixed_context();
        let norm = (x.transpose() * x)[(0, 0)].sqrt();
        let expected = DEFAULT_ALPHA * norm - 0.05 * COST_RESIDENTIAL;
        let got = arm.compute_ucb_score(&x, DEFAULT_ALPHA);
        assert!((got - expected).abs() < 1e-12, "got={got} want={expected}");
        // Deterministic across calls.
        assert_eq!(got, arm.compute_ucb_score(&x, DEFAULT_ALPHA));
    }

    #[test]
    fn update_shifts_score_toward_reward() {
        let arm = BanditArm::new("10.0.0.1:8080".to_string(), "residential".to_string());
        let x = fixed_context();
        let before = arm.compute_ucb_score(&x, DEFAULT_ALPHA);
        arm.update(&x, 1.0);
        let after_good = arm.compute_ucb_score(&x, DEFAULT_ALPHA);
        assert!(after_good > before, "reward 1.0 must raise UCB");
        let arm2 = BanditArm::new("10.0.0.2:8080".to_string(), "residential".to_string());
        arm2.update(&x, 0.0);
        // Zero reward still shrinks uncertainty (A_inv contracts) → score drops.
        assert!(arm2.compute_ucb_score(&x, DEFAULT_ALPHA) < before);
    }

    #[test]
    fn select_best_arm_prefers_trained_winner() {
        let e = LinUCBEngine::new(DEFAULT_ALPHA);
        let x = fixed_context();
        let winner = Arc::new(BanditArm::new(
            "10.0.0.1:8080".to_string(),
            "residential".to_string(),
        ));
        let loser = Arc::new(BanditArm::new(
            "10.0.0.2:8080".to_string(),
            "residential".to_string(),
        ));
        // Fresh arms tie (identical state); first max wins deterministically.
        let arms = vec![winner.clone(), loser.clone()];
        assert!(e.select_best_arm(&arms, &x).is_some());
        // Train: winner sees successes, loser sees blocks.
        for _ in 0..5 {
            winner.update(&x, 1.0);
            loser.update(&x, 0.0);
        }
        let picked = e.select_best_arm(&arms, &x).expect("arm");
        assert_eq!(picked.key, winner.key);
        // Empty pool → None (gateway maps this to 503).
        let empty: Vec<Arc<BanditArm>> = vec![];
        assert!(e.select_best_arm(&empty, &x).is_none());
    }

    #[test]
    fn reward_shape_matches_spec() {
        assert_eq!(compute_reward(200, Duration::from_millis(0)), 1.0);
        assert_eq!(compute_reward(403, Duration::from_millis(10)), 0.0);
        assert_eq!(compute_reward(429, Duration::from_millis(10)), 0.0);
        assert_eq!(compute_reward(502, Duration::from_millis(10)), 0.2);
        // 2s+ latency clamps the penalty at 0.5.
        assert_eq!(compute_reward(200, Duration::from_millis(5000)), 0.5);
    }

    /// Plan acceptance: mean `select_best_arm` cost < 200ns (release).
    /// Debug builds only report the value (optimizer off → not comparable).
    #[test]
    fn select_best_arm_avg_under_200ns() {
        let e = LinUCBEngine::new(DEFAULT_ALPHA);
        let x = fixed_context();
        let arms: Vec<Arc<BanditArm>> = (0..8)
            .map(|i| {
                Arc::new(BanditArm::new(
                    format!("10.0.0.{i}:8080"),
                    "residential".to_string(),
                ))
            })
            .collect();
        // Warm up (page in code/data) then measure.
        for _ in 0..1000 {
            let _ = e.select_best_arm(&arms, &x);
        }
        let iters = 100_000usize;
        let start = Instant::now();
        for _ in 0..iters {
            let picked = e.select_best_arm(&arms, &x);
            std::hint::black_box(picked);
        }
        let avg = start.elapsed() / iters as u32;
        eprintln!("select_best_arm over 8 arms: avg={avg:?} (release budget <200ns)");
        if !cfg!(debug_assertions) {
            assert!(avg < Duration::from_nanos(200), "routing too slow: {avg:?}");
        }
    }
}
