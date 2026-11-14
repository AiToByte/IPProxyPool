//! `metrics` 全部单测。
//!
//! **约定归一**：原文件里有两套写法——`mod tests { ... }` 里的 40 个，以及
//! 模块级的 4 个 `opt_r9_c2_*`（`#[test]` 直接写在模块顶层）。两者都合法
//! （`#[test]` 隐含 `cfg(test)` 门控），但同一个文件两套约定会让读者以为
//! "模块级那些是特殊测试"。本轮拆文件时统一收进本文件，不再分两处。

use super::*;

// ---- OPT-R9 C2：无界标签基数护栏（溢出桶）回归锁定 ----

/// 未超上限时，渲染输出与旧实现**逐字节一致**（向后兼容保证）。
/// 只多了一行 `__overflow__ 0`（恒渲染，用于判「上限是否已触达」）。
#[test]
fn opt_r9_c2_rendering_unchanged_below_cap() {
    let m = MetricsRegistry::new();
    m.note_free_exit("1.2.3.4");
    m.note_free_exit("1.2.3.4");
    m.note_free_exit("5.6.7.8");
    let text = m.render();
    // 既有序列形态不变。
    assert!(text.contains("free_pool_exit_total{exit_ip=\"1.2.3.4\"} 2"));
    assert!(text.contains("free_pool_exit_total{exit_ip=\"5.6.7.8\"} 1"));
    // 新增一行 0 值溢出桶（这是本轮唯一的新增行）。
    assert!(text.contains("free_pool_exit_total{exit_ip=\"__overflow__\"} 0"));
}

/// 超上限后：新出口并入溢出桶，**基数被真正限制住**。
#[test]
fn opt_r9_c2_overflow_bucket_absorbs_beyond_cap() {
    let m = MetricsRegistry::new();
    for i in 0..EXIT_LABEL_CAP {
        m.note_free_exit(&format!("10.0.{}.{}", i / 256, i % 256));
    }
    let text = m.render();
    assert!(
        text.contains("free_pool_exit_total{exit_ip=\"__overflow__\"} 0"),
        "恰好用满上限时不应溢出"
    );

    // 第 513 个出口开始进溢出桶。
    for i in 0..5 {
        m.note_free_exit(&format!("192.168.{i}.1"));
    }
    let text = m.render();
    assert!(
        text.contains("free_pool_exit_total{exit_ip=\"__overflow__\"} 5"),
        "超出上限的 5 次应全部计入溢出桶"
    );
    // 真实序列基数未增长。
    let real = text
        .lines()
        .filter(|l| l.starts_with("free_pool_exit_total{"))
        .filter(|l| !l.contains("__overflow__"))
        .count();
    assert_eq!(real, EXIT_LABEL_CAP, "真实序列数必须被上限钳住");
}

/// **关键语义**：已达上限后，**已存在的**序列必须继续正常递增。
///
/// 旧的无上限实现没有这个场景；一个偷懒的实现会在 `len() >= CAP` 时
/// 无条件把新计数也塞进溢出桶，导致前 512 个出口的统计**静默冻结**——
/// 那比无界增长更难发现（数据看起来还在，只是不动了）。
#[test]
fn opt_r9_c2_existing_series_keep_counting_after_cap() {
    let m = MetricsRegistry::new();
    m.note_free_exit("1.1.1.1");
    // 用满上限。
    for i in 0..EXIT_LABEL_CAP - 1 {
        m.note_free_exit(&format!("10.0.{}.{}", i / 256, i % 256));
    }
    // 触达上限后再写已存在的序列。
    for _ in 0..100 {
        m.note_free_exit("1.1.1.1");
    }
    let text = m.render();
    assert!(
        text.contains("free_pool_exit_total{exit_ip=\"1.1.1.1\"} 101"),
        "已存在的序列在达上限后必须继续递增，不得静默冻结"
    );
}

/// 空串不记（存量语义保留）。
#[test]
fn opt_r9_c2_empty_exit_ip_not_counted() {
    let m = MetricsRegistry::new();
    m.note_free_exit("");
    let text = m.render();
    assert!(
        text.contains("free_pool_exit_total{exit_ip=\"__overflow__\"} 0"),
        "空串不得进溢出桶"
    );
    let real = text
        .lines()
        .filter(|l| l.starts_with("free_pool_exit_total{"))
        .filter(|l| !l.contains("__overflow__"))
        .count();
    assert_eq!(real, 0, "空串不得产生任何真实序列");
}

#[test]
fn counters_and_histogram_render() {
    let m = MetricsRegistry::new();
    m.observe(200, Some("mock-a"), 100, Duration::from_millis(3));
    m.observe(403, Some("mock-a"), 0, Duration::from_millis(1200));
    m.observe(502, Some("mock-b"), 50, Duration::from_millis(30));
    let text = m.render();
    assert!(text.contains("proxy_requests_total{status=\"2xx\"} 1"));
    assert!(text.contains("proxy_requests_total{status=\"4xx\"} 1"));
    assert!(text.contains("proxy_requests_total{status=\"5xx\"} 1"));
    // Phase 1：per-provider 计数，所有状态均携带 provider 标签。
    assert!(text.contains("proxy_requests_total{status=\"2xx\", provider=\"mock-a\"} 1"));
    assert!(text.contains("proxy_requests_total{status=\"4xx\", provider=\"mock-a\"} 1"));
    assert!(text.contains("proxy_requests_total{status=\"5xx\", provider=\"mock-b\"} 1"));
    // 403 专属计数保持不变。
    assert!(text.contains("proxy_requests_forbidden_total{provider=\"mock-a\"} 1"));
    assert!(text.contains("gateway_transferred_bytes_total 150"));
    // Cumulative buckets: 3ms lands in le>=5; 30ms in le>=50; 1200ms in le>=2500.
    assert!(text.contains("gateway_processing_duration_ms_bucket{le=\"1\"} 0"));
    assert!(text.contains("gateway_processing_duration_ms_bucket{le=\"5\"} 1"));
    assert!(text.contains("gateway_processing_duration_ms_bucket{le=\"50\"} 2"));
    assert!(text.contains("gateway_processing_duration_ms_bucket{le=\"+Inf\"} 3"));
    assert!(text.contains("gateway_processing_duration_ms_count 3"));
}

#[test]
fn auth_bridge_quarantine_signals_rendered() {
    // NEXT-A4：402/429 独立计数（仍进 4xx 总盘）＋桥失败＋隔离水位。
    let m = MetricsRegistry::new();
    m.observe(402, None, 0, Duration::from_millis(1));
    m.observe(429, None, 0, Duration::from_millis(1));
    m.observe(403, None, 0, Duration::from_millis(1));
    m.note_bridge_error();
    m.set_quarantine_nodes(3);
    let text = m.render();
    assert!(text.contains("proxy_requests_unpaid_total 1"));
    assert!(text.contains("proxy_requests_limited_total 1"));
    assert!(text.contains("proxy_requests_total{status=\"4xx\"} 3"));
    assert!(text.contains("socks_bridge_errors_total 1"));
    assert!(text.contains("quarantine_nodes 3"));
}

