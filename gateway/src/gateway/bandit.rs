//! LinUCB 相关：取臂、上下文解析、分档配额选路、游离臂修剪、重试记账。
//!
//! 免费档与付费档的选路共用一套臂表（`bandit_arms`）。这里的方法解决三个
//! 容易被忽略的问题：
//! * **上下文只算一次**：`upstream_peer` 已算好则复用（`resolve_bandit_context`），
//!   省一次含时钟 syscall 的 `extract_context`；
//! * **分档配额**：`tier_quota_forced_tier` 保证每个有候选的 tier 在 100 次选路
//!   里至少被选一次——否则 free 档永远学不到数据（探索-利用的硬约束）；
//! * **重试记账**：失败地址去重入 `failed_addrs`（上限 16 防爆），与清字节的
//!   `reset_retry_accounting` 正交——一个管"换谁"，一个管"计多少"。

use super::*;

/// 取节点对应的 LinUCB 臂，没有则新建（首写竞态无害：新臂状态等价）。
///
/// # OPT-R10 B1：命中路径零分配
///
/// 旧实现第一行就是 `let key = node.addr.clone();`——**无论是否命中都克隆**。
/// 而调用点 `select_bandit_node_excluding` 是
/// `candidates.iter().map(|n| arm_for(&self.bandit_arms, n)).collect()`，
/// 即**每个候选一次** ⇒ 池大小 N 时每请求 N 次 `String` 克隆
/// （免费池缺省 `FREE_MAX_NODES=2000` ⇒ 约 2000 次/请求）。
/// 稳态下臂早已建好，**几乎每次调用都走命中分支**——克隆白付。
///
/// 修法：`DashMap::get` 接受 `&str`（`Borrow<str>`），**借用查询不需要拥有
/// `String`**。把克隆挪到「真正新建臂」的冷路径，命中路径只剩
/// 「一次哈希 + 一次 `Arc` 克隆」（`Arc` 克隆只加引用计数，不分配）。
///
/// 语义**零变化**：键仍是 `node.addr`，`prune_stale_arms` 的比对口径不变，
/// 臂的生命周期也不变——只改查询方式。
pub fn arm_for(arms: &DashMap<String, Arc<BanditArm>>, node: &ProxyNode) -> Arc<BanditArm> {
    // 命中路径：借用查询，零字符串分配。
    if let Some(existing) = arms.get(node.addr.as_str()) {
        return Arc::clone(existing.value());
    }
    // 冷路径：新建臂才需要拥有键（`BanditArm` 要持有它作 `key`）。
    let key = node.addr.clone();
    let arm = Arc::new(BanditArm::new(key.clone(), node.tier.clone()));
    arms.insert(key, arm.clone());
    arm
}

/// R2-6：取本请求的 bandit 上下文——`upstream_peer` 已算好则复用（选学一致，
/// 省一次 `extract_context` 含时钟 syscall），否则现算（粘滞路径/直调兼容）。
pub fn resolve_bandit_context(ctx: &ProxyContext, engine: &LinUCBEngine, domain: &str) -> VectorD {
    ctx.bandit_context
        .unwrap_or_else(|| engine.extract_context(domain))
}

/// R2-2 重试记账：把当前失败节点的 `addr` 记入 `failed_addrs`（去重、上限 16 防爆）。
///
/// 与 OPT-3 的 `reset_retry_accounting`（清字节）正交：一个管“换谁”，一个管“计多少”。
pub fn record_failed_addr(ctx: &mut ProxyContext) {
    if let Some(ref node) = ctx.current_node {
        let addr = node.addr.clone();
        if !ctx.failed_addrs.iter().any(|e| e == &addr) && ctx.failed_addrs.len() < 16 {
            ctx.failed_addrs.push(addr);
        }
    }
}
/// OPT-3 重试计量口径：新 attempt 从零累计，已失败 attempt 的字节不进账单。
///
/// - 语义冻结：只计 **最后一次 attempt** 的出站字节（`response_body_filter`
///   在每次 attempt 内累加，`fail_to_connect` 在发起重试前清零）；
/// - `logging` 侧不做补偿（直接按 `ctx.transferred_bytes` 计量/上报），
///   因此拦截态（无 attempt）与成功态口径天然一致；
/// - 已知限制：强制一次失败重试的端到端构造困难，覆盖以本单测 + 代码走查为准。
pub fn reset_retry_accounting(ctx: &mut ProxyContext) {
    ctx.transferred_bytes = 0;
    if let Some(settlement) = ctx.settlement.as_mut() {
        settlement.reset_bytes();
    }
}

