//! **读出侧**：`render()`（Prometheus 文本）与 `serve_metrics()`（HTTP 服务）。
//!
//! 渲染是**纯读**：只 `load(Ordering::Relaxed)` 原子量与遍历 DashMap，不改任何状态。
//! 输出必须满足 Prometheus 文本格式：每个指标族一对 `# HELP` / `# TYPE`，标签值
//! 需转义，且同一族的样本要**有序**（DashMap 遍历序不稳定，故先 `sort`）。
//!
//! `render()` 由 7 个连续输出段组成，段序就是 Prometheus exposition 的行序；每段
//! 负责一组相邻指标族。整篇输出由黄金快照逐字节锁定，格式结构另有回归断言。

use super::*;

impl MetricsRegistry {
    /// Render Prometheus text exposition (cumulative buckets).
    ///
    /// **本函数只负责"按序调用各段渲染器"**：原先它是 538 行的逐族 `push_str`，
    /// 改一个指标要在 500 行里定位。现按**输出顺序**切成 7 段，每段一个私有方法。
    ///
    /// 段序即输出序，**不可随意调整**：Prometheus 抓取端对同一族样本重复敏感，
    /// 且 `tests.rs` 的黄金快照逐字节断言整篇输出。要调整顺序必须同时更新快照，
    /// 并确认 `docs/OPERATION.md` 与告警规则不依赖行序。
    pub fn render(&self) -> String {
        let mut out = String::with_capacity(1024);
        self.render_requests_and_probe(&mut out);
        self.render_connect(&mut out);
        self.render_latency(&mut out);
        self.render_breaker(&mut out);
        self.render_telemetry(&mut out);
        self.render_free_pool_supply(&mut out);
        self.render_geoip(&mut out);
        self.render_free_pool_funnel(&mut out);
        out
    }

    /// 请求计数与探测结果（status 类、per-provider 403/402/429、桥错误、隔离水位、主动探测）。
    ///
    /// 见 [`Self::render`]：段序即输出序，调整会改变抓取端看到的行序。
    fn render_requests_and_probe(&self, out: &mut String) {
        out.push_str("# HELP proxy_requests_total Proxied responses by status class.\n");
        out.push_str("# TYPE proxy_requests_total counter\n");
        out.push_str(&format!(
            "proxy_requests_total{{status=\"2xx\"}} {}\n",
            self.req_2xx.load(Ordering::Relaxed)
        ));
        out.push_str(&format!(
            "proxy_requests_total{{status=\"4xx\"}} {}\n",
            self.req_4xx.load(Ordering::Relaxed)
        ));
        out.push_str(&format!(
            "proxy_requests_total{{status=\"5xx\"}} {}\n",
            self.req_5xx.load(Ordering::Relaxed)
        ));
        out.push_str(&format!(
            "proxy_requests_total{{status=\"other\"}} {}\n",
            self.req_other.load(Ordering::Relaxed)
        ));
        // 按 (status_class, provider) 输出 per-provider 请求计数，供
        // `sum by (provider)(rate(proxy_requests_total{status="2xx"}[5m]))` 等查询。
        // provider 基数有限（付费商 + free），不触发标签基数爆炸。
        let mut reqs: Vec<((String, String), u64)> = self
            .req_by_provider
            .iter()
            .map(|e| {
                (
                    (e.key().0.clone(), e.key().1.clone()),
                    e.value().load(Ordering::Relaxed),
                )
            })
            .collect();
        reqs.sort();
        for ((status_class, provider), n) in reqs {
            out.push_str(&format!(
                "proxy_requests_total{{status=\"{status_class}\", provider=\"{provider}\"}} {n}\n"
            ));
        }
        out.push_str("# HELP proxy_requests_forbidden_total 403 responses by provider.\n");
        out.push_str("# TYPE proxy_requests_forbidden_total counter\n");
        let mut providers: Vec<(String, u64)> = self
            .forbidden_by_provider
            .iter()
            .map(|e| (e.key().clone(), e.value().load(Ordering::Relaxed)))
            .collect();
        providers.sort();
        for (p, n) in providers {
            out.push_str(&format!(
                "proxy_requests_forbidden_total{{provider=\"{p}\"}} {n}\n"
            ));
        }
        // NEXT-A4：欠费/限流独立计数＋桥失败＋隔离水位（Dashboard 巡检用）。
        out.push_str("# HELP proxy_requests_unpaid_total Tenant out-of-balance (402) responses.\n");
        out.push_str("# TYPE proxy_requests_unpaid_total counter\n");
        out.push_str(&format!(
            "proxy_requests_unpaid_total {}\n",
            self.req_402.load(Ordering::Relaxed)
        ));
        out.push_str(
            "# HELP proxy_requests_limited_total Rate/concurrency limited (429) responses.\n",
        );
        out.push_str("# TYPE proxy_requests_limited_total counter\n");
        out.push_str(&format!(
            "proxy_requests_limited_total {}\n",
            self.req_429.load(Ordering::Relaxed)
        ));
        out.push_str("# HELP socks_bridge_errors_total SOCKS bridge egress failures.\n");
        out.push_str("# TYPE socks_bridge_errors_total counter\n");
        out.push_str(&format!(
            "socks_bridge_errors_total {}\n",
            self.bridge_errors.load(Ordering::Relaxed)
        ));
        out.push_str("# HELP quarantine_nodes Quarantined domain:ip entries in memory.\n");
        out.push_str("# TYPE quarantine_nodes gauge\n");
        out.push_str(&format!(
            "quarantine_nodes {}\n",
            self.quarantine_nodes.load(Ordering::Relaxed)
        ));
        // 步骤 54：主动健康探测结果与摘除水位。三行 counter 固定渲染（含 0），
        // 保证 Dashboard/告警在首次探测前也有稳定序列。
        out.push_str("# HELP active_probe_results_total Active health probe results by outcome.\n");
        out.push_str("# TYPE active_probe_results_total counter\n");
        for result in ["healthy", "degraded", "dead"] {
            let n = self
                .active_probe_results
                .get(result)
                .map(|v| v.value().load(Ordering::Relaxed))
                .unwrap_or(0);
            out.push_str(&format!(
                "active_probe_results_total{{result=\"{result}\"}} {n}\n"
            ));
        }
        out.push_str(
            "# HELP active_probe_unhealthy_nodes Nodes currently removed by active health.\n",
        );
        out.push_str("# TYPE active_probe_unhealthy_nodes gauge\n");
        out.push_str(&format!(
            "active_probe_unhealthy_nodes {}\n",
            self.active_probe_unhealthy.load(Ordering::Relaxed)
        ));
    }