#[test]
fn histogram_buckets_render_monotonic() {
    // R2-6：混合延迟 + 超界值下渲染桶恒单调（单原子写 + 前缀累加的直接收益）；
    // 超最大桶只进 +Inf/count，不污染各 le 行。
    // OPT-R7 D2：桶数由 10 扩到 14，原断言里的 `9999`（当时超界）现落在
    // `le=10000` 桶内，故「超界」样本改用 `60001`（>60s 截断上界）。
    let m = MetricsRegistry::new();
    for ms in [0, 1, 3, 30, 1200, 2500, 9999, 30, 3] {
        m.observe(200, None, 0, Duration::from_millis(ms));
    }
    // 追加一条超 60s 截断的样本：只进 count/+Inf，各 le 桶不动。
    m.observe(200, None, 0, Duration::from_millis(600_000));
    let text = m.render();
    let mut last = 0u64;
    let mut seen = 0usize;
    for line in text.lines() {
        if let Some(rest) = line.strip_prefix("gateway_processing_duration_ms_bucket{le=\"") {
            let value: u64 = rest.rsplit(' ').next().unwrap().parse().unwrap();
            assert!(
                value >= last,
                "buckets must be monotonic: {value} after {last} ({line})"
            );
            last = value;
            seen += 1;
        }
    }
    assert_eq!(seen, DURATION_BUCKETS_MS.len() + 1, "14 le + Inf");
    assert!(text.contains("gateway_processing_duration_ms_bucket{le=\"+Inf\"} 10"));
    assert!(text.contains("gateway_processing_duration_ms_count 10"));
}

/// OPT-R7 D2 专项：桶边界语义。扩桶是为了让慢时段 P99 落在**真实桶**内，
/// 因此必须逐个边界验证「样本落在正确的桶」，否则扩桶本身可能引入错位。
#[test]
fn opt_r7_d2_bucket_boundaries_place_samples_correctly() {
    // 桶下界必须严格单调递增且不移动（Prometheus 直方图桶下界语义：
    // 第 i 个桶覆盖 (prev_le, le]；移动下界会让存量直方图历史数据不可比）。
    assert!(
        DURATION_BUCKETS_MS.windows(2).all(|w| w[0] < w[1]),
        "桶边界必须严格递增：{:?}",
        DURATION_BUCKETS_MS
    );
    // 前 10 个桶（下界）与扩桶前完全一致——扩桶是**只加尾部**的向后兼容加法。
    assert_eq!(
        &DURATION_BUCKETS_MS[..10],
        &[1u64, 5, 10, 25, 50, 100, 250, 500, 1000, 2500],
        "既有下界不得移动，否则存量直方图语义漂移"
    );
    // 尾部桶与 `observe` 的 60s 截断对齐：最大桶恰为 60_000。
    assert_eq!(
        DURATION_BUCKETS_MS[DURATION_BUCKETS_MS.len() - 1],
        60_000,
        "最大桶必须与 observe() 的 60s 截断一致"
    );

    // 逐边界验证：恰好等于桶界的样本落进该桶（`<=` 语义），渲染值正确。
    for (i, bound) in DURATION_BUCKETS_MS.iter().enumerate() {
        let m = MetricsRegistry::new();
        m.observe(200, None, 0, Duration::from_millis(*bound));
        let text = m.render();
        // 该样本必须出现在 le=bound 这一行（累积值 1）。
        let needle = format!("gateway_processing_duration_ms_bucket{{le=\"{bound}\"}} 1");
        assert!(
            text.contains(&needle),
            "le={bound} 应累计到 1，实际渲染缺该行"
        );
        // 且不落到更小的桶上：更大 le 的行也都是 1（累积），更小 le 的行应为 0。
        for smaller in &DURATION_BUCKETS_MS[..i] {
            let line = format!("gateway_processing_duration_ms_bucket{{le=\"{smaller}\"}} 0");
            assert!(
                text.contains(&line),
                "le={smaller} 不应累计（样本 {bound} 属 le={bound} 桶）"
            );
        }
    }
}

/// OPT-R7 D2 专项：慢时段 P99 不再外推。
/// 此前桶顶 2500ms，而超时预算就有 5000/3000ms——>2500ms 全落 `+Inf`，
/// `histogram_quantile` 只能外推。现桶覆盖到 60s，慢样本有真实桶可落。
#[test]
fn opt_r7_d2_slow_samples_land_in_real_buckets_not_inf() {
    // 3000ms 属 peer 超时预算内的慢请求：必须落在 le=5000 桶，不能是 +Inf。
    let m = MetricsRegistry::new();
    m.observe(200, None, 0, Duration::from_millis(3_000));
    let text = m.render();
    assert!(
        text.contains("gateway_processing_duration_ms_bucket{le=\"2500\"} 0"),
        "3000ms 不应计入 le=2500（>2500）"
    );
    assert!(
        text.contains("gateway_processing_duration_ms_bucket{le=\"5000\"} 1"),
        "3000ms 必须计入 le=5000 真实桶"
    );
    // `+Inf` 行是**累积**值，等于 `_count`（Prometheus 直方图语义），
    // 样本落在 le=5000 时它必然也是 1——这不是「只进了 +Inf」。
    // 真正要验证的是：样本在**最大桶之前**就落进了真实桶（上面的 le=5000）。
    assert!(
        text.contains("gateway_processing_duration_ms_bucket{le=\"+Inf\"} 1"),
        "+Inf 为累积值，应等于 count"
    );
    assert!(
        text.contains("gateway_processing_duration_ms_count 1"),
        "count 应为 1"
    );

    // 20s 级慢样本：落在 le=30000 真实桶，不需外推。
    let m2 = MetricsRegistry::new();
    m2.observe(200, None, 0, Duration::from_millis(20_000));
    let t2 = m2.render();
    assert!(
        t2.contains("gateway_processing_duration_ms_bucket{le=\"30000\"} 1"),
        "20s 必须计入 le=30000 真实桶"
    );
    // +Inf 是累积值（== count），非「未落桶」指示器。
    assert!(
        t2.contains("gateway_processing_duration_ms_bucket{le=\"+Inf\"} 1"),
        "+Inf 为累积值，应等于 count"
    );

    // 超过 60s 截断的样本：既有口径为「截断到 60000 再入桶」，故 90s 截断后
    // 等于 60000，恰好命中 le=60000 桶（`<=` 边界语义）——不改此口径，
    // 只锁定它，避免将来误改截断值导致 sum 与桶不自洽。
    let m3 = MetricsRegistry::new();
    m3.observe(200, None, 0, Duration::from_millis(90_000));
    let t3 = m3.render();
    assert!(
        t3.contains("gateway_processing_duration_ms_bucket{le=\"60000\"} 1"),
        "90s 经 60s 截断后应命中 le=60000 桶"
    );
    assert!(
        t3.contains("gateway_processing_duration_ms_bucket{le=\"+Inf\"} 1"),
        "+Inf 为累积值，应等于 count"
    );
    assert!(
        t3.contains("gateway_processing_duration_ms_count 1"),
        "截断样本仍须计入 count"
    );
    // sum 记的是截断值而非原始 90000（既有 OPT-R4 A4 语义，锁定不回归）。
    assert!(
        t3.contains("gateway_processing_duration_ms_sum 60000"),
        "sum 须记截断值 60000，而非原始 90000"
    );
}

