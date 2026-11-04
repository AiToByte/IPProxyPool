-- ============================================================================
-- OPT-R7 A1/A2：存量表排序键迁移（proxy_telemetry_log → 正确排序键 + 跳过索引）
-- ============================================================================
--
-- **执行方式：手动、一次性。执行前请先备份**（见 `docs/OPERATION.md` 的备份节），
-- 并确认 CH 版本 ≥ 22（`RENAME TABLE ... EXCHANGE`/原子多表 rename 需支持）。
--
-- ----------------------------------------------------------------------------
-- 【2026-10-28 更正：本文件此前被自动执行，现已移出自动执行目录】
-- ----------------------------------------------------------------------------
-- 本文件原先放在 `deploy/clickhouse/002_reorder.sql`，而 `docker-compose.yml`
-- 把**整个** `deploy/clickhouse/` 挂到容器的 `/docker-entrypoint-initdb.d`。
-- 文件头当时写着「不会被 `docker-entrypoint-initdb.d` 自动执行（它只跑
-- `001_schema.sql`）」——**这句是错的**。CH 官方 `/entrypoint.sh` 的实现是：
--
--     for f in /docker-entrypoint-initdb.d/*; do
--         case "$f" in
--             *.sql) echo "running $f"; clickhouse-client --multiquery < "$f" ;;
--         esac
--     done
--
-- 即**目录内所有 `.sql` 按文件名顺序全部执行**，不存在「只跑 001」这回事。
-- 实测证据：`/entrypoint.sh` 第 182~196 行原文即上述循环；本机
-- `proxy_telemetry_log_old`（本文件步骤 3 的产物）确实存在，说明它确实被执行过。
--
-- **危害**：本文件是**破坏性**的（`CREATE _v2` → `INSERT SELECT` 全表复制 →
-- `RENAME` 把主表换成 `_v2`）。自动执行意味着：
--   ① 每次**全新部署**都会多做一次全表复制与换名，留下一张 `_old` 垃圾表；
--   ② 更严重的是，`_v2` 的列清单是**写死的快照**——此后 `001_schema.sql`
--      新增的列（如 `003_trace_id.sql` 的 `trace_id`）会被 `RENAME` 后的
--      旧快照表**覆盖丢失**，且 `003` 已在 `002` 之前执行完，不会自动补回。
--      ⇒ 全新部署得到一个**没有 trace_id 的仓库表**，而代码侧照常写入 ⇒
--      插入失败或字段静默丢弃。这正是本仓反复出现的「声明即失实」变体：
--      迁移脚本声称手动，实际自动，且自动会破坏后加的 schema。
--
-- **处置**：移到 `deploy/clickhouse/manual/`（不在 initdb 挂载的顶层扫描范围内，
-- 官方 entrypoint 只遍历顶层 `*`，子目录不会被递归执行）。本文件此后**只**
-- 由运维在需要重建排序键时手动执行。
-- ----------------------------------------------------------------------------
--
-- ----------------------------------------------------------------------------
-- 为什么必须重建表（不能 ALTER）
-- ----------------------------------------------------------------------------
-- ClickHouse 的 `ORDER BY` 是**物理存储顺序**。MergeTree **不支持**
-- `ALTER TABLE ... MODIFY ORDER BY`，唯一途径是重建：新建表 → 搬数据 → 原子换名。
-- `OPTIMIZE TABLE ... FINAL` 虽可重写，但同样会重写全部分区且不改变排序键定义。
--
-- ----------------------------------------------------------------------------
-- 为什么要改（问题陈述）
-- ----------------------------------------------------------------------------
-- 旧排序键：ORDER BY (target_domain, provider, status_code, event_time)
-- 唯一查询（gateway/src/analytics.rs 的 `sla_sql`）：
--   WHERE provider = ? AND country = ? AND event_time >= now64(3) - INTERVAL 5 MINUTE
--
-- 错位三处（详见 001_schema.sql 文件头）：
--   ① target_domain 首位却不在 WHERE → 主键零剪枝；
--   ② provider 次位，主键仅在前缀约束时生效；
--   ③ country 根本不在排序键里。
-- ⇒ 每次 SLA 查询全分区扫描；vendor_arbitrage 每分钟并发 9 次，随 90 天累积劣化。
--
-- ----------------------------------------------------------------------------
-- 执行步骤（按序，逐步验证）
-- ----------------------------------------------------------------------------
--
-- 【步骤 0】前置校验：确认旧表存在、列定义与 001 一致。
--   若列定义已漂移，先停下核对，不要盲搬。
--
-- 【步骤 1】建新表（同 001 的新定义）。用 `_v2` 临时名，避免与旧表冲突。
--
-- 【步骤 2】搬数据。INSERT ... SELECT 显式列出列名（不依赖 SELECT *，防列序漂移）。
--   - 大表可加分批：`WHERE event_time >= ... AND event_time < ...` 循环，降低内存/IO 峰值。
--   - 单表数据量估算：网关 5xx 遥测约每请求 1 行；按 1000 QPS ≈ 86M 行/天，
--     90 天上限约 7.7B 行。生产环境务必分批 + 观察 `system.query_log`。
--
-- 【步骤 3】原子换名。RENAME TABLE ... TO ... 原子，旧表短暂保留为 _old。
--
-- 【步骤 4】核验：新旧表行数一致、SLA 查询走新表且命中主键。
--
-- 【步骤 5】清理：DROP 旧表。若需保留一段时间做对比，先不 DROP，观察数日后再删。
--
-- ----------------------------------------------------------------------------
-- 回滚
-- ----------------------------------------------------------------------------
-- 步骤 3 之前：直接 DROP _v2，无损（旧表未动）。
-- 步骤 3 之后：把 _old 改回原名即可（RENAME 反向），再 DROP 新表。数据仍在 _old。
-- ============================================================================

-- ----------------------------------------------------------------------------
-- 【步骤 0】前置校验
-- ----------------------------------------------------------------------------
SELECT 'step0: existing tables' AS step;
SELECT name, engine, total_rows FROM system.tables
WHERE database = 'proxy' AND name LIKE 'proxy_telemetry_log%'
ORDER BY name;

-- 确认列定义（应与 001_schema.sql 完全一致；不一致则 STOP）
-- DESCRIBE TABLE proxy.proxy_telemetry_log;

-- ----------------------------------------------------------------------------
-- 【步骤 1】建新表
-- ----------------------------------------------------------------------------
CREATE TABLE IF NOT EXISTS proxy.proxy_telemetry_log_v2 (
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
    INDEX idx_provider provider TYPE set(64) GRANULARITY 4,
    INDEX idx_country country TYPE set(512) GRANULARITY 4
) ENGINE = MergeTree()
PARTITION BY toYYYYMM(event_time)
ORDER BY (provider, country, event_time)
TTL toDateTime(event_time) + INTERVAL 90 DAY
SETTINGS index_granularity = 8192;

-- ----------------------------------------------------------------------------
-- 【步骤 2】搬数据
-- ----------------------------------------------------------------------------
-- 显式列名（不 SELECT *）：保证源/目标列序一致，将来加列也不会错位。
INSERT INTO proxy.proxy_telemetry_log_v2
    (event_time, client_ip, tenant_id, target_domain, out_ip, provider,
     tier, country, status_code, latency_ms, transferred_bytes, retry_count, error_message)
SELECT
     event_time, client_ip, tenant_id, target_domain, out_ip, provider,
     tier, country, status_code, latency_ms, transferred_bytes, retry_count, error_message
FROM proxy.proxy_telemetry_log;

-- 大表分批版本（把上面两条替换为循环执行；按月分批与 PARTITION BY 对齐）：
-- INSERT INTO proxy.proxy_telemetry_log_v2 (...)
-- SELECT ... FROM proxy.proxy_telemetry_log
-- WHERE toYYYYMM(event_time) = 202609;

-- ----------------------------------------------------------------------------
-- 【步骤 3】原子换名
-- ----------------------------------------------------------------------------
-- 原子操作：读流量要么全在旧表、要么全在新表，不会读到中间态。
RENAME TABLE proxy.proxy_telemetry_log   TO proxy.proxy_telemetry_log_old,
             proxy.proxy_telemetry_log_v2 TO proxy.proxy_telemetry_log;

-- ----------------------------------------------------------------------------
-- 【步骤 4】核验
-- ----------------------------------------------------------------------------
-- 行数一致（应完全相等）
SELECT 'old' AS tbl, count() AS rows FROM proxy.proxy_telemetry_log_old
UNION ALL
SELECT 'new' AS tbl, count() AS rows FROM proxy.proxy_telemetry_log
FORMAT TSV;

-- 新表排序键与索引已生效
SHOW CREATE TABLE proxy.proxy_telemetry_log;

-- SLA 查询是否走主键剪枝（read_rows 应远小于 partition 总行数）
-- EXPLAIN indexes = 1
-- SELECT countIf(status_code >= 200 AND status_code < 400) / count() * 100.0
-- FROM proxy.proxy_telemetry_log
-- WHERE provider = 'mock-a' AND country = 'US'
--   AND event_time >= now64(3, 'UTC') - INTERVAL 5 MINUTE;

-- ----------------------------------------------------------------------------
-- 【步骤 5】清理（确认步骤 4 通过后；建议观察数日再执行）
-- ----------------------------------------------------------------------------
-- DROP TABLE proxy.proxy_telemetry_log_old SYNC;
