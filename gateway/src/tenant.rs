//! GW-4 multi-tenant auth, zero-lock throttling, and usage metering.
//!
//! Data-plane checks are allocation-free: one DashMap lookup, one token-bucket
//! probe, one CAS slot grab. Pricing (plan): DC $0.2/GB, Residential $3/GB,
//! Mobile $15/GB.

use atomic_float::AtomicF64;
use dashmap::DashMap;
use governor::clock::DefaultClock;
use governor::state::{InMemoryState, NotKeyed};
use governor::{Quota, RateLimiter};
use serde::{Deserialize, Serialize};
use std::num::NonZeroU32;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::Arc;

/// Egress $/GB price table.
pub const PRICE_DC_PER_GB: f64 = 0.2;
pub const PRICE_RESIDENTIAL_PER_GB: f64 = 3.0;
pub const PRICE_MOBILE_PER_GB: f64 = 15.0;
/// 免费线 $/GB 单价（R2-FreePool：免费节点仍计量字节，单价 0）。
pub const PRICE_FREE_PER_GB: f64 = 0.0;
const BYTES_PER_GB: f64 = 1024.0 * 1024.0 * 1024.0;

pub fn price_per_gb(tier: &str) -> f64 {
    match tier.to_ascii_lowercase().as_str() {
        "residential" | "res" => PRICE_RESIDENTIAL_PER_GB,
        "mobile" => PRICE_MOBILE_PER_GB,
        "free" => PRICE_FREE_PER_GB,
        _ => PRICE_DC_PER_GB,
    }
}

/// Tenant account with live throttle + metering state.
pub struct TenantAccount {
    pub tenant_id: String,
    /// Control-plane key id (map key); kept for billing joins (GW-R2).
    #[allow(dead_code)]
    pub api_key: String,
    /// 注册时的 QPS 配额，供 Admin API 租户详情展示（governor 内部不直接暴露 qps）。
    pub qps: u32,
    pub is_active: AtomicBool,
    pub limiter: RateLimiter<NotKeyed, InMemoryState, DefaultClock>,
    pub in_flight: AtomicUsize,
    pub max_concurrency: usize,
    pub total_bytes: AtomicU64,
    pub balance_usd: AtomicF64,
}

pub struct TenantManager {
    tenants: DashMap<String, Arc<TenantAccount>>,
    /// 待写回的租户及修改代数。只保存键与版本，不在请求路径复制账户快照。
    ///
    /// 写入期间仍可修改账户：旧快照写成功，只能确认它读取时对应的版本，
    /// 不能删除等待期间产生的新脏标记。版本比较与删除由同一个分片锁保护。
    dirty: DashMap<String, u64>,
    /// 周期写回与停机写回串行执行，避免较早的快照在较晚快照之后覆盖存储。
    /// 此锁只约束持久化调用，不阻塞请求计量；持有期间不保留 DashMap 守卫。
    flush_gate: tokio::sync::Mutex<()>,
    store: Store,
}

/// Cancellation-safe owner of an authenticated tenant slot and its final accounting.
///
/// The guard records response bytes as callbacks observe them. If Pingora skips the
/// logging callback because a request future is cancelled, Drop still settles once.
pub(crate) struct TenantSettlement {
    manager: Arc<TenantManager>,
    account: Option<Arc<TenantAccount>>,
    bytes: u64,
    price_per_gb: f64,
}

impl TenantSettlement {
    pub(crate) fn new(manager: Arc<TenantManager>, account: Arc<TenantAccount>) -> Self {
        Self {
            manager,
            account: Some(account),
            bytes: 0,
            price_per_gb: PRICE_DC_PER_GB,
        }
    }

    pub(crate) fn set_bytes(&mut self, bytes: u64) {
        self.bytes = bytes;
    }

    pub(crate) fn reset_bytes(&mut self) {
        self.bytes = 0;
    }

    pub(crate) fn add_bytes(&mut self, bytes: u64) {
        self.bytes = self.bytes.saturating_add(bytes);
    }

    pub(crate) fn set_tier(&mut self, tier: &str) {
        self.price_per_gb = price_per_gb(tier);
    }

    pub(crate) fn settle(mut self, bytes: u64, tier: &str) {
        self.set_tier(tier);
        self.bytes = bytes;
        self.release();
    }

    fn release(&mut self) {
        if let Some(account) = self.account.take() {
            self.manager
                .release_and_meter_at_price(&account, self.bytes, self.price_per_gb);
        }
    }
}

impl Drop for TenantSettlement {
    fn drop(&mut self) {
        self.release();
    }
}

/// 租户状态的持久化后端。
///
/// # 语义边界（重要）
///
/// **配置决定「有哪些租户」，持久化只决定「它们现在什么状态」。**
///
/// 具体说：`register_tenant`（来自 `API_KEY` / 未来的多租户配置）是租户
/// **存在性**的唯一来源；Redis 里的记录**不会**复活一个已经从配置里删掉的
/// 租户。反过来，`is_active` / `balance_usd` / `total_bytes` 这三个**运行期
/// 可变状态**才由 Redis 恢复。
///
/// 为什么不做成"Redis 即真相"：那样运维把某个 key 从配置里去掉后，它会因为
/// Redis 里还有记录而继续生效——一个"删不掉的租户"比丢余额更难排查。
// 体积由 `ConnectionManager`（224B）决定，而它在本进程里**只有一个实例**
// （`TenantManager` 是单例），多出来的字节毫无意义；反过来按 clippy 的建议
// 给 `conn` 加一层 `Box`，会让每次 `is_persistent()` 多一次指针跳转——
// 那是每请求都走的热路径。默认建议针对"大量实例"的枚举，与本用法不符。
#[allow(clippy::large_enum_variant)]
pub enum Store {
    /// 进程内（缺省）。重启即丢，与 OPT-R18 之前的行为一致。
    Memory,
    /// Redis 写穿：`TENANT_STORE=redis`。
    Redis {
        conn: redis::aio::ConnectionManager,
        /// key 前缀，实际键为 `{prefix}:{api_key}` 与索引集合 `{prefix}:index`。
        prefix: String,
    },
    /// **测试专用**：进程内记录器，语义等价于 Redis 后端。
    ///
    /// 存在的理由不是"图省事"，而是 CI 与本机都没有 Redis（`ci.yml` 未声明
    /// redis service），而 `flush_dirty` 里真正容易错的是**逐条删脏 / 失败
    /// 保留 / 索引维护**这套算法，不是 `HSET` 的调用语法。没有这个变体，
    /// 算法只能靠肉眼审阅——那正是"看起来对"的代码出事的方式。
    ///
    /// `#[cfg(test)]` 保证它进不了发布产物；同时 `is_persistent` 用穷尽
    /// `match` 而非 `matches!`，将来再加后端时编译器会强制你在这里做决定。
    #[cfg(test)]
    Recording(std::sync::Arc<RecordingBackend>),
}

/// 持久化后端的**动作**抽象。
///
/// 把"怎么存"（Redis 命令 / 测试记录器）与"存什么、何时删脏"分开：后者只写
/// 一遍（`flush_via` / `load_via`），两个后端共用，于是测试覆盖的算法就是
/// 生产跑的算法。
trait PersistBackend {
    /// 写入一条记录，并把它登记进索引（保证 `read_all` 下次能枚举到）。
    async fn write(&self, key: &str, rec: &PersistedTenant) -> Result<(), String>;

    /// Discover indexed and unindexed records; preserve decode failures for startup policy.
    async fn read_all(&self) -> Result<PersistedRecords, String>;
}

/// Persisted records returned by a backend, including keys that could not be decoded.
struct PersistedRecords {
    records: Vec<(String, PersistedTenant)>,
    indexed_keys: std::collections::HashSet<String>,
    malformed_keys: Vec<String>,
    orphan_keys: Vec<String>,
    missing_index_keys: Vec<String>,
}

/// Result of restoring configured tenant state.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct RestoreReport {
    pub applied: usize,
    pub configured: usize,
    pub has_persisted_records: bool,
    pub missing_configured_keys: Vec<String>,
    pub malformed_keys: Vec<String>,
    pub orphan_keys: Vec<String>,
}

impl RestoreReport {
    pub fn is_incomplete(&self) -> bool {
        !self.missing_configured_keys.is_empty() || !self.malformed_keys.is_empty()
    }
}

/// `RedisBackend` and the test recorder share this persistence contract.
struct RedisBackend {
    conn: redis::aio::ConnectionManager,
    prefix: String,
}

impl PersistBackend for RedisBackend {
    async fn write(&self, key: &str, rec: &PersistedTenant) -> Result<(), String> {
        // `ConnectionManager` 是 Arc 复用连接的多路复用器，clone 很便宜；
        // 之所以 clone 而不是 `&mut self.conn`：`write` 拿的是 `&self`
        // （多个 key 顺序写时不必持有可变借用）。
        let mut conn = self.conn.clone();
        let hash_key = format!("{}:{key}", self.prefix);
        // MULTI/EXEC prevents interleaving. Redis does not roll back runtime command
        // errors; restore discovery also checks for index/hash inconsistencies.
        let mut pipe = redis::pipe();
        pipe.atomic();
        pipe.hset_multiple(&hash_key, &rec.to_hash()).ignore();
        pipe.sadd(format!("{}:index", self.prefix), key).ignore();
        pipe.query_async::<()>(&mut conn)
            .await
            .map_err(|e| format!("租户状态写回失败（Redis 前缀已隐藏）: {e}"))
    }

    async fn read_all(&self) -> Result<PersistedRecords, String> {
        let mut conn = self.conn.clone();
        let index_key = format!("{}:index", self.prefix);
        let keys: std::collections::HashSet<String> =
            redis::AsyncCommands::smembers(&mut conn, &index_key)
                .await
                .map_err(|e| format!("租户状态索引读取失败: {e}"))?;
        // Redis MATCH interprets glob metacharacters; the configured prefix is literal.
        let key_prefix = format!("{}:", self.prefix);
        let mut pattern = String::with_capacity(key_prefix.len() + 1);
        for ch in key_prefix.chars() {
            if matches!(ch, '*' | '?' | '[' | ']') || ch == char::from(92) {
                pattern.push(char::from(92));
            }
            pattern.push(ch);
        }
        pattern.push('*');
        let mut cursor = 0u64;
        let mut discovered = std::collections::HashSet::new();
        loop {
            let (next, page): (u64, Vec<String>) = redis::cmd("SCAN")
                .arg(cursor)
                .arg("MATCH")
                .arg(&pattern)
                .arg("COUNT")
                .arg(128)
                .query_async(&mut conn)
                .await
                .map_err(|e| format!("租户状态键扫描失败: {e}"))?;
            cursor = next;
            for redis_key in page {
                let Some(suffix) = redis_key.strip_prefix(&key_prefix) else {
                    continue;
                };
                if suffix != "index" {
                    discovered.insert(suffix.to_string());
                }
            }
            if cursor == 0 {
                break;
            }
        }
        let mut missing_index_keys: Vec<String> = discovered.difference(&keys).cloned().collect();
        missing_index_keys.sort();
        let orphan_keys = missing_index_keys.clone();
        let indexed_keys = keys.clone();
        let mut keys: Vec<String> = keys.into_iter().collect();
        keys.extend(missing_index_keys.iter().cloned());
        keys.sort();
        let mut records = Vec::with_capacity(keys.len());
        let mut malformed_keys = Vec::new();
        for key in &keys {
            let hash_key = format!("{}:{key}", self.prefix);
            let hash: std::collections::HashMap<String, String> =
                match redis::AsyncCommands::hgetall(&mut conn, &hash_key).await {
                    Ok(hash) => hash,
                    Err(error) if error.code() == Some("WRONGTYPE") => {
                        malformed_keys.push(key.clone());
                        continue;
                    }
                    Err(error) => return Err(format!("tenant state read failed: {error}")),
                };
            match PersistedTenant::from_hash(&hash) {
                Some(record) => records.push((key.clone(), record)),
                None => {
                    malformed_keys.push(key.clone());
                    log::warn!(
                        "[Tenant] 租户状态记录无法解析（余额字段缺失或非有限值），保留内存值"
                    );
                }
            }
        }
        Ok(PersistedRecords {
            records,
            indexed_keys,
            malformed_keys,
            orphan_keys,
            missing_index_keys,
        })
    }
}

