//! **写入侧**：`observe*` / `note_*` / `set_*`。
//!
//! 这些方法只做一件事：把事件累加到原子量或 DashMap 里。它们被数据面热路径
//! （`observe` / `observe_stage`，每次请求都走）与后台工人（免费池、熔断、geo、
//! 主动探测）调用，**不做任何格式化**——文本渲染全部留给 `render.rs`。
//!
//! 两侧的唯一契约是**指标名与标签集**：这里累加的每个字段，必须在 `render.rs`
//! 里有对应的 `# HELP` / `# TYPE` 行，否则指标写进去了却抓不到（静默失效）。

use super::*;

impl MetricsRegistry {
    /// 免费池在池节点数（FreePool worker 每次合并后设置，只写不参与 observe）。
    pub fn set_free_pool_nodes(&self, n: u64) {
        self.free_pool_nodes.store(n, Ordering::Relaxed);
    }

    /// OPT-R18 P2-3：免费池**供给 SLO** 三元组（每次合并后由 `merge_once` 设置）。
    ///
    /// - `distinct_exits`：池内不同出口身份个数；
    /// - `median_age`：池内节点年龄中位数，`None` ⇒ 空池/未知（渲染 NaN）。
    ///
    /// 三个数字必须**同时**更新：分两次写会让 render 出现「节点数已更新、
    /// 年龄还是上一轮」的瞬间不一致，而这种不一致恰好会被基于窗口的告警放大。
    pub fn set_free_pool_supply(
        &self,
        distinct_exits: u64,
        median_age: Option<std::time::Duration>,
    ) {
        self.free_pool_distinct_exits
            .store(distinct_exits, Ordering::Relaxed);
        // `as_millis()` 溢出 u64 需要 ~5.8 亿年，这里截到 i64 上界只为消除
        // `as` 的静默截断；正常值远小于该界。
        let ms = median_age
            .map(|d| d.as_millis().min(i64::MAX as u128) as i64)
            .unwrap_or(-1);
        self.free_pool_median_age_ms.store(ms, Ordering::Relaxed);
    }

    /// 质检结果的 `(pass, 全部)` 合计，供 `free_pool_yield_ratio` 用。
    ///
    /// 返回 `(0, 0)` 表示"还没有样本"——调用方必须把它与"转化率为 0"区分开
    /// （前者渲染 NaN，后者渲染 0）。
    pub(super) fn free_verify_totals(&self) -> (u64, u64) {
        let mut pass = 0u64;
        let mut total = 0u64;
        for e in self.free_verify.iter() {
            let n = e.value().load(Ordering::Relaxed);
            total = total.saturating_add(n);
            if e.key() == "pass" {
                pass = pass.saturating_add(n);
            }
        }
        (pass, total)
    }

