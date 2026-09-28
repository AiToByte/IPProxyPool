//! 共享的 janitor 淘汰计数助手（OPT-R6 S2）。
//!
//! # 为什么要单列一个模块
//!
//! 网关有三处结构完全相同的「淘汰并报告删除个数」逻辑：
//! `RouterEngine::sweep_expired_at`（会话表＋隔离表）、`SocksBridge::evict_idle`、
//! `CanaryProber::evict_idle_clients_older_than`。三处都踩了同一个并发缺陷，
//! 因此修复也必须是同一份——**单一真源，杜绝「改了一处忘了另两处」**。
//!
//! # 缺陷本体（P0，OPT-R6 S2 根因）
//!
//! 这三处的计数写法都是：
//!
//! ```text
//! let before = map.len();   // ① 读长度
//! map.retain(|..| ..);      // ② 删除（此刻与数据面并发）
//! before - map.len()        // ③ 减法：这里假设 ② 只减不增
//! ```
//!
//! 步骤 ③ 的前提「retain 只会让长度变小」**不成立**：被淘汰的表同时被数据面写入。
//! 具体链路：
//!
//! - `session_store` 由数据面 `RouterEngine::select_node_excluding` 落表（新会话粘滞绑定）；
//! - `clients`（socks 桥 / prober 的 Client 缓存）由数据面 `client_for` 在首次用到某节点时建缓存。
//!
//! 若有写入恰好落在 ① 与 ③ 之间，则 `after > before`，
//! `usize` 的普通减法在 debug 下 **panic（`attempt to subtract with overflow`）**，
//! 在 release 下**回绕成接近 `usize::MAX` 的天文数字**（指标失真，调用方日志与判定全错）。
//!
//! debug 下的 panic 还有一个放大后果：本轮修复把这三处调用方之一的 `sweep` 循环
//! 纳入 supervisor 之前，它当时是裸 `tokio::spawn`——panic 会让整个淘汰循环
//! **静默死亡**（JoinHandle 被丢弃、无日志、无指标），此后会话表/隔离表
//! **无界增长直至 OOM**。这是本项定为 P0 的原因。
//!
//! # 修法：饱和减法，而非重试或加锁
//!
//! - **不用加锁**：`DashMap` 是分片并发结构，为了一个「本轮净删多少」的数字
//!   把淘汰与数据面写入串行化，代价远大于收益；且本仓禁区条款明确不跨 await 持锁。
//! - **不用重试**：第二次 `len()` 依旧可能竞态，无限重试既不收敛也不终止。
//! - **用 `saturating_sub`**：当 `after <= before`（纯删除，也就是绝大多数情况）
//!   它与普通减法**完全等价**；当 `after > before`（并发生长）它返回 0，
//!   恰好符合 `removed_count` 的本意「本轮净删除个数」——并发增长的那一轮
//!   净删除就是 0，**返回 0 是真值，不是掩盖**。
//!
//! 单测见本文件末尾，锁定「并发生长返回 0」「纯删除等价」「等长返回 0」三种语义。

/// janitor「本轮净删除个数」计算（OPT-R6 S2 单一真源）。
///
/// `before` / `after` 是同一张表在淘汰前后的长度。返回值语义为
/// **本轮净删除个数**（不是「淘汰检查到的条目数」）。
///
/// 并发表在淘汰窗口内被数据面写入时 `after` 可能大于 `before`；
/// 此时返回 `0` 而非 panic 或回绕，调用方据此打日志/打指标都拿到自洽的值。
///
/// # Examples
///
/// ```
/// use crate::janitor::removed_count;
///
/// // 纯删除：与普通减法等价。
/// assert_eq!(removed_count(5, 2), 3);
/// // 并发生长：净删除为 0（回归锁定，不得 panic）。
/// assert_eq!(removed_count(0, 3), 0);
/// // 无变化。
/// assert_eq!(removed_count(3, 3), 0);
/// ```
#[inline]
pub fn removed_count(before: usize, after: usize) -> usize {
    before.saturating_sub(after)
}

#[cfg(test)]
mod tests {
    use super::removed_count;

    /// 回归锁定：`sweep_expired_at` 的 panic 根因是「before/after 之间并发生长」。
    /// 旧实现 `before - after` 在本用例下 debug panic、release 回绕。
    #[test]
    fn removed_count_concurrent_growth_returns_zero_without_panic() {
        // 会话表在淘汰窗口内被数据面插入 3 条：before=0 采样后长到 3。
        assert_eq!(removed_count(0, 3), 0);
        // 采样时已有存量、并发生长 3 条。
        assert_eq!(removed_count(2, 5), 0);
        // 并发生长恰好抵消删除：净删除 0，仍不得 panic。
        assert_eq!(removed_count(4, 4), 0);
    }

    /// 纯删除路径必须与旧的普通减法**逐值等价**（存量行为零变化）。
    #[test]
    fn removed_count_pure_removal_matches_plain_subtraction() {
        for before in 0..16usize {
            for after in 0..=before {
                assert_eq!(
                    removed_count(before, after),
                    before - after,
                    "before={before} after={after} 必须与普通减法等价"
                );
            }
        }
    }

    /// 删除到空表（常见终态：TTL 全过期）。
    #[test]
    fn removed_count_drain_to_empty() {
        assert_eq!(removed_count(9, 0), 9);
        assert_eq!(removed_count(1, 0), 1);
        assert_eq!(removed_count(0, 0), 0);
    }
}
