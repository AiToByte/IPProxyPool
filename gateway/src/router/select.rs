//! select：从 `router.rs` 抽出的 `RouterEngine` 方法子集（纯搬移，行为不变）。
//!
//! 本文件是 `router` 的子模块，经 `use super::*;` 继承 `RouterEngine` 的私有字段
//! 与 `router` 模块的私有自由函数（Rust 规则：私有项对其**定义模块及其全部
//! 后代**可见，故子模块可直接访问父模块的私有内容）。
//!
//! **不做 `pub(crate) use x::*` 再导出**：本文件里的方法都是**固有方法**
//! （inherent method），调用方写 `engine.foo()` 即可，Rust 不要求为固有方法
//! import——再导出会触发 `unused_imports`（`use super::*` 的 glob-of-glob 不算
//! "使用"）。跨子模块需要共享的**自由函数**才需要显式可见，唯一的例子是
//! `select::pick_weighted_streaming`，由 `router::tests` 直接
//! `use super::select::pick_weighted_streaming;` 引入。

use super::*;
use crate::model::{ProxyNode, RoutingSpec};

use rand::Rng;

use std::sync::Arc;

use std::time::Instant;

impl RouterEngine {
    pub(crate) fn matches(&self, node: &ProxyNode, spec: &RoutingSpec, now: Instant) -> bool {
        // R2-1：weight==0 视为摘除（保留在池内以便 restore，只在选路时过滤）。
        // `snapshot_all` 绕过本函数，故修剪白名单不受影响。
        if node.weight == 0 {
            return false;
        }
        // P2：按出站协议隔离。显式约束须精确命中；默认（None）只放行 Http——
        // SOCKS 节点永不服务默认流量（翻译桥只处理显式 socks 请求，见 gateway filter）。
        if let Some(want) = spec.proto {
            if node.proto != want {
                return false;
            }
        } else if node.proto != crate::model::EgressProto::Http {
            return false;
        }
        if let Some(ref c) = spec.country {
            if !node.country.eq_ignore_ascii_case(c) {
                return false;
            }
        }
        if let Some(ref t) = spec.tier {
            // REVIEW-R2 Q8：两侧皆已归一（节点 `ProxyNode::new`＋本文件入口），
            // 直接比对零分配；短名/大小写语义与旧双分支一致（归一函数单源）。
            if node.tier != *t {
                return false;
            }
        }
        // OPT-R9 C1：隔离查表走 `is_node_quarantined`（零分配内层查表）。
        // 本函数只负责**保证入参已归一**：`get_healthy_candidates_excluding` /
        // `select_node_excluding` 的 sticky 复核路径都在节点循环**外**算一次
        // `normalize_domain` 并填入 `spec.target_domain` 旁的 `normalized_domain`。
        // 这里的 `&spec.target_domain` 由调用方保证已是归一值（见两处入口的
        // `spec.target_domain = normalize_domain(...)`），故**不重复归一**。
        if self.is_node_quarantined(node, &spec.target_domain, now) {
            return false;
        }
        // 步骤 54：主动健康摘除（探针判定节点整体不可用，与域级隔离无关）。
        // 必须在选路侧过这一关——否则摘除只影响指标与探测，数据面照旧发流量。
        // 查表而非计数器（见 `health::is_active_unhealthy` 的"为什么没有计数器"）。
        if self.is_active_unhealthy(&node.addr, now) {
            return false;
        }
        true
    }
    pub(crate) fn is_node_quarantined(
        &self,
        node: &ProxyNode,
        normalized_domain: &str,
        now: Instant,
    ) -> bool {
        self.is_quarantined_normalized(normalized_domain, &node.ip, now)
            || node
                .exit_ip
                .as_deref()
                .is_some_and(|e| self.is_quarantined_normalized(normalized_domain, e, now))
    }
    pub fn get_healthy_candidates(&self, spec: &RoutingSpec) -> Vec<Arc<ProxyNode>> {
        self.get_healthy_candidates_excluding(spec, &[])
    }
    pub fn with_pools<R>(&self, f: impl FnOnce(&[Arc<ProxyNode>]) -> R) -> R {
        let guard = self.pools.load();
        f(guard.as_ref())
    }
    pub fn matches_node(&self, node: &ProxyNode, spec: &RoutingSpec, now: Instant) -> bool {
        self.matches(node, spec, now)
    }
    pub fn get_healthy_candidates_excluding(
        &self,
        spec: &RoutingSpec,
        excluded: &[String],
    ) -> Vec<Arc<ProxyNode>> {
        let now = Instant::now();
        let guard = self.pools.load();
        // REVIEW-R2 Q8：tier 请求侧归一一次（逐节点循环外；matches 内零分配）。
        let mut spec = spec.clone();
        spec.tier = spec.tier.map(|t| crate::model::canonical_tier(&t));
        // OPT-R9 C1：目标域同样归一一次（逐节点循环外）。`matches` 内的隔离查表
        // 直接用该值做外层 `DashMap` 查找，**循环内零分配**——旧实现在
        // `is_quarantined` 里对每个节点做 `format!("{}:{ip}", normalize_domain(...))`，
        // 池 N 时每请求 2N 次堆分配（免费池缺省 2000 节点 ≈ 4000 次/请求）。
        // 归一后语义与旧实现完全一致（隔离查表键的第一层就是归一化 domain）。
        spec.target_domain = normalize_domain(&spec.target_domain);
        guard
            .iter()
            .filter(|n| !excluded.iter().any(|e| e == &n.addr))
            .filter(|n| self.matches(n, &spec, now))
            .cloned()
            .collect()
    }
    /// 无排除兼容入口（单测/外部调用）；网关重试路径走 `select_node_excluding`。
    ///
    /// `cfg_attr(not(test), allow(dead_code))`：生产数据面一律走
    /// `select_node_excluding`（需要排除失败节点），本入口**只被单测消费**
    /// （`circuit_breaker.rs` 与 `router/tests.rs` 的 `#[cfg(test)]` 模块）。
    #[cfg_attr(not(test), allow(dead_code))]
    pub fn select_node(&self, spec: &RoutingSpec) -> Option<Arc<ProxyNode>> {
        self.select_node_excluding(spec, &[])
    }
    pub fn select_node_excluding(
        &self,
        spec: &RoutingSpec,
        excluded: &[String],
    ) -> Option<Arc<ProxyNode>> {
        let now = Instant::now();
        // NEXT-A5：tier 请求侧归一前移到入口（粘滞复核与新鲜选择共用同一归一 spec；
        // 原 Q8 hoist 只在步骤 2，粘滞路径用的还是原文 tier）。
        let mut spec = spec.clone();
        spec.tier = spec.tier.map(|t| crate::model::canonical_tier(&t));
        // OPT-R9 C1：目标域同样在入口归一一次，粘滞复核与新鲜选择共用
        // （粘滞路径只复核单个节点，收益小于新路径，但统一归一可避免两处口径
        // 分叉——旧实现的 sticky 复核里 `is_node_quarantined` 走的是未归一的
        // 原文 domain，靠 `is_quarantined` 内部归一兜底；现在改为入口统一归一，
        // 内层查表不再重复归一）。
        spec.target_domain = normalize_domain(&spec.target_domain);

        // 1. Sticky session fast path (skip quarantined/derated/excluded bindings).
        // OPT-R4 A7：超长 session 当无 session 处理（不查表，直接走无状态新鲜选择，
        // 不哈希巨型 key，永不 panic）。
        if let Some(ref session_id) = spec.session_id {
            if session_id.len() <= SESSION_ID_MAX_BYTES {
                if let Some(entry) = self.session_store.get(session_id) {
                    let (node, created_at) = entry.value();
                    // R2-1：粘滞命中后必须复核“当前池”权重，derate 到 0 的会话要迁移，
                    // 不能沿用绑定时刻克隆的老权重。
                    // P2：同时复核 proto——绑 socks 节点的会话发默认请求必须迁走
                    // （否则 socks 节点漏进 HttpPeer 必失败；反之亦然）。
                    let want_proto = spec.proto.unwrap_or(crate::model::EgressProto::Http);
                    let still_live = self
                        .pools
                        .load()
                        .iter()
                        .find(|n| n.addr == node.addr)
                        .is_some_and(|n| n.weight > 0 && n.proto == want_proto);
                    let excluded_hit = excluded.iter().any(|e| e == &node.addr);
                    // REVIEW-R2 Q9：`elapsed()` 时钟回拨即 panic（数据面不可炸），与
                    // `sweep_expired_at` 同口径改 saturating（回拨按 0 处理，会话多活一轮）。
                    // NEXT-A5：粘滞命中复核 country/tier 约束（spec.tier 入口已归一，
                    // 与 matches 同语义；换约束即迁移，不再粘错节点）。
                    let country_ok = spec
                        .country
                        .as_deref()
                        .is_none_or(|c| node.country.eq_ignore_ascii_case(c));
                    let tier_ok = spec.tier.as_deref().is_none_or(|t| node.tier == *t);
                    if !excluded_hit
                        && now.saturating_duration_since(*created_at).as_secs() < SESSION_TTL_SECS
                        && still_live
                        && country_ok
                        && tier_ok
                        && !self.is_node_quarantined(node, &spec.target_domain, now)
                    {
                        return Some(Arc::clone(node));
                    }
                }
            }
        }

        // 2. Filter snapshot（spec 已在入口归一，直接用）。
        // GW-6 分桶快路径：当 spec 完全指定 tier+country+proto 时，O(1) 哈希命中
        // 对应桶，避免 O(N) 全量池扫描；否则回退原有全量扫描（语义完全一致，
        // 粘滞会话/quarantine/excluded 复核与原实现逐字同构）。
        let guard = self.pools.load();
        let selected = if let (Some(tier), Some(country), Some(proto)) =
            (spec.tier.as_deref(), spec.country.as_deref(), spec.proto)
        {
            let key = BucketKey {
                tier: tier.to_string(),
                country: country.to_string(),
                proto: Some(proto),
            };
            self.select_from_bucket(&key, excluded, &spec, now, &mut rand::thread_rng())
        } else {
            // 3. Weighted random pick（全量回退，与 OPT-R11 B1 流式两趟等价）。
            let mut rng = rand::thread_rng();
            pick_weighted_streaming(
                || {
                    guard
                        .iter()
                        .filter(|n| !excluded.iter().any(|e| e == &n.addr))
                        .filter(|n| self.matches(n, &spec, now))
                },
                &mut rng,
            )
        };

        // 4. Bind new session.
        // OPT-R4 A7：落表前过准入（超长/满水位新 key 不落表，只用无状态选中节点）。
        if let (Some(ref session_id), Some(ref node)) = (&spec.session_id, &selected) {
            if session_sticky_allowed(&self.session_store, session_id) {
                self.session_store
                    .insert(session_id.clone(), (Arc::clone(node), now));
            }
        }

        selected
    }
    pub(crate) fn select_from_bucket(
        &self,
        key: &BucketKey,
        excluded: &[String],
        spec: &RoutingSpec,
        now: Instant,
        rng: &mut impl Rng,
    ) -> Option<Arc<ProxyNode>> {
        let buckets_guard = self.buckets.load();
        let bucket = buckets_guard.get(key)?;
        pick_weighted_streaming(
            || {
                bucket.iter().filter(|n| {
                    !excluded.iter().any(|e| e == &n.addr) && self.matches(n, spec, now)
                })
            },
            rng,
        )
    }
}

pub(crate) fn pick_weighted_streaming<'a, I, F>(
    mut make_iter: F,
    rng: &mut impl Rng,
) -> Option<Arc<ProxyNode>>
where
    F: FnMut() -> I,
    I: Iterator<Item = &'a Arc<ProxyNode>>,
{
    // 第 1 趟：权重总和与候选个数（不物化、不克隆）。
    let mut total: u64 = 0;
    let mut count: usize = 0;
    for n in make_iter() {
        total += n.weight as u64;
        count += 1;
    }
    if count == 0 {
        return None;
    }
    if total == 0 {
        // 防御分支（matches 已滤 0，正常走不到）：退化均匀。
        // `choose` 内部对非空切片即 `gen_range(0..len)`，故与物化版同源。
        let idx = rng.gen_range(0..count);
        return make_iter().nth(idx).map(Arc::clone);
    }
    // 第 2 趟：按同一顺序消耗 roll。
    let mut roll = rng.gen_range(0..total);
    for n in make_iter() {
        let w = n.weight as u64;
        if roll < w {
            return Some(Arc::clone(n));
        }
        roll -= w;
    }
    None
}