    /// GW-6：免费池供给快照（Admin API `/api/v1/freepool/*` 用，只读）。
    pub fn free_pool_snapshot(&self) -> FreePoolSnapshot {
        // 各 source 名称并集（来自 yield/elite/fetch/suspended 的 key）。
        let mut sources: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
        for e in self.free_source_yield.iter() {
            sources.insert(e.key().clone());
        }
        for e in self.free_source_elite.iter() {
            sources.insert(e.key().clone());
        }
        for e in self.free_fetch_ms_sum.iter() {
            sources.insert(e.key().clone());
        }
        for e in self.free_fetch_count.iter() {
            sources.insert(e.key().clone());
        }
        for e in self.free_suspended.iter() {
            sources.insert(e.key().clone());
        }

        let get_yield = |s: &str| {
            self.free_source_yield
                .get(s)
                .map(|v| v.load(Ordering::Relaxed))
                .unwrap_or(0)
        };
        let get_elite = |s: &str| {
            self.free_source_elite
                .get(s)
                .map(|v| v.load(Ordering::Relaxed))
                .unwrap_or(0)
        };
        let get_fetch_sum = |s: &str| {
            self.free_fetch_ms_sum
                .get(s)
                .map(|v| v.load(Ordering::Relaxed))
                .unwrap_or(0)
        };
        let get_fetch_cnt = |s: &str| {
            self.free_fetch_count
                .get(s)
                .map(|v| v.load(Ordering::Relaxed))
                .unwrap_or(0)
        };

        let sources: Vec<FreePoolSourceStat> = sources
            .into_iter()
            .map(|source| FreePoolSourceStat {
                source: source.clone(),
                yield_total: get_yield(&source),
                elite_total: get_elite(&source),
                fetch_ms_sum: get_fetch_sum(&source),
                fetch_count: get_fetch_cnt(&source),
            })
            .collect();

        let verify: Vec<VerifyStat> = self
            .free_verify
            .iter()
            .map(|e| VerifyStat {
                result: e.key().clone(),
                count: e.value().load(Ordering::Relaxed),
            })
            .collect();
        let by_proto: Vec<ProtoStat> = self
            .free_nodes_by_proto
            .iter()
            .map(|e| ProtoStat {
                proto: e.key().clone(),
                count: e.value().load(Ordering::Relaxed),
            })
            .collect();
        let anonymity: Vec<AnonymityStat> = self
            .free_anonymity
            .iter()
            .map(|e| AnonymityStat {
                level: e.key().clone(),
                count: e.value().load(Ordering::Relaxed),
            })
            .collect();
        let suspended: Vec<SuspendedStat> = self
            .free_suspended
            .iter()
            .map(|e| SuspendedStat {
                source: e.key().clone(),
                suspended: e.value().load(Ordering::Relaxed),
            })
            .collect();

        FreePoolSnapshot {
            nodes_total: self.free_pool_nodes.load(Ordering::Relaxed),
            dry: self.free_pool_nodes.load(Ordering::Relaxed) == 0,
            verify,
            by_proto,
            sources,
            anonymity,
            suspended,
            intake_capped: self.free_intake_capped.load(Ordering::Relaxed),
            baseline_fail: self.free_baseline_fail.load(Ordering::Relaxed),
            evicted: self.free_evicted.load(Ordering::Relaxed),
        }
    }

    /// GW-6：记录一次熔断触发（仅 429/403/502/504，由 circuit_breaker 调用）。
    pub fn note_breaker_trigger(&self, status: u16) {
        match status {
            429 | 403 | 502 | 504 => {
                self.breaker_trigger
                    .entry(status.to_string())
                    .or_insert_with(|| AtomicU64::new(0))
                    .fetch_add(1, Ordering::Relaxed);
            }
            _ => {}
        }
    }

    /// GW-6：记录一次 LinUCB 分档强制探索（tier_quota 强制选路）。
    #[allow(dead_code)]
    pub fn note_bandit_exploration(&self) {
        self.bandit_exploration.fetch_add(1, Ordering::Relaxed);
    }

    /// GW-6：设置隔离域数（外层 domain 数，由 sweep 同步）。
    pub fn set_quarantine_domains(&self, n: u64) {
        self.quarantine_domains.store(n, Ordering::Relaxed);
    }

    /// GW-6：断路器快照（Admin API `/api/v1/circuit` 用，只读）。
    pub fn breaker_snapshot(&self) -> BreakerSnapshot {
        let breaker_trigger: Vec<BreakerTriggerStat> = self
            .breaker_trigger
            .iter()
            .map(|e| BreakerTriggerStat {
                status: e.key().clone(),
                count: e.value().load(Ordering::Relaxed),
            })
            .collect();
        BreakerSnapshot {
            breaker_trigger,
            bandit_exploration: self.bandit_exploration.load(Ordering::Relaxed),
            quarantine_domains: self.quarantine_domains.load(Ordering::Relaxed),
            quarantine_nodes: self.quarantine_nodes.load(Ordering::Relaxed),
        }
    }