#[test]
fn dropped_counter_shared_and_rendered() {
    // OPT-4：worker 与注册表共享同一个 Arc；worker 侧累加后 render 可见。
    let shared = Arc::new(AtomicU64::new(0));
    // R2-4：双计数器各自共享、各自渲染、互不干扰。
    let channel = Arc::new(AtomicU64::new(0));
    let m = MetricsRegistry::new_with_dropped(shared.clone(), channel.clone());
    // 零值也必须渲染（PromQL 告警依赖该行常驻）。
    assert!(m.render().contains("telemetry_dropped_total 0"));
    assert!(m.render().contains("telemetry_channel_dropped_total 0"));
    shared.fetch_add(7, Ordering::Relaxed);
    channel.fetch_add(3, Ordering::Relaxed);
    assert_eq!(m.dropped_count(), 7);
    assert_eq!(m.channel_dropped_count(), 3);
    assert!(m.render().contains("telemetry_dropped_total 7"));
    assert!(m.render().contains("telemetry_channel_dropped_total 3"));
}

/// OPT-R15：落地端健康三件套的**懒出现**语义。
///
/// 关键设计：未配置/未跑过的落地端**不渲染任何行**（而不是渲染 `up=0`）。
/// 否则「ClickHouse 压根没配」的部署会被 `telemetry_sink_up == 0` 告警直接误伤——
/// 告警的默认状态必须是"沉默"，而不是"一片红"。
#[test]
fn sink_health_is_lazy_per_backend() {
    let m = MetricsRegistry::new();
    let r = m.render();
    // HELP/TYPE 头行常驻（与其它指标族一致），所以只能断言**样本行**不存在。
    assert!(
        r.contains("# TYPE telemetry_sink_up gauge"),
        "指标族声明应常驻"
    );
    assert!(
        !r.contains("telemetry_sink_up{backend="),
        "未跑过时不得出现 sink 样本行，否则未配置的落地端会误报 down"
    );
    assert!(!r.contains("telemetry_sink_failures_total{backend="));
    assert!(!r.contains("telemetry_sink_last_ok_unixtime_seconds{backend="));
}

/// OPT-R15：失败→成功 的状态机与失败计数、last_ok 刷新。
///
/// 覆盖这次要修的真问题：鉴权/连通性失败时遥测**静默丢光**，而
/// `telemetry_dropped_total` 只说"丢了多少"、不说"哪一级丢了、还通不通"。
#[test]
fn sink_health_tracks_failure_then_recovery_per_backend() {
    let m = MetricsRegistry::new();
    // Redis 失败一次 → up=0、failures=1、last_ok 仍无（从未成功）。
    m.mark_sink_failed("redis");
    let r = m.render();
    assert!(r.contains("telemetry_sink_up{backend=\"redis\"} 0"));
    assert!(r.contains("telemetry_sink_failures_total{backend=\"redis\"} 1"));
    assert!(r.contains("telemetry_sink_last_ok_unixtime_seconds{backend=\"redis\"} 0"));
    // ClickHouse 成功 → 独立序列，不受 Redis 影响（两级必须可分辨）。
    m.mark_sink_ok("clickhouse");
    let r = m.render();
    assert!(r.contains("telemetry_sink_up{backend=\"clickhouse\"} 1"));
    assert!(r.contains("telemetry_sink_up{backend=\"redis\"} 0"));
    // last_ok 必须是**真实 Unix 秒**而不是 0/1 占位，否则告警算不出"多久没写"。
    let last_ok: u64 = r
        .lines()
        .find_map(|l| {
            l.strip_prefix("telemetry_sink_last_ok_unixtime_seconds{backend=\"clickhouse\"} ")
                .and_then(|v| v.trim().parse().ok())
        })
        .expect("clickhouse last_ok line");
    assert!(
        last_ok > 1_700_000_000,
        "last_ok={last_ok} 应为真实 Unix 秒"
    );
    // Redis 恢复 → up 回 1，但失败计数**保留**（累计故障史不能被恢复抹掉）。
    m.mark_sink_ok("redis");
    let r = m.render();
    assert!(r.contains("telemetry_sink_up{backend=\"redis\"} 1"));
    assert!(r.contains("telemetry_sink_failures_total{backend=\"redis\"} 1"));
    // 连续失败要累加。
    m.mark_sink_failed("redis");
    m.mark_sink_failed("redis");
    assert!(m
        .render()
        .contains("telemetry_sink_failures_total{backend=\"redis\"} 3"));
}

#[test]
fn supervisor_restarts_counted_and_rendered() {
    // R2-8：各 worker 重启数分别计数、排序渲染；无重启时无线。
    let m = MetricsRegistry::new();
    assert!(!m.render().contains("supervisor_restarts_total{"));
    m.note_supervisor_restart("sink");
    m.note_supervisor_restart("sink");
    m.note_supervisor_restart("circuit_breaker");
    let text = m.render();
    assert!(text.contains("supervisor_restarts_total{worker=\"circuit_breaker\"} 1"));
    assert!(text.contains("supervisor_restarts_total{worker=\"sink\"} 2"));
}

#[test]
fn log_sampling_shape() {
    // R2-8：5xx 全量；非 5xx 首条全量、随后 999 条采样、千条一循环；
    // 采样数可观测（`gateway_logs_sampled_total` 常驻行）。
    let m = MetricsRegistry::new();
    assert!(m.sample_full_log(500));
    assert!(m.sample_full_log(503));
    assert!(m.sample_full_log(200), "seq 0 must be full");
    for _ in 0..999 {
        assert!(!m.sample_full_log(200));
    }
    assert!(m.sample_full_log(200), "seq 1000 must be full again");
    assert!(m.render().contains("gateway_logs_sampled_total 999"));
}

#[test]
fn free_pool_nodes_rendered() {
    // 免费池水位：默认 0 常驻行；set 后渲染新值。
    let m = MetricsRegistry::new();
    assert!(m.render().contains("free_pool_nodes_total 0"));
    m.set_free_pool_nodes(37);
    assert!(m.render().contains("free_pool_nodes_total 37"));
}

#[test]
fn free_pool_supply_slo_rendered() {
    // OPT-R18 P2-3：三个供给 SLO 指标必须常驻（仪表盘/告警不能等"有数据才出现"）。
    let m = MetricsRegistry::new();
    let r = m.render();
    // 无样本 ⇒ 转化率 NaN，**不是 0**：0 会被读成"转化率极差"。
    assert!(
        r.contains("free_pool_yield_ratio NaN"),
        "无质检样本时必须渲染 NaN"
    );
    // 空池 ⇒ 年龄 NaN，不是 0（0 会被读成"刚进池"）。
    assert!(
        r.contains("free_pool_median_age_seconds NaN"),
        "空池时年龄必须渲染 NaN"
    );
    assert!(r.contains("free_pool_distinct_exit_ips 0"));

    // 3 pass / 1 fail ⇒ 0.75。
    for _ in 0..3 {
        m.note_free_verify("pass");
    }
    m.note_free_verify("full_fail");
    assert!(m.render().contains("free_pool_yield_ratio 0.75"));

    m.set_free_pool_supply(9, Some(std::time::Duration::from_millis(1500)));
    let r = m.render();
    assert!(r.contains("free_pool_distinct_exit_ips 9"));
    assert!(r.contains("free_pool_median_age_seconds 1.5"));
}