    /// CONNECT 隧道观测：协议、终止原因、并发、方向字节和寿命。
    fn render_connect(&self, out: &mut String) {
        out.push_str("# HELP connect_tunnels_established_total CONNECT tunnels established by upstream protocol.\n");
        out.push_str("# TYPE connect_tunnels_established_total counter\n");
        let mut established: Vec<_> = self
            .connect_tunnels_established
            .iter()
            .map(|entry| (entry.key().clone(), entry.value().load(Ordering::Relaxed)))
            .collect();
        established.sort();
        for (proto, count) in established {
            out.push_str(&format!(
                "connect_tunnels_established_total{{proto=\"{proto}\"}} {count}\n"
            ));
        }

        out.push_str(
            "# HELP connect_tunnel_terminated_total CONNECT tunnel terminations by reason.\n",
        );
        out.push_str("# TYPE connect_tunnel_terminated_total counter\n");
        let mut terminated: Vec<_> = self
            .connect_tunnel_terminated
            .iter()
            .map(|entry| (entry.key().clone(), entry.value().load(Ordering::Relaxed)))
            .collect();
        terminated.sort();
        for (reason, count) in terminated {
            out.push_str(&format!(
                "connect_tunnel_terminated_total{{reason=\"{reason}\"}} {count}\n"
            ));
        }

        out.push_str("# HELP connect_tunnel_active Current CONNECT connections.\n");
        out.push_str("# TYPE connect_tunnel_active gauge\n");
        out.push_str(&format!(
            "connect_tunnel_active {}\n",
            self.connect_active.load(Ordering::Relaxed)
        ));
        out.push_str(
            "# HELP connect_admission_rejected_total CONNECT admission limit rejections.\n",
        );
        out.push_str("# TYPE connect_admission_rejected_total counter\n");
        out.push_str(&format!(
            "connect_admission_rejected_total {}\n",
            self.connect_admission_rejected.load(Ordering::Relaxed)
        ));
        out.push_str("# HELP connect_bytes_total CONNECT bytes by direction.\n");
        out.push_str("# TYPE connect_bytes_total counter\n");
        out.push_str(&format!(
            "connect_bytes_total{{direction=\"upstream\"}} {}\n",
            self.connect_bytes_up.load(Ordering::Relaxed)
        ));
        out.push_str(&format!(
            "connect_bytes_total{{direction=\"downstream\"}} {}\n",
            self.connect_bytes_down.load(Ordering::Relaxed)
        ));
        out.push_str(
            "# HELP connect_tunnel_duration_ms_sum CONNECT tunnel duration sum in milliseconds.\n",
        );
        out.push_str("# TYPE connect_tunnel_duration_ms_sum counter\n");
        out.push_str(&format!(
            "connect_tunnel_duration_ms_sum {}\n",
            self.connect_tunnel_duration_ms.load(Ordering::Relaxed)
        ));
        out.push_str("# HELP connect_tunnel_closed_total CONNECT tunnels closed.\n");
        out.push_str("# TYPE connect_tunnel_closed_total counter\n");
        out.push_str(&format!(
            "connect_tunnel_closed_total {}\n",
            self.connect_tunnel_closed.load(Ordering::Relaxed)
        ));
    }

