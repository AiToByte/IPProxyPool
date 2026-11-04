//! quarantine：从 `router.rs` 抽出的 `RouterEngine` 方法子集（纯搬移，行为不变）。
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

use dashmap::DashMap;

use std::sync::Arc;

use std::time::{Duration, Instant};

impl RouterEngine {
    pub fn set_quarantine(&self, domain: &str, ip: &str, ttl_secs: u64) {
        let ttl = Duration::from_secs(ttl_secs.min(QUARANTINE_MAX_TTL_SECS));
        let expiry = Instant::now()
            .checked_add(ttl)
            .unwrap_or_else(|| Instant::now() + Duration::from_secs(QUARANTINE_MAX_TTL_SECS));
        // OPT-R11 C1：落表前过**两道准入**，与 `session_sticky_allowed` 同构。
        //
        // 理由（这是本项定为 P1 的原因）：本表外层 key 源自**客户端可控的
        // `Host`**——攻击者用随机/超长 Host 反复吃 403/429 即可在 TTL 窗口
        // （403→600s）内持续新增外层条目；`apply_delta` 的 Redis PubSub 入口
        // 更直接，`parse_delta_message` 只校验 `ttl` 范围与 domain/ip 非空，
        // 长度/条目数一概不设防。此前本表**既无长度上限也无水位**，而会话表
        // 两样都有（`SESSION_ID_MAX_BYTES` + `SESSION_MAX_ENTRIES`）——同一仓
        // 内两张客户端可影响的表，防护水平不一致。
        //
        // 语义与超长 session_id 一致：**拒绝落表，不截断**。截断会把两个不同
        // domain 映射到同一 key，造成误隔离（把无辜节点拖下水），比不隔离更糟。
        let domain = normalize_domain(domain);
        if domain.len() > QUARANTINE_DOMAIN_MAX_BYTES {
            log::warn!(
                "[quarantine] domain too long ({} bytes > {}), not quarantined",
                domain.len(),
                QUARANTINE_DOMAIN_MAX_BYTES
            );
            return;
        }
        if self.quarantine_map.len() >= QUARANTINE_MAX_DOMAINS
            && !self.quarantine_map.contains_key(&domain)
        {
            // 软水位：并发下允许轻微超限，只防无限膨胀。已达水位时**不驱逐**
            // 已有 domain——驱逐正在生效的隔离等于放行攻击流量；宁可新 domain
            // 暂不隔离（下轮窗口自然重试），也不动已有判定。
            log::warn!(
                "[quarantine] outer domain watermark reached ({} >= {}), new domain not quarantined",
                self.quarantine_map.len(),
                QUARANTINE_MAX_DOMAINS
            );
            return;
        }
        self.quarantine_map
            .entry(domain)
            .or_insert_with(DashMap::new)
            .insert(ip.to_string(), expiry);
    }
    pub fn remove_quarantine(&self, domain: &str, ip: &str) -> bool {
        let domain = normalize_domain(domain);
        if domain.is_empty() {
            return false;
        }
        let removed = self
            .quarantine_map
            .get(&domain)
            .map(|inner| inner.remove(ip).is_some())
            .unwrap_or(false);
        if removed {
            // 空内层回收。⚠️ 外层 `Ref` 守卫持分片读锁，必须先释放再取外层写锁
            //（`DashMap::remove` 与 `get` 同分片会死锁）——`drop(inner)` 显式断点，
            // 与 `sweep_expired_at` 的「先判后删两趟」同一纪律。
            if let Some(inner) = self.quarantine_map.get(&domain) {
                if inner.is_empty() {
                    drop(inner);
                    self.quarantine_map.remove(&domain);
                }
            }
        }
        removed
    }
    pub fn quarantine_domains(&self) -> usize {
        self.quarantine_map.len()
    }
    pub fn snapshot_all(&self) -> Vec<Arc<ProxyNode>> {
        self.pools.load().as_ref().clone()
    }
    pub fn quarantine_len(&self) -> usize {
        self.quarantine_map.iter().map(|kv| kv.value().len()).sum()
    }
    pub fn sweep_expired(&self) -> (usize, usize) {
        self.sweep_expired_at(Instant::now())
    }
    pub(crate) fn sweep_expired_at(&self, now: Instant) -> (usize, usize) {
        let sessions_before = self.session_store.len();
        // 会话：创建时间距 now 超过 TTL 即过期（saturating 防时钟回拨 panic）。
        self.session_store.retain(|_, (_, created_at)| {
            now.saturating_duration_since(*created_at).as_secs() < SESSION_TTL_SECS
        });
        // OPT-R6 S2：净删除经 `janitor::removed_count`（饱和减法）。会话表正被数据面
        // `select_node_excluding` 落表并发写入，两次 `len()` 之间可能增长；旧的
        // `before - len()` 会 debug panic / release 回绕，且当时 sweep 无 supervisor，
        // panic 即静默死亡 → 会话表无界增长至 OOM。
        let sessions_removed = janitor::removed_count(sessions_before, self.session_store.len());

        let quarantines_before = self.quarantine_len();
        // 隔离：到期时刻 <= now 即过期。
        //
        // OPT-R9 C1：两级结构下需**逐内层**判过期。顺带做**空内层回收**——
        // 一个 domain 的全部 ip 都过期后，外层条目若留着就是纯粹的内存泄漏
        // （key 空间由客户端可控的 `Host` 变体构成）。`DashMap::retain` 持锁期间
        // 不可再取同一分片的写锁，故用「先判后删」两趟，避免自死锁。
        self.quarantine_map.retain(|_, inner| {
            inner.retain(|_, expiry| *expiry > now);
            !inner.is_empty()
        });
        // OPT-R6 S2：同上，隔离表亦由熔断消费侧并发写入。
        let quarantines_removed = janitor::removed_count(quarantines_before, self.quarantine_len());

        // OPT-R4 B11：TTL 淘汰顺带清理僵尸因子（池内已无对应 vendor×country
        // 即删键；缺省 1.0 语义不变，恢复靠下轮 merge 健康快照，不靠残留 0 因子）。
        self.prune_free_scales();

        (sessions_removed, quarantines_removed)
    }
    pub(crate) fn is_quarantined_normalized(
        &self,
        normalized_domain: &str,
        ip: &str,
        now: Instant,
    ) -> bool {
        match self.quarantine_map.get(normalized_domain) {
            Some(inner) => inner.get(ip).is_some_and(|exp| *exp.value() > now),
            None => false,
        }
    }
    /// 仅供「不在节点循环内」的调用方与单元测试使用；节点循环**必须**走
    /// [`Self::is_quarantined_normalized`] 复用归一结果，否则会把分配
    /// 重新拉回 O(N)。生产热路径（`matches` / sticky 复核）已在各自入口
    /// 归一 `spec.target_domain`，本函数在生产路径上无调用方。
    ///
    /// `cfg_attr(not(test), allow(dead_code))`：非测试构建下本函数确实
    /// 无调用方（单测独占），但测试构建下**必须**被用上。
    #[cfg_attr(not(test), allow(dead_code))]
    pub(crate) fn is_quarantined(&self, domain: &str, ip: &str, now: Instant) -> bool {
        self.is_quarantined_normalized(&normalize_domain(domain), ip, now)
    }
}