#[test]
fn free_pool_extended_rendered() {
    // Task 3 水位计存量断言保留；新增四组：
    let m = MetricsRegistry::new();
    m.note_free_source_yield("api0", 12);
    m.note_free_verify("pass");
    m.note_free_verify("tcp_fail");
    m.note_free_anonymity("elite");
    m.set_free_source_suspended("gh0", true);
    let r = m.render();
    assert!(r.contains("free_pool_source_yield_total{source=\"api0\"} 12"));
    assert!(r.contains("free_pool_verify_total{result=\"pass\"} 1"));
    assert!(r.contains("free_pool_anonymity_total{level=\"elite\"} 1"));
    assert!(r.contains("free_pool_source_suspended{source=\"gh0\"} 1"));
    m.set_free_source_suspended("gh0", false);
    assert!(m
        .render()
        .contains("free_pool_source_suspended{source=\"gh0\"} 0"));
}

#[test]
fn free_pool_source_elite_rendered() {
    // NEXT-B5：无数据时无线（exposition 干净）；记两笔后按源名字序渲染。
    let m = MetricsRegistry::new();
    assert!(!m.render().contains("free_pool_source_elite_total"));
    m.note_free_source_elite("gh0");
    m.note_free_source_elite("api0");
    m.note_free_source_elite("api0");
    let r = m.render();
    assert!(r.contains("free_pool_source_elite_total{source=\"api0\"} 2"));
    assert!(r.contains("free_pool_source_elite_total{source=\"gh0\"} 1"));
}

#[test]
fn metrics_nodes_by_proto_rendered() {
    // P2-5：按出站协议水位（merge 后按快照聚合设置；need 行存在且值正确）。
    let m = MetricsRegistry::new();
    m.set_free_pool_nodes_proto("socks5", 2);
    m.set_free_pool_nodes_proto("http", 5);
    let r = m.render();
    assert!(r.contains("free_pool_nodes_by_proto{proto=\"socks5\"} 2"));
    assert!(r.contains("free_pool_nodes_by_proto{proto=\"http\"} 5"));
}

#[test]
fn metrics_geo_rendered() {
    // P3-4：lookup 分布＋mismatch 行存在；mismatch 常驻 0 行（counter 语义）。
    let m = MetricsRegistry::new();
    m.note_geo_lookup("disabled");
    m.note_geo_lookup("hit");
    m.note_geo_mismatch();
    let r = m.render();
    assert!(r.contains("geoip_lookups_total{result=\"disabled\"} 1"));
    assert!(r.contains("geoip_lookups_total{result=\"hit\"} 1"));
    assert!(r.contains("geoip_mismatch_total 1"));
}

#[test]
fn duration_sum_saturates_no_wrap() {
    // OPT-R4 A4：duration_sum_ms 饱和累加，长稳运行不回绕（Prom sum 曲线不倒退）。
    let m = MetricsRegistry::new();
    m.duration_sum_ms.store(u64::MAX - 10, Ordering::Relaxed);
    m.observe(200, None, 0, Duration::from_millis(5000));
    assert_eq!(m.duration_sum_ms.load(Ordering::Relaxed), u64::MAX);
    m.observe(200, None, 0, Duration::from_millis(5000));
    assert_eq!(m.duration_sum_ms.load(Ordering::Relaxed), u64::MAX);
    assert!(m
        .render()
        .contains(&format!("gateway_processing_duration_ms_sum {}", u64::MAX)));
}

#[test]
fn duration_single_observe_truncated() {
    // OPT-R4 A4：单次 observe 截断 60_000ms，避免单次超大 Duration 污染 sum。
    let m = MetricsRegistry::new();
    m.observe(200, None, 0, Duration::from_secs(3600));
    assert_eq!(m.duration_sum_ms.load(Ordering::Relaxed), 60_000);
}

#[test]
fn free_funnel_b9_rendered() {
    // OPT-R4 B9：免费漏斗四指标（intake_capped/baseline_fail/evicted/按源 fetch 耗时）。
    let m = MetricsRegistry::new();
    m.note_free_intake_capped(7);
    m.note_free_baseline_fail();
    m.note_free_evicted(3);
    m.note_free_source_fetch("api0", Duration::from_millis(120));
    m.note_free_source_fetch("api0", Duration::from_millis(80));
    let r = m.render();
    assert!(r.contains("free_pool_intake_capped_total 7"));
    assert!(r.contains("free_pool_baseline_fail_total 1"));
    assert!(r.contains("free_pool_evicted_total 3"));
    assert!(r.contains("free_pool_source_fetch_ms_sum{source=\"api0\"} 200"));
    assert!(r.contains("free_pool_source_fetch_ms_count{source=\"api0\"} 2"));
}

#[test]
fn free_fwd_latency_and_exit_rendered() {
    // OPT-R5 E5（S）：单节点转发延迟直方图＋出口分组计数（轮转由 PromQL changes() 看）。
    let m = MetricsRegistry::new();
    m.note_free_fwd_latency(3);
    m.note_free_fwd_latency(1200);
    m.note_free_exit("1.2.3.4");
    m.note_free_exit("1.2.3.4");
    m.note_free_exit("5.6.7.8");
    let r = m.render();
    // 桶行为：3ms 落 le>=5 首桶；1200ms 落 le>=2500；累计语义。
    assert!(r.contains("free_pool_fwd_latency_ms_bucket{le=\"5\"} 1"));
    assert!(r.contains("free_pool_fwd_latency_ms_bucket{le=\"+Inf\"} 2"));
    assert!(r.contains("free_pool_fwd_latency_ms_count 2"));
    assert!(r.contains("free_pool_exit_total{exit_ip=\"1.2.3.4\"} 2"));
    assert!(r.contains("free_pool_exit_total{exit_ip=\"5.6.7.8\"} 1"));
}

#[test]
fn active_probe_metrics_render_fixed_series() {
    // 步骤 54：三行 counter 固定渲染（首次探测前也有 0 序列），未知标签
    // 不建序列（防高基数）；gauge 由 prober tick 同步。
    let m = MetricsRegistry::new();
    let initial = m.render();
    assert!(initial.contains("active_probe_results_total{result=\"healthy\"} 0"));
    assert!(initial.contains("active_probe_results_total{result=\"degraded\"} 0"));
    assert!(initial.contains("active_probe_results_total{result=\"dead\"} 0"));
    assert!(initial.contains("active_probe_unhealthy_nodes 0"));

    m.note_active_probe_result("healthy");
    m.note_active_probe_result("dead");
    m.note_active_probe_result("node-10.0.0.1:8080"); // 非法标签必须忽略
    m.set_active_probe_unhealthy(2);
    let text = m.render();
    assert!(text.contains("active_probe_results_total{result=\"healthy\"} 1"));
    assert!(text.contains("active_probe_results_total{result=\"degraded\"} 0"));
    assert!(text.contains("active_probe_results_total{result=\"dead\"} 1"));
    assert!(text.contains("active_probe_unhealthy_nodes 2"));
    assert!(!text.contains("node-10.0.0.1:8080"));
}