/// 测试用后端：与 `RedisBackend` 同一套语义，落在进程内。
///
/// `pub` 是 `Store` 是 `pub` 的连带结果（`E0446`：公开枚举的字段类型不能更
/// 私密）。它被 `#[cfg(test)]` 挡在发布产物之外，因此不构成真实 API 面。
#[cfg(test)]
#[derive(Default)]
pub struct RecordingBackend {
    /// `api_key -> hash field -> value`，形态与 Redis hash 完全一致，因此
    /// `from_hash` / `to_hash` 的真实解析路径都会被走到。
    hashes: parking_lot::Mutex<
        std::collections::HashMap<String, std::collections::HashMap<String, String>>,
    >,
    index: parking_lot::Mutex<std::collections::HashSet<String>>,
    /// 模拟写失败（Redis 抖动 / 网络断）。
    fail_keys: parking_lot::Mutex<std::collections::BTreeSet<String>>,
    /// 成功写入次数（用于断言"重复 flush 不会重复写"）。
    writes: std::sync::atomic::AtomicUsize,
    pause_writes: AtomicBool,
    write_entered: tokio::sync::Notify,
    resume_writes: tokio::sync::Notify,
}

#[cfg(test)]
impl RecordingBackend {
    pub(crate) fn new() -> std::sync::Arc<Self> {
        std::sync::Arc::new(Self::default())
    }

    /// 预置一条**原始** hash（可以是坏的），用于验证加载路径对脏数据的容忍。
    fn put_raw(&self, key: &str, hash: &[(&str, &str)]) {
        self.index.lock().insert(key.to_string());
        let mut g = self.hashes.lock();
        g.insert(
            key.to_string(),
            hash.iter()
                .map(|(k, v)| ((*k).to_string(), (*v).to_string()))
                .collect(),
        );
    }

    fn remove_hash(&self, key: &str) {
        self.hashes.lock().remove(key);
    }

    fn remove_index_member(&self, key: &str) {
        self.index.lock().remove(key);
    }

    #[allow(dead_code)]
    fn store_hash_without_index(&self, key: &str, hash: &[(&str, &str)]) {
        self.hashes.lock().insert(
            key.to_string(),
            hash.iter()
                .map(|(field, value)| ((*field).to_string(), (*value).to_string()))
                .collect(),
        );
    }

    fn store_unindexed_raw(&self, key: &str, hash: &[(&str, &str)]) {
        self.hashes.lock().insert(
            key.to_string(),
            hash.iter()
                .map(|(field, value)| ((*field).to_string(), (*value).to_string()))
                .collect(),
        );
    }

    pub(crate) fn fail_on(&self, key: &str) {
        self.fail_keys.lock().insert(key.to_string());
    }

    pub(crate) fn stored(&self, key: &str) -> Option<std::collections::HashMap<String, String>> {
        self.hashes.lock().get(key).cloned()
    }

    pub(crate) fn writes(&self) -> usize {
        self.writes.load(Ordering::Relaxed)
    }

    pub(crate) fn pause_writes(&self) {
        self.pause_writes.store(true, Ordering::Release);
    }

    pub(crate) async fn wait_until_write_blocked(&self) {
        self.write_entered.notified().await;
    }

    pub(crate) fn resume_writes(&self) {
        self.pause_writes.store(false, Ordering::Release);
        self.resume_writes.notify_one();
    }
}

#[cfg(test)]
impl PersistBackend for RecordingBackend {
    async fn write(&self, key: &str, rec: &PersistedTenant) -> Result<(), String> {
        if self.pause_writes.load(Ordering::Acquire) {
            self.write_entered.notify_one();
            self.resume_writes.notified().await;
        }
        if self.fail_keys.lock().contains(key) {
            return Err("租户状态写回失败（测试注入）".to_string());
        }
        // 守卫在 return 前自然释放，不跨 await（本函数体内无 await）。
        self.index.lock().insert(key.to_string());
        self.hashes
            .lock()
            .insert(key.to_string(), rec.to_hash().into_iter().collect());
        self.writes.fetch_add(1, Ordering::Relaxed);
        Ok(())
    }

    async fn read_all(&self) -> Result<PersistedRecords, String> {
        let indexed_keys = self.index.lock().clone();
        let snapshot: Vec<(String, std::collections::HashMap<String, String>)> = self
            .hashes
            .lock()
            .iter()
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect();
        let unindexed_hashes = snapshot
            .iter()
            .map(|(key, _)| key.clone())
            .filter(|key| !indexed_keys.contains(key))
            .collect::<Vec<_>>();
        let mut records = Vec::new();
        let mut malformed_keys = Vec::new();
        let orphan_keys = unindexed_hashes;
        let missing_index_keys = orphan_keys.clone();
        let keys: std::collections::HashSet<_> = indexed_keys
            .iter()
            .chain(missing_index_keys.iter())
            .cloned()
            .collect();
        for key in &keys {
            match snapshot.iter().find(|(stored_key, _)| stored_key == key) {
                Some((_, hash)) => match PersistedTenant::from_hash(hash) {
                    Some(record) => records.push((key.clone(), record)),
                    None => malformed_keys.push(key.clone()),
                },
                None => malformed_keys.push(key.clone()),
            }
        }
        Ok(PersistedRecords {
            records,
            indexed_keys,
            malformed_keys,
            orphan_keys,
            missing_index_keys,
        })
    }
}

/// Redis hash 里的一条租户记录。
///
/// 字段名即 hash field 名。**新增字段必须给默认值**（`from_hash` 里 `?` 缺失
/// 字段时回落），否则升级后读旧记录会整体丢弃——那等于静默清零余额。
#[derive(Debug, Clone, PartialEq)]
pub struct PersistedTenant {
    /// 仅作记录，加载时**不**覆盖配置（`tenant_id` 由 `register_tenant` 决定）。
    pub tenant_id: String,
    /// 同上：qps 在注册时就编译进了 governor 的限流器，事后覆盖会让
    /// "配额"与"限流器"不一致。
    pub qps: u32,
    pub max_concurrency: usize,
    /// 运行期可变状态（会被恢复）。
    pub is_active: bool,
    pub balance_usd: f64,
    pub total_bytes: u64,
}

impl PersistedTenant {
    /// 从内存账户取快照（**必须在 await 之前调用**：`DashMap` 的读守卫不 Send）。
    pub fn from_account(a: &TenantAccount) -> Self {
        Self {
            tenant_id: a.tenant_id.clone(),
            qps: a.qps,
            max_concurrency: a.max_concurrency,
            is_active: a.is_active.load(Ordering::Relaxed),
            balance_usd: a.balance_usd.load(Ordering::Relaxed),
            total_bytes: a.total_bytes.load(Ordering::Relaxed),
        }
    }

    fn to_hash(&self) -> Vec<(String, String)> {
        vec![
            ("tenant_id".into(), self.tenant_id.clone()),
            ("qps".into(), self.qps.to_string()),
            ("max_concurrency".into(), self.max_concurrency.to_string()),
            ("is_active".into(), u8::from(self.is_active).to_string()),
            ("balance_usd".into(), self.balance_usd.to_string()),
            ("total_bytes".into(), self.total_bytes.to_string()),
        ]
    }

    /// 解析 Redis hash。**缺字段用默认值，不整体丢弃**。
    ///
    /// 只有 `balance_usd` 解析失败时才返回 `None`：余额是这个记录里唯一
    /// "错了就会算错钱"的字段，宁可当作没有记录（保持内存值）也不能猜。
    /// 其余字段坏了就回落默认，让账户至少还能被识别出来。
    fn from_hash(h: &std::collections::HashMap<String, String>) -> Option<Self> {
        let balance_usd: f64 = h.get("balance_usd")?.parse().ok()?;
        if !balance_usd.is_finite() {
            // NaN/±Inf 会绕过欠费门（`NaN <= 0.0` 为 false ⇒ 永不拒付），
            // 与 `set_balance` 的文档注释同一条理由。
            return None;
        }
        Some(Self {
            tenant_id: h.get("tenant_id").cloned().unwrap_or_default(),
            qps: h.get("qps").and_then(|v| v.parse().ok()).unwrap_or(0),
            max_concurrency: h
                .get("max_concurrency")
                .and_then(|v| v.parse().ok())
                .unwrap_or(0),
            is_active: h.get("is_active").map(|v| v == "1").unwrap_or(true),
            balance_usd,
            total_bytes: h
                .get("total_bytes")
                .and_then(|v| v.parse().ok())
                .unwrap_or(0),
        })
    }
}

impl std::fmt::Debug for TenantAccount {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TenantAccount")
            .field("tenant_id", &self.tenant_id)
            .field("is_active", &self.is_active)
            .field("max_concurrency", &self.max_concurrency)
            .finish_non_exhaustive()
    }
}