    ///
    /// 见 [`Self::render`]：段序即输出序，调整会改变抓取端看到的行序。
    fn render_latency(&self, out: &mut String) {
        out.push_str("# HELP gateway_transferred_bytes_total Egress bytes metered.\n");
        out.push_str("# TYPE gateway_transferred_bytes_total counter\n");
        out.push_str(&format!(
            "gateway_transferred_bytes_total {}\n",
            self.bytes_total.load(Ordering::Relaxed)
        ));
        out.push_str("# HELP gateway_processing_duration_ms Gateway overhead histogram.\n");
        out.push_str("# TYPE gateway_processing_duration_ms histogram\n");
        // R2-6：写侧只存单桶原始值，此处前缀累加成 Prometheus 累计桶。
        let mut cumulative = 0u64;
        for (i, bound) in DURATION_BUCKETS_MS.iter().enumerate() {
            cumulative += self.duration_buckets[i].load(Ordering::Relaxed);
            out.push_str(&format!(
                "gateway_processing_duration_ms_bucket{{le=\"{bound}\"}} {cumulative}\n"
            ));
        }
        let count = self.duration_count.load(Ordering::Relaxed);
        out.push_str(&format!(
            "gateway_processing_duration_ms_bucket{{le=\"+Inf\"}} {count}\n"
        ));
        out.push_str(&format!("gateway_processing_duration_ms_count {count}\n"));
        out.push_str(&format!(
            "gateway_processing_duration_ms_sum {}\n",
            self.duration_sum_ms.load(Ordering::Relaxed)
        ));
        // GW-6：五阶段耗时直方图（stage 标签区分 connect/ttfb/transfer；
        // tls 握手暂并入 connect，Pingora 不单独暴露，后续可细化）。
        //
        // **HELP/TYPE 必须在循环外、只写一次**：Prometheus 文本格式规定同一指标名
        // 只能有一条 `# HELP`，`promtool check metrics` 对重复者直接报
        // `text format parsing error: second HELP line for metric name`。
        // 此处原先把它写在 per-stage 循环**内部**，于是 3 个 stage 输出 3 份 ——
        // 后果不是"少一个指标"，而是**抓取端解析失败、整次抓取作废**（该实例
        // 所有指标一起丢）。实测证据：把 `render()` 的输出喂给
        // `promtool check metrics` 会在此处报错（2026-11-01 复现）。
        out.push_str("# HELP proxy_stage_duration_ms Per-stage gateway overhead histogram (ms).\n");
        out.push_str("# TYPE proxy_stage_duration_ms histogram\n");
        for (stage, buckets, count, sum) in [
            (
                "connect",
                &self.stage_connect_buckets,
                &self.stage_connect_count,
                &self.stage_connect_sum_ms,
            ),
            (
                "ttfb",
                &self.stage_ttfb_buckets,
                &self.stage_ttfb_count,
                &self.stage_ttfb_sum_ms,
            ),
            (
                "transfer",
                &self.stage_transfer_buckets,
                &self.stage_transfer_count,
                &self.stage_transfer_sum_ms,
            ),
        ] {
            let mut cumulative = 0u64;
            for (i, bound) in DURATION_BUCKETS_MS.iter().enumerate() {
                cumulative += buckets[i].load(Ordering::Relaxed);
                out.push_str(&format!(
                    "proxy_stage_duration_ms_bucket{{stage=\"{stage}\",le=\"{bound}\"}} {cumulative}\n"
                ));
            }
            let n = count.load(Ordering::Relaxed);
            out.push_str(&format!(
                "proxy_stage_duration_ms_bucket{{stage=\"{stage}\",le=\"+Inf\"}} {n}\n"
            ));
            out.push_str(&format!(
                "proxy_stage_duration_ms_count{{stage=\"{stage}\"}} {n}\n"
            ));
            out.push_str(&format!(
                "proxy_stage_duration_ms_sum{{stage=\"{stage}\"}} {}\n",
                sum.load(Ordering::Relaxed)
            ));
        }
        // GW-6：断路器触发计数（per status：429/403/502/504）。
    }