#[tokio::test]
async fn metrics_endpoint_serves_render() {
    let m = Arc::new(MetricsRegistry::new());
    m.observe(200, None, 10, Duration::from_millis(1));
    tokio::spawn(serve_metrics(m, "127.0.0.1:19091"));
    tokio::time::sleep(Duration::from_millis(200)).await;
    let body = reqwest::get("http://127.0.0.1:19091/metrics")
        .await
        .expect("GET /metrics")
        .text()
        .await
        .expect("body");
    assert!(body.contains("proxy_requests_total"), "{body}");
}

/// 覆盖全部指标族的确定性状态（黄金快照与结构断言共用）。
///
/// 刻意**不**调 `mark_sink_ok`（内部取当前时间）、**不**设 free 节点年龄
/// （空池渲染 `NaN`），故任何时刻、任何机器都得到同一份文本。
fn populated_registry() -> MetricsRegistry {
    let m =
        MetricsRegistry::new_with_dropped(Arc::new(AtomicU64::new(3)), Arc::new(AtomicU64::new(1)));
    // 请求与字节/延迟
    m.observe(200, Some("mock-a"), 100, Duration::from_millis(3));
    m.observe(403, Some("mock-a"), 0, Duration::from_millis(1200));
    m.observe(402, Some("mock-b"), 7, Duration::from_millis(9));
    m.observe(429, None, 0, Duration::from_millis(1));
    m.observe(502, None, 10, Duration::from_millis(50));
    m.observe_stage("auth", 2);
    m.observe_stage("route", 5);
    // 桥 / 探测 / 熔断 / 选路
    m.note_bridge_error();
    m.set_quarantine_nodes(1);
    m.note_active_probe_result("ok");
    m.set_active_probe_unhealthy(1);
    m.note_breaker_trigger(502);
    m.note_bandit_exploration();
    m.set_quarantine_domains(2);
    // 遥测管道（mark_sink_failed 只自增计数，不写时间）
    m.mark_sink_failed("clickhouse");
    m.note_supervisor_restart("sink");
    m.sample_full_log(200);
    // 免费线
    m.set_free_pool_nodes(4);
    m.note_free_source_yield("api", 5);
    m.note_free_source_elite("api");
    m.note_free_verify("pass");
    m.note_free_verify("tcp_fail");
    m.set_free_pool_supply(3, None);
    m.note_free_anonymity("elite");
    m.set_free_source_suspended("api", true);
    m.set_free_pool_nodes_proto("http", 3);
    m.note_geo_lookup("hit");
    m.note_geo_mismatch();
    m.note_free_intake_capped(7);
    m.note_free_baseline_fail();
    m.note_free_evicted(2);
    m.note_free_source_fetch("api", Duration::from_millis(12));
    m.note_free_fwd_latency(40);
    m.note_free_exit("1.2.3.4");
    m.note_free_exit("1.2.3.4");
    m.note_free_exit("5.6.7.8");
    m
}

/// **每个指标名只能有一条 `# HELP` / 一条 `# TYPE`**。
///
/// # 为什么这条规则是硬的
///
/// Prometheus 文本格式规定同一指标名只允许一条 HELP。违反时抓取端**解析失败**，
/// 后果不是"少一个指标"，而是**整次抓取作废**——该实例所有指标一起丢。
///
/// # 这条测试是怎么来的
///
/// 2026-11-01 给 `render()` 建黄金快照时，顺手把输出喂给
/// `promtool check metrics`，报：
/// `text format parsing error in line 76: second HELP line for metric name
/// "proxy_stage_duration_ms"`。根因是 HELP/TYPE 被写在了 per-stage 循环**内部**，
/// 3 个 stage 输出 3 份。已修（提到循环外）。
///
/// 黄金快照能抓到这一处，但抓不到"以后新增指标族时又犯一次"；本测试按名字计数，
/// 对**任何**指标族都成立，故两者互补。
#[test]
fn each_metric_has_single_help_and_type() {
    let text = populated_registry().render();
    let mut helps: Vec<&str> = Vec::new();
    let mut types: Vec<&str> = Vec::new();
    for line in text.lines() {
        if let Some(rest) = line.strip_prefix("# HELP ") {
            helps.push(rest.split_whitespace().next().unwrap_or(""));
        } else if let Some(rest) = line.strip_prefix("# TYPE ") {
            types.push(rest.split_whitespace().next().unwrap_or(""));
        }
    }
    for (kind, names) in [("# HELP", &helps), ("# TYPE", &types)] {
        for (i, name) in names.iter().enumerate() {
            assert!(
                !names[i + 1..].contains(name),
                "指标 `{name}` 出现了多条 `{kind}` —— Prometheus 会拒绝整次抓取\
                 （不是丢这一个指标，是所有指标一起丢）。HELP/TYPE 必须在循环外只写一次。"
            );
        }
    }
    assert!(!helps.is_empty(), "渲染输出里没有任何 # HELP，测试自身失效");
}

/// **黄金输出快照**：一次性覆盖全部指标族，断言 `render()` 的**逐字节**输出。
///
/// # 为什么需要它
///
/// `render()` 原是一个 538 行的逐族 `push_str` 长函数。把它拆成
/// `render_<section>` 时必须保证**行序与格式一字不差**：Prometheus 文本协议里
/// 同一族的样本重复出现会被抓取端拒绝，而既有测试只逐族断言片段，没有
/// 「整篇输出」这道护栏。
///
/// # 为什么这份输入是确定性的
///
/// 刻意**不**调 `mark_sink_ok`（内部取当前时间）、**不**设 free 节点年龄
/// （空池渲染 `NaN`）、不依赖任何 `Instant`/系统时钟，故本测试在任何时刻、
/// 任何机器上都得到同一份文本。改动本测试的期望值 = 改动对外契约，必须
/// 同时更新 `docs/OPERATION.md` 与告警规则。
#[test]
fn golden_render_full_exposition() {
    let m = populated_registry();

    let text = m.render();
    assert_eq!(
        text, EXPECTED,
        "render() 输出与黄金快照不一致 —— 若本次改动**有意**改变对外契约，\n         必须同步更新 docs/OPERATION.md 与 deploy/prometheus/rules.yml，\n         并把下面的 EXPECTED 一起改掉（它不是测试数据，是对外契约）"
    );
}