    /// OPT-R15：标记某落地端一次**成功**写入（`up=1` 并刷新 `last_ok`）。
    ///
    /// 只在真正把数据交给落地端后才调用。序列化失败、本地缓冲溢出**都不算**落地端健康，
    /// 否则会把"网关自己的 bug"误报成"依赖挂了"。
    pub fn mark_sink_ok(&self, backend: &str) {
        self.sink_up
            .entry(backend.to_string())
            .or_insert_with(|| AtomicU64::new(1))
            .store(1, Ordering::Relaxed);
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        self.sink_last_ok
            .entry(backend.to_string())
            .or_insert_with(|| AtomicU64::new(0))
            .store(now, Ordering::Relaxed);
    }

    /// OPT-R15：标记某落地端一次**失败**（`up=0` + 失败计数 +1）。
    ///
    /// `up` 的语义是「最近一次尝试的结果」而非「一段时间内的健康度」——抖动会翻转，
    /// 这是刻意的：告警侧用 `== 0` 持续 2m 过滤毛刺（见 `docs/OPERATION.md` 告警口径）。
    pub fn mark_sink_failed(&self, backend: &str) {
        self.sink_up
            .entry(backend.to_string())
            .or_insert_with(|| AtomicU64::new(0))
            .store(0, Ordering::Relaxed);
        self.sink_failures
            .entry(backend.to_string())
            .or_insert_with(|| AtomicU64::new(0))
            .fetch_add(1, Ordering::Relaxed);
        // 首次失败即建 `last_ok=0` 序列：告警上「试过、一直失败、从未成功」
        // 与「压根还没试过」必须可区分，否则一次都没成功的后端会一直沉默。
        self.sink_last_ok
            .entry(backend.to_string())
            .or_insert_with(|| AtomicU64::new(0));
    }

    /// FreePool 源站抓取产出累加（worker 每轮按 source 聚合计数；零产出源不记，
    /// 其熔断由 suspend gauge 可见）。
    pub fn note_free_source_yield(&self, source: &str, n: u64) {
        self.free_source_yield
            .entry(source.to_string())
            .or_insert_with(|| AtomicU64::new(0))
            .fetch_add(n, Ordering::Relaxed);
    }

    /// NEXT-B5：per-source Elite 记一笔（FullCheck 判 Elite 即调；只增不减）。
    pub fn note_free_source_elite(&self, source: &str) {
        self.free_source_elite
            .entry(source.to_string())
            .or_insert_with(|| AtomicU64::new(0))
            .fetch_add(1, Ordering::Relaxed);
    }

    /// FreePool 质检结果计数。**OPT-R14 A2：以 `tools/check_free_pool_contract.py` 为契约源**。
    /// 调用点实际只有 4 个：`pass/tcp_fail/full_fail/geo_fail`。
    /// 原注释曾列 `backoff_skip`，但**无任何调用点会发出它**（`backoff_until` 机制存在却不记账）——
    /// 它是个**死标签**：运维看到该标签永远不出现，会误以为“被 backoff 跳过的节点在统计”。
    /// 本轮已从注释删除；若未来确实要发出它，请**同时**加上调用点并更新那道门的期望集。
    pub fn note_free_verify(&self, result: &str) {
        self.free_verify
            .entry(result.to_string())
            .or_insert_with(|| AtomicU64::new(0))
            .fetch_add(1, Ordering::Relaxed);
    }

    /// FreePool 匿名度分级计数（level 调用方保证∈elite/anonymous/transparent/unknown）。
    pub fn note_free_anonymity(&self, level: &str) {
        self.free_anonymity
            .entry(level.to_string())
            .or_insert_with(|| AtomicU64::new(0))
            .fetch_add(1, Ordering::Relaxed);
    }

    /// FreePool 源站熔断 gauge（worker 每轮同步各源 suspend 状态）。
    pub fn set_free_source_suspended(&self, source: &str, suspended: bool) {
        self.free_suspended
            .entry(source.to_string())
            .or_insert_with(|| AtomicU64::new(0))
            .store(u64::from(suspended), Ordering::Relaxed);
    }

