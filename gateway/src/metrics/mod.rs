//! GW-4 Prometheus exposition: hand-rolled text format, zero new deps.
//!
//! Served on `127.0.0.1:9091/metrics` (see `deploy/prometheus/prometheus.yml`,
//! which already scrapes it). Four manual-A4 signals are covered:
//! success-rate counters, P99 duration histogram, per-provider 403 ratio
//! counters, and total transferred bytes.

use dashmap::DashMap;
use std::sync::atomic::{AtomicI64, AtomicU64, AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

/// Histogram upper bounds in ms (`+Inf` is implicit as `_count`).
///
/// OPT-R7 D2：尾部桶扩展到与 `observe` 的 60s 截断（`duration.as_millis().min(60_000)`）
/// 对齐。此前桶顶仅 2500ms，而网关 peer 超时预算是 1500/5000/3000ms
/// （`gateway.rs` 的 `build_http_peer`），叠加重试（失败集上限 16）后单请求耗时
/// **经常超过 2500ms**——>2500ms 的观测全部落进 `+Inf`，各 `le` 桶不动。
///
/// 后果是 `histogram_quantile(0.99, ...)` 在 >1% 样本超 2500ms 时，会在
/// `le=2500` 与 `+Inf` 之间外推，返回无意义值甚至 `+Inf`——**慢的时候 P99 反而
/// 显示得好看**，是最坏的失败方向（面板掩盖劣化而非暴露劣化）。
///
/// 扩展后 P99 在 5s~30s 区段仍有真实桶可落，无需外推。
///
/// **下界一律不移动**（仍从 1ms 起）：Prometheus 直方图的桶下界语义是「该桶覆盖
/// [prev_le, le]」，移动下界会让存量已累积的直方图在换版后语义漂移
/// （旧序列的历史数据不可比）。只加尾部桶是向后兼容的加法。
pub const DURATION_BUCKETS_MS: [u64; 14] = [
    1, 5, 10, 25, 50, 100, 250, 500, 1000, 2500, 5000, 10_000, 30_000, 60_000,
];
/// Metrics HTTP endpoint (matches the Prometheus scrape target).
pub const METRICS_ADDR: &str = "127.0.0.1:9091";
/// R2-8 `/metrics` 并发上限（零新依赖：超限即关连接；Prom 抓取低频，64 绰绰有余）。
pub const METRICS_MAX_CONCURRENT: usize = 64;
/// R2-8 `/metrics` 读超时（慢连接不占 worker）。
pub const METRICS_READ_TIMEOUT: Duration = Duration::from_secs(5);
/// R2-8 数据面日志采样分母（非 5xx 每 1000 条全量一条，其余降 `debug!`）。
pub const LOG_SAMPLE_EVERY: u64 = 1000;

/// OPT-R9 C2：`free_exit_total` 的出口 IP **标签基数上限**。
///
/// 免费代理的出口 IP 是公网随机 IP，取值空间无界；不设上限时
/// `MetricsRegistry::free_exit_total`（`DashMap`）与 Prometheus TSDB
/// 都会随运行时长无界增长。512 与 `free_pool` 侧 `FullChecker` 的 HTTP Client
/// 缓存上限同量级（见 `free_pool.rs`），即「一个出口在生命周期内被复用」的
/// 合理规模。超过后新出口并入 [`EXIT_OVERFLOW_LABEL`] 桶。
pub const EXIT_LABEL_CAP: usize = 512;

/// OPT-R9 C2：溢出桶的 `exit_ip` 标签值（第 513 个及以后的出口都归这里）。
///
/// 用双下划线前缀是为了**不可能与真实 IP 相同**（真实 IP 形如 `1.2.3.4` 或
/// `2409:8c54::11`，不含下划线），故该序列在语义上无歧义。
pub const EXIT_OVERFLOW_LABEL: &str = "__overflow__";

pub struct MetricsRegistry {
    req_2xx: AtomicU64,
    req_4xx: AtomicU64,
    req_5xx: AtomicU64,
    req_other: AtomicU64,
    /// NEXT-A4：欠费 402（租户余额门）与限流 429（QPS/并发）独立计数（`observe` 分流）。
    req_402: AtomicU64,
    req_429: AtomicU64,
    /// NEXT-A4：SOCKS 桥出站失败计数（网关 `serve_via_socks` Err 分支）。
    bridge_errors: AtomicU64,
    /// NEXT-A4：内存隔离表水位（sweep 滴答同步 `quarantine_map.len()`）。
    quarantine_nodes: AtomicU64,
    /// 按 provider 统计 403 响应数（per-provider 封禁比，与 `req_by_provider` 正交）。
    forbidden_by_provider: DashMap<String, AtomicU64>,
    /// 按 (status_class, provider) 统计请求数，用于 per-provider 成功率/403 比例查询。
    /// status_class ∈ {2xx, 4xx, 5xx, other}，provider 缺失时为 "none"。
    /// 与现有 `forbidden_by_provider`（仅 403）正交，两者不冲突。
    /// provider 基数有限（付费商 + free），不触发标签基数爆炸。
    req_by_provider: DashMap<(String, String), AtomicU64>,
    bytes_total: AtomicU64,
    duration_buckets: Vec<AtomicU64>,
    duration_count: AtomicU64,
    duration_sum_ms: AtomicU64,
    /// OPT-4 遥测落库丢弃计数：与 `TelemetryWorker` 共享同一个 `Arc`
    ///（worker 重试一次仍失败时整批累加），此处只读渲染，不参与 `observe`。
    /// R2-4 口径冻结：只计落库失败（通道满另计 `channel_dropped`）。
    telemetry_dropped: Arc<AtomicU64>,
    /// R2-4 通道丢弃计数：与 `TelemetryPublisher` 共享同一个 `Arc`
    ///（emit 满队列/已关闭时累加），此处只读渲染，不参与 `observe`。
    channel_dropped: Arc<AtomicU64>,
    /// OPT-R15 遥测**落地端**健康（`telemetry_sink_up{backend}` 渲染），`backend` = `redis` | `clickhouse`。
    /// 存在的理由：Redis / ClickHouse 鉴权或连通性失败时，遥测**静默丢光**——唯一信号是
    /// `telemetry_dropped_total` 在涨，但它既不告诉你是**哪一级**落地失败，也分不清
    /// 「本来就没有事件」与「落地端挂了」。这个 gauge 让两者可编程区分。
    /// 懒出现（首次 flush/insert 才建序列，不预置 `redis`/`clickhouse`）：未配置的落地端
    /// 不该被渲染成 `up=0` 而误报告警；「从未成功过」用 `absent()` + `last_ok=0` 表达。
    sink_up: DashMap<String, AtomicU64>,
    /// OPT-R15：落地端写入失败次数（`telemetry_sink_failures_total{backend}`），含重试后仍失败。
    sink_failures: DashMap<String, AtomicU64>,
    /// OPT-R15：最近一次**成功**写入的 Unix 秒（`telemetry_sink_last_ok_unixtime_seconds{backend}`）。
    /// 0 = 存在但从未成功过；与 `telemetry_sink_up` 组合可区分「彻底不通」与「通但很久没写」。
    sink_last_ok: DashMap<String, AtomicU64>,
    /// CONNECT listener 与隧道成功建立数；按上游协议标记。
    connect_tunnels_established: DashMap<String, AtomicU64>,
    /// CONNECT 隧道终止数；reason 是固定枚举字符串，不接受动态错误详情。
    connect_tunnel_terminated: DashMap<String, AtomicU64>,
    /// CONNECT 隧道存活时长累计毫秒。
    connect_tunnel_duration_ms: AtomicU64,
    /// 已结束的 CONNECT 隧道总数。
    connect_tunnel_closed: AtomicU64,
    /// CONNECT 上下行传输字节累计。
    connect_bytes_up: AtomicU64,
    connect_bytes_down: AtomicU64,
    /// CONNECT listener 活跃连接数，包含未认证头部读取。
    connect_active: AtomicU64,
    /// 达到 admission 上限而拒绝的连接数。
    connect_admission_rejected: AtomicU64,
    /// R2-8 后台重启计数（supervisor 直写，`supervisor_restarts_total{worker}` 渲染）。
    supervisor_restarts: DashMap<String, AtomicU64>,
    /// R2-8 日志采样序号（`sample_full_log` 发号，单调递增；5xx 不经过此处）。
    log_sample_seq: AtomicU64,
    /// R2-8 被采样掉的非 5xx 日志数（`gateway_logs_sampled_total` 渲染，可观测）。
    logs_sampled: AtomicU64,
    /// FreePool 免费池在池节点数（worker 每次合并后设置，只写不参与 observe）。
    free_pool_nodes: AtomicU64,
    /// OPT-R18 P2-3：池内**不同出口身份**个数（`free_pool_distinct_exit_ips` 渲染）。
    ///
    /// 与 `free_pool_nodes` 的差值就是"多节点共用一个出口"的程度：共用出口时
    /// 节点数再多，WAF 侧看到的也只有一个 IP——这是免费池最典型的假供给。
    free_pool_distinct_exits: AtomicU64,
    /// OPT-R18 P2-3：池内节点年龄**中位数**，单位毫秒（`free_pool_median_age_seconds` 渲染）。
    ///
    /// `-1` 表示**未知/空池**（渲染成 `NaN`）。用 i64 而不是 `AtomicU64` + 哨兵
    /// `u64::MAX`，是因为 `-1` 在打印与比较里都不容易和真实值混淆。
    free_pool_median_age_ms: AtomicI64,
    /// FreePool per-source 抓取产出（`free_pool_source_yield_total{source}` 渲染）。
    free_source_yield: DashMap<String, AtomicU64>,
    /// NEXT-B5：per-source Elite 产出（`free_pool_source_elite_total{source}` 渲染；
    /// 回答“哪个源真出货”，yield 高≠elite 高，调用方保证源名集合）。
    free_source_elite: DashMap<String, AtomicU64>,
    /// FreePool 质检结果计数（`free_pool_verify_total{result}` 渲染；
    /// result 实际只有 4 个：pass/tcp_fail/full_fail/geo_fail（无 backoff_skip，见 L259 说明）。
    free_verify: DashMap<String, AtomicU64>,
    /// FreePool 匿名度分级计数（`free_pool_anonymity_total{level}` 渲染；
    /// level∈elite/anonymous/transparent/unknown，调用方保证集合）。
    free_anonymity: DashMap<String, AtomicU64>,
    /// FreePool 源站熔断 gauge（`free_pool_source_suspended{source}` 0/1 渲染）。
    free_suspended: DashMap<String, AtomicU64>,
    /// P2 FreePool 按出站协议水位（`free_pool_nodes_by_proto{proto}` gauge 渲染；
    /// proto∈http/socks5/socks4，merge 后三档恒设，仪表盘行稳定）。
    free_nodes_by_proto: DashMap<String, AtomicU64>,
    /// P3 GeoIP 观察计数（`geoip_lookups_total{result}` 渲染；result∈hit/miss/disabled/error）。
    geo_lookups: DashMap<String, AtomicU64>,
    /// P3 exit-国家分歧计数（`geoip_mismatch_total` 渲染；只收实锤分歧，见 `exit_matches_source`）。
    geo_mismatch: AtomicU64,
    /// OPT-R4 B9：进检截断丢弃累计（`free_pool_intake_capped_total` 渲染；cap_intake 截掉量）。
    free_intake_capped: AtomicU64,
    /// OPT-R4 B9：基线不可达降级轮次（`free_pool_baseline_fail_total` 渲染；baseline None 即记）。
    free_baseline_fail: AtomicU64,
    /// OPT-R4 B9：容量淘汰累计（`free_pool_evicted_total` 渲染；evict_if_over_capacity 淘汰量）。
    free_evicted: AtomicU64,
    /// OPT-R4 B9：按源抓取耗时和（`free_pool_source_fetch_ms_sum{source}` 渲染；饱和累加）。
    free_fetch_ms_sum: DashMap<String, AtomicU64>,
    /// OPT-R4 B9：按源抓取次数（`free_pool_source_fetch_ms_count{source}` 渲染；与 sum 配对算均值）。
    free_fetch_count: DashMap<String, AtomicU64>,
    /// OPT-R5 E5：单节点转发延迟直方图写侧（`free_pool_fwd_latency_ms` 渲染；
    /// 桶口径复用 `DURATION_BUCKETS_MS`，单原子写＋render 前缀累加，与主 histogram 同模式）。
    free_fwd_buckets: Vec<AtomicU64>,
    /// OPT-R5 E5：单节点转发延迟总数（`free_pool_fwd_latency_ms_count` 渲染；`+Inf` 行同值）。
    free_fwd_count: AtomicU64,
    /// OPT-R5 E5：单节点转发延迟和（`free_pool_fwd_latency_ms_sum` 渲染；饱和累加）。
    free_fwd_sum_ms: AtomicU64,
    /// OPT-R5 E5：按出口分组计数（`free_pool_exit_total{exit_ip}` 渲染；
    /// 轮转语义不在此处存“上次 exit”状态，由 PromQL `changes()` 看，见 render 注释）。
    free_exit_total: DashMap<String, AtomicU64>,
    /// GW-6 五阶段耗时直方图写侧（stage ∈ connect/ttfb/transfer；ms；单原子写＋render 前缀累加，
    /// 与主 histogram 同模式；tls 握手暂并入 connect，Pingora 不单独暴露）。
    stage_connect_buckets: Vec<AtomicU64>,
    stage_ttfb_buckets: Vec<AtomicU64>,
    stage_transfer_buckets: Vec<AtomicU64>,
    stage_connect_count: AtomicU64,
    stage_connect_sum_ms: AtomicU64,
    stage_ttfb_count: AtomicU64,
    stage_ttfb_sum_ms: AtomicU64,
    stage_transfer_count: AtomicU64,
    stage_transfer_sum_ms: AtomicU64,
    // GW-6：熔断触发计数，per status（429/403/502/504）。
    breaker_trigger: DashMap<String, AtomicU64>,
    // GW-6：LinUCB 分档强制探索总次数。
    bandit_exploration: AtomicU64,
    // GW-6：隔离域数（外层 domain 数，由 sweep 同步）。
    quarantine_domains: AtomicU64,
    /// 步骤 54：主动健康探测结果计数（`active_probe_results_total{result}`；
    /// result 固定为 healthy/degraded/dead，调用方不得传任意标签）。
    active_probe_results: DashMap<String, AtomicU64>,
    /// 步骤 54：当前被主动健康摘除的节点数（prober tick 结束同步）。
    active_probe_unhealthy: AtomicU64,
}

/// OPT-R4 A4：u64 饱和累加（CAS 循环；长稳运行 sum 不回绕，Prometheus sum 曲线不倒退）。
fn saturating_add(target: &AtomicU64, delta: u64) {
    let mut cur = target.load(Ordering::Relaxed);
    loop {
        let next = cur.saturating_add(delta);
        match target.compare_exchange_weak(cur, next, Ordering::Relaxed, Ordering::Relaxed) {
            Ok(_) => break,
            Err(actual) => cur = actual,
        }
    }
}

/// GW-6：免费池供给快照（Admin API `/api/v1/freepool/*` 用）。
/// 所有字段均从已有计数器只读聚合，不新增指标字段；前端按
/// `pass/(pass+tcp_fail+full_fail+geo_fail)` 计算整体验证成功率，
/// 按 `nodes_total==0` 判定当前干旱，按 `elite/yield` 看各源出货质量。
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, utoipa::ToSchema)]
pub struct FreePoolSnapshot {
    /// 当前合并进池的免费节点总数（gauge；0 即干旱）。
    pub nodes_total: u64,
    /// 当前是否干旱（nodes_total==0 的即时镜像）。
    pub dry: bool,
    /// 全局质检结果汇总。
    pub verify: Vec<VerifyStat>,
    /// 按出站协议分布，proto∈http/socks5/socks4。
    pub by_proto: Vec<ProtoStat>,
    /// 各源产出与抓取明细。
    pub sources: Vec<FreePoolSourceStat>,
    /// 匿名度分级，level∈elite/anonymous/transparent/unknown。
    pub anonymity: Vec<AnonymityStat>,
    /// 源站熔断状态。
    pub suspended: Vec<SuspendedStat>,
    /// 进检截断丢弃累计。
    pub intake_capped: u64,
    /// 基线不可达降级轮次。
    pub baseline_fail: u64,
    /// 容量淘汰累计。
    pub evicted: u64,
}