/// 修剪游离臂：删除不在路由器全量池中的臂状态。
///
/// - 白名单必须是 `snapshot_all` 全量快照，不能用健康候选
///   （被隔离节点的臂是“暂时不用”，不是“已下线”，误删会丢学习成果）；
/// - 返回删除个数，供后台 ticker 打日志。
pub fn prune_stale_arms(arms: &DashMap<String, Arc<BanditArm>>, router: &RouterEngine) -> usize {
    let alive: std::collections::HashSet<String> = router
        .snapshot_all()
        .iter()
        .map(|n| n.addr.clone())
        .collect();
    let before = arms.len();
    arms.retain(|key, _| alive.contains(key));
    // OPT-R11 A1：原为裸 `before - arms.len()`，是 OPT-R6 S2 **漏掉的第 4 处**
    // 同构写法（`janitor.rs` 的文档当时只列了「三处」，此处未同步）。
    //
    // 危害与前 3 处完全同型，但本表的写入方是**数据面** `arm_for()` 的冷路径
    // `arms.insert(...)`（见本文件 `arm_for`）——后台 sweep 每 60s 修剪一次，
    // 两次 `len()` 之间若有新臂插入，`after > before` ⇒ debug 下
    // `attempt to subtract with overflow` panic（sweep 任务死亡），release 下
    // 回绕成 `usize::MAX` 并经 `main.rs` 的池计数二次溢出，日志永久输出天文数字。
    //
    // 红测见本文件末尾 `opt_r11_a1_*`：并发守护测试在修复前实测 panic 于
    // 本行（`src/gateway.rs:247`）。
    crate::janitor::removed_count(before, arms.len())
}
impl SmartProxyGateway {
    /// 无状态请求的 LinUCB 选路（无排除兼容入口；网关重试路径走 excluding 版）。
    #[allow(dead_code)]
    pub(super) fn select_bandit_node(&self, spec: &RoutingSpec) -> Option<Arc<ProxyNode>> {
        let context = self.bandit_engine.extract_context(&spec.target_domain);
        self.select_bandit_node_excluding(spec, &[], &context)
    }

    /// R2-2：带失败排除的 LinUCB 选路（重试路径用；`excluded` 为 `ip:port` 集合）。
    /// R2-6：`context` 由调用方（`upstream_peer`）算好传入，本函数只选不算；
    /// 候选/命中均为池内 `Arc` 句柄（零 `String` 克隆）。
    ///
    /// OPT-R11 B1：原实现先把候选物化成 `Vec<Arc<ProxyNode>>`、再物化成
    /// `Vec<Arc<BanditArm>>`、选完臂后还要 `find(|n| n.addr == best.key)`
    /// **第三次**扫一遍候选找回节点——而最终只用到一个节点。池 2000 时这是
    /// 每请求三次 O(N) 遍历 ＋ 两次 O(N) 分配（2000 时约 2KB + 16KB）。
    ///
    /// 现改为**流式单趟**：遍历候选 → 逐个 `arm_for` 打分 → 只保留最优的
    /// `(节点, 臂)` 一对，**不物化任何 Vec、不做第三次扫描**。
    ///
    /// # 三处等价性论证（防止"优化"悄悄改语义）
    ///
    /// 1. **臂打分**：仍走 `bandit_engine`，仍单趟、严格 `>` ⇒ 首个最大值
    ///    胜出，与 `select_best_arm` 一致（`select_best_arm_owned` 逐字同构）。
    /// 2. **节点找回**：臂键即 `node.addr`（`arm_for` 冷路径
    ///    `BanditArm::new(key.clone(), ...)`），且池内 `addr` 唯一 ⇒
    ///    "按 best.key 找节点" 与 "记录打分时该臂所属节点" 等价。
    /// 3. **空候选**：无候选时 `select_best_arm_owned` 返回 `None` ⇒
    ///    函数返回 `None`（网关映射 503），与旧实现一致。
    pub(super) fn select_bandit_node_excluding(
        &self,
        spec: &RoutingSpec,
        excluded: &[String],
        context: &VectorD,
    ) -> Option<Arc<ProxyNode>> {
        let now = std::time::Instant::now();
        // OPT-R15：分档最小探索配额。仅在初始选路（excluded 为空）时检查与更新窗口；
        // 重试路径属于同一逻辑请求，不重复计数。
        let forced_tier = if excluded.is_empty() {
            self.tier_quota_forced_tier(spec, excluded, now)
        } else {
            None
        };
        // 过滤条件与顺序逐字沿用 `RouterEngine::get_healthy_candidates_excluding`
        // （`excluded` 先判、`matches` 后判）⇒ 候选集合与顺序不变。
        let mut best: Option<(Arc<ProxyNode>, Arc<BanditArm>)> = None;
        let mut best_score = f64::NEG_INFINITY;
        // OPT-R13：`t`（全局选路步数）**每个请求取一次**，放在候选循环**外**。
        // 若在 `compute_ucb_score` 内部自增，`t` 会按候选数增长（池 2000 时
        // 一请求 +2000），`t` 失去"请求数"含义、探索项被放大到失真。
        let t = self.bandit_engine.next_selection_step();
        self.router.with_pools(|pool| {
            for n in pool {
                if excluded.iter().any(|e| e == &n.addr) {
                    continue;
                }
                if !self.router.matches_node(n, spec, now) {
                    continue;
                }
                // OPT-R15：配额强制——若指定了 tier，跳过非该 tier 的候选。
                if let Some(ref ft) = forced_tier {
                    if n.tier != *ft {
                        continue;
                    }
                }
                // `arm_for` 内部只做**短生命周期** `get()`（OPT-R10 B1 已改为借用
                // 查询），故此处不跨调用持有 `bandit_arms` 的 ref——
                // 冷路径 `insert` 若与长生命周期 ref 同分片会死锁。
                let arm = arm_for(&self.bandit_arms, n);
                let score = arm.compute_ucb_score(context, self.bandit_engine.alpha, t);
                if score > best_score {
                    best_score = score;
                    best = Some((Arc::clone(n), arm));
                }
            }
        });
        // OPT-R15：初始选路完成后，将选中 tier 推入配额窗口。
        if excluded.is_empty() {
            if let Some((ref node, _)) = best {
                let mut w = self.tier_quota_window.lock().unwrap();
                w.push_back(node.tier.clone());
                if w.len() > TIER_QUOTA_WINDOW {
                    w.pop_front();
                }
            }
        }
        best.map(|(node, _)| node)
    }

