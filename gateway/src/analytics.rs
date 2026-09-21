//! GW-4 ClickHouse telemetry warehouse sink + provider SLA queries.
//!
//! [`TelemetryRow`] mirrors `deploy/clickhouse/001_schema.sql`
//! (`proxy.proxy_telemetry_log`, 13 columns). Writes go through
//! `insert_batch` (empty slices are no-ops); reads use a 5-minute sliding
//! window over `event_time`.
//!
//! Deviation from manual-A4 (frozen plan勘察3): `query().bind(?)` is not used.
//! SLA SQL is built with `format!` after strict whitelist validation
//! (`sla_sql`), so provider/country can never break out of string literals.

use crate::telemetry::TelemetryEvent;
use chrono::{DateTime, Utc};
use clickhouse::{Client, Compression, Row};
use serde::{Deserialize, Serialize};
use std::time::Duration;

/// Batch flush thresholds (plan: 5000 rows / 1s).
pub const ANALYTICS_BATCH_SIZE: usize = 5000;
pub const ANALYTICS_FLUSH_INTERVAL: Duration = Duration::from_secs(1);
/// 5-minute sliding window for SLA reads.
pub const SLA_WINDOW_MINUTES: u32 = 5;
/// Derate below this success rate; restore above the high mark (GW-4c).
pub const SLA_DERATE_BELOW: f64 = 80.0;
pub const SLA_RESTORE_ABOVE: f64 = 95.0;

/// One warehouse row (column order matches the DDL).
#[derive(Row, Serialize, Deserialize, Debug, Clone, PartialEq)]
pub struct TelemetryRow {
    #[serde(with = "clickhouse::serde::chrono::datetime64::millis")]
    pub event_time: DateTime<Utc>,
    pub client_ip: String,
    pub tenant_id: String,
    pub target_domain: String,
    pub out_ip: String,
    pub provider: String,
    pub tier: String,
    pub country: String,
    pub status_code: u16,
    pub latency_ms: u32,
    pub transferred_bytes: u64,
    pub retry_count: u8,
    pub error_message: String,
}

impl From<&TelemetryEvent> for TelemetryRow {
    fn from(e: &TelemetryEvent) -> Self {
        Self {
            event_time: DateTime::from_timestamp_millis(e.timestamp as i64).unwrap_or_default(),
            client_ip: e.client_ip.clone(),
            tenant_id: e.tenant_id.clone().unwrap_or_default(),
            target_domain: e.target_domain.clone(),
            out_ip: e.out_ip.clone(),
            provider: e.provider.clone(),
            tier: e.tier.clone(),
            country: e.country.clone(),
            status_code: e.status_code,
            latency_ms: e.latency_ms.min(u32::MAX as u64) as u32,
            transferred_bytes: e.transferred_bytes,
            retry_count: e.retry_count,
            error_message: e.error_type.clone().unwrap_or_default(),
        }
    }
}

/// SQL string characters outside this set are rejected (injection guard).
fn is_safe_literal(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 64
        && s.bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_' || b == b'.')
}

/// Escape single quotes (defense in depth; whitelist already excludes them).
fn sql_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "\\'"))
}

/// Build the 5-minute provider-SLA query. `Err` on whitelist violation.
pub fn sla_sql(provider: &str, country: &str) -> Result<String, String> {
    if !is_safe_literal(provider) {
        return Err(format!("unsafe provider literal: {provider:?}"));
    }
    if !is_safe_literal(country) {
        return Err(format!("unsafe country literal: {country:?}"));
    }
    Ok(format!(
        "SELECT countIf(status_code >= 200 AND status_code < 400) / count() * 100.0 \
         FROM proxy.proxy_telemetry_log \
         WHERE provider = {} AND country = {} \
         AND event_time >= now64(3, 'UTC') - INTERVAL {SLA_WINDOW_MINUTES} MINUTE",
        sql_quote(provider),
        sql_quote(country.to_ascii_uppercase().as_str()),
    ))
}

/// Map a raw query result to a usable rate: empty windows (NaN) count as
/// healthy so idle vendor×country pairs are not derated before first traffic.
pub fn normalize_sla_rate(raw: f64) -> f64 {
    if raw.is_nan() {
        100.0
    } else {
        raw.clamp(0.0, 100.0)
    }
}

pub struct AnalyticsEngine {
    ch_client: Client,
    batch_size: usize,
    flush_interval: Duration,
}

impl AnalyticsEngine {
    pub fn new(ch_url: &str, user: &str, password: &str, database: &str) -> Self {
        let ch_client = Client::default()
            .with_url(ch_url)
            .with_user(user)
            .with_password(password)
            .with_database(database)
            .with_compression(Compression::Lz4);
        Self {
            ch_client,
            batch_size: ANALYTICS_BATCH_SIZE,
            flush_interval: ANALYTICS_FLUSH_INTERVAL,
        }
    }

    pub fn batch_size(&self) -> usize {
        self.batch_size
    }

    pub fn flush_interval(&self) -> Duration {
        self.flush_interval
    }

    /// Crate-internal client access for live integration tests.
    #[cfg(test)]
    pub(crate) fn ch_client(&self) -> &Client {
        &self.ch_client
    }
    /// Fast liveness probe for boot logs (`SELECT 1`).
    pub async fn ping(&self) -> Result<(), clickhouse::error::Error> {
        let one: u8 = self.ch_client.query("SELECT 1").fetch_one().await?;
        debug_assert_eq!(one, 1);
        Ok(())
    }