    /// 熔断触发计数、LinUCB 强制探索、隔离域数。
    ///
    /// 见 [`Self::render`]：段序即输出序，调整会改变抓取端看到的行序。
    fn render_breaker(&self, out: &mut String) {
        out.push_str("# HELP breaker_trigger_total Circuit breaker trigger count by status.\n");
        out.push_str("# TYPE breaker_trigger_total counter\n");
        let mut triggers: Vec<(String, u64)> = self
            .breaker_trigger
            .iter()
            .map(|e| (e.key().clone(), e.value().load(Ordering::Relaxed)))
            .collect();
        triggers.sort();
        for (s, n) in triggers {
            out.push_str(&format!("breaker_trigger_total{{status=\"{s}\"}} {n}\n"));
        }
        // GW-6：LinUCB 分档强制探索总次数。
        out.push_str("# HELP bandit_exploration_total LinUCB forced tier exploration count.\n");
        out.push_str("# TYPE bandit_exploration_total counter\n");
        out.push_str(&format!(
            "bandit_exploration_total {}\n",
            self.bandit_exploration.load(Ordering::Relaxed)
        ));
        // GW-6：隔离域数（外层 domain 数，区别于 quarantine_nodes 的内层 ip 数）。
        out.push_str("# HELP quarantine_domains_total Quarantined domain count (outer level).\n");
        out.push_str("# TYPE quarantine_domains_total gauge\n");
        out.push_str(&format!(
            "quarantine_domains_total {}\n",
            self.quarantine_domains.load(Ordering::Relaxed)
        ));
        // OPT-4：落库重试一次仍失败的丢弃事件总数（常驻 0 行，便于 PromQL 告警）。
    }

    /// 遥测管道（丢弃、sink 可达性/失败/最后成功时刻）、后台工人重启、日志采样。
    ///
    /// 见 [`Self::render`]：段序即输出序，调整会改变抓取端看到的行序。
    fn render_telemetry(&self, out: &mut String) {
        out.push_str("# HELP telemetry_dropped_total Telemetry events dropped after one retry.\n");
        out.push_str("# TYPE telemetry_dropped_total counter\n");
        out.push_str(&format!(
            "telemetry_dropped_total {}\n",
            self.telemetry_dropped.load(Ordering::Relaxed)
        ));
        // R2-4：通道满/关闭丢弃总数（常驻 0 行；与落库口径分离，归因不混）。
        out.push_str("# HELP telemetry_channel_dropped_total Telemetry events dropped on a full/closed channel.\n");
        out.push_str("# TYPE telemetry_channel_dropped_total counter\n");
        out.push_str(&format!(
            "telemetry_channel_dropped_total {}\n",
            self.channel_dropped.load(Ordering::Relaxed)
        ));
        // OPT-R15：落地端健康三件套。缺哪级 backend 的序列就渲染哪级——未配置的落地端
        // 不出现（而非渲染成 `up=0` 误报）；告警口径见 docs/OPERATION.md。
        out.push_str(
            "# HELP telemetry_sink_up Telemetry sink reachable (1) or failing (0), last attempt.\n",
        );
        out.push_str("# TYPE telemetry_sink_up gauge\n");
        let mut ups: Vec<(String, u64)> = self
            .sink_up
            .iter()
            .map(|e| (e.key().clone(), e.value().load(Ordering::Relaxed)))
            .collect();
        ups.sort();
        for (b, v) in ups {
            out.push_str(&format!("telemetry_sink_up{{backend=\"{b}\"}} {v}\n"));
        }
        out.push_str(
            "# HELP telemetry_sink_failures_total Telemetry sink write attempts that failed.\n",
        );
        out.push_str("# TYPE telemetry_sink_failures_total counter\n");
        let mut fails: Vec<(String, u64)> = self
            .sink_failures
            .iter()
            .map(|e| (e.key().clone(), e.value().load(Ordering::Relaxed)))
            .collect();
        fails.sort();
        for (b, v) in fails {
            out.push_str(&format!(
                "telemetry_sink_failures_total{{backend=\"{b}\"}} {v}\n"
            ));
        }
        out.push_str(
            "# HELP telemetry_sink_last_ok_unixtime_seconds Unix time of last successful sink write (0 = never).\n",
        );
        out.push_str("# TYPE telemetry_sink_last_ok_unixtime_seconds gauge\n");
        let mut oks: Vec<(String, u64)> = self
            .sink_last_ok
            .iter()
            .map(|e| (e.key().clone(), e.value().load(Ordering::Relaxed)))
            .collect();
        oks.sort();
        for (b, v) in oks {
            out.push_str(&format!(
                "telemetry_sink_last_ok_unixtime_seconds{{backend=\"{b}\"}} {v}\n"
            ));
        }
        // R2-8：后台 worker 重启数（supervisor 计数；无重启时无线，保持 exposition 干净）。
        out.push_str("# HELP supervisor_restarts_total Background worker restarts by worker.\n");
        out.push_str("# TYPE supervisor_restarts_total counter\n");
        let mut workers: Vec<(String, u64)> = self
            .supervisor_restarts
            .iter()
            .map(|e| (e.key().clone(), e.value().load(Ordering::Relaxed)))
            .collect();
        workers.sort();
        for (w, n) in workers {
            out.push_str(&format!(
                "supervisor_restarts_total{{worker=\"{w}\"}} {n}\n"
            ));
        }
        // R2-8：被采样掉的非 5xx 数据面日志数（常驻行，采样本身可观测）。
        out.push_str("# HELP gateway_logs_sampled_total Data-plane log lines sampled to debug.\n");
        out.push_str("# TYPE gateway_logs_sampled_total counter\n");
        out.push_str(&format!(
            "gateway_logs_sampled_total {}\n",
            self.logs_sampled.load(Ordering::Relaxed)
        ));
    }