// =====================================================================
// OPT-R12：鉴权密钥卫生（常量单一真源 ＋ 策略纯函数）
// =====================================================================
//
// # 为何把 `DEFAULT_API_KEY` 搬到 `tenant.rs`
//
// 它此前定义在 `gateway.rs`（**数据面**模块），却被 SDK／脚本／文档当**鉴权
// 公共约定**引用——结构性错位：改数据面的人会以为碰不到鉴权，改鉴权的人却要
// 去数据面找常量。鉴权常量的归属地就是租户模块（key 的持有者正是
// `TenantManager`）。**纯搬迁，行为不变。**（本仓惯例：新模块 2026-09-28
// 单独成文件而非塞进既有模块，故沿用。）
//
// # `REQUIRE_API_KEY=0` 的真实语义（此前**没有任何地方这样写**）
//
// 关掉鉴权门**不等于**"无鉴权"：无头请求会被
// `unwrap_or_else(|| DEFAULT_API_KEY.to_string())` **静默补成默认租户**，
// 而默认租户是 qps/并发 10000/10000。实测（release 二进制）：不带任何
// `X-API-Key` 的请求返回 **200** ＋ 完整代理服务。
//
// 也就是说 `REQUIRE_API_KEY=0` 的语义是"**全网共享一个满额身份**"，
// 而不是"没有鉴权"。这才是本项定级为 P1 的真正理由——**弱默认值的失败
// 模式没有被写下来**，运维读文档以为关掉了鉴权，实际是敞开。
//
// # 三条不变式（本组全部改动都必须保持）
//
// 1. `API_KEY` **未设置** ⇒ 仍注册 `default_key`（本机开发与本仓自检依赖它，
//    改动会让存量部署升级即挂）。
// 2. `API_KEY` **已设置** ⇒ 只注册它，`default_key` **立即失效**（请求 403）。
//    这是本轮用户明确选择的最强档，不再做双 key 并存过渡。
// 3. 任何情况下都不因配置而**拒绝启动**；风险一律以 `warn!` 表达
//    （fail-closed 只用于**误配**这一条，见 `resolve_api_key`）。

/// 兜底 API Key（**仅本机开发/自检**）。生产必须用 `API_KEY` 覆盖。
pub const DEFAULT_API_KEY: &str = "default_key";

/// 默认租户的哨兵配额（qps 与并发同值）。用于识别"满配额"。
///
/// 单独成为常量而非内联 `10_000`：A1 的告警判定必须引用**同一份**数值，
/// 否则改了注册处的配额而忘了改判定，告警就成了永远不触发的死代码。
pub const TENANT_SENTINEL_QUOTA: u32 = 10_000;

/// 从 `API_KEY` env 解析生效的默认租户 key。
///
/// # 语义
///
/// - `Ok(None)` ⇒ 未设置 ⇒ 调用方回退 [`DEFAULT_API_KEY`]（开发默认）。
/// - `Ok(Some(k))` ⇒ 使用 `k`，且**此时 `default_key` 失效**。
/// - `Err(_)` ⇒ 误配（空串／纯空白／显式等于 `default_key`）⇒ **fail-closed**：
///   宁可不注册任何默认租户（全部 403），也不接受一个等于"没配"的 key——
///   后者会让运维以为配好了，实际仍在用公开的弱默认值。
///
/// 纯函数（不读 env 本身，只吃入参）以便单测穷举；env 读取在 `main.rs`。
pub fn resolve_api_key(raw: Option<&str>) -> Result<Option<String>, &'static str> {
    match raw {
        None => Ok(None),
        Some(s) => {
            let t = s.trim();
            if t.is_empty() {
                return Err("API_KEY is set but empty/whitespace; refusing to fall back to the public default key");
            }
            if t == DEFAULT_API_KEY {
                return Err(
                    "API_KEY is set to the public default value; refusing — set a strong random value instead",
                );
            }
            Ok(Some(t.to_string()))
        }
    }
}

/// A1：判定当前默认租户配置是否属于「危险组合」并给出告警文本。
///
/// 危险 = **同时**满足：弱默认值 ＋ 满配额 ＋ 非回环监听。三者缺一不告警——
/// 这是"防噪音"的关键：本机开发（弱默认值 ＋ 满配额 ＋ **回环**）与
/// 生产正确配置（强 key ＋ 满配额 ＋ 非回环）都**不该**被这条告警打扰。
///
/// # 为何只是 `warn!` 而非拒绝启动
///
/// 满足三条件确实是真实事故组合，但直接 panic 会让存量部署**升级即挂**，
/// 违反本轮"存量行为零变化"铁律。`warn!` 能在零行为变更前提下把"静默敞开"
/// 变成"日志里明写敞开"，并逐条给出整改动作。
///
/// 返回 `None` 表示无需告警（**不要**返回空串占位——那会让调用方难以区分
/// "不告警"与"告警但无内容"）。
pub fn weak_auth_warning(
    key_is_default: bool,
    qps: u32,
    max_concurrency: usize,
    listen: &str,
) -> Option<String> {
    if !key_is_default {
        return None; // 已用 API_KEY 覆盖，不是弱默认值。
    }
    let full_quota =
        qps >= TENANT_SENTINEL_QUOTA && max_concurrency >= TENANT_SENTINEL_QUOTA as usize;
    if !full_quota {
        return None; // 配额已收窄，风险已被限制。
    }
    if is_loopback_listen(listen) {
        return None; // 只监听回环 ⇒ 仅本机可达，外部无法滥用。
    }
    Some(format!(
        "INSECURE DEFAULT AUTH: tenant 'default' is using the public key {DEFAULT_API_KEY:?} \
         with sentinel quota {qps} qps / {max_concurrency} concurrent, listening on {listen} \
         (non-loopback). Anyone who can reach this port shares ONE full-rate identity. \
         Remediation (do at least 1): 1) set API_KEY to a strong random value \
         (default_key then stops working); 2) narrow the quota below {TENANT_SENTINEL_QUOTA}; \
         3) bind loopback (GATEWAY_ADDR=127.0.0.1:8916) and front the port with a WAF/mTLS proxy."
    ))
}

/// 监听地址是否回环（`127.0.0.0/8`、`::1`、`localhost`）。
///
/// 保守取向：**只**认这些明确的回环写法；`0.0.0.0`／`::`／空串／任何无法
/// 解析出 ip 的字符串都**不**算回环（宁可多告警一次，也不要漏掉真实暴露）。
pub fn is_loopback_listen(listen: &str) -> bool {
    let s = listen.trim();
    // 先尝试把**整串**直接当 IP 解析——这是裸 IPv6（如 `::1`）唯一正确的读法。
    //
    // 【踩坑记录】首版直接 `rsplit_once(':')` 剥端口，于是 `::1` 被切成
    // host=`:` / port=`1`，解析失败 ⇒ 判成"非回环" ⇒ 对**合法的回环绑定**
    // 误发告警。而本函数的价值恰恰在于"不该响的时候不响"，被自己的误判
    // 破掉就等于噪音门。故必须先试整体解析。
    if let Ok(ip) = s.parse::<std::net::IpAddr>() {
        return ip.is_loopback();
    }
    // 带端口（或带方括号）的写法：剥掉端口再解析。
    let host = match s.rsplit_once(':') {
        Some((h, _port)) => h,
        None => s,
    };
    let host = host.trim().trim_matches('[').trim_matches(']');
    if host.eq_ignore_ascii_case("localhost") {
        return true;
    }
    match host.parse::<std::net::IpAddr>() {
        Ok(ip) => ip.is_loopback(),
        Err(_) => false,
    }
}

/// Admin API 租户列表条目（只读，不暴露 governor 内部限流器状态）。
#[derive(Debug, Clone, Serialize, Deserialize, utoipa::ToSchema)]
pub struct TenantSummary {
    pub tenant_id: String,
    pub api_key: String,
    pub is_active: bool,
    pub qps: u32,
    pub max_concurrency: usize,
    pub in_flight: usize,
    pub total_bytes: u64,
    pub balance_usd: f64,
}

/// Admin API 租户详情（与 TenantSummary 同形态，保留扩展字段的位置，便于未来追加）。
#[derive(Debug, Clone, Serialize, Deserialize, utoipa::ToSchema)]
pub struct TenantDetail {
    pub tenant_id: String,
    pub api_key: String,
    pub is_active: bool,
    pub qps: u32,
    pub max_concurrency: usize,
    pub in_flight: usize,
    pub total_bytes: u64,
    pub balance_usd: f64,
}

impl TenantManager {
    /// 纯内存后端（OPT-R18 P2-2 之前的唯一形态）。
    ///
    /// 只在测试里用：生产路径一律走 [`with_store`](Self::with_store)，因为
    /// 持久化后端必须**显式**由 `TENANT_STORE` 决定——留一个 `new()` 让
    /// `main.rs` 顺手调用，正是"升级后行为悄悄变化"的入口。`#[cfg(test)]`
    /// 把这条边界写进类型系统，而不是靠注释约束。
    #[cfg(test)]
    pub fn new() -> Self {
        Self::with_store(Store::Memory)
    }

    /// 指定持久化后端构造（`main.rs` 在 `TENANT_STORE=redis` 时用）。
    pub fn with_store(store: Store) -> Self {
        Self {
            tenants: DashMap::new(),
            dirty: DashMap::new(),
            flush_gate: tokio::sync::Mutex::new(()),
            store,
        }
    }

    /// 持久化是否启用。热路径上的开销闸门：关闭时 `mark_dirty` 是纯分支，
    /// 零分配。
    ///
    /// 用穷尽 `match` 而非 `matches!`：新增后端时编译器会在这里拦一下，
    /// 强迫作者显式回答"它要不要参与脏标记"。
    #[inline]
    fn is_persistent(&self) -> bool {
        match &self.store {
            Store::Memory => false,
            Store::Redis { .. } => true,
            #[cfg(test)]
            Store::Recording(_) => true,
        }
    }

    /// 修改完成后推进脏版本；未启用持久化时为空操作。
    /// 同一租户的多次修改合并为一条记录，但不能合并掉“写入中又修改”的事实。
    #[inline]
    fn mark_dirty(&self, api_key: &str) {
        if self.is_persistent() {
            self.dirty
                .entry(api_key.to_string())
                .and_modify(|version| *version = version.wrapping_add(1))
                .or_insert(1);
        }
    }

    /// Load persisted state and return the number of applied records.
    pub async fn load_persisted(&self) -> Result<usize, String> {
        Ok(self.load_persisted_report().await?.applied)
    }

    /// Load persisted state with enough detail for required-store startup policy.
    pub async fn load_persisted_report(&self) -> Result<RestoreReport, String> {
        match &self.store {
            Store::Memory => Ok(RestoreReport::default()),
            Store::Redis { conn, prefix } => {
                let backend = RedisBackend {
                    conn: conn.clone(),
                    prefix: prefix.clone(),
                };
                self.load_via(&backend).await
            }
            #[cfg(test)]
            Store::Recording(b) => self.load_via(b.as_ref()).await,
        }
    }

