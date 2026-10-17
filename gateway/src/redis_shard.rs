//! GW-6 Redis Stream 分片预留（SPIKE-R3 / 集群部署待放行）。
//!
//! 默认 `REDIS_STREAM_SHARDS=1`：单 stream `stream:proxy:telemetry`，与现有
//! `TelemetryWorker` / `ChSinkWorker` / `CircuitBreaker` 行为逐字节一致。
//! 设置 `REDIS_STREAM_SHARDS=N (N>1)`：按 `target_domain` hash 分发到
//! `stream:proxy:telemetry:{0..N-1}`，各分片独立消费组，横向扩展遥测写入
//! 与 ClickHouse 落库吞吐。
//!
//! Redis Cluster 部署时，各分片 key 建议含 `{telemetry}` hash tag（如
//! `stream:proxy:telemetry:{telemetry}:{i}`），保证同 slot，消费组可用。
//! 当前 key 模式未带 hash tag，单实例部署无影响；集群迁移时再加 tag。

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

/// Redis Stream 分片路由：按 `target_domain` hash 到 shard index。
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