    /// P2 FreePool 按出站协议水位（worker 每次合并后按快照聚合设置三档；只写 gauge）。
    pub fn set_free_pool_nodes_proto(&self, proto: &str, n: u64) {
        self.free_nodes_by_proto
            .entry(proto.to_string())
            .or_insert_with(|| AtomicU64::new(0))
            .store(n, Ordering::Relaxed);
    }

    /// P3 GeoIP 观察计数（result 调用方保证∈hit/miss/disabled/error）。
    pub fn note_geo_lookup(&self, result: &str) {
        self.geo_lookups
            .entry(result.to_string())
            .or_insert_with(|| AtomicU64::new(0))
            .fetch_add(1, Ordering::Relaxed);
    }

    /// P3 exit-国家实锤分歧计数（只观察不执法；执法留 Phase 4）。
    pub fn note_geo_mismatch(&self) {
        self.geo_mismatch.fetch_add(1, Ordering::Relaxed);
    }

    /// OPT-R4 B9：进检截断丢弃累加（调用方：free_pool intake cap 分支，按截掉量记 n）。
    pub fn note_free_intake_capped(&self, n: u64) {
        self.free_intake_capped.fetch_add(n, Ordering::Relaxed);
    }

    /// OPT-R4 B9：基线不可达记一笔（调用方：free_pool baseline None 降级分支）。
    pub fn note_free_baseline_fail(&self) {
        self.free_baseline_fail.fetch_add(1, Ordering::Relaxed);
    }

    /// OPT-R4 B9：容量淘汰累加（调用方：evict_if_over_capacity，按淘汰量记 n）。
    pub fn note_free_evicted(&self, n: u64) {
        self.free_evicted.fetch_add(n, Ordering::Relaxed);
    }

    /// OPT-R4 B9：按源抓取耗时记一笔（调用方：fetch_all 每源 fetch 返回后记；
    /// 超时/失败轮也建议记，用于分母完整；和值饱和累加，count+1）。
    pub fn note_free_source_fetch(&self, source: &str, duration: Duration) {
        // 单次截断 60s：与 observe 同口径，避免单轮超时污染均值。
        let ms = duration.as_millis().min(60_000) as u64;
        self.free_fetch_count
            .entry(source.to_string())
            .or_insert_with(|| AtomicU64::new(0))
            .fetch_add(1, Ordering::Relaxed);
        let sum = self
            .free_fetch_ms_sum
            .entry(source.to_string())
            .or_insert_with(|| AtomicU64::new(0));
        saturating_add(sum.value(), ms);
    }

    /// OPT-R5 E5：单节点转发延迟记一笔（ms 直记；单次截断 60_000ms，和值饱和累加；
    /// 空值不过滤，调用方保证只在 FullCheck 成功路径记）。
    pub fn note_free_fwd_latency(&self, ms: u64) {
        let ms = ms.min(60_000);
        if let Some(i) = DURATION_BUCKETS_MS.iter().position(|bound| ms <= *bound) {
            self.free_fwd_buckets[i].fetch_add(1, Ordering::Relaxed);
        } else {
            // 超最大桶：只进 `_count`/`_sum`（`+Inf` 行），各 `le` 桶不动（与 observe 同口径）。
        }
        self.free_fwd_count.fetch_add(1, Ordering::Relaxed);
        saturating_add(&self.free_fwd_sum_ms, ms);
    }