    /// 加载算法本体（与后端无关，因此测试覆盖的就是生产代码）。
    async fn load_via<B: PersistBackend>(&self, backend: &B) -> Result<RestoreReport, String> {
        let records = backend.read_all().await?;
        let mut report = RestoreReport {
            configured: self.tenants.len(),
            has_persisted_records: !records.indexed_keys.is_empty()
                || !records.malformed_keys.is_empty()
                || !records.records.is_empty()
                || !records.orphan_keys.is_empty(),
            ..RestoreReport::default()
        };
        let malformed_keys: std::collections::HashSet<String> = records
            .malformed_keys
            .iter()
            .chain(records.missing_index_keys.iter())
            .filter(|key| self.tenants.contains_key(*key))
            .cloned()
            .collect();
        let mut restored_keys = std::collections::HashSet::new();
        let mut orphan_keys: Vec<String> = records
            .indexed_keys
            .iter()
            .chain(records.missing_index_keys.iter())
            .chain(records.malformed_keys.iter())
            .filter(|key| !self.tenants.contains_key(*key))
            .cloned()
            .collect();
        for (key, rec) in records.records {
            let Some(acct) = self.tenants.get(&key).map(|e| e.value().clone()) else {
                orphan_keys.push(key);
                continue;
            };
            if malformed_keys.contains(&key) {
                continue;
            }
            acct.is_active.store(rec.is_active, Ordering::Relaxed);
            acct.balance_usd.store(rec.balance_usd, Ordering::Relaxed);
            acct.total_bytes.store(rec.total_bytes, Ordering::Relaxed);
            restored_keys.insert(key);
            report.applied += 1;
        }
        let existing_state = report.has_persisted_records;
        report.missing_configured_keys = self
            .tenants
            .iter()
            .map(|entry| entry.key().clone())
            .filter(|key| {
                existing_state && !restored_keys.contains(key) && !malformed_keys.contains(key)
            })
            .collect();
        report.missing_configured_keys.sort();
        report.malformed_keys = malformed_keys.into_iter().collect();
        report.malformed_keys.sort();
        report.orphan_keys = orphan_keys;
        report.orphan_keys.sort();
        report.orphan_keys.dedup();
        if !report.orphan_keys.is_empty() {
            log::warn!(
                "[Tenant] ignored {} orphan persisted record(s); no tenants created",
                report.orphan_keys.len()
            );
        }
        if !report.missing_configured_keys.is_empty() {
            log::warn!(
                "[Tenant] configured tenant state is incomplete; {} record(s) remain at defaults",
                report.missing_configured_keys.len()
            );
        }
        Ok(report)
    }

    /// 把脏集写回后端。返回写入成功的条数。
    ///
    /// 失败语义：**逐条删脏**，失败的那条（及其后的）保留在脏集里，下一轮
    /// 重试。不采用"先清空再写"——那样写失败就等于静默丢掉余额更新，而
    /// 接口早已对运维报过成功。
    /// A successful snapshot write is not proof that concurrent updates were flushed.
    pub(crate) fn has_dirty_state(&self) -> bool {
        !self.dirty.is_empty()
    }

    pub async fn flush_dirty(&self) -> Result<usize, String> {
        match &self.store {
            Store::Memory => Ok(0),
            Store::Redis { conn, prefix } => {
                let backend = RedisBackend {
                    conn: conn.clone(),
                    prefix: prefix.clone(),
                };
                self.flush_via(&backend).await
            }
            #[cfg(test)]
            Store::Recording(b) => self.flush_via(b.as_ref()).await,
        }
    }

    /// 串行写回算法。取消或失败不确认版本，下次调用仍会读取并重试最新状态。
    async fn flush_via<B: PersistBackend>(&self, backend: &B) -> Result<usize, String> {
        let _flush = self.flush_gate.lock().await;
        // 先捕获版本，再读账户：反序会把“旧值 + 新版本”写出并错误确认新修改。
        // 不跨 await 持有迭代守卫，写入期间数据面可正常标记更新。
        let keys: Vec<(String, u64)> = self
            .dirty
            .iter()
            .map(|e| (e.key().clone(), *e.value()))
            .collect();
        let mut written = 0usize;
        for (key, version) in keys {
            let rec = match self.tenants.get(&key) {
                Some(e) => PersistedTenant::from_account(e.value()),
                None => {
                    self.dirty.remove_if(&key, |_, current| *current == version);
                    continue;
                }
            };
            backend
                .write(&key, &rec)
                .await
                .map_err(|e| format!("{e} (已写 {written} 条)"))?;
            // 比较与删除必须原子完成；先 get 再 remove 会再次引入丢更新窗口。
            self.dirty.remove_if(&key, |_, current| *current == version);
            written += 1;
        }
        Ok(written)
    }

    /// 启动后台 flush 循环。未启用持久化时返回 `None`（不起空转任务）。
    ///
    /// The interval is the healthy writeback cadence, not a data-loss limit.
    /// Dependency outages and backlog can retain older unpersisted updates.
    pub fn spawn_flusher(
        self: &Arc<Self>,
        interval: std::time::Duration,
    ) -> Option<(
        tokio::sync::oneshot::Sender<()>,
        tokio::task::JoinHandle<()>,
    )> {
        if !self.is_persistent() {
            return None;
        }
        let me = Arc::clone(self);
        let (stop_tx, mut stop_rx) = tokio::sync::oneshot::channel();
        let task = tokio::spawn(async move {
            let mut ticker = tokio::time::interval(interval);
            // 慢机器上漏拍时按 Delay 补齐，不要 Burst 连发（连发会在
            // 积压后瞬间打出几百次 Redis 往返）。
            ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            loop {
                tokio::select! {
                    _ = &mut stop_rx => break,
                    _ = ticker.tick() => match me.flush_dirty().await {
                        Ok(0) => {}
                        Ok(n) => log::debug!("[Tenant] flushed {n} record(s) to Redis"),
                        Err(_) => log::warn!("[Tenant] flush failed; retaining dirty updates for retry"),
                    },
                }
            }
        });
        Some((stop_tx, task))
    }

    /// R2-3 默认 burst：`qps/10`（至少 1）。突发容忍与限流强度解耦，
    /// 老调用按此换算后行为不变（`qps=1 → burst=1`，存量单测语义冻结）。
    pub fn default_burst_for_qps(qps: u32) -> u32 {
        (qps / 10).max(1)
    }

    pub fn register_tenant(
        &self,
        tenant_id: &str,
        api_key: &str,
        qps: u32,
        max_concurrency: usize,
        burst: u32,
    ) {
        let quota = Quota::per_second(NonZeroU32::new(qps.max(1)).expect("qps must be non-zero"))
            .allow_burst(NonZeroU32::new(burst.max(1)).expect("burst must be non-zero"));
        let account = Arc::new(TenantAccount {
            tenant_id: tenant_id.to_string(),
            api_key: api_key.to_string(),
            qps,
            is_active: AtomicBool::new(true),
            limiter: RateLimiter::direct(quota),
            in_flight: AtomicUsize::new(0),
            max_concurrency,
            total_bytes: AtomicU64::new(0),
            balance_usd: AtomicF64::new(100.0),
        });
        self.tenants.insert(api_key.to_string(), account);
        // 首次注册也要落盘：否则重启后 Redis 里没有这条记录，
        // 而运维在重启前 PATCH 过的余额也就无处可恢复。
        self.mark_dirty(api_key);
    }

    /// Deactivate without dropping live references (billing holds `Arc`s).
    /// Control-plane op；Admin API `PATCH /tenants/{api_key}/active` 实时调用。
    pub fn set_active(&self, api_key: &str, active: bool) -> bool {
        match self.tenants.get(api_key) {
            Some(entry) => {
                entry.value().is_active.store(active, Ordering::Relaxed);
                self.mark_dirty(api_key);
                true
            }
            None => false,
        }
    }

    /// Admin API：修改租户余额（USD），写入即生效（`AtomicF64` 共享有状态账户，
    /// 存量持有 `Arc` 的账务引用下一帧读取即见新值）。返回是否存在该租户。
    ///
    /// # 非有限值由调用方拒绝（Admin handler 的 400）
    ///
    /// `NaN` 会绕过欠费门（`NaN <= 0.0` 为 false ⇒ 永不拒付），且 `fetch_sub`
    /// 之后余额永久 isNaN——所以 `PATCH balance` 在 handler 侧必须校验
    /// `is_finite()`，本方法不做二次裁决（单一职责：校验在 API 边界做一次）。
    pub fn set_balance(&self, api_key: &str, balance_usd: f64) -> bool {
        match self.tenants.get(api_key) {
            Some(entry) => {
                entry
                    .value()
                    .balance_usd
                    .store(balance_usd, Ordering::Relaxed);
                self.mark_dirty(api_key);
                true
            }
            None => false,
        }
    }

    /// Admin API：列出所有租户的摘要信息（只读，按注册顺序返回 DashMap 快照）。
    pub fn list_tenants(&self) -> Vec<TenantSummary> {
        self.tenants
            .iter()
            .map(|entry| {
                let acc = entry.value();
                TenantSummary {
                    tenant_id: acc.tenant_id.clone(),
                    api_key: acc.api_key.clone(),
                    is_active: acc.is_active.load(Ordering::Relaxed),
                    qps: acc.qps,
                    max_concurrency: acc.max_concurrency,
                    in_flight: acc.in_flight.load(Ordering::Relaxed),
                    total_bytes: acc.total_bytes.load(Ordering::Relaxed),
                    balance_usd: acc.balance_usd.load(Ordering::Relaxed),
                }
            })
            .collect()
    }

    /// Admin API：按 API Key 查询单个租户详情；不存在时返回 None。
    pub fn tenant_detail(&self, api_key: &str) -> Option<TenantDetail> {
        self.tenants.get(api_key).map(|entry| {
            let acc = entry.value();
            TenantDetail {
                tenant_id: acc.tenant_id.clone(),
                api_key: acc.api_key.clone(),
                is_active: acc.is_active.load(Ordering::Relaxed),
                qps: acc.qps,
                max_concurrency: acc.max_concurrency,
                in_flight: acc.in_flight.load(Ordering::Relaxed),
                total_bytes: acc.total_bytes.load(Ordering::Relaxed),
                balance_usd: acc.balance_usd.load(Ordering::Relaxed),
            }
        })
    }