    /// 免费线供给：节点数、各源产出/Elite、质检分布、转化率、出口多样性、年龄、匿名度、源暂停、协议分布。
    ///
    /// 见 [`Self::render`]：段序即输出序，调整会改变抓取端看到的行序。
    fn render_free_pool_supply(&self, out: &mut String) {
        out.push_str(
            "# HELP free_pool_nodes_total Free-tier nodes currently merged into the pool.\n",
        );
        out.push_str("# TYPE free_pool_nodes_total gauge\n");
        out.push_str(&format!(
            "free_pool_nodes_total {}\n",
            self.free_pool_nodes.load(Ordering::Relaxed)
        ));
        // FreePool per-source 抓取产出（label 源名内部生成，无引号；调用方保证集合）。
        out.push_str("# HELP free_pool_source_yield_total FreePool fetched nodes by source.\n");
        out.push_str("# TYPE free_pool_source_yield_total counter\n");
        let mut yields: Vec<(String, u64)> = self
            .free_source_yield
            .iter()
            .map(|e| (e.key().clone(), e.value().load(Ordering::Relaxed)))
            .collect();
        yields.sort();
        for (s, n) in yields {
            out.push_str(&format!(
                "free_pool_source_yield_total{{source=\"{s}\"}} {n}\n"
            ));
        }
        // NEXT-B5：per-source Elite（只在有数据时出 HELP/TYPE＋行，保持干净）。
        if !self.free_source_elite.is_empty() {
            out.push_str("# HELP free_pool_source_elite_total FreePool Elite nodes by source.\n");
            out.push_str("# TYPE free_pool_source_elite_total counter\n");
            let mut elites: Vec<(String, u64)> = self
                .free_source_elite
                .iter()
                .map(|e| (e.key().clone(), e.value().load(Ordering::Relaxed)))
                .collect();
            elites.sort();
            for (s, n) in elites {
                out.push_str(&format!(
                    "free_pool_source_elite_total{{source=\"{s}\"}} {n}\n"
                ));
            }
        }
        // FreePool 质检结果分布。
        out.push_str("# HELP free_pool_verify_total FreePool verify outcomes by result.\n");
        out.push_str("# TYPE free_pool_verify_total counter\n");
        let mut verifies: Vec<(String, u64)> = self
            .free_verify
            .iter()
            .map(|e| (e.key().clone(), e.value().load(Ordering::Relaxed)))
            .collect();
        verifies.sort();
        for (r, n) in verifies {
            out.push_str(&format!("free_pool_verify_total{{result=\"{r}\"}} {n}\n"));
        }
        // OPT-R18 P2-3：供给 SLO —— 三个只有"供给视角"才能回答的问题。
        //
        // 已有的 `free_pool_nodes_total` 只说"池里有几个节点"，回答不了：
        // ① 这些节点**还能活多久**（年龄分布）② 出口**多样性**够不够
        // ③ 抓来的原始代理**有多少能过质检**（转化率）。
        // 免费池的失效方式恰恰是这三种，而不是"节点数变少"。
        let (pass, total) = self.free_verify_totals();
        // 转化率：pass / 全部质检结果。分母为 0 时渲染 NaN 而不是 0——0 会被
        // 读成"转化率极差"，而真相是"还没有样本"。Prometheus 里 NaN 参与任何
        // 比较都是 false，故基于它的告警在无样本时**不会**误报。
        out.push_str(
            "# HELP free_pool_yield_ratio Share of verified free proxies that passed quality gates.\n",
        );
        out.push_str("# TYPE free_pool_yield_ratio gauge\n");
        if total == 0 {
            out.push_str("free_pool_yield_ratio NaN\n");
        } else {
            out.push_str(&format!(
                "free_pool_yield_ratio {}\n",
                pass as f64 / total as f64
            ));
        }
        // 出口多样性：池内**不同出口身份**的个数（exit_ip 优先，未知回落 ip）。
        // 它与 nodes_total 的差值就是"多个节点共用一个出口"的程度——共用出口
        // 时"节点数很多"是假象，WAF 侧看到的只有一个 IP。
        out.push_str(
            "# HELP free_pool_distinct_exit_ips Distinct egress identities among pooled free nodes.\n",
        );
        out.push_str("# TYPE free_pool_distinct_exit_ips gauge\n");
        out.push_str(&format!(
            "free_pool_distinct_exit_ips {}\n",
            self.free_pool_distinct_exits.load(Ordering::Relaxed)
        ));
        // 年龄中位数：池内节点的"已存活时长"中位数。空池渲染 NaN（不是 0）——
        // 0 会被读成"刚进池"，与"池是空的"正好相反。
        out.push_str("# HELP free_pool_median_age_seconds Median age of pooled free nodes.\n");
        out.push_str("# TYPE free_pool_median_age_seconds gauge\n");
        let age_ms = self.free_pool_median_age_ms.load(Ordering::Relaxed);
        if age_ms < 0 {
            out.push_str("free_pool_median_age_seconds NaN\n");
        } else {
            out.push_str(&format!(
                "free_pool_median_age_seconds {}\n",
                age_ms as f64 / 1000.0
            ));
        }
        // FreePool 匿名度分级分布。
        out.push_str("# HELP free_pool_anonymity_total FreePool anonymity levels by level.\n");
        out.push_str("# TYPE free_pool_anonymity_total counter\n");
        let mut anons: Vec<(String, u64)> = self
            .free_anonymity
            .iter()
            .map(|e| (e.key().clone(), e.value().load(Ordering::Relaxed)))
            .collect();
        anons.sort();
        for (l, n) in anons {
            out.push_str(&format!("free_pool_anonymity_total{{level=\"{l}\"}} {n}\n"));
        }
        // FreePool 源站熔断状态（0/1 gauge；无数据时无线，保持 exposition 干净）。
        out.push_str(
            "# HELP free_pool_source_suspended FreePool source suspended flag by source.\n",
        );
        out.push_str("# TYPE free_pool_source_suspended gauge\n");
        let mut susps: Vec<(String, u64)> = self
            .free_suspended
            .iter()
            .map(|e| (e.key().clone(), e.value().load(Ordering::Relaxed)))
            .collect();
        susps.sort();
        for (s, n) in susps {
            out.push_str(&format!(
                "free_pool_source_suspended{{source=\"{s}\"}} {n}\n"
            ));
        }
        // P2 FreePool 按出站协议水位（merge 后三档恒设；proto 白名单 http/socks5/socks4）。
        out.push_str("# HELP free_pool_nodes_by_proto FreePool merged nodes by egress proto.\n");
        out.push_str("# TYPE free_pool_nodes_by_proto gauge\n");
        let mut protos: Vec<(String, u64)> = self
            .free_nodes_by_proto
            .iter()
            .map(|e| (e.key().clone(), e.value().load(Ordering::Relaxed)))
            .collect();
        protos.sort();
        for (p, n) in protos {
            out.push_str(&format!("free_pool_nodes_by_proto{{proto=\"{p}\"}} {n}\n"));
        }
    }

