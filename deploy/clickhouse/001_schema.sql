-- GW-R1 ClickHouse telemetry warehouse schema（OPT-R7 A1/A2 重构）
-- Applied via docker-entrypoint-initdb.d on first `docker compose up`
-- DB `proxy` is created by CLICKHOUSE_DB env; keep CREATE DATABASE IF NOT EXISTS for bare-metal runs.
--
-- ============================================================================
-- OPT-R7 A1：排序键重构说明（为什么是 (provider, country, event_time)）
-- ============================================================================
-- 旧排序键：ORDER BY (target_domain, provider, status_code, event_time)
--
-- 与唯一查询模式的三处错位（本轮已用线上 `SHOW CREATE TABLE` 实证）：
--   ① `target_domain` 排在第 1 位，却**不在任何 WHERE 子句里** → 主键索引零剪枝；
--   ② `provider` 虽在第 2 位，但主键索引仅在**前缀被约束**时才生效；
--   ③ `country` **根本不在排序键中**。
--
-- 唯一查询模式见 `gateway/src/analytics.rs` 的 `sla_sql`：
--   SELECT countIf(status_code >= 200 AND status_code < 400) / count() * 100.0
--   FROM proxy.proxy_telemetry_log
--   WHERE provider = ? AND country = ? AND event_time >= now64(3) - INTERVAL 5 MINUTE
--
-- ⇒ 旧排序键使每次 SLA 查询退化为**分区全扫描**；而 `vendor_arbitrage` 每分钟并发
--   9 次（3 供应商 × 3 国家），随 90 天 TTL 累积线性劣化。
--
-- 新排序键与查询完全对齐：
--   - `provider` 提升为首位：唯一查询的第一过滤条件，LowCardinality，取值数十级；
--   - `country` 第二：唯一查询的第二过滤条件，LowCardinality，取值数百级；
--   - `event_time` 第三：时间局部性，且 90 天 TTL 的滚动清理依赖它。
--
-- **为何去掉 `status_code`**：它有 ~600 个取值，夹在 `event_time` 之前会把同一
-- 时间窗的数据打散到 600 个排序组，破坏时间局部性；而 SLA 查询用的是
-- `countIf(status_code ...)`（**聚合**而非过滤），根本不需要它进排序键。
--
-- **为何 `target_domain` 降为普通列**：它不在任何 WHERE 里；将来若出现按域查询的
-- 需求，用下面的 `set()` 跳过索引解决，不必为它重排全表。
--
-- **为何不能原地 ALTER**：ClickHouse 的 ORDER BY 是**物理存储顺序**，
-- MergeTree 不支持 `ALTER TABLE ... MODIFY ORDER BY`。唯一途径是重建表
-- （`OPTIMIZE ... FINAL` 会重写全部分区）。存量迁移见 `002_reorder.sql`。
-- ============================================================================

CREATE DATABASE IF NOT EXISTS proxy;

CREATE TABLE IF NOT EXISTS proxy.proxy_telemetry_log (
    event_time DateTime64(3, 'UTC'),
    client_ip String,
    tenant_id LowCardinality(String),
    target_domain LowCardinality(String),
    out_ip String,
    provider LowCardinality(String),
    tier LowCardinality(String),
    country LowCardinality(String),
    status_code UInt16,
    latency_ms UInt32,
    transferred_bytes UInt64,
    retry_count UInt8,
    error_message String,
    -- OPT-R7 A2：数据跳过索引。
    -- 为什么用 `set()` 而不是 `minmax`/`bloom_filter`：
    --   * provider / country 都是**低基数离散字典**（数十 / 数百个取值），
    --     `set(N)` 直接存取值集合；查询条件不命中集合时可整 granule 跳过。
    --   * `minmax` 对离散字典无效——min/max 会覆盖全值域，永远「命中」。
    --   * `bloom_filter` 面向高基数数值/字符串列，这里用不上。
    -- 为什么放在 `PARTITION BY`/`ORDER BY` 之外（而非并入排序键）：
    --   排序键已重排数据，再叠索引会显著增加写放大；而 SLA 查询是
    --   **每分钟 9 次的高频查询**，索引收益远大于成本。
    -- GRANULARITY 4：每 4 个 granule 建一个索引条目，兼顾跳过量与索引体积。
    INDEX idx_provider provider TYPE set(64) GRANULARITY 4,
    INDEX idx_country country TYPE set(512) GRANULARITY 4
) ENGINE = MergeTree()
PARTITION BY toYYYYMM(event_time)
-- OPT-R7 A1：与 `analytics.rs` 的 `sla_sql` 唯一查询模式对齐（详见文件头）。
ORDER BY (provider, country, event_time)
-- C4：遥测只做 90 天滚动窗口（运维产品化；TTL 到期由后台合并异步清理，
-- 非精确整点删除，报表按 event_time 过滤口径不变）。
-- 写法注记：event_time 是 DateTime64，直接 `+ INTERVAL` 会报 BAD_TTL_EXPRESSION
--（CH 24 要求 TTL 产出 Date/DateTime），故包一层 toDateTime，语义等价。
TTL toDateTime(event_time) + INTERVAL 90 DAY
SETTINGS index_granularity = 8192;

-- C4（保留注记）：存量表（2026-09-24 前建表、无 TTL）就地补 TTL，数据不动：
-- ALTER TABLE proxy.proxy_telemetry_log MODIFY TTL toDateTime(event_time) + INTERVAL 90 DAY;
-- OPT-R7 注记：排序键**没有**对应的就地 ALTER 途径，须走 `002_reorder.sql` 重建。

-- 5-minute sliding-window SLA helper (used by vendor_arbitrage, GW-4):
-- SELECT countIf(status_code >= 200 AND status_code < 400) / count() * 100.0 AS success_rate
-- FROM proxy.proxy_telemetry_log
-- WHERE provider = '<vendor>' AND country = '<cc>'
--   AND event_time >= now64(3, 'UTC') - INTERVAL 5 MINUTE;