    /// Auth + balance + QPS + concurrency gate. On success the caller owns one
    /// in-flight slot and MUST call [`Self::release_and_meter`].
    ///
    /// R2-3：余额门置于 QPS/并发之前——欠费租户快速失败，不烧 rate 令牌、
    /// 不占并发槽（ shaping 顺序：身份 → 开关 → 余额 → 速率 → 并发）。
    pub fn authenticate_and_throttle(
        &self,
        api_key: &str,
    ) -> Result<Arc<TenantAccount>, &'static str> {
        let tenant = self.tenants.get(api_key).ok_or("Invalid API Key")?;
        if !tenant.is_active.load(Ordering::Relaxed) {
            return Err("Tenant is disabled");
        }
        if tenant.balance_usd.load(Ordering::Relaxed) <= 0.0 {
            return Err("Insufficient balance");
        }
        if tenant.limiter.check().is_err() {
            return Err("Rate limit exceeded (QPS)");
        }
        let current = tenant.in_flight.fetch_add(1, Ordering::Relaxed);
        if current >= tenant.max_concurrency {
            tenant.in_flight.fetch_sub(1, Ordering::Relaxed);
            return Err("Max concurrency limit reached");
        }
        Ok(tenant.clone())
    }

    /// Release the slot, add bytes, and deduct the tier price.
    ///
    /// R2-3：允许一次扣成负数（当次流量不断流），欠费由下次 `authenticate` 拦截。
    pub fn release_and_meter(&self, tenant: &TenantAccount, bytes: u64, tier: &str) {
        self.release_and_meter_at_price(tenant, bytes, price_per_gb(tier));
    }

    fn release_and_meter_at_price(&self, tenant: &TenantAccount, bytes: u64, price: f64) {
        // OPT-R4 A1：旧 `load==0` 检查与 `fetch_sub` 非原子，并发双释放可同时过检
        // 把 in_flight 绕回 usize::MAX（永久熔断该租户并发）；改 CAS 循环原子扣槽
        //（不用 `fetch_sub` 返回值判定：零槽先减后加会瞬时暴露 MAX，可被并发
        // authenticate 观测到）。零槽误调直接丢弃＋warn。
        // 可观测说明：MetricsRegistry 暂无租户级 double-release 计数器，不新增指标，
        // 沿 REVIEW-R2 Q6 口径只打 warn 日志（正常单次释放行为不变）。
        let mut cur = tenant.in_flight.load(Ordering::Relaxed);
        loop {
            if cur == 0 {
                // 误调整单直接丢弃（首次释放已计量，重复计量即双重扣费）；只 warn 可观测。
                log::warn!(
                    "[Tenant] release without slot tenant={} (double-release suspected, dropped)",
                    tenant.tenant_id
                );
                return;
            }
            match tenant.in_flight.compare_exchange_weak(
                cur,
                cur - 1,
                Ordering::Relaxed,
                Ordering::Relaxed,
            ) {
                Ok(_) => break,
                Err(actual) => cur = actual,
            }
        }
        tenant.total_bytes.fetch_add(bytes, Ordering::Relaxed);
        let cost = (bytes as f64 / BYTES_PER_GB) * price;
        tenant.balance_usd.fetch_sub(cost, Ordering::Relaxed);
        // 计量改变了余额 ⇒ 标脏。**这是热路径上唯一新增的开销**，且仅在
        // 持久化开启时发生（`mark_dirty` 内部先判后端）。零字节请求直接跳过，
        // 省掉一次 String 克隆。
        if bytes > 0 {
            self.mark_dirty(&tenant.api_key);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn manager_with(api_key: &str, qps: u32, max_c: usize) -> TenantManager {
        let m = TenantManager::new();
        // 存量 helper 走默认 burst（qps/10≥1），语义与 R2-3 前一致。
        m.register_tenant(
            "t-1",
            api_key,
            qps,
            max_c,
            TenantManager::default_burst_for_qps(qps),
        );
        m
    }

    #[test]
    fn price_table_matches_plan() {
        assert_eq!(price_per_gb("datacenter"), 0.2);
        assert_eq!(price_per_gb("dc"), 0.2);
        assert_eq!(price_per_gb("residential"), 3.0);
        assert_eq!(price_per_gb("mobile"), 15.0);
        assert_eq!(price_per_gb("weird"), 0.2);
        assert_eq!(price_per_gb("free"), 0.0);
    }

    #[test]
    fn qps_overflow_rejected() {
        let m = manager_with("k1", 1, 100);
        let a = m.authenticate_and_throttle("k1").expect("first");
        // Second immediate check exhausts the 1/s bucket.
        let err = m.authenticate_and_throttle("k1").unwrap_err();
        assert!(err.contains("Rate limit"), "{err}");
        m.release_and_meter(&a, 0, "datacenter");
    }

    #[test]
    fn concurrency_cap_rejected_and_slot_returned() {
        let m = manager_with("k2", 10_000, 1);
        let a = m.authenticate_and_throttle("k2").expect("slot 1");
        let err = m.authenticate_and_throttle("k2").unwrap_err();
        assert!(err.contains("concurrency"), "{err}");
        // Rejected attempt must not leak the slot.
        assert_eq!(a.in_flight.load(Ordering::Relaxed), 1);
        m.release_and_meter(&a, 512, "residential");
        assert_eq!(a.in_flight.load(Ordering::Relaxed), 0);
        assert_eq!(a.total_bytes.load(Ordering::Relaxed), 512);
    }

    #[test]
    fn double_release_does_not_wrap() {
        // REVIEW-R2 Q6：误调第二次 release 不得 wrap 到 MAX（永久熔断并发）；槽回 0。
        let m = manager_with("k-double", 10_000, 100);
        let a = m.authenticate_and_throttle("k-double").expect("auth");
        m.release_and_meter(&a, 0, "datacenter");
        assert_eq!(a.in_flight.load(Ordering::Relaxed), 0);
        m.release_and_meter(&a, 0, "datacenter");
        assert_eq!(
            a.in_flight.load(Ordering::Relaxed),
            0,
            "double release must not wrap in_flight"
        );
    }

    #[test]
    fn concurrent_double_release_no_wraparound() {
        // OPT-R4 A1：`load==0` 检查与 `fetch_sub` 非原子，并发双释放可同时过检
        // 把 in_flight 绕回 usize::MAX（永久熔断该租户并发）。
        // Barrier 对齐 8 线程同抢 1 个槽位：旧写法高概率回绕（红），CAS 修复后恒为 0（绿）。
        // bytes=0 使计量无副作用，只验槽位有界。
        let m = manager_with("k-a1-race", 10_000, 100);
        let a = m.authenticate_and_throttle("k-a1-race").expect("auth");
        assert_eq!(a.in_flight.load(Ordering::Relaxed), 1);
        for _ in 0..200 {
            a.in_flight.store(1, Ordering::Relaxed);
            let barrier = Arc::new(std::sync::Barrier::new(8));
            let mgr = &m;
            std::thread::scope(|s| {
                for _ in 0..8 {
                    let b = barrier.clone();
                    let acc = a.clone();
                    s.spawn(move || {
                        b.wait();
                        mgr.release_and_meter(&acc, 0, "datacenter");
                    });
                }
            });
            let v = a.in_flight.load(Ordering::Relaxed);
            assert!(
                v <= 1,
                "concurrent double-release must not wrap in_flight, got {v}"
            );
            assert_eq!(v, 0, "8 releases on 1 slot must drain to 0, got {v}");
        }
        // 计量无副作用（bytes=0）：total_bytes 保持 0，balance 保持 100。
        assert_eq!(a.total_bytes.load(Ordering::Relaxed), 0);
        assert!((a.balance_usd.load(Ordering::Relaxed) - 100.0).abs() < 1e-9);
    }

    #[test]
    fn metering_deducts_tier_price() {
        let m = manager_with("k3", 10_000, 100);
        let a = m.authenticate_and_throttle("k3").expect("auth");
        let one_gb = 1024 * 1024 * 1024u64;
        m.release_and_meter(&a, one_gb, "residential");
        assert!((a.balance_usd.load(Ordering::Relaxed) - 97.0).abs() < 1e-9);
        let b = m.authenticate_and_throttle("k3").expect("auth");
        m.release_and_meter(&b, one_gb, "mobile");
        assert!((b.balance_usd.load(Ordering::Relaxed) - 82.0).abs() < 1e-9);
    }

    #[test]
    fn unknown_and_disabled_keys_rejected() {
        let m = manager_with("k4", 10_000, 100);
        assert_eq!(
            m.authenticate_and_throttle("nope").unwrap_err(),
            "Invalid API Key"
        );
        assert!(m.set_active("k4", false));
        assert_eq!(
            m.authenticate_and_throttle("k4").unwrap_err(),
            "Tenant is disabled"
        );
        assert!(!m.set_active("ghost", false));
    }

    #[test]
    fn default_burst_scales_with_qps() {
        // R2-3：qps/10，至少 1（qps=1 老行为 burst=1 不变）。
        assert_eq!(TenantManager::default_burst_for_qps(1), 1);
        assert_eq!(TenantManager::default_burst_for_qps(9), 1);
        assert_eq!(TenantManager::default_burst_for_qps(50), 5);
        assert_eq!(TenantManager::default_burst_for_qps(10_000), 1_000);
    }

    #[test]
    fn burst_allows_short_burst_then_shapes() {
        // R2-3：burst=5 时 5 连击全过、第 6 次被整形；拒绝不占槽。
        let m = TenantManager::new();
        m.register_tenant("t-b", "kb", 5, 10, 5);
        let mut held = Vec::new();
        for _ in 0..5 {
            held.push(m.authenticate_and_throttle("kb").expect("burst slot"));
        }
        let err = m.authenticate_and_throttle("kb").unwrap_err();
        assert!(err.contains("Rate limit"), "{err}");
        assert_eq!(held[0].in_flight.load(Ordering::Relaxed), 5);
        for a in &held {
            m.release_and_meter(a, 0, "datacenter");
        }
        assert_eq!(held[0].in_flight.load(Ordering::Relaxed), 0);
    }

    #[test]
    fn insufficient_balance_rejected_without_slot() {
        // R2-3：一次性扣穿（1TB residential ≈ $3072 > $100 起步余额），
        // 当次不断流（release 照常），下次鉴权拦截且不占槽。
        let m = manager_with("k5", 10_000, 100);
        let a = m.authenticate_and_throttle("k5").expect("auth");
        let one_tb = 1024u64 * 1024 * 1024 * 1024;
        m.release_and_meter(&a, one_tb, "residential");
        assert!(a.balance_usd.load(Ordering::Relaxed) < 0.0);
        let before = a.in_flight.load(Ordering::Relaxed);
        let err = m.authenticate_and_throttle("k5").unwrap_err();
        assert_eq!(err, "Insufficient balance");
        // 拒绝路径不取槽。
        assert_eq!(a.in_flight.load(Ordering::Relaxed), before);
    }

    // ---- Admin API：set_balance（PATCH /tenants/{api_key}/balance 实装）----

    /// `set_balance` 写入即生效：detail 立即反映新值；**欠费门即刻跟上**
    ///（置 0 ⇒ 下次鉴权 `Insufficient balance` 且不占槽；恢复余额后可通行）。
    /// 这是 PATCH balance 从「改本地副本的假实现」到「实装」的核心验收。
    #[test]
    fn admin_set_balance_persists_and_gate_follows() {
        let m = manager_with("k9", 10_000, 100);
        assert!(m.set_balance("k9", 3.5));
        let d = m.tenant_detail("k9").expect("detail");
        assert!((d.balance_usd - 3.5).abs() < 1e-9, "detail 必须读到新余额");

        // 置 0 ⇒ 欠费门拦截（不占槽；拒绝发生在 QPS/并发之前）。
        assert!(m.set_balance("k9", 0.0));
        let err = m.authenticate_and_throttle("k9").unwrap_err();
        assert_eq!(err, "Insufficient balance");

        // 恢复余额 ⇒ 可正常取槽；用完照常释放计量。
        assert!(m.set_balance("k9", 1.0));
        let acc = m.authenticate_and_throttle("k9").expect("balance restored");
        m.release_and_meter(&acc, 0, "datacenter");
    }

    /// 幂等/容错面：未知 key 返回 false（与 `set_active` 同风格），
    /// 不触碰任何既有租户。
    #[test]
    fn admin_set_balance_unknown_key_returns_false() {
        let m = manager_with("k10", 10_000, 100);
        assert!(!m.set_balance("ghost", 1.0));
        assert!(
            (m.tenant_detail("k10").expect("existing").balance_usd - 100.0).abs() < 1e-9,
            "未知 key 的调用不得影响既有租户余额"
        );
    }

    // ---- OPT-R12：鉴权密钥卫生 ----

    /// B1：`API_KEY` 解析三态，且**误配 fail-closed**。
    #[test]
    fn opt_r12_resolve_api_key_states() {
        use super::{resolve_api_key, DEFAULT_API_KEY};
        // 1) 未设置 → None（调用方回退开发默认）。
        assert_eq!(resolve_api_key(None).unwrap(), None);
        // 2) 设置为强值 → 使用它，且不得等于默认值。
        let k = resolve_api_key(Some("  s3cret-value  ")).unwrap().unwrap();
        assert_eq!(k, "s3cret-value", "应 trim");
        assert_ne!(k, DEFAULT_API_KEY);
        // 3) 误配：空串 / 纯空白 / 显式等于 default_key → 全部 Err（不回退）。
        assert!(
            resolve_api_key(Some("")).is_err(),
            "空串必须拒（否则等于没配）"
        );
        assert!(resolve_api_key(Some("   \t\n")).is_err(), "纯空白必须拒");
        assert!(
            resolve_api_key(Some(DEFAULT_API_KEY)).is_err(),
            "显式设为 default_key 必须拒（否则运维不变且误以为已配）"
        );
    }

    /// A1：危险组合判定——**三条同时才响**。
    ///
    /// 重点是「**不该响的不响**」：本机开发与生产正确配置
    /// 都不应被这条告警扰动——否则它会被当成噪声而弃用。
    #[test]
    fn opt_r12_weak_auth_warning_only_for_genuine_risk() {
        use super::{weak_auth_warning, DEFAULT_API_KEY, TENANT_SENTINEL_QUOTA};
        let q = TENANT_SENTINEL_QUOTA;
        let conc = TENANT_SENTINEL_QUOTA as usize;

        // 应响：弱默认 key 且满配额且**非回环**监听。
        let w = weak_auth_warning(true, q, conc, "0.0.0.0:8916");
        assert!(w.is_some(), "0.0.0.0 上的弱默认 key 必须告警");
        let msg = w.unwrap();
        assert!(msg.contains("API_KEY"), "告警必须给出整改动作（API_KEY）");
        assert!(
            msg.contains("loopback") || msg.contains("127.0.0.1"),
            "告警必须给出绑回环的动作"
        );
        assert!(
            msg.contains(DEFAULT_API_KEY),
            "告警必须明说用的是公开默认值"
        );

        // 不应响 1：本机开发（弱默认 但回环监听）。
        assert!(
            weak_auth_warning(true, q, conc, "127.0.0.1:8916").is_none(),
            "回环监听时不应告警（本机开发场景）"
        );
        // 不应响 2：已用 API_KEY 覆盖（非弱默认）。
        assert!(
            weak_auth_warning(false, q, conc, "0.0.0.0:8916").is_none(),
            "已用 API_KEY 时不应告警"
        );
        // 不应响 3：配额已收窄（危险已被限制）。
        assert!(
            weak_auth_warning(true, 100, 100, "0.0.0.0:8916").is_none(),
            "配额低于哨兵时不应告警"
        );
    }

    /// 回环判定：只认明确回环写法，**保守取向**。
    #[test]
    fn opt_r12_is_loopback_listen_conservative() {
        use super::is_loopback_listen;
        for yes in [
            "127.0.0.1:8916",
            "127.0.0.1",
            " 127.0.0.1:1 ",
            "localhost:8916",
            "LOCALHOST:8916",
            "127.5.5.5:8916",
            "[::1]:8916",
            "::1",
        ] {
            assert!(is_loopback_listen(yes), "应判为回环: {yes:?}");
        }
        for no in [
            "0.0.0.0:8916",
            "0.0.0.0",
            "::",
            "[::]:8916",
            "10.0.0.5:8916",
            "192.168.1.9:8916",
            "example.com:8916",
            "",
            "   ",
        ] {
            assert!(!is_loopback_listen(no), "不应判为回环: {no:?}");
        }
    }

    // ── OPT-R18 P2-2：租户状态持久化 ──────────────────────────────────
    //
    // 背景（实测留痕，见 TASK_PLAN 步骤 60）：改之前租户表纯内存，
    // `PATCH balance=42.5` → 重启 → 又变回 100.0。下面这组测试锁住修复。

    /// 接在记录器上的 manager（与 `TENANT_STORE=redis` 走同一套算法）。
    fn persistent_manager() -> (TenantManager, std::sync::Arc<RecordingBackend>) {
        let backend = RecordingBackend::new();
        let m = TenantManager::with_store(Store::Recording(std::sync::Arc::clone(&backend)));
        (m, backend)
    }

    fn register_default(m: &TenantManager, key: &str) {
        m.register_tenant(
            "t-1",
            key,
            100,
            100,
            TenantManager::default_burst_for_qps(100),
        );
    }

    /// 可控阻塞后端：精确停在快照已生成、写入未完成的窗口，避免依赖 sleep 猜竞态。
    struct PausedWriteBackend {
        inner: Arc<RecordingBackend>,
        entered: tokio::sync::Notify,
        resume: tokio::sync::Notify,
    }

    impl PausedWriteBackend {
        fn new(inner: Arc<RecordingBackend>) -> Self {
            Self {
                inner,
                entered: tokio::sync::Notify::new(),
                resume: tokio::sync::Notify::new(),
            }
        }
    }

    impl PersistBackend for PausedWriteBackend {
        async fn write(&self, key: &str, rec: &PersistedTenant) -> Result<(), String> {
            self.entered.notify_one();
            self.resume.notified().await;
            self.inner.write(key, rec).await
        }

        async fn read_all(&self) -> Result<PersistedRecords, String> {
            self.inner.read_all().await
        }
    }

    /// 老快照写成功不能确认等待期间产生的新修改；空闲后仍须补写最新余额。
    #[tokio::test]
    async fn flush_preserves_changes_made_during_write() {
        let (m, backend) = persistent_manager();
        register_default(&m, "k1");
        let paused = PausedWriteBackend::new(Arc::clone(&backend));
        let (result, ()) = tokio::join!(m.flush_via(&paused), async {
            paused.entered.notified().await;
            assert!(m.set_balance("k1", 42.5));
            assert!(m.set_active("k1", false));
            paused.resume.notify_one();
        });
        assert_eq!(result.expect("首次写入"), 1);
        assert_eq!(backend.stored("k1").unwrap()["balance_usd"], "100");
        assert_eq!(m.flush_dirty().await.expect("补写新版本"), 1);
        let stored = backend.stored("k1").unwrap();
        assert_eq!(stored["balance_usd"], "42.5");
        assert_eq!(stored["is_active"], "0");
        assert_eq!(m.flush_dirty().await.expect("已持久化后空闲"), 0);
    }

    /// 取消进行中的 flush 不得吞掉待保存状态，下一轮仍可重试。
    #[tokio::test]
    async fn cancelled_flush_keeps_pending_state() {
        let (m, backend) = persistent_manager();
        register_default(&m, "k1");
        let paused = PausedWriteBackend::new(Arc::clone(&backend));
        {
            let pending = m.flush_via(&paused);
            tokio::pin!(pending);
            assert!(futures::poll!(pending.as_mut()).is_pending());
        }
        assert_eq!(backend.writes(), 0);
        assert_eq!(m.flush_dirty().await.expect("取消后重试"), 1);
    }

    /// 周期写回占用提交槽时，停机写回必须等待，不能用新快照抢先覆盖存储。
    #[tokio::test]
    async fn concurrent_flushes_commit_in_order() {
        let (m, backend) = persistent_manager();
        register_default(&m, "k1");
        let paused = PausedWriteBackend::new(Arc::clone(&backend));
        let (first, ()) = tokio::join!(m.flush_via(&paused), async {
            paused.entered.notified().await;
            m.set_balance("k1", 17.0);
            {
                let second = m.flush_dirty();
                tokio::pin!(second);
                assert!(
                    futures::poll!(second.as_mut()).is_pending(),
                    "旧写回未完成时，新写回不得提交"
                );
            }
            paused.resume.notify_one();
        });
        assert_eq!(first.expect("旧版本完成"), 1);
        assert_eq!(m.flush_dirty().await.expect("新版本补写"), 1);
        assert_eq!(backend.stored("k1").unwrap()["balance_usd"], "17");
    }

    #[tokio::test]
    #[ignore = "requires isolated Redis at 127.0.0.1:48173"]
    async fn redis_backend_restore_discovers_unindexed_hashes() {
        let client = redis::Client::open("redis://127.0.0.1:48173/").unwrap();
        let conn = redis::aio::ConnectionManager::new(client).await.unwrap();
        let prefix = format!(
            "restore:test:[{}]*?",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        );
        let backend = RedisBackend {
            conn: conn.clone(),
            prefix: prefix.clone(),
        };
        let manager = TenantManager::with_store(Store::Redis {
            conn: conn.clone(),
            prefix: prefix.clone(),
        });
        manager.register_tenant("test", "k1", 100, 100, 100);
        let record = PersistedTenant {
            tenant_id: "test".into(),
            qps: 100,
            max_concurrency: 100,
            is_active: false,
            balance_usd: 42.5,
            total_bytes: 7,
        };
        backend.write("k1", &record).await.unwrap();
        let mut conn = conn;
        let legacy = format!("{prefix}:legacy");
        redis::cmd("HSET")
            .arg(&legacy)
            .arg("balance_usd")
            .arg("9")
            .query_async::<()>(&mut conn)
            .await
            .unwrap();
        let raw = backend.read_all().await.unwrap();
        assert_eq!(raw.missing_index_keys, vec!["legacy"]);
        assert!(raw
            .records
            .iter()
            .any(|(key, value)| key == "legacy" && value.balance_usd == 9.0));
        let restored = manager.load_persisted_report().await.unwrap();
        assert_eq!(restored.applied, 1);
        assert_eq!(restored.orphan_keys, vec!["legacy"]);
        assert!(!restored.is_incomplete());
        assert_eq!(manager.tenant_detail("k1").unwrap().balance_usd, 42.5);
        assert!(manager.tenant_detail("legacy").is_none());

        // A foreign prefixed value is diagnostic; a configured wrong type is incomplete.
        redis::cmd("DEL")
            .arg(&legacy)
            .query_async::<()>(&mut conn)
            .await
            .unwrap();
        redis::cmd("SET")
            .arg(&legacy)
            .arg("not-a-hash")
            .query_async::<()>(&mut conn)
            .await
            .unwrap();
        let restored = manager.load_persisted_report().await.unwrap();
        assert_eq!(restored.orphan_keys, vec!["legacy"]);
        assert!(!restored.is_incomplete());
        let configured = format!("{prefix}:k1");
        redis::cmd("SET")
            .arg(&configured)
            .arg("not-a-hash")
            .query_async::<()>(&mut conn)
            .await
            .unwrap();
        let restored = manager.load_persisted_report().await.unwrap();
        assert_eq!(restored.malformed_keys, vec!["k1"]);
        assert_eq!(restored.applied, 0);
        assert!(restored.is_incomplete());
        redis::cmd("DEL")
            .arg(&configured)
            .query_async::<()>(&mut conn)
            .await
            .unwrap();
        redis::cmd("HSET")
            .arg(&configured)
            .arg("balance_usd")
            .arg("5")
            .query_async::<()>(&mut conn)
            .await
            .unwrap();
        redis::cmd("SREM")
            .arg(format!("{prefix}:index"))
            .arg("k1")
            .query_async::<()>(&mut conn)
            .await
            .unwrap();
        let restored = manager.load_persisted_report().await.unwrap();
        assert_eq!(restored.malformed_keys, vec!["k1"]);
        assert_eq!(restored.applied, 0);
        assert_eq!(manager.tenant_detail("k1").unwrap().balance_usd, 42.5);
        assert!(restored.is_incomplete());
        // Delete only this test's exact keys, never the shared database.
        redis::cmd("DEL")
            .arg(&configured)
            .arg(&legacy)
            .arg(format!("{prefix}:index"))
            .query_async::<()>(&mut conn)
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn unknown_unindexed_record_is_only_one_orphan_diagnostic() {
        let backend = RecordingBackend::new();
        backend.put_raw("k1", &[("balance_usd", "42.5")]);
        backend.store_unindexed_raw("legacy", &[("balance_usd", "9")]);
        let manager = TenantManager::with_store(Store::Recording(backend));
        manager.register_tenant("t-1", "k1", 100, 100, 100);
        let report = manager.load_persisted_report().await.unwrap();
        assert_eq!(report.applied, 1);
        assert_eq!(report.orphan_keys, vec!["legacy"]);
        assert!(report.malformed_keys.is_empty());
        assert!(!report.is_incomplete());
        assert!(manager.tenant_detail("legacy").is_none());
    }

    #[tokio::test]
    async fn malformed_unknown_record_is_orphan_only() {
        let backend = RecordingBackend::new();
        backend.put_raw("k1", &[("balance_usd", "42.5")]);
        backend.put_raw("legacy", &[("balance_usd", "NaN")]);
        let manager = TenantManager::with_store(Store::Recording(backend));
        manager.register_tenant("t-1", "k1", 100, 100, 100);
        let report = manager.load_persisted_report().await.unwrap();
        assert_eq!(report.orphan_keys, vec!["legacy"]);
        assert!(report.malformed_keys.is_empty());
        assert!(!report.is_incomplete());
    }

    #[tokio::test]
    async fn complete_restore_reports_all_configured_tenants_applied() {
        let backend = RecordingBackend::new();
        backend.put_raw(
            "k1",
            &[
                ("tenant_id", "t-1"),
                ("qps", "100"),
                ("max_concurrency", "100"),
                ("is_active", "1"),
                ("balance_usd", "42.5"),
                ("total_bytes", "7"),
            ],
        );
        let manager = TenantManager::with_store(Store::Recording(Arc::clone(&backend)));
        manager.register_tenant("t-1", "k1", 100, 100, 100);

        let report = manager
            .load_persisted_report()
            .await
            .expect("restore report");

        assert_eq!(report.applied, 1);
        assert!(report.has_persisted_records);
        assert!(report.missing_configured_keys.is_empty());
        assert!(report.malformed_keys.is_empty());
        assert!(!report.is_incomplete());
    }

    #[tokio::test]
    async fn indexed_configured_tenant_with_missing_hash_is_incomplete() {
        let backend = RecordingBackend::new();
        backend.put_raw(
            "k1",
            &[
                ("tenant_id", "t-1"),
                ("qps", "100"),
                ("max_concurrency", "100"),
                ("is_active", "1"),
                ("balance_usd", "42.5"),
                ("total_bytes", "7"),
            ],
        );
        backend.remove_hash("k1");
        let manager = TenantManager::with_store(Store::Recording(Arc::clone(&backend)));
        manager.register_tenant("t-1", "k1", 100, 100, 100);

        let report = manager
            .load_persisted_report()
            .await
            .expect("restore report");

        assert!(report.has_persisted_records);
        assert!(report.malformed_keys.contains(&"k1".to_string()));
        assert!(report.is_incomplete());
    }

    #[tokio::test]
    async fn orphaned_persisted_tenant_does_not_resurrect_or_mark_restore_incomplete() {
        let backend = RecordingBackend::new();
        backend.put_raw(
            "orphan-key",
            &[
                ("tenant_id", "deleted-tenant"),
                ("qps", "100"),
                ("max_concurrency", "100"),
                ("is_active", "1"),
                ("balance_usd", "9.0"),
                ("total_bytes", "1"),
            ],
        );
        let manager = TenantManager::with_store(Store::Recording(Arc::clone(&backend)));

        let report = manager
            .load_persisted_report()
            .await
            .expect("restore report");

        assert!(manager.tenant_detail("orphan-key").is_none());
        assert!(report.has_persisted_records);
        assert_eq!(report.orphan_keys, vec!["orphan-key"]);
        assert_eq!(report.configured, 0);
        assert_eq!(report.applied, 0);
        assert!(!report.is_incomplete());
    }

    #[tokio::test]
    async fn configured_tenant_without_persisted_index_is_incomplete_after_restore_exists() {
        let backend = RecordingBackend::new();
        backend.store_unindexed_raw(
            "other-key",
            &[
                ("tenant_id", "other"),
                ("qps", "100"),
                ("max_concurrency", "100"),
                ("is_active", "1"),
                ("balance_usd", "42.5"),
                ("total_bytes", "7"),
            ],
        );
        let manager = TenantManager::with_store(Store::Recording(Arc::clone(&backend)));
        manager.register_tenant("t-2", "k2", 100, 100, 100);

        let report = manager
            .load_persisted_report()
            .await
            .expect("restore report");

        assert!(report.has_persisted_records);
        assert_eq!(report.missing_configured_keys, vec!["k2"]);
        assert!(report.is_incomplete());
    }

    #[tokio::test]
    async fn removed_persisted_index_member_reports_malformed_tenant() {
        let backend = RecordingBackend::new();
        backend.put_raw(
            "k1",
            &[
                ("tenant_id", "t-1"),
                ("qps", "100"),
                ("max_concurrency", "100"),
                ("is_active", "1"),
                ("balance_usd", "42.5"),
                ("total_bytes", "7"),
            ],
        );
        backend.remove_index_member("k1");
        let manager = TenantManager::with_store(Store::Recording(Arc::clone(&backend)));
        manager.register_tenant("t-1", "k1", 100, 100, 100);

        let report = manager
            .load_persisted_report()
            .await
            .expect("restore report");

        assert!(report.has_persisted_records);
        assert!(report.orphan_keys.is_empty());
        assert_eq!(report.malformed_keys, vec!["k1"]);
        assert_eq!(report.applied, 0);
        assert_eq!(manager.tenant_detail("k1").unwrap().balance_usd, 100.0);
        assert!(report.is_incomplete());
    }

    #[tokio::test]
    async fn indexed_configured_tenant_with_missing_hash_is_reported_malformed() {
        let backend = RecordingBackend::new();
        backend.put_raw(
            "k1",
            &[
                ("tenant_id", "t-1"),
                ("qps", "100"),
                ("max_concurrency", "100"),
                ("is_active", "0"),
                ("balance_usd", "42.5"),
                ("total_bytes", "7"),
            ],
        );
        backend.remove_hash("k1");
        let manager = TenantManager::with_store(Store::Recording(Arc::clone(&backend)));
        manager.register_tenant("t-1", "k1", 100, 100, 100);

        let report = manager
            .load_persisted_report()
            .await
            .expect("restore report");

        assert_eq!(report.applied, 0);
        assert!(report.malformed_keys.contains(&"k1".to_string()));
        assert!(report.is_incomplete());
    }

    #[tokio::test]
    async fn malformed_indexed_tenant_record_is_reported_and_not_applied() {
        let backend = RecordingBackend::new();
        backend.put_raw("k1", &[("balance_usd", "NaN")]);
        let manager = TenantManager::with_store(Store::Recording(Arc::clone(&backend)));
        manager.register_tenant("t-1", "k1", 100, 100, 100);

        let report = manager
            .load_persisted_report()
            .await
            .expect("restore report");

        assert_eq!(report.applied, 0);
        assert_eq!(report.malformed_keys, vec!["k1"]);
        assert!(report.is_incomplete());
        assert_eq!(manager.tenant_detail("k1").unwrap().balance_usd, 100.0);
    }

    #[tokio::test]
    async fn required_restore_reports_missing_configured_tenant_state() {
        let backend = RecordingBackend::new();
        backend.put_raw(
            "k1",
            &[
                ("tenant_id", "t-1"),
                ("qps", "100"),
                ("max_concurrency", "100"),
                ("is_active", "1"),
                ("balance_usd", "42.5"),
                ("total_bytes", "7"),
            ],
        );
        let manager = TenantManager::with_store(Store::Recording(Arc::clone(&backend)));
        manager.register_tenant("t-1", "k1", 100, 100, 100);
        manager.register_tenant("t-2", "k2", 100, 100, 100);

        let report = manager
            .load_persisted_report()
            .await
            .expect("restore report");

        assert_eq!(report.applied, 1);
        assert!(report.has_persisted_records);
        assert_eq!(report.missing_configured_keys, vec!["k2"]);
        assert!(report.is_incomplete());
    }

    /// 写出去 → 读回来，运行期可变状态一字不差。
    #[tokio::test]
    async fn p2_2_flush_then_load_roundtrips_runtime_state() {
        let (m, backend) = persistent_manager();
        register_default(&m, "k1");
        // 先把"刚注册"这轮写掉，清空脏集，好让后面的断言各自独立。
        assert_eq!(m.flush_dirty().await.expect("flush#0"), 1);

        // 计量**必须自己标脏**：此时脏集是空的，若 `release_and_meter` 忘了
        // 标脏，下面这次 flush 会返回 0——用掉的流量与余额就永久丢失，
        // 而且是"重启后才暴露"的那种丢失。
        let acct = m.authenticate_and_throttle("k1").expect("auth");
        m.release_and_meter(&acct, 1_073_741_824, "datacenter"); // 1 GiB × $0.2
        assert_eq!(
            m.flush_dirty().await.expect("flush#1"),
            1,
            "计量后必须重新变脏，否则本次用量不会落盘"
        );
        // 落盘的内容必须是**扣费之后**的值：100 - 0.2。
        let after_meter = backend.stored("k1").expect("hash");
        let bal: f64 = after_meter["balance_usd"].parse().expect("balance");
        assert!((bal - 99.8).abs() < 1e-9, "计量扣费必须落盘，实际 {bal}");
        assert_eq!(after_meter["total_bytes"], "1073741824");

        assert!(m.set_balance("k1", 42.5));
        // 停用必须放在鉴权之后：停用后 `authenticate_and_throttle` 直接拒。
        assert!(m.set_active("k1", false));

        assert_eq!(m.flush_dirty().await.expect("flush#2"), 1);
        let stored = backend.stored("k1").expect("hash 已写入");
        assert_eq!(stored.get("is_active").map(String::as_str), Some("0"));
        assert_eq!(stored.get("tenant_id").map(String::as_str), Some("t-1"));

        // 模拟重启：同一份存储，全新的 manager（余额从 100 起）。
        let m2 = TenantManager::with_store(Store::Recording(std::sync::Arc::clone(&backend)));
        register_default(&m2, "k1");
        assert_eq!(m2.tenant_detail("k1").unwrap().balance_usd, 100.0);
        assert_eq!(m2.load_persisted().await.expect("load"), 1);

        let d = m2.tenant_detail("k1").unwrap();
        assert!(!d.is_active, "启停状态必须跨重启保留");
        assert!(
            (d.balance_usd - 42.5).abs() < 1e-9,
            "余额必须跨重启保留（PATCH 覆盖了扣费后的值），实际 {}",
            d.balance_usd
        );
        assert_eq!(d.total_bytes, 1_073_741_824, "累计流量必须跨重启保留");
    }

    #[tokio::test]
    async fn flusher_stops_when_signaled() {
        let (manager, backend) = persistent_manager();
        register_default(&manager, "k1");
        let (stop, task) = Arc::new(manager)
            .spawn_flusher(std::time::Duration::from_secs(60))
            .expect("persistent flusher");
        stop.send(()).expect("stop flusher");
        task.await.expect("flusher joins after stop");
        assert_eq!(backend.writes(), 0);
    }

    #[tokio::test]
    async fn flusher_stop_waits_for_in_progress_persist_write_then_joins() {
        let (manager, backend) = persistent_manager();
        register_default(&manager, "k1");
        backend.pause_writes();
        let flusher_manager = Arc::new(manager);
        let (stop, task) = flusher_manager
            .spawn_flusher(std::time::Duration::from_millis(1))
            .expect("persistent test flusher");

        backend.wait_until_write_blocked().await;
        let stopped = tokio::spawn(async move {
            stop.send(()).expect("send stop");
            task.await.expect("flusher task");
        });
        tokio::task::yield_now().await;
        assert!(
            !stopped.is_finished(),
            "active write must finish before join"
        );
        backend.resume_writes();
        stopped.await.expect("stop and join flusher");
        assert_eq!(backend.writes(), 1);
    }

    /// 缺省（memory）后端**必须**是完全的空操作：这是"升级后行为零变化"的保证，
    /// 也是"热路径零开销"的前提。
    #[tokio::test]
    async fn p2_2_memory_mode_is_a_pure_noop() {
        // `spawn_flusher` 的接收者是 `&Arc<Self>`（与 `main.rs` 一致），故这里
        // 一开始就用 Arc 包起来。
        let m = std::sync::Arc::new(TenantManager::new());
        register_default(&m, "k1");
        m.set_balance("k1", 1.0);
        m.set_active("k1", false);
        assert!(
            m.dirty.is_empty(),
            "memory 后端不得产生脏标记（否则每请求白克隆一个 String）"
        );
        assert_eq!(m.flush_dirty().await.expect("flush"), 0);
        assert_eq!(m.load_persisted().await.expect("load"), 0);
        assert!(
            m.spawn_flusher(std::time::Duration::from_secs(1)).is_none(),
            "memory 后端不应起一个永远返回 0 的空转任务"
        );
    }

    /// 写失败时**不得**删脏。删早了就是静默丢余额：接口早已对运维报过成功，
    /// 而这条更新再也不会被重试。
    #[tokio::test]
    async fn p2_2_failed_flush_keeps_the_key_dirty() {
        let (m, backend) = persistent_manager();
        register_default(&m, "k1");
        m.set_balance("k1", 7.0);
        backend.fail_on("k1");

        let err = m.flush_dirty().await.expect_err("注入失败必须冒泡给调用方");
        assert!(err.contains("租户状态写回失败"), "{err}");
        assert_eq!(backend.writes(), 0);
        assert!(!m.dirty.is_empty(), "失败后脏标记必须保留");

        // 恢复后下一轮必须重试到成功。
        backend.fail_keys.lock().clear();
        assert_eq!(m.flush_dirty().await.expect("retry"), 1);
        assert_eq!(backend.writes(), 1);
        assert!(
            (backend
                .stored("k1")
                .and_then(|h| h.get("balance_usd").cloned())
                .expect("balance 已写入")
                .parse::<f64>()
                .unwrap()
                - 7.0)
                .abs()
                < 1e-12
        );

        // 成功后脏集为空 ⇒ 再 flush 不重复写。
        assert_eq!(m.flush_dirty().await.expect("idle"), 0);
        assert_eq!(backend.writes(), 1, "已成功的 key 不得重复写");
    }

    /// 多 key 场景下**一个都不能丢**：中途失败会让本轮提前返回，剩下的必须
    /// 留在脏集里，由下一轮补齐。
    #[tokio::test]
    async fn p2_2_partial_failure_loses_no_key() {
        let (m, backend) = persistent_manager();
        for k in ["k1", "k2", "k3"] {
            register_default(&m, k);
        }
        backend.fail_on("k2");
        m.flush_dirty().await.expect_err("k2 必然失败");
        let first_round = backend.writes();
        assert!(
            first_round < 3,
            "k2 失败后本轮应提前返回，实际写了 {first_round} 条"
        );

        backend.fail_keys.lock().clear();
        let second_round = m.flush_dirty().await.expect("retry");
        assert_eq!(
            first_round + second_round,
            3,
            "两轮合计必须覆盖全部 3 个 key（丢一个就等于丢一个租户的余额）"
        );
        assert_eq!(backend.writes(), 3, "成功过的 key 不得重写");
    }

    /// 配置里已删除的租户**不因 Redis 里有记录而复活**。
    ///
    /// 反例（Redis 即真相）会造出"删不掉的租户"：运维从配置里去掉 key，
    /// 它却继续生效——比丢余额更难排查。
    #[tokio::test]
    async fn p2_2_load_does_not_resurrect_tenant_absent_from_config() {
        let (m, backend) = persistent_manager();
        register_default(&m, "k1");
        backend.put_raw(
            "ghost",
            &[
                ("tenant_id", "g"),
                ("qps", "1"),
                ("max_concurrency", "1"),
                ("is_active", "1"),
                ("balance_usd", "7.0"),
                ("total_bytes", "0"),
            ],
        );
        assert_eq!(m.load_persisted().await.expect("load"), 0);
        assert!(
            m.tenant_detail("ghost").is_none(),
            "Redis 记录不得复活已从配置删除的租户"
        );
    }

    /// 坏余额整条丢弃并保持内存值；其余字段坏了只回落，不连坐。
    ///
    /// `NaN`/`inf` 会绕过欠费门（`NaN <= 0.0` 为 false ⇒ 永不拒付），所以
    /// 余额是这条记录里唯一"错了就会算错钱"的字段。
    #[tokio::test]
    async fn p2_2_load_rejects_non_finite_or_missing_balance() {
        let (m, backend) = persistent_manager();
        register_default(&m, "k1");
        for bad in ["NaN", "inf", "-inf", "not-a-number", ""] {
            backend.put_raw(
                "k1",
                &[
                    ("balance_usd", bad),
                    ("is_active", "0"),
                    ("total_bytes", "999"),
                ],
            );
            assert_eq!(
                m.load_persisted().await.expect("load"),
                0,
                "坏余额必须整条丢弃: {bad:?}"
            );
            let d = m.tenant_detail("k1").unwrap();
            assert_eq!(d.balance_usd, 100.0, "内存值必须保持不变: {bad:?}");
            assert!(d.is_active, "坏记录不得改动 is_active: {bad:?}");
            assert_eq!(d.total_bytes, 0, "坏记录不得改动 total_bytes: {bad:?}");
        }
        // 字段整个缺失同样视为坏记录（不猜）。
        backend.put_raw("k1", &[("is_active", "0")]);
        assert_eq!(m.load_persisted().await.expect("load"), 0);
        assert_eq!(m.tenant_detail("k1").unwrap().balance_usd, 100.0);
    }

    /// 旧版本写下的记录缺新字段时仍可用（升级不能等于清零）。
    #[tokio::test]
    async fn p2_2_load_tolerates_missing_optional_fields() {
        let (m, backend) = persistent_manager();
        register_default(&m, "k1");
        backend.put_raw("k1", &[("balance_usd", "12.5")]);
        assert_eq!(m.load_persisted().await.expect("load"), 1);
        let d = m.tenant_detail("k1").unwrap();
        assert!((d.balance_usd - 12.5).abs() < 1e-12);
        assert!(d.is_active, "缺 is_active 时回落 true（与注册默认一致）");
        assert_eq!(d.total_bytes, 0);
    }

    /// **这条测试是 `main.rs` 里"先 load 再起 flusher"的证据。**
    ///
    /// 只把顺序写进注释是不够的：注释不会变红。这里把两种顺序都跑一遍，
    /// 证明反序确实会把余额覆盖回 100——于是"顺序不可颠倒"不再是传说。
    #[tokio::test]
    async fn p2_2_load_must_precede_flush() {
        let seeded = [
            ("balance_usd", "42.5"),
            ("is_active", "1"),
            ("total_bytes", "0"),
        ];

        // 正确顺序：register（标脏）→ load → flush ⇒ 42.5 被保住。
        let ok_backend = RecordingBackend::new();
        ok_backend.put_raw("k1", &seeded);
        let ok = TenantManager::with_store(Store::Recording(std::sync::Arc::clone(&ok_backend)));
        register_default(&ok, "k1");
        assert_eq!(ok.load_persisted().await.expect("load"), 1);
        assert_eq!(ok.flush_dirty().await.expect("flush"), 1);
        assert!(
            (ok.tenant_detail("k1").unwrap().balance_usd - 42.5).abs() < 1e-12,
            "先加载后 flush 必须保住上次余额"
        );

        // 反序：register → flush → load ⇒ 首轮 flush 已把 100 写回，load 读到 100。
        let bad_backend = RecordingBackend::new();
        bad_backend.put_raw("k1", &seeded);
        let bad = TenantManager::with_store(Store::Recording(std::sync::Arc::clone(&bad_backend)));
        register_default(&bad, "k1");
        assert_eq!(bad.flush_dirty().await.expect("flush"), 1);
        assert_eq!(bad.load_persisted().await.expect("load"), 1);
        assert!(
            (bad.tenant_detail("k1").unwrap().balance_usd - 100.0).abs() < 1e-12,
            "反序会把余额覆盖回 100 —— 这正是 main.rs 里顺序不可颠倒的理由"
        );
    }
}