    /// GeoIP 观察分布与实锤分歧。
    ///
    /// 见 [`Self::render`]：段序即输出序，调整会改变抓取端看到的行序。
    fn render_geoip(&self, out: &mut String) {
        // P3 GeoIP 观察分布＋实锤分歧（无数据时只 HELP/TYPE，沿 suspend 惯例保持干净）。
        out.push_str("# HELP geoip_lookups_total GeoIP exit-country lookups by result.\n");
        out.push_str("# TYPE geoip_lookups_total counter\n");
        let mut geos: Vec<(String, u64)> = self
            .geo_lookups
            .iter()
            .map(|e| (e.key().clone(), e.value().load(Ordering::Relaxed)))
            .collect();
        geos.sort();
        for (r, n) in geos {
            out.push_str(&format!("geoip_lookups_total{{result=\"{r}\"}} {n}\n"));
        }
        out.push_str("# HELP geoip_mismatch_total GeoIP exit-country hard mismatches.\n");
        out.push_str("# TYPE geoip_mismatch_total counter\n");
        out.push_str(&format!(
            "geoip_mismatch_total {}\n",
            self.geo_mismatch.load(Ordering::Relaxed)
        ));
    }

    /// 免费线漏斗与出口分布：intake 截断、基线失败、容量淘汰、per-source 耗时、单节点转发延迟、exit TopK（含溢出桶）。
    ///
    /// 见 [`Self::render`]：段序即输出序，调整会改变抓取端看到的行序。
    fn render_free_pool_funnel(&self, out: &mut String) {
        // OPT-R4 B9：免费漏斗四指标（常驻 0 行，PromQL 分母完整；按源耗时 sum+count 配对）。
        out.push_str(
            "# HELP free_pool_intake_capped_total FreePool intake nodes dropped by cap.\n",
        );
        out.push_str("# TYPE free_pool_intake_capped_total counter\n");
        out.push_str(&format!(
            "free_pool_intake_capped_total {}\n",
            self.free_intake_capped.load(Ordering::Relaxed)
        ));
        out.push_str("# HELP free_pool_baseline_fail_total FreePool ticks degraded on baseline unreachable.\n");
        out.push_str("# TYPE free_pool_baseline_fail_total counter\n");
        out.push_str(&format!(
            "free_pool_baseline_fail_total {}\n",
            self.free_baseline_fail.load(Ordering::Relaxed)
        ));
        out.push_str("# HELP free_pool_evicted_total FreePool nodes evicted over capacity.\n");
        out.push_str("# TYPE free_pool_evicted_total counter\n");
        out.push_str(&format!(
            "free_pool_evicted_total {}\n",
            self.free_evicted.load(Ordering::Relaxed)
        ));
        out.push_str(
            "# HELP free_pool_source_fetch_ms_sum FreePool per-source fetch latency sum in ms.\n",
        );
        out.push_str("# TYPE free_pool_source_fetch_ms_sum counter\n");
        let mut fetch_sums: Vec<(String, u64)> = self
            .free_fetch_ms_sum
            .iter()
            .map(|e| (e.key().clone(), e.value().load(Ordering::Relaxed)))
            .collect();
        fetch_sums.sort();
        for (s, n) in fetch_sums {
            out.push_str(&format!(
                "free_pool_source_fetch_ms_sum{{source=\"{s}\"}} {n}\n"
            ));
        }
        out.push_str("# HELP free_pool_source_fetch_ms_count FreePool per-source fetch rounds.\n");
        out.push_str("# TYPE free_pool_source_fetch_ms_count counter\n");
        let mut fetch_counts: Vec<(String, u64)> = self
            .free_fetch_count
            .iter()
            .map(|e| (e.key().clone(), e.value().load(Ordering::Relaxed)))
            .collect();
        fetch_counts.sort();
        for (s, n) in fetch_counts {
            out.push_str(&format!(
                "free_pool_source_fetch_ms_count{{source=\"{s}\"}} {n}\n"
            ));
        }
        // OPT-R5 E5：单节点转发延迟直方图（桶口径复用 DURATION_BUCKETS_MS；
        // 写侧单原子＋此处前缀累加成累计桶，渲染值恒单调）。
        out.push_str(
            "# HELP free_pool_fwd_latency_ms FreePool single-node forward latency in ms.\n",
        );
        out.push_str("# TYPE free_pool_fwd_latency_ms histogram\n");
        let mut fwd_cumulative = 0u64;
        for (i, bound) in DURATION_BUCKETS_MS.iter().enumerate() {
            fwd_cumulative += self.free_fwd_buckets[i].load(Ordering::Relaxed);
            out.push_str(&format!(
                "free_pool_fwd_latency_ms_bucket{{le=\"{bound}\"}} {fwd_cumulative}\n"
            ));
        }
        let fwd_count = self.free_fwd_count.load(Ordering::Relaxed);
        out.push_str(&format!(
            "free_pool_fwd_latency_ms_bucket{{le=\"+Inf\"}} {fwd_count}\n"
        ));
        out.push_str(&format!("free_pool_fwd_latency_ms_count {fwd_count}\n"));
        out.push_str(&format!(
            "free_pool_fwd_latency_ms_sum {}\n",
            self.free_fwd_sum_ms.load(Ordering::Relaxed)
        ));
        // OPT-R5 E5：出口分组计数（轮转不在此处算状态机，由 PromQL
        // `changes(free_pool_exit_total[5m])` 看轮转；无数据时只 HELP/TYPE 无行）。
        out.push_str("# HELP free_pool_exit_total FreePool forward passes by exit ip.\n");
        out.push_str("# TYPE free_pool_exit_total counter\n");
        let mut exits: Vec<(String, u64)> = self
            .free_exit_total
            .iter()
            .map(|e| (e.key().clone(), e.value().load(Ordering::Relaxed)))
            .collect();
        exits.sort();
        // OPT-R9 C2：溢出桶**恒渲染**（未溢出时值为 0），使面板/告警可判
        // 「标签基数上限是否已被触达」。放最后一行，便于人眼在长列表末尾看到。
        let overflow = exits
            .iter()
            .find(|(k, _)| k == EXIT_OVERFLOW_LABEL)
            .map(|(_, v)| *v);
        if let Some(v) = overflow {
            exits.retain(|(k, _)| k != EXIT_OVERFLOW_LABEL);
            for (ip, n) in exits {
                out.push_str(&format!("free_pool_exit_total{{exit_ip=\"{ip}\"}} {n}\n"));
            }
            out.push_str(&format!(
                "free_pool_exit_total{{exit_ip=\"{EXIT_OVERFLOW_LABEL}\"}} {v}\n"
            ));
        } else {
            for (ip, n) in exits {
                out.push_str(&format!("free_pool_exit_total{{exit_ip=\"{ip}\"}} {n}\n"));
            }
            // 未触达上限：仍渲染 0 值，让「上限已触达」这一状态可被主动查询，
            // 而不必依赖「序列是否出现」这种间接推断。
            out.push_str(&format!(
                "free_pool_exit_total{{exit_ip=\"{EXIT_OVERFLOW_LABEL}\"}} 0\n"
            ));
        }
    }
}