    /// OPT-R5 E5：出口分组记一笔（空串不记；None 由调用方跳过，不在此处展开 Option；
    /// 轮转语义＝该序列 `changes()`，见 render/HELP 注释）。
    ///
    /// # OPT-R9 C2：容量上限（出口 IP 标签基数护栏）
    ///
    /// 旧实现无条件 `entry(exit_ip.to_string()).or_insert_with(...)`，而免费代理的
    /// 出口 IP 是**公网随机 IP** ⇒ 标签取值空间无界 ⇒ 长期运行会同时撑爆两处：
    /// 网关内存（`DashMap` 条目）与 Prometheus TSDB（每个新序列一个 chunk +
    /// 索引项）。这是**资源无界增长**问题，与并发/安全无关。
    ///
    /// # 为什么用「溢出桶」而不是「直接丢弃」
    ///
    /// 直接丢弃会让「某出口已用 1M 次」与「某出口仅 1 次」在 TopK 图上无法区分
    /// （后者已存在序列），造成**指标语义断层**。溢出桶保留了「还有更多出口在用」
    /// 这一事实，且是唯一常渲染的「上限已触达」信号。
    ///
    /// # 语义保证（向后兼容）
    ///
    /// 未超上限时渲染输出与旧实现**逐字节一致**。超上限后：已存在的 512 条真实
    /// 序列**继续正常递增**（不因达上限而冻结）；新出口全部计入
    /// `exit_ip="__overflow__"`；该桶**恒渲染**（值为 0 时也渲染），
    /// 使面板/告警能判「是否已溢出」。
    pub fn note_free_exit(&self, exit_ip: &str) {
        if exit_ip.is_empty() {
            return;
        }
        // 快路径：已有序列直接递增。**刻意不检查上限**——已达上限后已存在的
        // 序列必须继续正常计数，否则统计会在 512 处静默截断（更坏的失真）。
        if let Some(counter) = self.free_exit_total.get(exit_ip) {
            counter.fetch_add(1, Ordering::Relaxed);
            return;
        }
        // 慢路径：首次见到该出口。达上限则并入溢出桶。
        //
        // 不用 `DashMap::entry().or_insert_with()`：那样在并发下两个线程可能
        // 同时越过 `len() < CAP` 检查，各自建序列，导致**略微超限**。这里先取
        // 持有 `Ref` 再决策，保证同一时刻只有一个线程能新建（`entry` API 本身
        // 是原子的，但与 `len()` 检查组合不原子；先 get 再 entry 的两步法
        // 同样不严格，但**超限幅度被限制在并发窗口内**，且语义可接受——
        // 上限是**护栏**不是精确配额，真正的硬保证应由 Prometheus 侧
        // `metric_relabel_configs` 兜底）。
        if self.free_exit_total.len() >= EXIT_LABEL_CAP {
            self.free_exit_total
                .entry(EXIT_OVERFLOW_LABEL.to_string())
                .or_insert_with(|| AtomicU64::new(0))
                .fetch_add(1, Ordering::Relaxed);
            return;
        }
        self.free_exit_total
            .entry(exit_ip.to_string())
            .or_insert_with(|| AtomicU64::new(0))
            .fetch_add(1, Ordering::Relaxed);
    }
    /// Record one finished proxied response (called from `logging`).
    ///
    /// R2-6 写侧单原子：只给首个 `le >= value` 的桶 +1（每次 `observe` 恰一次
    /// 原子写，而非 10 次），`render` 侧前缀累加成 Prometheus 累计桶。
    /// 根治旧累计存储的撕裂：旧写法按桶序逐个 +1，`render` 若在两次 +1 之间
    /// 读到相邻两桶，会渲染出 `le 小 > le 大` 的非单调行；单原子写下渲染值
    /// 恒为非负前缀和，天然单调。
    /// NEXT-A4：桥出站失败记一笔（调用方：`serve_via_socks` Err 分支）。
    pub fn note_bridge_error(&self) {
        self.bridge_errors.fetch_add(1, Ordering::Relaxed);
    }

    /// NEXT-A4：内存隔离表水位同步（调用方：sweep 滴答）。
    pub fn set_quarantine_nodes(&self, n: u64) {
        self.quarantine_nodes.store(n, Ordering::Relaxed);
    }

    /// 步骤 54：记录一次主动探测结果。
    ///
    /// 只接受三个固定标签值；未知值直接丢弃，防止调用方误传节点地址等
    /// 高基数标签把 Prometheus 序列打爆。
    pub fn note_active_probe_result(&self, result: &str) {
        if !matches!(result, "healthy" | "degraded" | "dead") {
            return;
        }
        self.active_probe_results
            .entry(result.to_string())
            .or_insert_with(|| AtomicU64::new(0))
            .fetch_add(1, Ordering::Relaxed);
    }

