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
/// R2-6 遗忘节拍：每 N 次 `update` 做一次轻 reset（向先验回 blended 10%），
/// 重开探索空间。N=10k 按当前 QPS 约数小时一次，开销可忽略（一次 O(d²) blending）。
/// 存量冻结（付费线行为）；免费线走短窗（P3-1）。
pub const FORGET_EVERY_N_UPDATES: u64 = 10_000;
/// P3 免费臂遗忘短窗（v2-F7：免费/residential IP 高 churn，平均可见 4.56 天，
/// 60% 九十天只出现一次——声誉半衰期短一个量级，遗忘快 10 倍）。
pub const FORGET_EVERY_FREE_UPDATES: u64 = 1_000;
/// P3 免费风险溢价（v2-F5：免费≈全数据中心 IP→高 JA4 风控面；全 JA4 门需 TLS 面仍 out，
/// 此处为诚实替代：UCB 上的可解释常数惩罚，不碰 context 维度，不过拟合）。
pub const FREE_RISK_PREMIUM: f64 = 0.15;
/// 轻 reset 保留比例（学到方向的 90% + 先验的 10%）。
pub const FORGET_KEEP: f64 = 0.9;

/// P3：按 tier 取遗忘节拍（大小写不敏感；未知 tier 走付费默认——fail-safe 向保守）。
pub fn forget_every_for_tier(tier: &str) -> u64 {
    if tier.eq_ignore_ascii_case("free") {
        FORGET_EVERY_FREE_UPDATES
    } else {
        FORGET_EVERY_N_UPDATES
    }
}

pub type VectorD = SVector<f64, CONTEXT_DIM>;
pub type MatrixD = SMatrix<f64, CONTEXT_DIM, CONTEXT_DIM>;

/// Cost weight for an egress tier.
pub fn cost_weight_for_tier(tier: &str) -> f64 {
    match tier.to_ascii_lowercase().as_str() {
        "datacenter" | "dc" => COST_DC,
        "mobile" => COST_MOBILE,
        // 免费线探索成本 0（池权重 10 已压住其选中率，此处不再双重惩罚）。
        "free" => 0.0,
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
    /// Egress tier（GW-4 套利/遥测 join 键；P3 起消费：分档遗忘节拍＋免费风险溢价）。
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
    /// R2-6：累计 `update` 次数（`FORGET_EVERY_N_UPDATES` 触发轻 reset 的节拍器）。
    pub updates: u64,
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
                updates: 0,
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
        let mut score = expected_reward + alpha * variance - 0.05 * self.cost_weight;
        // P3 免费风险溢价（tier 精确匹配 free；大小写不敏感，key 隔离见 cost 惯例）。
        if self.tier.eq_ignore_ascii_case("free") {
            score -= FREE_RISK_PREMIUM;
        }
        score
    }

    /// Online update after passive feedback (Sherman-Morrison, O(d²)).
    ///
    /// R2-6 遗忘：节拍触发一次轻 reset（见 [`apply_forgetting`]）。方案原议的
    /// `A_inv *= 0.999 / b *= 0.999` 未采用：逆矩阵参数化下均匀收缩只会加速
    /// `A_inv → 0`（探索更快归零，且抹掉已学方向）；向先验的 blend 才是打开
    /// 不确定性的正确方向（单测锁定数学形态）。
    /// P3：节拍按臂 tier 分档（free 1k，其余 10k；见 [`forget_every_for_tier`]）。
    pub fn update(&self, context: &VectorD, reward: f64) {
        let mut state = self.state.write();
        state.b += reward * context;
        let a_inv_x = state.a_inv * context;
        let denominator = 1.0 + context.dot(&a_inv_x);
        let numerator = a_inv_x * a_inv_x.transpose();
        state.a_inv -= numerator / denominator;
        state.updates += 1;
        if state
            .updates
            .is_multiple_of(forget_every_for_tier(&self.tier))
        {
            apply_forgetting(&mut state);
        }
    }
}

