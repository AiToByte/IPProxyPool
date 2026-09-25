-- GW-R1 ClickHouse telemetry warehouse schema
-- Applied via docker-entrypoint-initdb.d on first `docker compose up`
-- DB `proxy` is created by CLICKHOUSE_DB env; keep CREATE DATABASE IF NOT EXISTS for bare-metal runs.

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
    error_message String
) ENGINE = MergeTree()
PARTITION BY toYYYYMM(event_time)
ORDER BY (target_domain, provider, status_code, event_time)
-- C4：遥测只做 90 天滚动窗口（运维产品化；TTL 到期由后台合并异步清理，
-- 非精确整点删除，报表按 event_time 过滤口径不变）。
-- 写法注记：event_time 是 DateTime64，直接 `+ INTERVAL` 会报 BAD_TTL_EXPRESSION
--（CH 24 要求 TTL 产出 Date/DateTime），故包一层 toDateTime，语义等价。
TTL toDateTime(event_time) + INTERVAL 90 DAY
SETTINGS index_granularity = 8192;

-- C4：存量表（2026-09-24 前建表、无 TTL）就地补 TTL，数据不动：
-- ALTER TABLE proxy.proxy_telemetry_log MODIFY TTL toDateTime(event_time) + INTERVAL 90 DAY;

-- 5-minute sliding-window SLA helper (used by vendor_arbitrage, GW-4):
-- SELECT countIf(status_code >= 200 AND status_code < 400) / count() * 100.0 AS success_rate
-- FROM proxy.proxy_telemetry_log
-- WHERE provider = '<vendor>' AND country = '<cc>'
--   AND event_time >= now64(3, 'UTC') - INTERVAL 5 MINUTE;
