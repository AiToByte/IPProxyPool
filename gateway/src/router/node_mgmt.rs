//! node_mgmt：从 `router.rs` 抽出的 `RouterEngine` 方法子集（纯搬移，行为不变）。
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
use crate::model::ProxyNode;

use std::sync::Arc;

impl RouterEngine {
    pub(crate) fn store_nodes(&self, next: Vec<Arc<ProxyNode>>) {
        let buckets = Self::build_buckets(&next);
        let hot_nodes = Self::build_hot_nodes(&next);
        self.pools.store(Arc::new(next));
        self.buckets.store(Arc::new(buckets));
        self.hot_nodes.store(Arc::new(hot_nodes));
        // 步骤 54：池已换 ⇒ 旧的主动健康条目里有相当一部分指向**已不存在**的节点
        // （僵尸）。不清理的话，这些 addr 会一直占着健康表，且计数类指标失真。
        // 在此清理（而非等探针 tick）是必要的：控制面换池后**立刻**就该反映现状。
        self.prune_active_health_orphans();
    }
    /// 全量原子替换池子（稳定 API：控制面热更新，零停机）。
    /// 生产流量走快照读；测试与运维手工调用。
    ///
    /// `cfg_attr(not(test), allow(dead_code))`：控制面现役写路径是
    /// `replace_vendor_nodes` / `adjust_vendor_weight` / `set_paid_weight_boost`
    /// / `scale_vendor_weights`，本入口**只被单测消费**（`router/tests.rs`）。
    #[cfg_attr(not(test), allow(dead_code))]
    pub fn reload_nodes(&self, new_nodes: Vec<ProxyNode>) {
        let _guard = self.write_lock.lock();
        let pools_vec: Vec<Arc<ProxyNode>> = new_nodes.into_iter().map(Arc::new).collect();
        self.store_nodes(pools_vec);
    }
    pub fn replace_vendor_nodes(&self, prefix: &str, nodes: Vec<ProxyNode>) {
        let _guard = self.write_lock.lock();
        let guard = self.pools.load();
        let mut next: Vec<Arc<ProxyNode>> = guard
            .iter()
            .filter(|n| !n.provider.starts_with(prefix))
            .map(Arc::clone)
            .collect();
        next.extend(nodes.into_iter().map(Arc::new));
        drop(guard);
        self.store_nodes(next);
        // OPT-R4 B11：merge（含空快照）后顺带清理僵尸因子。
        self.prune_free_scales();
    }
    pub fn add_node(&self, node: ProxyNode) -> Result<(), String> {
        let _guard = self.write_lock.lock();
        let guard = self.pools.load();
        if guard.iter().any(|n| n.addr == node.addr) {
            return Err(format!("node {} already exists", node.addr));
        }
        let mut next: Vec<Arc<ProxyNode>> = guard.iter().cloned().collect();
        next.push(Arc::new(node));
        drop(guard);
        self.store_nodes(next);
        Ok(())
    }
    pub fn remove_node(&self, addr: &str) -> Option<ProxyNode> {
        let _guard = self.write_lock.lock();
        let guard = self.pools.load();
        let removed = guard
            .iter()
            .find(|n| n.addr == addr)
            .map(|n| (**n).clone())?;
        let next: Vec<Arc<ProxyNode>> = guard.iter().filter(|n| n.addr != addr).cloned().collect();
        drop(guard);
        self.store_nodes(next);
        // 自动调权基准表也要清，否则同 addr 重新加入后会沿用旧基准。
        self.paid_base_weight.remove(addr);
        Some(removed)
    }
    pub fn adjust_vendor_weight(&self, vendor: &str, country: &str, weight: u32) {
        let _guard = self.write_lock.lock();
        let guard = self.pools.load();
        let next: Vec<Arc<ProxyNode>> = guard
            .iter()
            .map(|n| {
                if n.provider == vendor && n.country.eq_ignore_ascii_case(country) {
                    let mut updated = (**n).clone();
                    updated.weight = weight;
                    Arc::new(updated)
                } else {
                    Arc::clone(n)
                }
            })
            .collect();
        drop(guard);
        self.store_nodes(next);
    }
    pub fn set_paid_weight_boost(&self, boost_percent: u32) {
        let _guard = self.write_lock.lock();
        let boost = boost_percent.clamp(1, 500);
        let guard = self.pools.load();
        let next: Vec<Arc<ProxyNode>> = guard
            .iter()
            .map(|n| {
                if n.provider.starts_with("free") {
                    return Arc::clone(n);
                }
                let base = *self
                    .paid_base_weight
                    .entry(n.addr.clone())
                    .or_insert_with(|| n.weight);
                let mut updated = (**n).clone();
                updated.weight = base.saturating_mul(boost).saturating_div(100).clamp(1, 200);
                Arc::new(updated)
            })
            .collect();
        drop(guard);
        self.store_nodes(next);
    }
    pub fn get_node_by_addr(&self, addr: &str) -> Option<Arc<ProxyNode>> {
        let guard = self.pools.load();
        guard.iter().find(|n| n.addr == addr).cloned()
    }
    pub fn list_nodes(&self, filter: &NodeFilter) -> Vec<Arc<ProxyNode>> {
        let guard = self.pools.load();
        let tier = filter.tier.as_deref().map(|t: &str| t.to_ascii_lowercase());
        let country = filter
            .country
            .as_deref()
            .map(|c: &str| c.to_ascii_lowercase());
        let proto = filter
            .proto
            .as_deref()
            .map(|p: &str| p.to_ascii_lowercase());
        guard
            .iter()
            .filter(|n| {
                tier.as_deref()
                    .is_none_or(|t| n.tier.eq_ignore_ascii_case(t))
                    && country
                        .as_deref()
                        .is_none_or(|c| n.country.eq_ignore_ascii_case(c))
                    && proto
                        .as_deref()
                        .is_none_or(|p| format!("{:?}", n.proto).to_ascii_lowercase() == *p)
            })
            .cloned()
            .collect()
    }
    pub(crate) fn prune_free_scales(&self) {
        let live: std::collections::HashSet<(String, String)> = self
            .pools
            .load()
            .iter()
            .map(|n| scale_key(&n.provider, &n.country))
            .collect();
        self.free_scale.retain(|k, _| live.contains(k));
    }
    pub fn scale_vendor_weights(&self, vendor: &str, country: &str, factor: f64) {
        let _guard = self.write_lock.lock();
        // NEXT-B6：因子同步记表（含 factor<=0 显式摘除，合并后依然生效）。
        // OPT-R4 B11：显式 1.0 等价缺省——删键不残留（表随 scale 插入永不清理即僵尸）。
        if factor == 1.0 {
            self.free_scale.remove(&scale_key(vendor, country));
        } else {
            self.free_scale.insert(scale_key(vendor, country), factor);
        }
        let guard = self.pools.load();
        let next: Vec<Arc<ProxyNode>> = guard
            .iter()
            .map(|n| {
                if n.provider == vendor && n.country.eq_ignore_ascii_case(country) {
                    let mut updated = (**n).clone();
                    // REVIEW-R2 Q5：factor<=0 即显式摘除（matches 滤 0）；factor>0 下限钳 1——
                    // 复乘不得几何衰减到 0（生产 factor 仅 0.0/0.5 触发不了，任意小 factor 才暴露；
                    // 非 finite（NaN）`as u32` 饱和为 0，同样被下限兜住，永不意外摘除）。
                    updated.weight = if factor <= 0.0 {
                        0
                    } else {
                        ((n.weight as f64 * factor).round() as u32).max(1)
                    };
                    Arc::new(updated)
                } else {
                    Arc::clone(n)
                }
            })
            .collect();
        drop(guard);
        // 步骤 52：此前只 store `pools`，漏重建 buckets/hot_nodes ⇒ **对全指定
        // tier+country+proto 的桶快路径仍拿旧权重选路**（factor=0 摘除对桶无效）。
        self.store_nodes(next);
    }
}
