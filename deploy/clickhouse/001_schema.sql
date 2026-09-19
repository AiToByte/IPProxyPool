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
SETTINGS index_granularity = 8192;

-- 5-minute sliding-window SLA helper (used by vendor_arbitrage, GW-4):
-- SELECT countIf(status_code >= 200 AND status_code < 400) / count() * 100.0 AS success_rate
-- FROM proxy.proxy_telemetry_log
-- WHERE provider = '<vendor>' AND country = '<cc>'
--   AND event_time >= now64(3, 'UTC') - INTERVAL 5 MINUTE;