/// 上面测试的期望值。**不是测试数据，是对外契约**：Prometheus 抓取端与
/// Grafana 面板都依赖这些指标名、标签名与行序。
const EXPECTED: &str = r#"# HELP proxy_requests_total Proxied responses by status class.
# TYPE proxy_requests_total counter
proxy_requests_total{status="2xx"} 1
proxy_requests_total{status="4xx"} 3
proxy_requests_total{status="5xx"} 1
proxy_requests_total{status="other"} 0
proxy_requests_total{status="2xx", provider="mock-a"} 1
proxy_requests_total{status="4xx", provider="mock-a"} 1
proxy_requests_total{status="4xx", provider="mock-b"} 1
proxy_requests_total{status="4xx", provider="none"} 1
proxy_requests_total{status="5xx", provider="none"} 1
# HELP proxy_requests_forbidden_total 403 responses by provider.
# TYPE proxy_requests_forbidden_total counter
proxy_requests_forbidden_total{provider="mock-a"} 1
# HELP proxy_requests_unpaid_total Tenant out-of-balance (402) responses.
# TYPE proxy_requests_unpaid_total counter
proxy_requests_unpaid_total 1
# HELP proxy_requests_limited_total Rate/concurrency limited (429) responses.
# TYPE proxy_requests_limited_total counter
proxy_requests_limited_total 1
# HELP socks_bridge_errors_total SOCKS bridge egress failures.
# TYPE socks_bridge_errors_total counter
socks_bridge_errors_total 1
# HELP quarantine_nodes Quarantined domain:ip entries in memory.
# TYPE quarantine_nodes gauge
quarantine_nodes 1
# HELP active_probe_results_total Active health probe results by outcome.
# TYPE active_probe_results_total counter
active_probe_results_total{result="healthy"} 0
active_probe_results_total{result="degraded"} 0
active_probe_results_total{result="dead"} 0
# HELP active_probe_unhealthy_nodes Nodes currently removed by active health.
# TYPE active_probe_unhealthy_nodes gauge
active_probe_unhealthy_nodes 1
# HELP connect_tunnels_established_total CONNECT tunnels established by upstream protocol.
# TYPE connect_tunnels_established_total counter
# HELP connect_tunnel_terminated_total CONNECT tunnel terminations by reason.
# TYPE connect_tunnel_terminated_total counter
# HELP connect_tunnel_active Current CONNECT connections.
# TYPE connect_tunnel_active gauge
connect_tunnel_active 0
# HELP connect_admission_rejected_total CONNECT admission limit rejections.
# TYPE connect_admission_rejected_total counter
connect_admission_rejected_total 0
# HELP connect_bytes_total CONNECT bytes by direction.
# TYPE connect_bytes_total counter
connect_bytes_total{direction="upstream"} 0
connect_bytes_total{direction="downstream"} 0
# HELP connect_tunnel_duration_ms_sum CONNECT tunnel duration sum in milliseconds.
# TYPE connect_tunnel_duration_ms_sum counter
connect_tunnel_duration_ms_sum 0
# HELP connect_tunnel_closed_total CONNECT tunnels closed.
# TYPE connect_tunnel_closed_total counter
connect_tunnel_closed_total 0
# HELP gateway_transferred_bytes_total Egress bytes metered.
# TYPE gateway_transferred_bytes_total counter
gateway_transferred_bytes_total 117
# HELP gateway_processing_duration_ms Gateway overhead histogram.
# TYPE gateway_processing_duration_ms histogram
gateway_processing_duration_ms_bucket{le="1"} 1
gateway_processing_duration_ms_bucket{le="5"} 2
gateway_processing_duration_ms_bucket{le="10"} 3
gateway_processing_duration_ms_bucket{le="25"} 3
gateway_processing_duration_ms_bucket{le="50"} 4
gateway_processing_duration_ms_bucket{le="100"} 4
gateway_processing_duration_ms_bucket{le="250"} 4
gateway_processing_duration_ms_bucket{le="500"} 4
gateway_processing_duration_ms_bucket{le="1000"} 4
gateway_processing_duration_ms_bucket{le="2500"} 5
gateway_processing_duration_ms_bucket{le="5000"} 5
gateway_processing_duration_ms_bucket{le="10000"} 5
gateway_processing_duration_ms_bucket{le="30000"} 5
gateway_processing_duration_ms_bucket{le="60000"} 5
gateway_processing_duration_ms_bucket{le="+Inf"} 5
gateway_processing_duration_ms_count 5
gateway_processing_duration_ms_sum 1263
# HELP proxy_stage_duration_ms Per-stage gateway overhead histogram (ms).
# TYPE proxy_stage_duration_ms histogram
proxy_stage_duration_ms_bucket{stage="connect",le="1"} 0
proxy_stage_duration_ms_bucket{stage="connect",le="5"} 0
proxy_stage_duration_ms_bucket{stage="connect",le="10"} 0
proxy_stage_duration_ms_bucket{stage="connect",le="25"} 0
proxy_stage_duration_ms_bucket{stage="connect",le="50"} 0
proxy_stage_duration_ms_bucket{stage="connect",le="100"} 0
proxy_stage_duration_ms_bucket{stage="connect",le="250"} 0
proxy_stage_duration_ms_bucket{stage="connect",le="500"} 0
proxy_stage_duration_ms_bucket{stage="connect",le="1000"} 0
proxy_stage_duration_ms_bucket{stage="connect",le="2500"} 0
proxy_stage_duration_ms_bucket{stage="connect",le="5000"} 0
proxy_stage_duration_ms_bucket{stage="connect",le="10000"} 0
proxy_stage_duration_ms_bucket{stage="connect",le="30000"} 0
proxy_stage_duration_ms_bucket{stage="connect",le="60000"} 0
proxy_stage_duration_ms_bucket{stage="connect",le="+Inf"} 0
proxy_stage_duration_ms_count{stage="connect"} 0
proxy_stage_duration_ms_sum{stage="connect"} 0
proxy_stage_duration_ms_bucket{stage="ttfb",le="1"} 0
proxy_stage_duration_ms_bucket{stage="ttfb",le="5"} 0
proxy_stage_duration_ms_bucket{stage="ttfb",le="10"} 0
proxy_stage_duration_ms_bucket{stage="ttfb",le="25"} 0
proxy_stage_duration_ms_bucket{stage="ttfb",le="50"} 0
proxy_stage_duration_ms_bucket{stage="ttfb",le="100"} 0
proxy_stage_duration_ms_bucket{stage="ttfb",le="250"} 0
proxy_stage_duration_ms_bucket{stage="ttfb",le="500"} 0
proxy_stage_duration_ms_bucket{stage="ttfb",le="1000"} 0
proxy_stage_duration_ms_bucket{stage="ttfb",le="2500"} 0
proxy_stage_duration_ms_bucket{stage="ttfb",le="5000"} 0
proxy_stage_duration_ms_bucket{stage="ttfb",le="10000"} 0
proxy_stage_duration_ms_bucket{stage="ttfb",le="30000"} 0
proxy_stage_duration_ms_bucket{stage="ttfb",le="60000"} 0
proxy_stage_duration_ms_bucket{stage="ttfb",le="+Inf"} 0
proxy_stage_duration_ms_count{stage="ttfb"} 0
proxy_stage_duration_ms_sum{stage="ttfb"} 0
proxy_stage_duration_ms_bucket{stage="transfer",le="1"} 0
proxy_stage_duration_ms_bucket{stage="transfer",le="5"} 0
proxy_stage_duration_ms_bucket{stage="transfer",le="10"} 0
proxy_stage_duration_ms_bucket{stage="transfer",le="25"} 0
proxy_stage_duration_ms_bucket{stage="transfer",le="50"} 0
proxy_stage_duration_ms_bucket{stage="transfer",le="100"} 0
proxy_stage_duration_ms_bucket{stage="transfer",le="250"} 0
proxy_stage_duration_ms_bucket{stage="transfer",le="500"} 0
proxy_stage_duration_ms_bucket{stage="transfer",le="1000"} 0
proxy_stage_duration_ms_bucket{stage="transfer",le="2500"} 0
proxy_stage_duration_ms_bucket{stage="transfer",le="5000"} 0
proxy_stage_duration_ms_bucket{stage="transfer",le="10000"} 0
proxy_stage_duration_ms_bucket{stage="transfer",le="30000"} 0
proxy_stage_duration_ms_bucket{stage="transfer",le="60000"} 0
proxy_stage_duration_ms_bucket{stage="transfer",le="+Inf"} 0
proxy_stage_duration_ms_count{stage="transfer"} 0
proxy_stage_duration_ms_sum{stage="transfer"} 0
# HELP breaker_trigger_total Circuit breaker trigger count by status.
# TYPE breaker_trigger_total counter
breaker_trigger_total{status="502"} 1
# HELP bandit_exploration_total LinUCB forced tier exploration count.
# TYPE bandit_exploration_total counter
bandit_exploration_total 1
# HELP quarantine_domains_total Quarantined domain count (outer level).
# TYPE quarantine_domains_total gauge
quarantine_domains_total 2
# HELP telemetry_dropped_total Telemetry events dropped after one retry.
# TYPE telemetry_dropped_total counter
telemetry_dropped_total 3
# HELP telemetry_channel_dropped_total Telemetry events dropped on a full/closed channel.
# TYPE telemetry_channel_dropped_total counter
telemetry_channel_dropped_total 1
# HELP telemetry_sink_up Telemetry sink reachable (1) or failing (0), last attempt.
# TYPE telemetry_sink_up gauge
telemetry_sink_up{backend="clickhouse"} 0
# HELP telemetry_sink_failures_total Telemetry sink write attempts that failed.
# TYPE telemetry_sink_failures_total counter
telemetry_sink_failures_total{backend="clickhouse"} 1
# HELP telemetry_sink_last_ok_unixtime_seconds Unix time of last successful sink write (0 = never).
# TYPE telemetry_sink_last_ok_unixtime_seconds gauge
telemetry_sink_last_ok_unixtime_seconds{backend="clickhouse"} 0
# HELP supervisor_restarts_total Background worker restarts by worker.
# TYPE supervisor_restarts_total counter
supervisor_restarts_total{worker="sink"} 1
# HELP gateway_logs_sampled_total Data-plane log lines sampled to debug.
# TYPE gateway_logs_sampled_total counter
gateway_logs_sampled_total 0
# HELP free_pool_nodes_total Free-tier nodes currently merged into the pool.
# TYPE free_pool_nodes_total gauge
free_pool_nodes_total 4
# HELP free_pool_source_yield_total FreePool fetched nodes by source.
# TYPE free_pool_source_yield_total counter
free_pool_source_yield_total{source="api"} 5
# HELP free_pool_source_elite_total FreePool Elite nodes by source.
# TYPE free_pool_source_elite_total counter
free_pool_source_elite_total{source="api"} 1
# HELP free_pool_verify_total FreePool verify outcomes by result.
# TYPE free_pool_verify_total counter
free_pool_verify_total{result="pass"} 1
free_pool_verify_total{result="tcp_fail"} 1
# HELP free_pool_yield_ratio Share of verified free proxies that passed quality gates.
# TYPE free_pool_yield_ratio gauge
free_pool_yield_ratio 0.5
# HELP free_pool_distinct_exit_ips Distinct egress identities among pooled free nodes.
# TYPE free_pool_distinct_exit_ips gauge
free_pool_distinct_exit_ips 3
# HELP free_pool_median_age_seconds Median age of pooled free nodes.
# TYPE free_pool_median_age_seconds gauge
free_pool_median_age_seconds NaN
# HELP free_pool_anonymity_total FreePool anonymity levels by level.
# TYPE free_pool_anonymity_total counter
free_pool_anonymity_total{level="elite"} 1
# HELP free_pool_source_suspended FreePool source suspended flag by source.
# TYPE free_pool_source_suspended gauge
free_pool_source_suspended{source="api"} 1
# HELP free_pool_nodes_by_proto FreePool merged nodes by egress proto.
# TYPE free_pool_nodes_by_proto gauge
free_pool_nodes_by_proto{proto="http"} 3
# HELP geoip_lookups_total GeoIP exit-country lookups by result.
# TYPE geoip_lookups_total counter
geoip_lookups_total{result="hit"} 1
# HELP geoip_mismatch_total GeoIP exit-country hard mismatches.
# TYPE geoip_mismatch_total counter
geoip_mismatch_total 1
# HELP free_pool_intake_capped_total FreePool intake nodes dropped by cap.
# TYPE free_pool_intake_capped_total counter
free_pool_intake_capped_total 7
# HELP free_pool_baseline_fail_total FreePool ticks degraded on baseline unreachable.
# TYPE free_pool_baseline_fail_total counter
free_pool_baseline_fail_total 1
# HELP free_pool_evicted_total FreePool nodes evicted over capacity.
# TYPE free_pool_evicted_total counter
free_pool_evicted_total 2
# HELP free_pool_source_fetch_ms_sum FreePool per-source fetch latency sum in ms.
# TYPE free_pool_source_fetch_ms_sum counter
free_pool_source_fetch_ms_sum{source="api"} 12
# HELP free_pool_source_fetch_ms_count FreePool per-source fetch rounds.
# TYPE free_pool_source_fetch_ms_count counter
free_pool_source_fetch_ms_count{source="api"} 1
# HELP free_pool_fwd_latency_ms FreePool single-node forward latency in ms.
# TYPE free_pool_fwd_latency_ms histogram
free_pool_fwd_latency_ms_bucket{le="1"} 0
free_pool_fwd_latency_ms_bucket{le="5"} 0
free_pool_fwd_latency_ms_bucket{le="10"} 0
free_pool_fwd_latency_ms_bucket{le="25"} 0
free_pool_fwd_latency_ms_bucket{le="50"} 1
free_pool_fwd_latency_ms_bucket{le="100"} 1
free_pool_fwd_latency_ms_bucket{le="250"} 1
free_pool_fwd_latency_ms_bucket{le="500"} 1
free_pool_fwd_latency_ms_bucket{le="1000"} 1
free_pool_fwd_latency_ms_bucket{le="2500"} 1
free_pool_fwd_latency_ms_bucket{le="5000"} 1
free_pool_fwd_latency_ms_bucket{le="10000"} 1
free_pool_fwd_latency_ms_bucket{le="30000"} 1
free_pool_fwd_latency_ms_bucket{le="60000"} 1
free_pool_fwd_latency_ms_bucket{le="+Inf"} 1
free_pool_fwd_latency_ms_count 1
free_pool_fwd_latency_ms_sum 40
# HELP free_pool_exit_total FreePool forward passes by exit ip.
# TYPE free_pool_exit_total counter
free_pool_exit_total{exit_ip="1.2.3.4"} 2
free_pool_exit_total{exit_ip="5.6.7.8"} 1
free_pool_exit_total{exit_ip="__overflow__"} 0
"#;