/// R2-6 轻 reset：保留 90% 已学方向，10% 拉回先验（`A_inv → I` 重开不确定性，
/// `b` 同比收缩；计数器本身不清零，节拍恒定）。
fn apply_forgetting(state: &mut ArmState) {
    state.a_inv = state.a_inv * FORGET_KEEP + MatrixD::identity() * (1.0 - FORGET_KEEP);
    state.b *= FORGET_KEEP;
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
///
/// R2-6：查配置表（线性扫描，7 条内分支可预测；新增厂商只加行，不动逻辑）。
/// 同档 WAF 同分：Cloudflare/Turnstile 0.9；Akamai/DataDome/PerimeterX/Kasada/Imperva 0.85；其余 0.3。
const DOMAIN_RISK_TABLE: &[(&str, f64)] = &[
    ("cloudflare", 0.9),
    ("turnstile", 0.9),
    ("akamai", 0.85),
    ("datadome", 0.85),
    ("perimeterx", 0.85),
    ("kasada", 0.85),
    ("imperva", 0.85),
];
const DOMAIN_RISK_DEFAULT: f64 = 0.3;

fn domain_risk(target_domain: &str) -> f64 {
    let d = target_domain.to_ascii_lowercase();
    DOMAIN_RISK_TABLE
        .iter()
        .find(|(needle, _)| d.contains(needle))
        .map(|(_, risk)| *risk)
        .unwrap_or(DOMAIN_RISK_DEFAULT)
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
        assert_eq!(cost_weight_for_tier("free"), 0.0);
    }

    #[test]
    fn context_domain_risk_mapping() {
        let e = LinUCBEngine::new(DEFAULT_ALPHA);
        assert_eq!(e.extract_context("x.cloudflare.com")[0], 0.9);
        assert_eq!(e.extract_context("cdn.akamai.net")[0], 0.85);
        assert_eq!(e.extract_context("plain.example")[0], 0.3);
        // R2-6：新增三家 bot-mitigation 厂商与 DataDome 同档（0.85）。
        assert_eq!(e.extract_context("px.perimeterx.com")[0], 0.85);
        assert_eq!(e.extract_context("edge.kasada.io")[0], 0.85);
        assert_eq!(e.extract_context("cdn.imperva.com")[0], 0.85);
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
    fn forgetting_blend_math_is_exact() {
        // R2-6：轻 reset 数学形态锁定（90% 已学 + 10% 先验；计数器不动）。
        let mut state = ArmState {
            a_inv: MatrixD::identity() * 0.5,
            b: VectorD::new(1.0, 2.0, 3.0, 4.0),
            updates: 41,
        };
        apply_forgetting(&mut state);
        let expect_a =
            MatrixD::identity() * 0.5 * FORGET_KEEP + MatrixD::identity() * (1.0 - FORGET_KEEP);
        assert!((state.a_inv - expect_a).norm() < 1e-12);
        assert!((state.b - VectorD::new(0.9, 1.8, 2.7, 3.6)).norm() < 1e-12);
        assert_eq!(state.updates, 41);
    }

    #[test]
    fn forgetting_fires_every_10k_updates_and_keeps_learning() {
        // R2-6：1 万次 update 后节拍恰好触发一次（计数器可观测），分数有限、
        // 仍显著优于 fresh 臂（学习成果保留，不是清零）。
        // 10k × O(d²) 约毫秒级，单测可全量跑（不 mock 节拍，防常量漂移）。
        let arm = BanditArm::new("10.0.0.1:8080".to_string(), "residential".to_string());
        let x = fixed_context();
        for _ in 0..FORGET_EVERY_N_UPDATES {
            arm.update(&x, 1.0);
        }
        let state = arm.state.read();
        assert_eq!(state.updates, FORGET_EVERY_N_UPDATES);
        drop(state);
        let score = arm.compute_ucb_score(&x, DEFAULT_ALPHA);
        assert!(score.is_finite(), "score must stay finite, got={score}");
        let fresh = BanditArm::new("10.0.0.2:8080".to_string(), "residential".to_string());
        assert!(
            score > fresh.compute_ucb_score(&x, DEFAULT_ALPHA),
            "trained arm must still beat fresh (score={score})"
        );
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

    #[test]
    fn forgetting_is_faster_for_free_tier() {
        // P3-1：免费臂 1k 即 blend（短窗），付费臂 10k 不变；blend 数学与 R2-6 同 90/10。
        assert_eq!(forget_every_for_tier("free"), FORGET_EVERY_FREE_UPDATES);
        assert_eq!(forget_every_for_tier("FREE"), FORGET_EVERY_FREE_UPDATES);
        assert_eq!(forget_every_for_tier("residential"), FORGET_EVERY_N_UPDATES);
        assert_eq!(forget_every_for_tier("datacenter"), FORGET_EVERY_N_UPDATES);
        let x = fixed_context();
        let free = BanditArm::new("10.0.0.9:8080".to_string(), "free".to_string());
        let res = BanditArm::new("10.0.0.1:8080".to_string(), "residential".to_string());
        for _ in 0..FORGET_EVERY_FREE_UPDATES {
            free.update(&x, 1.0);
            res.update(&x, 1.0);
        }
        // 同序列更新下唯一差别是一次 blend → 矩阵必分叉；residential 侧 1k 内无 blend。
        let fa = free.state.read().a_inv;
        let ra = res.state.read().a_inv;
        assert!(
            (fa - ra).norm() > 1e-9,
            "free arm must have blended at 1k while residential did not"
        );
        assert_eq!(free.state.read().updates, FORGET_EVERY_FREE_UPDATES);
    }

    #[test]
    fn free_arm_pays_risk_premium() {
        // P2-1：同 context 下 free 臂 UCB 低于 residential 臂。
        // gap 构成：premium(0.15) − cost 差(0.05×(1.0−0.0)＝0.05) ＝ 0.10
        // （free cost 0 本就优惠 0.05，premium 净效应仍为罚 0.10，方向正确）；
        // DC/mobile 公式不变（R2-6 形态冻结）。
        let x = fixed_context();
        let norm = (x.transpose() * x)[(0, 0)].sqrt();
        let free = BanditArm::new("10.0.0.9:8080".to_string(), "free".to_string());
        let res = BanditArm::new("10.0.0.1:8080".to_string(), "residential".to_string());
        let gap =
            res.compute_ucb_score(&x, DEFAULT_ALPHA) - free.compute_ucb_score(&x, DEFAULT_ALPHA);
        let expected_gap =
            FREE_RISK_PREMIUM - 0.05 * (COST_RESIDENTIAL - cost_weight_for_tier("free"));
        assert!(
            (gap - expected_gap).abs() < 1e-12,
            "gap={gap} want={expected_gap}"
        );
        assert!(gap > 0.0, "premium must net-penalize free arms");
        let dc = BanditArm::new("10.0.0.2:8080".to_string(), "dc".to_string());
        let expected_dc = DEFAULT_ALPHA * norm - 0.05 * COST_DC;
        assert!((dc.compute_ucb_score(&x, DEFAULT_ALPHA) - expected_dc).abs() < 1e-12);
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