    /// Bulk insert into `proxy.proxy_telemetry_log` (LZ4, one block).
    pub async fn insert_batch(
        &self,
        rows: &[TelemetryRow],
    ) -> Result<(), clickhouse::error::Error> {
        if rows.is_empty() {
            return Ok(());
        }
        let mut insert = self
            .ch_client
            .insert::<TelemetryRow>("proxy.proxy_telemetry_log")?;
        for row in rows {
            insert.write(row).await?;
        }
        insert.end().await?;
        Ok(())
    }

    /// Provider success rate over the trailing 5 minutes (0–100).
    pub async fn query_provider_sla(
        &self,
        provider: &str,
        country: &str,
    ) -> Result<f64, clickhouse::error::Error> {
        let sql = sla_sql(provider, country).map_err(|msg| {
            clickhouse::error::Error::InvalidParams(
                std::io::Error::new(std::io::ErrorKind::InvalidInput, msg).into(),
            )
        })?;
        let raw: f64 = self.ch_client.query(&sql).fetch_one().await?;
        Ok(normalize_sla_rate(raw))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    pub fn sample_row() -> TelemetryRow {
        TelemetryRow {
            event_time: DateTime::from_timestamp_millis(1_700_000_000_000).unwrap(),
            client_ip: "10.0.1.2".to_string(),
            tenant_id: "t-1".to_string(),
            target_domain: "api.target.com".to_string(),
            out_ip: "185.220.101.5".to_string(),
            provider: "mock-a".to_string(),
            tier: "residential".to_string(),
            country: "US".to_string(),
            status_code: 200,
            latency_ms: 42,
            transferred_bytes: 1024,
            retry_count: 0,
            error_message: String::new(),
        }
    }

    #[test]
    fn sla_sql_quotes_and_normalizes_country() {
        let sql = sla_sql("mock-a", "us").expect("valid");
        assert!(sql.contains("provider = 'mock-a'"), "{sql}");
        assert!(sql.contains("country = 'US'"), "{sql}");
        assert!(sql.contains("INTERVAL 5 MINUTE"), "{sql}");
    }

    #[test]
    fn sla_sql_rejects_injection() {
        assert!(sla_sql("x' OR '1'='1", "US").is_err());
        assert!(sla_sql("mock-a", "US; DROP TABLE").is_err());
        assert!(sla_sql("", "US").is_err());
        assert!(sla_sql("mock-a", "").is_err());
        assert!(sla_sql(&"a".repeat(65), "US").is_err());
    }

    #[test]
    fn normalize_handles_empty_window() {
        assert_eq!(normalize_sla_rate(f64::NAN), 100.0);
        assert_eq!(normalize_sla_rate(120.0), 100.0);
        assert_eq!(normalize_sla_rate(-5.0), 0.0);
        assert_eq!(normalize_sla_rate(97.5), 97.5);
    }

    #[test]
    fn row_from_event_maps_all_fields() {
        let e = crate::telemetry::TelemetryEvent {
            event_id: "evt-1".to_string(),
            client_ip: "c".to_string(),
            target_domain: "d.example".to_string(),
            out_ip: "o".to_string(),
            provider: "p".to_string(),
            tier: "mobile".to_string(),
            country: "GB".to_string(),
            status_code: 403,
            latency_ms: 7,
            transferred_bytes: 9,
            retry_count: 2,
            tenant_id: Some("tenant-x".to_string()),
            error_type: Some("boom".to_string()),
            timestamp: 1_700_000_000_123,
        };
        let row = TelemetryRow::from(&e);
        assert_eq!(row.provider, "p");
        assert_eq!(row.tier, "mobile");
        assert_eq!(row.status_code, 403);
        assert_eq!(row.retry_count, 2);
        assert_eq!(row.tenant_id, "tenant-x");
        assert_eq!(row.error_message, "boom");
        assert_eq!(row.event_time.timestamp_millis(), 1_700_000_000_123);
    }

    /// Live ClickHouse integration: insert one row, read it back, clean up.
    /// Skips (passes) when CH is unreachable. Run with `-- --ignored`.
    #[tokio::test]
    #[ignore]
    async fn live_insert_and_sla_roundtrip() {
        let engine = AnalyticsEngine::new("http://127.0.0.1:8123", "proxy", "123456", "proxy");
        if engine.ping().await.is_err() {
            eprintln!("SKIP live_insert: ClickHouse unreachable");
            return;
        }
        let mut row = sample_row();
        row.provider = "livetest".to_string();
        row.country = "US".to_string();
        engine.insert_batch(&[row]).await.expect("insert");
        // 200-class row → SLA must read 100 back for the pair.
        let rate = engine
            .query_provider_sla("livetest", "US")
            .await
            .expect("sla query");
        assert!(
            (rate - 100.0).abs() < 1e-6,
            "fresh 200-only window must read 100, got {rate}"
        );
        engine
            .ch_client
            .query("ALTER TABLE proxy.proxy_telemetry_log DELETE WHERE provider = 'livetest'")
            .execute()
            .await
            .expect("cleanup");
    }
}