/// `/metrics` 输出必须是**合法的 Prometheus 文本格式**。
///
/// # 为什么需要这条
///
/// 2026-11-01 实测发现 `proxy_stage_duration_ms` 把 `# HELP`/`# TYPE` 写在
/// per-stage 循环内部，3 个 stage 输出 3 份 ⇒ 抓取端解析失败 ⇒ **整次抓取作废**
/// （该实例所有指标一起丢，不是丢一个）。当时是靠外部 `promtool check metrics`
/// 才发现的，而 promtool 不在 `cargo test` 里、也不是人人都有。
///
/// 本测试把 promtool **会拒绝**的几类结构错误固化成断言，让同类回归在本地就红。
/// 规则与 promtool（prom/prometheus:v2.53.0）的解析器对齐，并已用同一份输出
/// 双向交叉验证过：**修好前后分别喂给 promtool 与本测试，结论一致**。
///
/// # 校验规则
///
/// 1. `# TYPE` 取值必须在 counter/gauge/histogram/summary/untyped 内；
/// 2. 同一指标名的 `# HELP` / `# TYPE` 各只能有一条；
/// 3. 每条样本的指标名必须有已声明的 `# TYPE`（拼错即抓不到）；
/// 4. histogram 家族必须有 `_bucket`（且含 `le="+Inf"`）、`_count`、`_sum`；
/// 5. 同一 `(指标名, 标签集)` 只能出现一次（重复样本被抓取端拒绝）；
/// 6. 标签名符合 `[a-zA-Z_][a-zA-Z0-9_]*`。
///
/// 规则 3/4 抓"改名/漏渲染"，规则 5 抓"某段被渲染两次"，规则 1/2 抓"格式违规"。
#[test]
fn render_is_valid_prometheus_text() {
    let text = populated_registry().render();

    let mut helps: Vec<String> = Vec::new();
    let mut types: std::collections::BTreeMap<String, String> = std::collections::BTreeMap::new();
    let mut samples: Vec<(String, String)> = Vec::new(); // (name, label set)
    let mut buckets_inf: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();

    const ALLOWED_TYPES: [&str; 5] = ["counter", "gauge", "histogram", "summary", "untyped"];

    for (lineno, line) in text.lines().enumerate() {
        let n = lineno + 1;
        if line.is_empty() {
            continue;
        }
        if let Some(rest) = line.strip_prefix("# HELP ") {
            let name = rest.split_whitespace().next().unwrap_or("");
            assert!(!name.is_empty(), "L{n}: # HELP 缺指标名");
            assert!(
                !helps.iter().any(|h| h == name),
                "L{n}: 指标 `{name}` 出现多条 # HELP —— Prometheus 会拒绝整次抓取"
            );
            helps.push(name.to_string());
            continue;
        }
        if let Some(rest) = line.strip_prefix("# TYPE ") {
            let mut it = rest.split_whitespace();
            let (name, ty) = (it.next().unwrap_or(""), it.next().unwrap_or(""));
            assert!(
                ALLOWED_TYPES.contains(&ty),
                "L{n}: `{name}` 的 TYPE `{ty}` 非法（只允许 {ALLOWED_TYPES:?}）"
            );
            assert!(
                types.insert(name.to_string(), ty.to_string()).is_none(),
                "L{n}: 指标 `{name}` 出现多条 # TYPE"
            );
            continue;
        }
        assert!(!line.starts_with('#'), "L{n}: 无法识别的注释行：{line}");

        // 样本行：`name{labels} value` 或 `name value`
        let (head, _value) = line
            .rsplit_once(' ')
            .unwrap_or_else(|| panic!("L{n}: 样本行缺值：{line}"));
        let (name, labels) = match head.split_once('{') {
            Some((mname, rest)) => {
                let inner = rest
                    .strip_suffix('}')
                    .unwrap_or_else(|| panic!("L{n}: 标签未闭合：{line}"));
                // 标签分隔是 `, `（逗号+空格），故 key 要 trim——否则 `" provider"`
                // 会被误判为不合规范（本测试首跑就是这么红的）。
                for kv in inner.split(',') {
                    let k = kv.split('=').next().unwrap_or("").trim();
                    assert!(
                        !k.is_empty()
                            && k.chars()
                                .next()
                                .is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
                            && k.chars().all(|c| c.is_ascii_alphanumeric() || c == '_'),
                        "L{n}: 标签名 `{k}` 不合规范（{line}）"
                    );
                }
                (mname, inner.to_string())
            }
            None => (head, String::new()),
        };
        samples.push((name.to_string(), labels.clone()));
        if name.ends_with("_bucket") && labels.contains("le=\"+Inf\"") {
            buckets_inf.insert(name.trim_end_matches("_bucket").to_string());
        }
    }

    // HELP 与 TYPE 必须属于同一组指标族。此前 `free_pool_exit_total` 的样本
    // 已存在且 dashboard 正在查询，但 HELP/TYPE 错写成 `free_exit_total`；
    // 单靠「样本必须有 TYPE」能抓到它，集合相等则把此契约明确固化。
    let help_names: std::collections::BTreeSet<String> = helps.into_iter().collect();
    let type_names: std::collections::BTreeSet<String> = types.keys().cloned().collect();
    assert_eq!(
        help_names, type_names,
        "# HELP 与 # TYPE 指标名集合不一致 —— exposition family 名称漂移"
    );

    // 规则 3：每条样本的家族名必须已声明 TYPE。
    //
    // **不能无条件剥 `_bucket`/`_count`/`_sum`**：`free_pool_source_fetch_ms_sum`
    // 本身就是一个 counter（`_sum` 只是名字的一部分，不是直方图后缀），
    // 无条件剥离会去找不存在的 `free_pool_source_fetch_ms` 而误报。
    // 正确顺序：① 整名已声明 ⇒ 用它；② 否则剥后缀，且**仅当基名声明为 histogram**
    // 才算（这才是直方图的 `_bucket`/`_count`/`_sum`）。
    for (name, _) in &samples {
        let declared = types.get(name.as_str());
        let base = if declared.is_some() {
            name.as_str()
        } else {
            ["_bucket", "_count", "_sum"]
                .iter()
                .find_map(|s| name.strip_suffix(s))
                .filter(|b| types.get(*b).map(|t| t == "histogram").unwrap_or(false))
                .unwrap_or(name.as_str())
        };
        assert!(
            types.contains_key(base),
            "样本 `{name}` 没有对应的 # TYPE —— 抓取端会把它当未声明家族"
        );
    }

    // 规则 4：histogram 必须有 _bucket（含 +Inf）/ _count / _sum。
    for (name, ty) in &types {
        if ty != "histogram" {
            continue;
        }
        assert!(
            buckets_inf.contains(name),
            "histogram `{name}` 缺 `_bucket{{le=\"+Inf\"}}` 样本"
        );
        for suffix in ["_count", "_sum"] {
            assert!(
                samples.iter().any(|(s, _)| s == &format!("{name}{suffix}")),
                "histogram `{name}` 缺 `{suffix}` 样本"
            );
        }
    }

    // 规则 5：同一 (指标名, 标签集) 只能出现一次。
    let mut seen = std::collections::BTreeSet::new();
    for (name, labels) in &samples {
        assert!(
            seen.insert((name.clone(), labels.clone())),
            "样本 `{name}{{{labels}}}` 重复出现 —— 抓取端会拒绝整次抓取"
        );
    }

    assert!(!samples.is_empty(), "渲染输出里没有任何样本，测试自身失效");
}