/// 单源免费池产出与抓取明细（verify 结果按 result 全局汇总，不按源拆分）。
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, utoipa::ToSchema)]
pub struct FreePoolSourceStat {
    pub source: String,
    /// 抓取产出节点数。
    pub yield_total: u64,
    /// Elite 节点数（`elite/yield` 可近似该源出货质量）。
    pub elite_total: u64,
    /// 来源抓取耗时和（ms，用于前端算均值）。
    pub fetch_ms_sum: u64,
    /// 来源抓取次数。
    pub fetch_count: u64,
}

/// GW-6 断路器快照（Admin API `/api/v1/circuit` 用）。
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, utoipa::ToSchema)]
pub struct BreakerSnapshot {
    /// 熔断触发计数，per status（429/403/502/504）。
    pub breaker_trigger: Vec<BreakerTriggerStat>,
    /// LinUCB 分档强制探索总次数（tier_quota 强制选路）。
    pub bandit_exploration: u64,
    /// 隔离域数（外层 domain 数，区别于 quarantine_nodes 的内层 ip 数）。
    pub quarantine_domains: u64,
    /// 隔离条目总数（内层 ip 数）。
    pub quarantine_nodes: u64,
}

/// 单状态熔断触发计数：(status, count)。
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, utoipa::ToSchema)]
pub struct BreakerTriggerStat {
    pub status: String,
    pub count: u64,
}