    /// OPT-R15：检查配额窗口，返回需要强制选路的 tier（若有）。
    ///
    /// 语义：若某 tier 有候选但缺席最近 `TIER_QUOTA_WINDOW` 次选路，则该 tier
    /// 本次必须被选中。这保证每个 tier 在任意 100 次选路中至少获得 1 次流量，
    /// 使 free 供给从第 1 个请求起就可被评估（anytime 保证）。
    ///
    /// 返回 `None` 表示无需强制（正常 UCB 选路）。
    fn tier_quota_forced_tier(
        &self,
        spec: &RoutingSpec,
        excluded: &[String],
        now: std::time::Instant,
    ) -> Option<String> {
        // 第一遍：收集有候选的 tier 集合。
        let mut candidate_tiers: Vec<String> = Vec::new();
        self.router.with_pools(|pool| {
            for n in pool {
                if excluded.iter().any(|e| e == &n.addr) {
                    continue;
                }
                if !self.router.matches_node(n, spec, now) {
                    continue;
                }
                if !candidate_tiers.contains(&n.tier) {
                    candidate_tiers.push(n.tier.clone());
                }
            }
        });
        if candidate_tiers.len() <= 1 {
            // 只有一个 tier 有候选时，配额无意义（无处可切）。
            return None;
        }
        // 检查窗口中缺席的 tier。
        let w = self.tier_quota_window.lock().unwrap();
        let absent: Vec<&String> = candidate_tiers
            .iter()
            .filter(|t| !w.iter().any(|x| x == *t))
            .collect();
        if absent.is_empty() {
            return None;
        }
        // 选候选数最少的缺席 tier（最小化对选路的干扰）。
        drop(w);
        let mut best_tier: Option<&String> = None;
        let mut best_count = usize::MAX;
        for t in &absent {
            let count = self.router.with_pools(|pool| {
                pool.iter()
                    .filter(|n| {
                        !excluded.iter().any(|e| e == &n.addr)
                            && self.router.matches_node(n, spec, now)
                            && n.tier == **t
                    })
                    .count()
            });
            if count < best_count {
                best_count = count;
                best_tier = Some(t);
            }
        }
        best_tier.cloned()
    }

    /// 修剪本网关的游离臂（单测与外部调用方使用）。
    /// 注：后台 ticker 走 `prune_stale_arms` 自由函数，因为网关对象建成后
    /// 所有权归 Pingora service，主函数拿不到 `&self`，只持有拆出的 Arc。
    #[allow(dead_code)]
    pub fn prune_arms(&self) -> usize {
        prune_stale_arms(&self.bandit_arms, &self.router)
    }
}
