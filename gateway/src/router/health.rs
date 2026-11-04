//! health：主动健康探测结果回灌（OPT-R18 P2-1 从 `router.rs` 抽出的子模块）。
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

use std::collections::HashSet;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::Instant;

impl RouterEngine {
    /// 步骤 54：连续失败阈值（构造时注入，运行期不可变）。
    pub fn probe_failure_threshold(&self) -> u32 {
        self.probe_failure_threshold
    }

    /// 步骤 54：摘除冷却秒数（构造时注入，运行期不可变）。
    pub fn probe_unhealthy_cooldown_secs(&self) -> u64 {
        self.probe_unhealthy_cooldown.as_secs()
    }

    /// 当前是否被主动健康摘除（热路径，每候选一次）。
    ///
    /// # 为什么这里**没有**「空表快速短路」计数器
    ///
    /// 此前用 `active_unhealthy_count == 0` 做短路，而那个计数器由各写入点
    /// **增量维护**：`sweep_active_health` 把「过期条目数」累加到循环外统一
    /// 递减，递减时锁已释放，窗口期内条目可能已被 `record_probe_failure`
    /// 重新标记 ⇒ 计数器**低估**。低估有三重后果：
    /// ① 短路误判「无人被摘除」，坏节点继续接流量；
    /// ② 同一计数器被安全阀复用，低估会放行「摘到全池 503」；
    /// ③ gauge 与真实状态漂移，排障时指标显示 0。
    ///
    /// 处置：**删除派生状态**——热路径直接查表（表即真相）。
    pub(crate) fn is_active_unhealthy(&self, addr: &str, now: Instant) -> bool {
        self.active_health
            .get(addr)
            .is_some_and(|st| st.unhealthy_until.is_some_and(|until| until > now))
    }

    /// 当前处于主动健康摘除状态的节点数（**全表精确扫描**）。
    ///
    /// 只在探针每 tick 结束时调用一次，**不在数据面热路径**。
    pub fn active_unhealthy_len(&self) -> usize {
        let now = Instant::now();
        self.active_health
            .iter()
            .filter(|e| e.value().unhealthy_until.is_some_and(|u| u > now))
            .count()
    }

    /// 节点的连续失败次数（从未失败过则为 0）。
    pub fn probe_consecutive_failures(&self, addr: &str) -> u32 {
        self.active_health
            .get(addr)
            .map_or(0, |st| st.value().consecutive_failures)
    }

    /// 节点是否「可用」：在当前池中且 `weight>0`。
    pub(crate) fn is_eligible(&self, addr: &str) -> bool {
        let guard = self.pools.load();
        guard.iter().any(|n| n.addr == addr && n.weight > 0)
    }

    /// 同一时刻、同一池快照下的精确计数：`(可用节点数, 其中被摘除的节点数)`。
    ///
    /// 安全阀用它判断「再摘一个是否就没人可用了」。**必须**用同一次池快照与
    /// 同一时刻算出这两个数——分两次读会跨越池替换窗口，得到彼此不自洽的组合，
    /// 正是旧版增量计数器低估的根源。
    pub(crate) fn count_eligible_and_unhealthy(&self, now: Instant) -> (usize, usize) {
        let guard = self.pools.load();
        let mut eligible = 0usize;
        let mut unhealthy = 0usize;
        for n in guard.iter() {
            if n.weight == 0 {
                continue;
            }
            eligible += 1;
            if self.is_active_unhealthy(&n.addr, now) {
                unhealthy += 1;
            }
        }
        (eligible, unhealthy)
    }

    /// 探测候选：**全池** `weight>0` 节点，付费 100%、免费按轮转游标抽样。
    ///
    /// # 为什么必须包含已被摘除的节点
    ///
    /// 摘除是「停止给它发流量」，不是「放弃这个节点」。若探测候选排除摘除节点，
    /// 被摘节点就永远得不到探测结果 ⇒ 永远没有恢复机会 ⇒ 一次误判即永久损失。
    /// 故此处取全池 `weight>0`，不按健康状态过滤。
    ///
    /// # 为什么免费池要抽样
    ///
    /// 免费池动辄数百地址且大量已死，全量探测会把日志与探测预算吃光。付费节点
    /// 是 SLA 主体，**永远 100% 探测**。
    ///
    /// # 为什么用轮转游标而不是随机抽样
    ///
    /// 随机不保证覆盖：小池可能连续多轮抽不到尾部节点，那些节点就永远得不到
    /// 恢复机会。游标保证 `ceil(100/pct)` 轮内全覆盖。
    pub fn probe_candidates(&self, free_sample_percent: u32) -> Vec<Arc<ProxyNode>> {
        let percent = free_sample_percent.clamp(1, 100);
        // 抽样步长：25% ⇒ 每 4 个免费节点取 1 个。`max(1)` 防除零。
        let step = (100 / percent).max(1) as usize;
        let cursor = self.probe_cursor.fetch_add(1, Ordering::Relaxed) % step;
        let guard = self.pools.load();
        let mut out = Vec::new();
        let mut free_seen = 0usize;
        for n in guard.iter() {
            if n.weight == 0 {
                continue;
            }
            if is_free_provider(&n.provider) {
                if free_seen % step == cursor {
                    out.push(Arc::clone(n));
                }
                free_seen += 1;
            } else {
                out.push(Arc::clone(n));
            }
        }
        out
    }