/// 质检结果单项：(result, count)，result∈pass/tcp_fail/full_fail/geo_fail。
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, utoipa::ToSchema)]
pub struct VerifyStat {
    pub result: String,
    pub count: u64,
}

/// 按协议的节点水位：(proto, count)。
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, utoipa::ToSchema)]
pub struct ProtoStat {
    pub proto: String,
    pub count: u64,
}

/// 匿名度单项：(level, count)。
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, utoipa::ToSchema)]
pub struct AnonymityStat {
    pub level: String,
    pub count: u64,
}

/// 源站熔断单项：(source, 0/1)。
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, utoipa::ToSchema)]
pub struct SuspendedStat {
    pub source: String,
    pub suspended: u64,
}

impl MetricsRegistry {
    pub fn new() -> Self {
        Self::new_with_dropped(Arc::new(AtomicU64::new(0)), Arc::new(AtomicU64::new(0)))
    }

    /// OPT-4 装配入口：与落库 worker 共享同一个丢弃计数器。
    /// `new()` 等价于传入全新计数器（存量单测行为不变）；线上 `main` 传入共享实例。
    /// R2-4：第二个参数为通道丢弃计数器（与 publisher 共享）。
    pub fn new_with_dropped(
        telemetry_dropped: Arc<AtomicU64>,
        channel_dropped: Arc<AtomicU64>,
    ) -> Self {
        Self {
            req_2xx: AtomicU64::new(0),
            req_4xx: AtomicU64::new(0),
            req_5xx: AtomicU64::new(0),
            req_other: AtomicU64::new(0),
            req_402: AtomicU64::new(0),
            req_429: AtomicU64::new(0),
            bridge_errors: AtomicU64::new(0),
            quarantine_nodes: AtomicU64::new(0),
            forbidden_by_provider: DashMap::new(),
            req_by_provider: DashMap::new(),
            bytes_total: AtomicU64::new(0),
            duration_buckets: (0..DURATION_BUCKETS_MS.len())
                .map(|_| AtomicU64::new(0))
                .collect(),
            duration_count: AtomicU64::new(0),
            duration_sum_ms: AtomicU64::new(0),
            telemetry_dropped,
            channel_dropped,
            sink_up: DashMap::new(),
            sink_failures: DashMap::new(),
            sink_last_ok: DashMap::new(),
            connect_tunnels_established: DashMap::new(),
            connect_tunnel_terminated: DashMap::new(),
            connect_tunnel_duration_ms: AtomicU64::new(0),
            connect_tunnel_closed: AtomicU64::new(0),
            connect_bytes_up: AtomicU64::new(0),
            connect_bytes_down: AtomicU64::new(0),
            connect_active: AtomicU64::new(0),
            connect_admission_rejected: AtomicU64::new(0),
            supervisor_restarts: DashMap::new(),
            log_sample_seq: AtomicU64::new(0),
            logs_sampled: AtomicU64::new(0),
            free_pool_nodes: AtomicU64::new(0),
            free_pool_distinct_exits: AtomicU64::new(0),
            free_pool_median_age_ms: AtomicI64::new(-1),
            free_source_yield: DashMap::new(),
            free_source_elite: DashMap::new(),
            free_verify: DashMap::new(),
            free_anonymity: DashMap::new(),
            free_suspended: DashMap::new(),
            free_nodes_by_proto: DashMap::new(),
            geo_lookups: DashMap::new(),
            geo_mismatch: AtomicU64::new(0),
            free_intake_capped: AtomicU64::new(0),
            free_baseline_fail: AtomicU64::new(0),
            free_evicted: AtomicU64::new(0),
            free_fetch_ms_sum: DashMap::new(),
            free_fetch_count: DashMap::new(),
            free_fwd_buckets: (0..DURATION_BUCKETS_MS.len())
                .map(|_| AtomicU64::new(0))
                .collect(),
            free_fwd_count: AtomicU64::new(0),
            free_fwd_sum_ms: AtomicU64::new(0),
            free_exit_total: DashMap::new(),
            stage_connect_buckets: (0..DURATION_BUCKETS_MS.len())
                .map(|_| AtomicU64::new(0))
                .collect(),
            stage_ttfb_buckets: (0..DURATION_BUCKETS_MS.len())
                .map(|_| AtomicU64::new(0))
                .collect(),
            stage_transfer_buckets: (0..DURATION_BUCKETS_MS.len())
                .map(|_| AtomicU64::new(0))
                .collect(),
            stage_connect_count: AtomicU64::new(0),
            stage_connect_sum_ms: AtomicU64::new(0),
            stage_ttfb_count: AtomicU64::new(0),
            stage_ttfb_sum_ms: AtomicU64::new(0),
            stage_transfer_count: AtomicU64::new(0),
            stage_transfer_sum_ms: AtomicU64::new(0),
            breaker_trigger: DashMap::new(),
            bandit_exploration: AtomicU64::new(0),
            quarantine_domains: AtomicU64::new(0),
            active_probe_results: DashMap::new(),
            active_probe_unhealthy: AtomicU64::new(0),
        }
    }