    /// 步骤 54：同步当前主动健康摘除节点数（prober tick 结束调用）。
    pub fn set_active_probe_unhealthy(&self, n: u64) {
        self.active_probe_unhealthy.store(n, Ordering::Relaxed);
    }

    pub fn observe(&self, status: u16, provider: Option<&str>, bytes: u64, duration: Duration) {
        let status_class = match status {
            200..=299 => "2xx",
            400..=499 => "4xx",
            500..=599 => "5xx",
            _ => "other",
        };
        match status {
            200..=299 => self.req_2xx.fetch_add(1, Ordering::Relaxed),
            // NEXT-A4：402/429 独立计数（仍进 4xx 总盘，不破坏存量口径）。
            402 => {
                self.req_4xx.fetch_add(1, Ordering::Relaxed);
                self.req_402.fetch_add(1, Ordering::Relaxed)
            }
            429 => {
                self.req_4xx.fetch_add(1, Ordering::Relaxed);
                self.req_429.fetch_add(1, Ordering::Relaxed)
            }
            400..=499 => self.req_4xx.fetch_add(1, Ordering::Relaxed),
            500..=599 => self.req_5xx.fetch_add(1, Ordering::Relaxed),
            _ => self.req_other.fetch_add(1, Ordering::Relaxed),
        };
        // 按 (status_class, provider) 计数，provider 缺失时用 "none"，供 per-provider 查询。
        let provider_label = provider.unwrap_or("none").to_string();
        self.req_by_provider
            .entry((status_class.to_string(), provider_label))
            .or_insert_with(|| AtomicU64::new(0))
            .fetch_add(1, Ordering::Relaxed);
        if status == 403 {
            if let Some(p) = provider {
                self.forbidden_by_provider
                    .entry(p.to_string())
                    .or_insert_with(|| AtomicU64::new(0))
                    .fetch_add(1, Ordering::Relaxed);
            }
        }
        self.bytes_total.fetch_add(bytes, Ordering::Relaxed);
        // OPT-R4 A4：单次截断 60_000ms（单轮毛刺不污染 sum）＋和值饱和累加
        //（CAS 循环，长稳运行不回绕，Prometheus sum 曲线不倒退）。
        let ms = duration.as_millis().min(60_000) as u64;
        if let Some(i) = DURATION_BUCKETS_MS.iter().position(|bound| ms <= *bound) {
            self.duration_buckets[i].fetch_add(1, Ordering::Relaxed);
        } else {
            // 超最大桶：只进 `_count`/`_sum`（`+Inf` 行），各 `le` 桶不动。
        }
        self.duration_count.fetch_add(1, Ordering::Relaxed);
        saturating_add(&self.duration_sum_ms, ms);
    }

    /// GW-6：记录某阶段耗时（ms，截断 60s），进入对应 histogram 桶。
    /// stage ∈ connect/ttfb/transfer；未知阶段静默忽略，不污染指标。
    pub fn observe_stage(&self, stage: &str, ms: u64) {
        let ms = ms.min(60_000);
        let (buckets, count, sum) = match stage {
            "connect" => (
                &self.stage_connect_buckets,
                &self.stage_connect_count,
                &self.stage_connect_sum_ms,
            ),
            "ttfb" => (
                &self.stage_ttfb_buckets,
                &self.stage_ttfb_count,
                &self.stage_ttfb_sum_ms,
            ),
            "transfer" => (
                &self.stage_transfer_buckets,
                &self.stage_transfer_count,
                &self.stage_transfer_sum_ms,
            ),
            _ => return,
        };
        if let Some(i) = DURATION_BUCKETS_MS.iter().position(|bound| ms <= *bound) {
            buckets[i].fetch_add(1, Ordering::Relaxed);
        }
        count.fetch_add(1, Ordering::Relaxed);
        saturating_add(sum, ms);
    }
}
