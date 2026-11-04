//! buckets：从 `router.rs` 抽出的 `RouterEngine` 方法子集（纯搬移，行为不变）。
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

use std::collections::HashMap;
use std::sync::Arc;

impl RouterEngine {
    pub(crate) fn build_buckets(
        pools: &[Arc<ProxyNode>],
    ) -> HashMap<BucketKey, Vec<Arc<ProxyNode>>> {
        let mut map: HashMap<BucketKey, Vec<Arc<ProxyNode>>> = HashMap::new();
        for n in pools {
            if n.weight == 0 {
                continue;
            }
            let key = BucketKey {
                tier: n.tier.clone(),
                country: n.country.clone(),
                proto: Some(n.proto),
            };
            map.entry(key).or_default().push(Arc::clone(n));
        }
        map
    }
    pub(crate) fn build_hot_nodes(pools: &[Arc<ProxyNode>]) -> Vec<Arc<ProxyNode>> {
        let mut live: Vec<Arc<ProxyNode>> = pools
            .iter()
            .filter(|n| n.weight > 0)
            .map(Arc::clone)
            .collect();
        live.sort_by_key(|a| std::cmp::Reverse(a.weight));
        live.truncate(HOT_NODES_K);
        live
    }
    pub fn free_factor(&self, vendor: &str, country: &str) -> f64 {
        self.free_scale
            .get(&scale_key(vendor, country))
            .map(|v| *v.value())
            .unwrap_or(1.0)
    }
}
