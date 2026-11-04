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
    /// GW-6 链路追踪 ID（OPT-R18 P1-1 修正）。
    ///
    /// **这个字段曾经不存在**：`TelemetryEvent.trace_id` 一直有（随 Redis
    /// Stream 的 JSON payload 传递），但本结构体没有对应字段 ⇒ `From` 转换时
    /// 被**完整丢弃**，CH 表也没有该列。于是 `docs/CAPABILITY-AUDIT.md` §4 与
    /// 计划文档声称的「trace_id 与 CH 遥测同键互查」在仓库这一半是**失实**的：
    /// 日志侧有 ID、仓库侧无 ID，排障时两边对不上。
    ///
    /// 字段位置**必须与 DDL 的列序一致**（`clickhouse` crate 的 `Row` derive
    /// 按字段声明顺序生成 `COLUMN_NAMES`，`insert` 用它拼
    /// `INSERT INTO t(col...) FORMAT RowBinary`）——故此处紧跟 `event_time`，
    /// 与 `001_schema.sql` 的 `trace_id` 位置对应。
    pub trace_id: String,
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
            // 直接透传：`TelemetryEvent.trace_id` 由 `gateway.rs` 的
            // `next_trace_id()`（或下游传入的 `X-Trace-Id`）填写，此处**不得**
            // 重新生成——重新生成会让「日志里的 ID」与「仓库里的 ID」不是同一个，
            // 互查能力名存实亡。空串如实表达「该事件产生时未打点」。
            trace_id: e.trace_id.clone(),
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
    /// 默认批次构造器（5000 行 / 1s，与 OPT-R5 口径一致）。
    /// 保留供向后兼容；线上 main 已改用 `new_with_batch`（按 `CH_BATCH_SIZE` env）。
    #[allow(dead_code)]
    pub fn new(ch_url: &str, user: &str, password: &str, database: &str) -> Self {
        Self::new_with_batch(ch_url, user, password, database, ANALYTICS_BATCH_SIZE)
    }

    /// 批次可配构造器（供 main.rs 按 `CH_BATCH_SIZE` env 装配；下限 1）。
    pub fn new_with_batch(
        ch_url: &str,
        user: &str,
        password: &str,
        database: &str,
        batch_size: usize,
    ) -> Self {
        let ch_client = Client::default()
            .with_url(ch_url)
            .with_user(user)
            .with_password(password)
            .with_database(database)
            .with_compression(Compression::Lz4);
        Self {
            ch_client,
            batch_size: batch_size.max(1),
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
            trace_id: "1700000000000-1-1".to_string(),
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
            // OPT-R18 P1-1 修正：此处**必须**用非空 trace_id。
            // 原先写 `String::new()`，而期望值也是空串 ⇒ 即使 `From` 转换
            // 完全丢弃 trace_id，本测试**照样通过**——零鉴别力。
            // 这正是本仓反复出现的「测试证明了自己，没证明行为」。
            trace_id: "1700000000123-4242-7".to_string(),
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
        // 透传而非重新生成：两边必须是**同一个** ID，否则「同键互查」不成立。
        assert_eq!(
            row.trace_id, "1700000000123-4242-7",
            "trace_id 必须原样透传到仓库行；重新生成会让日志与仓库对不上"
        );
        assert_eq!(row.provider, "p");
        assert_eq!(row.tier, "mobile");
        assert_eq!(row.status_code, 403);
        assert_eq!(row.retry_count, 2);
        assert_eq!(row.tenant_id, "tenant-x");
        assert_eq!(row.error_message, "boom");
        assert_eq!(row.event_time.timestamp_millis(), 1_700_000_000_123);
    }

    /// `trace_id` 缺失（老 JSON 无此字段）时必须是空串，**不得 panic、不得
    /// 编造**。空串如实表达「该事件产生时未打点」——排障时看到空串应理解为
    /// 「这条老数据没有 trace」，而不是「trace 丢了」。
    #[test]
    fn row_from_event_tolerates_missing_trace_id() {
        let e = crate::telemetry::TelemetryEvent {
            event_id: "evt-2".to_string(),
            trace_id: String::new(),
            client_ip: "c".to_string(),
            target_domain: "d.example".to_string(),
            out_ip: "o".to_string(),
            provider: "p".to_string(),
            tier: "dc".to_string(),
            country: "US".to_string(),
            status_code: 200,
            latency_ms: 1,
            transferred_bytes: 1,
            retry_count: 0,
            tenant_id: None,
            error_type: None,
            timestamp: 1_700_000_000_000,
        };
        let row = TelemetryRow::from(&e);
        assert_eq!(row.trace_id, "", "缺失的 trace_id 必须是空串，不得编造");
    }

    /// **列集合契约**：`TelemetryRow` 的字段集合必须与 `001_schema.sql` 的
    /// 列集合**完全相等**（双向）。
    ///
    /// # 为什么是「集合」而不是「顺序」
    ///
    /// 初版写的是「顺序必须一致」，理由写的是「顺序错位会静默串列」——**那是错的**，
    /// 已实测推翻：`clickhouse` crate 的 `insert` 拼的是
    /// `INSERT INTO t(col1, col2, ...) FORMAT RowBinary`，**列名是显式列出的**，
    /// 服务端**按名映射**，故结构体字段顺序与 DDL 列序无关。
    /// 实证：把 `trace_id` 从 `event_time` 之后移到 `client_ip` 之后，
    /// live 插入与按 trace_id 点查**照常成功**。
    ///
    /// # 两个方向的不对称（已用 CH 24 实测）
    ///
    /// | 不一致方向 | CH 的行为 | 危害 |
    /// | --- | --- | --- |
    /// | 结构体有、DDL 没有 | **响亮报错** `NO_SUCH_COLUMN_IN_TABLE`（Code 16） | 插入全失败，立刻可见 |
    /// | DDL 有、结构体没有 | **静默补默认值**（`String` → `''`），无任何错误 | 该列永远是空/零，**没人会发现** |
    ///
    /// 危险的是**第二种**：表里有一列永远不写、查询永远查不到，而插入"成功"。
    /// 这正是本轮 `trace_id` 缺陷的形态——字段在 `TelemetryEvent` 里有、在
    /// `TelemetryRow` 里没有，于是那一列的值被**静默丢弃**。
    /// 故断言必须是**双向集合相等**，且必须解释清哪个方向是静默的。
    ///
    /// # 为什么解析真实 DDL 而不是手抄常量
    ///
    /// 手抄的常量会与 SQL 各自漂移，等于给「两边一致」写了个自证式断言。
    /// 这里用 `include_str!` 编译期嵌入真实 DDL 文件——SQL 一改，断言输入就跟着变。
    #[test]
    fn row_columns_match_ddl_columns() {
        /// 编译期嵌入真实 DDL——文件一改，本测试的输入就跟着变。
        const DDL: &str = include_str!("../../deploy/clickhouse/001_schema.sql");

        let ddl_cols = parse_telemetry_log_columns(DDL);
        assert!(
            !ddl_cols.is_empty(),
            "没能从 001_schema.sql 解析出列名——解析器与 DDL 格式已脱节，\
             本测试会变成假绿，必须先修解析器"
        );

        use clickhouse::Row as _;
        let mut struct_cols: Vec<&str> = TelemetryRow::COLUMN_NAMES.to_vec();
        let mut ddl_sorted = ddl_cols.clone();

        // 方向 ①：结构体有、DDL 没有 ⇒ CH 会响亮报错，这里提前拦住。
        for c in &struct_cols {
            assert!(
                ddl_sorted.iter().any(|d| d == c),
                "`TelemetryRow` 有字段 `{c}`，但 001_schema.sql 没有该列 ⇒ \
                 每次插入都会被 CH 拒绝（NO_SUCH_COLUMN_IN_TABLE）"
            );
        }
        // 方向 ②：DDL 有、结构体没有 ⇒ **静默**永远写不进值，最危险。
        let struct_owned: Vec<String> = struct_cols.iter().map(|s| s.to_string()).collect();
        for d in &ddl_sorted {
            assert!(
                struct_owned.iter().any(|s| s == d),
                "001_schema.sql 有列 `{d}`，但 `TelemetryRow` 没有对应字段 ⇒ \
                 该列会被 CH 静默补默认值（String → 空串），永远查不到数据，\
                 且插入**不报错**。这正是 trace_id 缺陷的形态。"
            );
        }

        struct_cols.sort_unstable();
        ddl_sorted.sort();
        assert_eq!(
            struct_cols, ddl_sorted,
            "字段集合与 DDL 列集合必须完全相等（顺序无关）"
        );
    }

    /// 从 `001_schema.sql` 里解析 `proxy_telemetry_log` 的列名（按声明顺序）。
    ///
    /// 只做本仓需要的最小解析，刻意保持简单：
    /// * 定位 `CREATE TABLE ... proxy.proxy_telemetry_log (`；
    /// * 按**括号深度**截到匹配的 `)`（列定义里可能有 `DateTime64(3, 'UTC')`、
    ///   `set(64)` 这类嵌套括号，不能用「第一行以 ) 开头」这种脆弱判据）；
    /// * 逐行剥掉 `--` 注释、取首个 token 作列名；
    /// * 跳过 `INDEX ...` 行（它们是索引声明，不是列）。
    fn parse_telemetry_log_columns(ddl: &str) -> Vec<String> {
        let start = ddl
            .find("CREATE TABLE IF NOT EXISTS proxy.proxy_telemetry_log")
            .expect("001_schema.sql 里找不到 proxy_telemetry_log 的 CREATE TABLE");
        let after = &ddl[start..];
        let open = after.find('(').expect("CREATE TABLE 后应有列定义括号");

        let mut depth = 0usize;
        let mut end = None;
        for (i, ch) in after[open..].char_indices() {
            match ch {
                '(' => depth += 1,
                ')' => {
                    depth -= 1;
                    if depth == 0 {
                        end = Some(open + i);
                        break;
                    }
                }
                _ => {}
            }
        }
        let body = &after[open + 1..end.expect("列定义括号未闭合")];

        body.lines()
            .map(|line| {
                // 剥掉行尾注释：本 DDL 的注释全是整行 `--`，但行内注释也一并处理。
                let code = match line.find("--") {
                    Some(i) => &line[..i],
                    None => line,
                };
                code.trim().to_string()
            })
            .filter(|l| !l.is_empty())
            .filter(|l| !l.to_ascii_uppercase().starts_with("INDEX "))
            .filter_map(|l| {
                // 列名 = 首个空白分隔的 token，且必须是合法标识符。
                let name = l.split_whitespace().next()?.trim_end_matches(',');
                let ok =
                    !name.is_empty() && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_');
                if ok {
                    Some(name.to_string())
                } else {
                    None
                }
            })
            .collect()
    }

    /// Live ClickHouse integration: insert one row, read it back, clean up.
    /// Skips (passes) when CH is unreachable. Run with `-- --ignored`.
    #[tokio::test]
    #[ignore]
    async fn live_insert_and_sla_roundtrip() {
        // OPT-R4 C6：live 测试读 CLICKHOUSE_*（带密 compose），缺省沿用开发缺省。
        let engine = AnalyticsEngine::new(
            &std::env::var("CLICKHOUSE_URL")
                .unwrap_or_else(|_| "http://127.0.0.1:8123".to_string()),
            &std::env::var("CLICKHOUSE_USER").unwrap_or_else(|_| "proxy".to_string()),
            &std::env::var("CLICKHOUSE_PASSWORD").unwrap_or_else(|_| "123456".to_string()),
            &std::env::var("CLICKHOUSE_DB").unwrap_or_else(|_| "proxy".to_string()),
        );
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

    /// Live：`trace_id` 必须真的落到 ClickHouse 并能按它点查回来
    /// （OPT-R18 P1-1 的「同键互查」端到端验证）。
    ///
    /// 为什么单测不够：`row_from_event_maps_all_fields` 只证明**内存里**的
    /// 结构体带着 trace_id；它证明不了「这一列在 CH 表里存在」「插入没串列」
    /// 「按它查得回来」。而这三件事任何一件失败，运维在排障时都拿不到那条记录——
    /// 且**都不会让任何非 live 测试变红**（插入失败会被 ChSink 当故障重试，
    /// 串列则静默错位）。
    ///
    /// 需真实 ClickHouse：`-- --ignored`，配置见 `CLICKHOUSE_*` 环境变量。
    /// CH 不可达时 SKIP（与同文件其它 live 测试同口径）。
    #[tokio::test]
    #[ignore]
    async fn live_trace_id_roundtrip() {
        let engine = AnalyticsEngine::new(
            &std::env::var("CLICKHOUSE_URL")
                .unwrap_or_else(|_| "http://127.0.0.1:8123".to_string()),
            &std::env::var("CLICKHOUSE_USER").unwrap_or_else(|_| "proxy".to_string()),
            &std::env::var("CLICKHOUSE_PASSWORD").unwrap_or_else(|_| "123456".to_string()),
            &std::env::var("CLICKHOUSE_DB").unwrap_or_else(|_| "proxy".to_string()),
        );
        if engine.ping().await.is_err() {
            eprintln!("SKIP live_trace_id: ClickHouse unreachable");
            return;
        }

        // 用时间戳造一个必然唯一的 trace_id，避免与历史数据撞车。
        let trace = format!(
            "livetest-{}-{}",
            std::process::id(),
            TelemetryEvent::now_unix_ms()
        );
        let mut row = sample_row();
        row.provider = "livetest-trace".to_string();
        row.trace_id = trace.clone();
        engine.insert_batch(&[row]).await.expect("insert");

        // 直接按 trace_id 点查（这正是 OPERATION §4.3 给运维的查询形状）。
        let got: u64 = engine
            .ch_client
            .query("SELECT count() FROM proxy.proxy_telemetry_log WHERE trace_id = ?")
            .bind(&trace)
            .fetch_one()
            .await
            .expect("trace_id point query");
        assert_eq!(
            got, 1,
            "按 trace_id `{trace}` 应恰好查到 1 行；查到 {got} 行说明\
             trace_id 没落库、被串列或被丢弃"
        );

        engine
            .ch_client
            .query("ALTER TABLE proxy.proxy_telemetry_log DELETE WHERE provider = 'livetest-trace'")
            .execute()
            .await
            .expect("cleanup");
    }
}