    /// 当前落库丢弃总数（单测断言用；线上看 `/metrics` 渲染行）。
    #[cfg(test)]
    pub fn dropped_count(&self) -> u64 {
        self.telemetry_dropped.load(Ordering::Relaxed)
    }

    /// 当前通道丢弃总数（单测断言用；线上看 `/metrics` 渲染行）。
    #[cfg(test)]
    pub fn channel_dropped_count(&self) -> u64 {
        self.channel_dropped.load(Ordering::Relaxed)
    }

    /// R2-8：记录一次后台 worker 重启（supervisor 在 panic/意外返回后调用）。
    pub fn note_supervisor_restart(&self, worker: &str) {
        self.supervisor_restarts
            .entry(worker.to_string())
            .or_insert_with(|| AtomicU64::new(0))
            .fetch_add(1, Ordering::Relaxed);
    }

    /// R2-8 数据面日志采样：`status>=500` 全量；其余每 `LOG_SAMPLE_EVERY`
    /// 条全量一条（序号 0 起首条全量），被采样掉的计 `logs_sampled`。
    /// 决定性（计数器取模，无随机），单测锁定形态。
    pub fn sample_full_log(&self, status: u16) -> bool {
        if status >= 500 {
            return true;
        }
        if self
            .log_sample_seq
            .fetch_add(1, Ordering::Relaxed)
            .is_multiple_of(LOG_SAMPLE_EVERY)
        {
            true
        } else {
            self.logs_sampled.fetch_add(1, Ordering::Relaxed);
            false
        }
    }
}

