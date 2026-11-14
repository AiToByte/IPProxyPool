//! GW-6 Redis Stream 分片纯工具预留（SPIKE-R3 / 集群部署待放行）。
//!
//! 当前启动仅支持 `REDIS_STREAM_SHARDS=1`，未设置时也默认 1：单 stream
//! `stream:proxy:telemetry`，与现有遥测发布、落库及熔断消费行为一致。
//! 临时拒绝多分片：publisher 已能分片写入，但 ChSinkWorker / CircuitBreaker
//! 仍只消费单 stream；放行 >1 会使分片遥测无人消费，造成落库和熔断失效。
//!
//! 以下 hash、stream key、消费组名纯工具保留供将来消费端接通后使用，
//! 不代表当前可通过配置启用多分片。Redis Cluster 的 key / hash tag 策略
//! 同样需要在未来接入时验证。

use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};

/// 缺省分片数：1（单 stream，兼容现有行为）。
#[allow(dead_code)]
pub const REDIS_STREAM_SHARDS_DEFAULT: usize = 1;
/// Stream key 基名（shards=1 时即为最终 key）。
#[allow(dead_code)]
pub const TELEMETRY_STREAM_BASE: &str = "stream:proxy:telemetry";
/// 消费组名前缀（shards=1 时：`ch_sink_group`；shards>1 时：`ch_sink_group:{i}`）。
#[allow(dead_code)]
pub const SINK_GROUP_PREFIX: &str = "ch_sink_group";
/// 消费组名前缀（shards=1 时：`circuit_breaker_group`；shards>1 时：`circuit_breaker_group:{i}`）。
#[allow(dead_code)]
pub const CB_GROUP_PREFIX: &str = "circuit_breaker_group";

/// 启动时解析并校验 `REDIS_STREAM_SHARDS`。
///
/// 当前只有单 stream 消费链完整，因此显式拒绝 0、空值、非法值及 >1；
/// 未设置时返回兼容默认值 1。错误文本固定，不回显环境变量内容或其它 secret。
pub fn parse_redis_stream_shards(
    value: Result<String, std::env::VarError>,
) -> Result<usize, &'static str> {
    const CONFIG_ERROR: &str =
        "REDIS_STREAM_SHARDS must be exactly 1 (single stream is temporarily supported)";

    let raw = match value {
        Ok(raw) if !raw.is_empty() => raw,
        Ok(_) | Err(std::env::VarError::NotUnicode(_)) => return Err(CONFIG_ERROR),
        Err(std::env::VarError::NotPresent) => return Ok(REDIS_STREAM_SHARDS_DEFAULT),
    };

    match raw.parse::<usize>() {
        Ok(1) => Ok(1),
        Ok(_) | Err(_) => Err(CONFIG_ERROR),
    }
}

/// 预留的纯分片工具：按 `target_domain` hash 到 shard index；不负责启动配置校验。
#[derive(Debug, Clone, Copy, Default)]
#[allow(dead_code)]
pub struct RedisStreamSharder {
    shards: usize,
}

impl RedisStreamSharder {
    pub fn new(shards: usize) -> Self {
        Self {
            shards: shards.max(1),
        }
    }

    /// 目标域所属 shard index [0, shards)。
    pub fn shard_for_domain(&self, domain: &str) -> usize {
        let mut hasher = DefaultHasher::new();
        domain.hash(&mut hasher);
        (hasher.finish() as usize) % self.shards
    }

    /// 第 i 个分片的 stream key（shards=1 时返回基名，兼容现有行为）。
    pub fn stream_key_for(&self, i: usize) -> String {
        if self.shards == 1 {
            TELEMETRY_STREAM_BASE.to_string()
        } else {
            format!("{TELEMETRY_STREAM_BASE}:{i}")
        }
    }

    /// 第 i 个分片的 ClickHouse 落库消费组名（集群模式预留，当前 TelemetryWorker
    /// shards=1 时不启用多分片消费组）。
    #[allow(dead_code)]
    pub fn sink_group_for(&self, i: usize) -> String {
        if self.shards == 1 {
            SINK_GROUP_PREFIX.to_string()
        } else {
            format!("{SINK_GROUP_PREFIX}:{i}")
        }
    }

