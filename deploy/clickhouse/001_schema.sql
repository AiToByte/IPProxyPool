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
    -- OPT-R18 P1-1 修正（2026-10-28）：GW-6 链路追踪 ID。
    --
    -- **为什么这一列曾经不存在**：`TelemetryEvent.trace_id` 一直有（Redis
    -- Stream 的 JSON payload 里带着），但 `TelemetryRow` 没有对应字段、
    -- 本表也没有这一列 ⇒ `From<&TelemetryEvent>` 转换时该字段被**完整丢弃**。
    -- 于是「日志侧有 trace_id、仓库侧没有」——排障时「CH 里有这条 502、
    -- 日志里对不上是哪行」这个**最初要解决的问题依然存在**，
    -- 「同键互查」在落地仓库这一半是失实的。
    --
    -- **为什么用普通 `String` 而不是 `LowCardinality(String)`**：本表其余
    -- 维度列（tenant_id/provider/tier/country）都是低基数字典，LowCardinality
    -- 用字典编码省空间；而 trace_id 是 `{unix_ms}-{pid}-{seq}`，**每请求一个
    -- 取值**，基数与行数同阶——低基数优化在这里只会退化（字典几乎全 miss）。
    --
    -- **为什么不进 ORDER BY**：排序键与唯一高频查询（`analytics.rs::sla_sql`
    -- 的 provider+country+时间窗）对齐。trace_id 是**点查键**而非过滤前缀，
    -- 塞进排序键会让 SLA 查询的主键剪枝彻底失效（provider/country 不再是
    -- 前缀）。为排障便利牺牲主查询性能不划算，点查靠下面的 bloom_filter。
    trace_id String,
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
    INDEX idx_country country TYPE set(512) GRANULARITY 4,
    -- OPT-R18 P1-1：trace_id 点查索引。
    -- **必须用 bloom_filter 而非 set()**：trace_id 高基数（每请求一值），
    -- set 存的是「取值集合」，高基数下每个 granule 的集合会膨胀到与数据同阶，
    -- 索引体积失控。bloom_filter 正是为「高基数 + 等值点查」设计的：
    -- 误判率可调、体积固定。0.001 = 0.1% 假阳性（排障场景可接受）。
    -- GRANULARITY 1：点查场景索引越细剪枝越准（代价是索引条目变多）。
    -- 没有它，按 trace_id 查 90 天 TTL 的表会退化为**分区全扫描**——
    -- 而排障恰恰是最需要秒级返回的时候，查 30 秒的排障接口等于没有。
    INDEX idx_trace_id trace_id TYPE bloom_filter(0.001) GRANULARITY 1
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
-- OPT-R7 注记：排序键**没有**对应的就地 ALTER 途径，须走
-- `manual/002_reorder.sql` 重建（**手动**，见下方目录约定）。
-- OPT-R18 P1-1 注记：存量表补 `trace_id` 列走 `manual/003_trace_id.sql`
-- （`ADD COLUMN IF NOT EXISTS`，元数据操作、不重写存量分区、瞬间完成）。

-- ----------------------------------------------------------------------------
-- 【目录约定】本目录下**只有** `001_schema.sql` 会被自动执行
-- ----------------------------------------------------------------------------
-- `docker-compose.yml` 把整个 `deploy/clickhouse/` 挂到容器的
-- `/docker-entrypoint-initdb.d`，而 CH 官方 `/entrypoint.sh` 的实现是：
--
--     for f in /docker-entrypoint-initdb.d/*; do
--         case "$f" in *.sql) clickhouse-client --multiquery < "$f" ;; esac
--     done
--
-- 即**顶层所有 `.sql` 按文件名顺序全部自动执行**（子目录不递归）。
--
-- 因此本目录的纪律是：
--   * `001_schema.sql` —— 唯一被自动执行的脚本，必须是**幂等**的
--     （`CREATE ... IF NOT EXISTS`），且必须是**当前**权威 schema；
--   * `manual/` —— **只**放运维手动执行的一次性迁移脚本，绝不自动跑。
--
-- 这条纪律是**实测踩出来的**：`002_reorder.sql` 曾放在本目录顶层，其文件头
-- 声称「不会被 initdb 自动执行（它只跑 001）」——**那是错的**。它是破坏性的
-- （`INSERT SELECT` 全表复制 + `RENAME` 换名），自动执行会导致：全新部署多做
-- 一次全表复制、留下垃圾表；更严重的是它的 `_v2` 列清单是**写死的快照**，
-- 会把此后新增的列（如 `trace_id`）**覆盖丢失**，得到一个没有 trace_id 的
-- 仓库表而代码侧照常写入。现已移入 `manual/`。
-- ----------------------------------------------------------------------------

-- 5-minute sliding-window SLA helper (used by vendor_arbitrage, GW-4):
-- SELECT countIf(status_code >= 200 AND status_code < 400) / count() * 100.0 AS success_rate
-- FROM proxy.proxy_telemetry_log
-- WHERE provider = '<vendor>' AND country = '<cc>'
--   AND event_time >= now64(3, 'UTC') - INTERVAL 5 MINUTE;