/// Serve `GET /metrics` until process exit (minimal HTTP, no new deps).
///
/// R2-8 加固：读超时 5s（慢连接不占 worker）+ 在途连接超 64 即关（零新依赖
/// 的并发上限；Prom 抓取低频）+ 单连接 body 上限沿用 1024。
pub async fn serve_metrics(registry: Arc<MetricsRegistry>, addr: &str) {
    let listener = match tokio::net::TcpListener::bind(addr).await {
        Ok(l) => l,
        Err(e) => {
            log::error!("[Metrics] bind {addr} failed: {e:?}");
            return;
        }
    };
    log::info!("[Metrics] exposition on http://{addr}/metrics");
    let inflight = Arc::new(AtomicUsize::new(0));
    loop {
        let (mut stream, _) = match listener.accept().await {
            Ok(pair) => pair,
            Err(e) => {
                log::debug!("[Metrics] accept failed: {e:?}");
                continue;
            }
        };
        if inflight.fetch_add(1, Ordering::SeqCst) >= METRICS_MAX_CONCURRENT {
            inflight.fetch_sub(1, Ordering::SeqCst);
            log::debug!("[Metrics] over connection cap, closing");
            continue;
        }
        let registry = registry.clone();
        let inflight = inflight.clone();
        tokio::spawn(async move {
            use tokio::io::{AsyncReadExt, AsyncWriteExt};
            let mut buf = [0u8; 1024];
            let n = match tokio::time::timeout(METRICS_READ_TIMEOUT, stream.read(&mut buf)).await {
                Ok(Ok(n)) => n,
                _ => {
                    inflight.fetch_sub(1, Ordering::SeqCst);
                    return;
                }
            };
            let head = String::from_utf8_lossy(&buf[..n]);
            let (code, reason, body) = if head.starts_with("GET /metrics") {
                ("200", "OK", registry.render())
            } else {
                ("404", "Not Found", "not found\n".to_string())
            };
            let _ = stream
                .write_all(
                    format!(
                        "HTTP/1.1 {code} {reason}\r\ncontent-type: text/plain; version=0.0.4\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                        body.len()
                    )
                    .as_bytes(),
                )
                .await;
            inflight.fetch_sub(1, Ordering::SeqCst);
        });
    }
}