    /// 第 i 个分片的熔断器消费组名（集群模式预留，当前不启用多分片消费组）。
    #[allow(dead_code)]
    pub fn cb_group_for(&self, i: usize) -> String {
        if self.shards == 1 {
            CB_GROUP_PREFIX.to_string()
        } else {
            format!("{CB_GROUP_PREFIX}:{i}")
        }
    }

    /// 分片数。
    pub fn shards(&self) -> usize {
        self.shards
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn config_unset_defaults_to_single_stream() {
        assert_eq!(
            parse_redis_stream_shards(Err(std::env::VarError::NotPresent)),
            Ok(1)
        );
    }

    #[test]
    fn config_explicit_one_keeps_single_stream_keys() {
        let shards = parse_redis_stream_shards(Ok("1".to_owned())).unwrap();
        let sharder = RedisStreamSharder::new(shards);
        assert_eq!(sharder.shards(), 1);
        assert_eq!(sharder.stream_key_for(0), TELEMETRY_STREAM_BASE);
        assert_eq!(sharder.sink_group_for(0), SINK_GROUP_PREFIX);
        assert_eq!(sharder.cb_group_for(0), CB_GROUP_PREFIX);
    }

    #[test]
    fn config_rejects_multiple_shards() {
        for value in ["2", "4", "16", &usize::MAX.to_string()] {
            assert!(parse_redis_stream_shards(Ok(value.to_owned())).is_err());
        }
    }

    #[test]
    fn config_rejects_zero_empty_and_invalid_values() {
        for value in [
            "0",
            "",
            " ",
            "\t",
            "abc",
            "-1",
            "1.0",
            " 1",
            "1 ",
            "184467440737095516160",
        ] {
            assert!(parse_redis_stream_shards(Ok(value.to_owned())).is_err());
        }
    }

    #[test]
    fn config_rejects_non_unicode_without_exposing_value() {
        let error = parse_redis_stream_shards(Err(std::env::VarError::NotUnicode(
            std::ffi::OsString::from("non-unicode-secret-sentinel"),
        )))
        .unwrap_err();
        assert!(error.contains("REDIS_STREAM_SHARDS"));
        assert!(!error.contains("non-unicode-secret-sentinel"));
    }

    #[test]
    fn config_errors_are_actionable_and_do_not_echo_input() {
        let error = parse_redis_stream_shards(Ok("2".to_owned())).unwrap_err();
        assert!(error.contains("REDIS_STREAM_SHARDS"));
        assert!(error.contains("1"));
        assert!(error.contains("single stream"));
        for value in [
            "redis://user:secret-sentinel@127.0.0.1:6379",
            "secret-sentinel\nforged-log-entry",
            "",
            "0",
            "invalid",
        ] {
            assert_eq!(
                parse_redis_stream_shards(Ok(value.to_owned())).unwrap_err(),
                error
            );
        }
    }

    #[test]
    fn shard_single_is_base_key() {
        let s = RedisStreamSharder::new(1);
        assert_eq!(s.stream_key_for(0), TELEMETRY_STREAM_BASE);
        assert_eq!(s.sink_group_for(0), SINK_GROUP_PREFIX);
        assert_eq!(s.cb_group_for(0), CB_GROUP_PREFIX);
        assert_eq!(s.shard_for_domain("example.com"), 0);
    }

    #[test]
    fn shard_multi_distributes() {
        let s = RedisStreamSharder::new(4);
        let idxs: Vec<_> = ["a.com", "b.com", "c.com", "d.com", "e.com"]
            .iter()
            .map(|d| s.shard_for_domain(d))
            .collect();
        assert!(idxs.iter().all(|&i| i < 4));
        // 同一域固定分片。
        assert_eq!(s.shard_for_domain("a.com"), s.shard_for_domain("a.com"));
    }

    #[test]
    fn shard_zero_defaults_to_one() {
        let s = RedisStreamSharder::new(0);
        assert_eq!(s.shards(), 1);
        assert_eq!(s.stream_key_for(0), TELEMETRY_STREAM_BASE);
    }
}