    /// 清扫主动健康表。
    ///
    /// 两件事：
    /// ① **僵尸清理**——节点已不在池中（被删或被换池）⇒ 整条删除；
    /// ② **到期清理**——冷却已过的条目清掉摘除标记（节点重新变为可用）。
    ///
    /// 连续失败次数**保留**：冷却到期后若再失败，应当**立即**重新摘除，
    /// 而不是重新累计阈值（否则一个持续不稳定的节点会在"摘除—放行—再摘"之间
    /// 反复横跳，每一轮都要重新失败三次）。
    pub fn sweep_active_health(&self) {
        self.prune_active_health_orphans();
        let now = Instant::now();
        for mut e in self.active_health.iter_mut() {
            if e.value().unhealthy_until.is_some_and(|u| u <= now) {
                e.value_mut().unhealthy_until = None;
            }
        }
    }

    /// 删除池中已不存在（僵尸）的健康条目。
    ///
    /// 池替换（`reload_nodes` / `remove_node` / `replace_vendor_nodes`）后调用，
    /// 否则被删节点的健康记录会一直占着表。
    pub(crate) fn prune_active_health_orphans(&self) {
        let guard = self.pools.load();
        let live: HashSet<&str> = guard.iter().map(|n| n.addr.as_str()).collect();
        self.active_health
            .retain(|addr, _| live.contains(addr.as_str()));
    }

    /// 探测成功：清掉该节点的健康记录。
    ///
    /// 返回是否此前存在记录（调用方据此区分「这是恢复」与「本来就健康」）。
    pub fn record_probe_success(&self, addr: &str) -> bool {
        self.active_health.remove(addr).is_some()
    }

    /// 探测失败：累计连续失败次数，达阈值则摘除该节点。
    ///
    /// # 为什么判定与落状态必须持锁
    ///
    /// 安全阀要读一个**跨条目**的事实（"当前还剩几个可用节点"），并与随后的
    /// 落状态构成一次原子操作。若两个线程各自读计数再各自落状态，它们可能同时
    /// 通过"还剩一个"的检查而一起被摘 ⇒ 安全阀形同虚设 ⇒ 全池 503。
    /// 摘除是低频事件（每节点每冷却周期至多一次），用一把锁换原子性代价可忽略。
    pub fn record_probe_failure(&self, addr: &str) -> ProbeFailureOutcome {
        let now = Instant::now();
        let _guard = self.probe_mark_lock.lock();

        // 探测期间节点已被删除或权重归零 ⇒ 不落任何健康状态。
        if !self.is_eligible(addr) {
            return ProbeFailureOutcome::NodeGone;
        }

        let cooldown = self.probe_unhealthy_cooldown;

        // 已在摘除中 ⇒ 只刷新冷却截止时间。
        if self.is_active_unhealthy(addr, now) {
            if let Some(mut e) = self.active_health.get_mut(addr) {
                e.value_mut().unhealthy_until = Some(now + cooldown);
            }
            return ProbeFailureOutcome::RefreshedUnhealthy;
        }

        // 未达阈值：只累计失败次数。
        let failures = {
            let mut e = self.active_health.entry(addr.to_string()).or_default();
            e.consecutive_failures = e.consecutive_failures.saturating_add(1);
            e.consecutive_failures
        };
        if failures < self.probe_failure_threshold {
            return ProbeFailureOutcome::BelowThreshold;
        }

        // 安全阀：再摘一个就没有 `weight>0` 的节点可用了 ⇒ 抑制摘除。
        // 计数必须取**同一次池快照 + 同一时刻**（见 `count_eligible_and_unhealthy`）。
        let (eligible, unhealthy) = self.count_eligible_and_unhealthy(now);
        if eligible.saturating_sub(unhealthy) <= 1 {
            return ProbeFailureOutcome::SuppressedLastNode;
        }

        if let Some(mut e) = self.active_health.get_mut(addr) {
            e.value_mut().unhealthy_until = Some(now + cooldown);
        }
        // 粘滞会话必须一并失效：否则被摘节点仍会被会话绑定钉住继续接流量
        // （"已摘除却仍在发流量"的漏网路径）。
        self.session_store.retain(|_, (node, _)| node.addr != addr);
        ProbeFailureOutcome::MarkedUnhealthy
    }
}