impl Default for MetricsRegistry {
    fn default() -> Self {
        Self::new()
    }
}

// ---------------------------------------------------------------------------
// 子模块划分（OPT-R18 P2-1 第四批）
// ---------------------------------------------------------------------------
// 本文件曾 2097 行。切分轴是**数据流方向**，不是指标族：
//
// | 子模块 | 职责 |
// | --- | --- |
// | `record.rs` | **写入侧**：`observe*` / `note_*` / `set_*`，数据面与后台工人调用 |
// | `render.rs` | **读出侧**：`render()`（Prometheus 文本）＋ `serve_metrics()`（HTTP 服务） |
// | `tests.rs` | 全部单测 |
//
// **为什么不按指标族切**（免费线 / 熔断 / geo / 探测各一个文件）：这些族的记录
// 方法在 impl 里是**交织**的（免费线那组散落在 6 个不相邻区间），按族切会把一个
// impl 拆成十几段残片，读者要跳 6 个文件才能看完一个指标族的生命周期，反而更难读。
// 而"写入 vs 读出"是一次干净的一刀：写侧只改原子量、读侧只读原子量，两侧唯一的
// 契约是**指标名与标签**（见各方法注释与 `render()` 里的 HELP/TYPE 行）。
//
// **已知项（下一步）**：`render()` 本身 **538 行**，是一个逐族 `push_str` 的长函数。
// 它机械上可拆（每个指标族独立 append 到 `out`），但 Prometheus 文本输出对**行序**
// 敏感（部分测试逐字节断言），拆它属于行为敏感重构，故本轮只做纯搬移、不混入。

mod record;
mod render;

pub use render::serve_metrics;

#[cfg(test)]
mod tests;
