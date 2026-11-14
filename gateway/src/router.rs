//! GW-1 无锁路由器：ArcSwap 快照 + DashMap 会话 + 隔离表。
//!
//! 读路径无锁（`ArcSwap::load`）；控制面原子替换整个池子。
//! OPT-1 补齐：`sweep_expired` 周期清理过期条目（会话/隔离），
//! `snapshot_all` 导出全量快照（供网关臂表修剪用），三者皆为长稳运行防内存泄漏之用。

use crate::janitor;
use crate::model::{EgressProto, ProxyNode};
use arc_swap::ArcSwap;
use dashmap::{DashMap, DashSet};
use parking_lot::Mutex;
// `pick_weighted` 是**测试期参考实现**（`#[cfg(test)]`），`SliceRandom::choose`
// 与 `Rng::gen_range` 仅它独有；故这两条 import 也必须是 `cfg(test)`——
// 否则非测试构建里它们无人使用，`clippy -D warnings` 会判红。
#[cfg(test)]
use rand::seq::SliceRandom;
#[cfg(test)]
use rand::Rng;
use std::collections::HashMap;
use std::sync::atomic::AtomicUsize;
use std::sync::Arc;
use std::time::{Duration, Instant};

/// 会话粘性有效期（秒）：超时后会话绑定失效，可被清理。
const SESSION_TTL_SECS: u64 = 600;

/// OPT-R4 A7：会话 id 长度上限（字节）。超长 session 当无 session 处理
/// （只走无状态选路，不查表不落表），防随机长 key 在两次 sweep 间打爆 DashMap；
/// 按字节数判定（不做切片截断，多字节字符不断裂，永不 panic）。
const SESSION_ID_MAX_BYTES: usize = 64;

/// OPT-R4 A7：会话表水位上限（条目数）。满水位后新 key 拒绝粘滞只走无状态
/// （不新增条目但仍返回无状态选中节点）；已存 key 仍可粘滞复用。
/// 并发下 len 检查与 insert 非原子，水位为软上限（允许轻微超限），只防无限膨胀。
const SESSION_MAX_ENTRIES: usize = 8192;

/// 隔离 TTL 上限（秒，24h）：`set_quarantine` 与 PubSub 解析共用。
/// 复审结论：`Instant + Duration` 会溢出 panic——u64::MAX 级输入（毒报文/非法 env）
/// 必须钳制；上限远超业务 TTL（60/600s），钳制无行为影响。
pub const QUARANTINE_MAX_TTL_SECS: u64 = 86400;

/// OPT-R11 C1：隔离 domain 长度上限（字节）。
///
/// 隔离表外层 key 是**客户端可控的归一化 `Host`**（写入点 `set_quarantine`
/// 源自遥测 `domain` 字段，`apply_delta` 侧来自 Redis PubSub 报文）。攻击者
/// 或可写 Redis 者可用超长 domain 直接撑爆外层 `DashMap` 的 key 存储。
///
/// 判定用**字节数**且**不截断**（与 `SESSION_ID_MAX_BYTES` 同一风格）——
/// 超长即拒绝落表，而不是截短成可能撞车的另一个 key。
pub const QUARANTINE_DOMAIN_MAX_BYTES: usize = 253;

/// OPT-R11 C1：隔离表**外层** domain 条目水位。
///
/// 已有 `SESSION_MAX_ENTRIES=8192` 这一同构先例：满水位后新 key 拒绝落表、
/// 已存 key 仍可用。会话表按 session_id 有界，隔离表此前**两样都没有**。
///
/// # 为何是「外层」条目数而不是「内层 ip 数」
///
/// `quarantine_len()` 统计的是**内层** ip 条目总数（`metrics` 的
/// `quarantine_nodes` gauge 用的就是它）。内存放大主要来自外层：每个外层
/// 条目都要付一次 `DashMap` 的 entry 分配 ＋ 一个内层 `DashMap` 的
/// `Vec<AtomicUsize>` 桶开销，而**内层为空的外层条目**（所有 ip 都过期后
/// sweep 前的窗口）几乎只花钱不办事。故水位卡外层。
///
/// 与 `SESSION_MAX_ENTRIES` 同理：并发下 `len()` 检查与 `insert` 非原子，
/// 水位为**软上限**（允许轻微超限），只防无限膨胀。
pub const QUARANTINE_MAX_DOMAINS: usize = 8192;

/// R2-2 域名归一化：去空白 → 剥端口（`host:port`，端口须全数字）→ 去尾点 → 小写。
///
/// - 目标是让隔离 key 与选路匹配对大小写/端口变体一致；
/// - IPv6 字面量（`[::1]:8080`）按括号处理；解析失败时原样小写返回（永不 panic）。
pub fn normalize_domain(raw: &str) -> String {
    let mut host = raw.trim();
    if host.is_empty() {
        return String::new();
    }
    // IPv6 `[::1]:8080` → 取括号内。
    if let Some(stripped) = host.strip_prefix('[') {
        if let Some(end) = stripped.find(']') {
            host = &stripped[..end];
        }
    } else if let Some(colon) = host.rfind(':') {
        // `example.com:8080` → 剥端口；`a:b`（后缀非数字）视为域名本身保留。
        if host[colon + 1..].bytes().all(|b| b.is_ascii_digit()) && colon + 1 < host.len() {
            host = &host[..colon];
        }
    }
    host.trim_end_matches('.').to_ascii_lowercase()
}

/// GW-6 Top-K 热节点数量（按 weight 降序截断，供分桶未命中时的快速兜底）。
/// 64 个节点足以覆盖高流量热点，内存占用 negligible；每次池替换时重排。
pub(crate) const HOT_NODES_K: usize = 64;

/// GW-6 选路分桶键（tier + country + proto）。
/// 用于 O(1) 哈希命中候选桶，替代全量池 O(N) 扫描，显著降低数据面延迟。
/// `proto` 存 `Some`（Http/Socks5/Socks4），分桶与 `matches` 的 proto 隔离语义对齐。
#[derive(Debug, Clone, Hash, PartialEq, Eq)]
pub(crate) struct BucketKey {
    pub tier: String,
    pub country: String,
    pub proto: Option<EgressProto>,
}

/// 步骤 54：主动健康连续失败阈值默认值（连续 `Dead` 达到该值才摘除）。
pub const DEFAULT_PROBE_FAILURE_THRESHOLD: u32 = 3;

/// 步骤 54：主动健康阈值上限（防误配把"一次抖动"直接变成摘除）。
pub const PROBE_FAILURE_THRESHOLD_MAX: u32 = 10;

/// 步骤 54：主动健康摘除冷却默认值（秒）。
pub const DEFAULT_PROBE_UNHEALTHY_COOLDOWN_SECS: u64 = 300;

/// 步骤 54：主动健康摘除冷却上限（与隔离 TTL 同口径，24h）。
pub const PROBE_UNHEALTHY_COOLDOWN_MAX_SECS: u64 = QUARANTINE_MAX_TTL_SECS;

/// OPT-R18：免费池探测抽样比例的默认值（百分比）。
///
/// 付费/托管节点**永远 100% 探测**（SLA 主体），只有免费池按比例抽样：
/// 免费池动辄数百地址且大量已死，全量探测会把日志与探测预算吃光。
/// 25% 意味着 4 个 tick（约 4 分钟）覆盖全部免费节点一轮，相对默认
/// 300s 的摘除冷却与 1800s 的免费池 TTL 都足够快。
pub const DEFAULT_PROBE_FREE_SAMPLE_PERCENT: u32 = 25;

/// 步骤 54：单次失败回灌的结果。调用方据此打不同日志，测试据此锁定语义。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProbeFailureOutcome {
    /// 失败已计数，但未达到摘除阈值。
    BelowThreshold,
    /// 本次首次进入主动健康摘除。
    MarkedUnhealthy,
    /// 已在摘除中，本次失败刷新冷却截止时间。
    RefreshedUnhealthy,
    /// 安全阀阻止摘除：再摘一个就没有 `weight>0` 节点可用了。
    SuppressedLastNode,
    /// 探测期间节点已被删除或权重归零，不落健康状态。
    NodeGone,
}

/// 步骤 54：单节点主动健康状态。
#[derive(Debug, Clone, Copy, Default)]
struct ActiveHealthState {
    /// 连续 `Dead` 次数；成功时整条删除，冷却到期时保留以便立即重摘。
    consecutive_failures: u32,
    /// `Some(t)` = 主动健康摘除到 t；`None` = 当前未被主动健康摘除。
    unhealthy_until: Option<Instant>,
}

pub struct RouterEngine {
    /// R2-6：池内节点以 `Arc` 持有。读路径只做 `Arc` 克隆（一次原子 +1，
    /// 不碰 `ip/country/tier/provider` 四个 `String`）；控制面整体替换仍无锁。
    pools: ArcSwap<Vec<Arc<ProxyNode>>>,
    /// 步骤 52：**所有池写入**的串行化锁。
    ///
    /// # 为什么必须有
    ///
    /// 此前 `reload_nodes` / `replace_vendor_nodes` / `adjust_vendor_weight` /
    /// `set_paid_weight_boost` / `scale_vendor_weights` 各自做
    /// `load → 构造新 Vec → store`，彼此不互斥。FreePool 的 merge 与 Admin
    /// 调权/增删并发时，**后写者会整体覆盖先写者**（lost update）：例如
    /// merge 刚加入的免费节点被一次调权写回旧快照洗掉，或新增的托管节点被
    /// merge 覆盖。读路径仍无锁（ArcSwap 快照），锁只在低频写路径持有，
    /// 临界区内不做 I/O、不跨 `.await`。
    write_lock: Mutex<()>,
    /// 会话绑定同样存 `Arc`（粘滞命中直接返回引用计数句柄，零 `String` 克隆）。
    session_store: DashMap<String, (Arc<ProxyNode>, Instant)>,
    /// Key = `{domain}:{ip}`, value = quarantine expiry.
    /// 两级隔离表：归一化 domain → (node ip → 隔离到期时刻)。
    ///
    /// OPT-R9 C1：由原扁平 `DashMap<String /*"{domain}:{ip}"*/, Instant>` 改为两级。
    ///
    /// # 为什么改（热路径 O(N) 分配）
    ///
    /// 旧 `is_quarantined` 每次调用都执行 `format!("{}:{ip}", normalize_domain(domain))`
    /// ——即 **1 次 `normalize_domain` 分配 ＋ 1 次 `format!` 分配**。而它经
    /// `is_node_quarantined`（R3-2：代理入口 ip ＋ 真 egress exit_ip **双检**）
    /// 被 `matches` 在**每个节点上**调用 ⇒ 池大小 N 时每请求产生 **2N 次堆分配**
    /// （免费池缺省 `FREE_MAX_NODES=2000` ⇒ 约 4000 次/请求）。
    ///
    /// # 为什么是「两级」而不是「缓存 key」
    ///
    /// 缓存 `domain+ip → String` 的 memo 省不掉分配——查表仍需把 key 拼出来
    /// （一次 `String` clone）。且 key 空间由客户端可控（`Host` 任意变体），
    /// 缓存即无界内存。两级结构把**拼接彻底消除**：外层查 domain（每请求一次
    /// `normalize_domain`），内层直接按节点已有的 `node.ip` 查，**零分配**。
    ///
    /// # 顺带修掉的潜在缺陷：扁平 key 的分隔符碰撞
    ///
    /// 旧 key 用 `:` 拼接，而 `normalize_domain` **允许 domain 含 `:`**
    /// （IPv6 字面量：`normalize_domain("[::1]:8080") == "::1"`；单测
    /// `normalize_domain_cases` 也锁定了 `normalize_domain("a:b") == "a:b"`）。
    /// 于是 `(domain="a:b", ip="10.0.0.1")` 与 `(domain="a", ip="b:10.0.0.1")`
    /// 产生**同一个扁平 key** `a:b:10.0.0.1` —— 互相污染隔离条目。
    /// 两级结构以 `DashMap` 的键相等性取代字符串拼接，**从根上消除该隐忧**。
    ///
    /// # 跨进程协议不变
    ///
    /// Redis `SETEX` / PubSub `PUBLISH` 用的仍是 `circuit_breaker.rs` 侧生成的
    /// 扁平字符串 key（形如 `quarantine:{domain}:{ip}`），本字段只服务**内存查询**。
    /// `set_quarantine` 的对外签名（domain 与 ip 两个入参）保持不变。
    quarantine_map: DashMap<String, DashMap<String, Instant>>,
    /// NEXT-B6：免费套利因子表（(provider, country小写)→factor；`merge_once` 应用，
    /// 审计 scale 不再被下轮合并覆盖；缺省 1.0；键空间随池多样性有界）。
    free_scale: DashMap<(String, String), f64>,
    /// GW-6：按 (tier, country, proto) 分桶的只读索引。
    /// 写路径（reload/replace/adjust）原子重建，读路径 O(1) 哈希命中候选桶，
    /// 避免 O(N) 全量池扫描，显著降低数据面选路延迟（免费池缺省 2000 节点时
    /// 每请求从 ~2000 次归一/匹配降至同桶十几次）。
    buckets: ArcSwap<HashMap<BucketKey, Vec<Arc<ProxyNode>>>>,
    /// GW-6：Top-K 热节点（按 weight 降序截断 HOT_NODES_K），供分桶未命中时的
    /// 快速兜底。每次池替换时重排，无访问计数器开销，实现简单且可预测。
    hot_nodes: ArcSwap<Vec<Arc<ProxyNode>>>,
    /// GW-6 自动调权：付费节点基准权重（addr → boost 前的原始权重），供
    /// 免费池干旱时上调付费权重、恢复后还原。仅记录非 free provider 节点。
    paid_base_weight: DashMap<String, u32>,
    /// SLA 摘除状态独立于当前权重，便于恢复 SLA 自动摘除而不复活手动禁用节点。
    sla_derated_nodes: DashSet<String>,
    /// 步骤 54：主动健康摘除表（addr → 连续失败/冷却截止）。
    ///
    /// 与域级 `quarantine_map` **刻意分离**：后者是"某目标域对该出口熔断"，
    /// 有 TTL 且由真实流量/PubSub 写入；本表是"探针认为节点整体不可用"，
    /// 不依赖目标域，也不写 Redis。两者在 `matches` 中都要过。
    active_health: DashMap<String, ActiveHealthState>,
    /// 主动健康「摘除判定」的串行锁。
    ///
    /// 安全阀要读一个**跨条目**的事实（"当前有多少节点被摘除"），并与随后的
    /// 落状态构成原子操作。若各线程各自读计数再各自落状态，两个节点可能同时
    /// 通过"还剩一个"的检查而一起被摘——安全阀形同虚设。摘除是低频事件
    /// （每节点每冷却周期至多一次），用一把锁换判定与写入的原子性，代价可忽略。
    ///
    /// 与 `write_lock` 的分工：`write_lock` 保护池快照（增删/调权），本锁只保护
    /// 健康表的判定+写入；两者**不同时持有**，故无锁序问题。
    probe_mark_lock: Mutex<()>,
    /// 免费池探测的轮转游标（见 [`Self::probe_candidates`]）。
    ///
    /// 用游标而不是随机抽样：随机不保证覆盖，小池可能连续多轮抽不到尾部节点，
    /// 那些节点就永远得不到恢复机会；游标保证 `ceil(100/pct)` 轮内全覆盖。
    probe_cursor: AtomicUsize,
    /// 连续失败阈值（构造时注入，运行期不可变）。
    probe_failure_threshold: u32,
    /// 摘除冷却（构造时注入，运行期不可变）。
    probe_unhealthy_cooldown: Duration,
}

/// 节点是否来自免费池供给线（provider 前缀判定）。
///
/// 判定逻辑与常量统一放在 `node_registry`（`RESERVED_PROVIDER_PREFIX` 的家），
/// 本处只做转发，避免 `"free-"` 字面量在本仓散落多份。
pub(crate) fn is_free_provider(provider: &str) -> bool {
    crate::node_registry::is_free_provider(provider)
}

/// NEXT-B6：因子表键（provider 精确＋country 小写；与匹配侧 `eq_ignore_ascii_case` 同语义）。
fn scale_key(vendor: &str, country: &str) -> (String, String) {
    (vendor.to_string(), country.to_ascii_lowercase())
}

/// OPT-R4 A7：会话粘滞准入（超长/超量 session → 当无 session 处理）。
/// - 超长：`session_id.len()`（字节）> 64 即拒绝（不查表不落表，只走无状态）；
/// - 超量：表满水位且 key 尚未在表内即拒绝（已存 key 仍可粘滞复用）；
/// - 永不 panic（只做长度/存在性判断，不做字符串切片截断）。
fn session_sticky_allowed(
    store: &DashMap<String, (Arc<ProxyNode>, Instant)>,
    session_id: &str,
) -> bool {
    if session_id.len() > SESSION_ID_MAX_BYTES {
        return false;
    }
    if store.len() >= SESSION_MAX_ENTRIES && !store.contains_key(session_id) {
        return false;
    }
    true
}

// =====================================================================
// GW-5 Admin REST API 只读查询访问器（Phase 1：节点/租户管理后台数据源）。
// NodeFilter 定义在模块级，供 `list_nodes` 与 `admin.rs` 引用。
// =====================================================================

/// Admin API 节点列表过滤条件。全部字段可选，未设置则不过滤。
/// 需 `Deserialize` 以供 axum `Query` extractor 从 URL 查询字符串反序列化。
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize, utoipa::ToSchema)]
pub struct NodeFilter {
    pub tier: Option<String>,
    pub country: Option<String>,
    pub proto: Option<String>,
}

impl RouterEngine {
    /// 默认策略构造器（阈值 3 / 冷却 300s）。
    ///
    /// 生产路径走 [`Self::new_with_probe_policy`]（策略由 env 注入）；
    /// 本构造器是**默认策略**的稳定入口，供单测与「不需要自定义策略」的
    /// 调用方使用。
    ///
    /// `cfg_attr(not(test), allow(dead_code))`：非测试构建里主流程已改用
    /// `new_with_probe_policy`，本构造器**只被单测消费**（`admin.rs` /
    /// `pool.rs` / `free_pool.rs` / `gateway.rs` / `vendor_arbitrage.rs` /
    /// `circuit_breaker.rs` 的 `#[cfg(test)]` 模块），故在非测试构建下
    /// 允许 dead——但测试构建下**必须真的被用上**（否则该属性会掩盖问题）。
    #[cfg_attr(not(test), allow(dead_code))]
    pub fn new(initial_nodes: Vec<ProxyNode>) -> Self {
        Self::new_with_probe_policy(
            initial_nodes,
            DEFAULT_PROBE_FAILURE_THRESHOLD,
            DEFAULT_PROBE_UNHEALTHY_COOLDOWN_SECS,
        )
    }

    /// 步骤 54：显式注入主动健康策略的构造器。
    ///
    /// 阈值与冷却由调用方（`main.rs`）从 env 读出并**钳制**后传入——本层
    /// 不做钳制，因为钳制规则属于运维策略（env → 上下限），而构造器只应
    /// 忠实保存已定策略。`cooldown_secs` 为 0 会让摘除立即到期，调用方
    /// 需保证至少 1 秒（`main.rs` 的 `env_u64_clamped` 已钳到 ≥1）。
    pub fn new_with_probe_policy(
        initial_nodes: Vec<ProxyNode>,
        probe_failure_threshold: u32,
        probe_unhealthy_cooldown_secs: u64,
    ) -> Self {
        let pools_vec: Vec<Arc<ProxyNode>> = initial_nodes.into_iter().map(Arc::new).collect();
        let buckets = Self::build_buckets(&pools_vec);
        let hot_nodes = Self::build_hot_nodes(&pools_vec);
        Self {
            pools: ArcSwap::from_pointee(pools_vec),
            write_lock: Mutex::new(()),
            session_store: DashMap::new(),
            quarantine_map: DashMap::new(),
            free_scale: DashMap::new(),
            buckets: ArcSwap::from_pointee(buckets),
            hot_nodes: ArcSwap::from_pointee(hot_nodes),
            paid_base_weight: DashMap::new(),
            sla_derated_nodes: DashSet::new(),
            active_health: DashMap::new(),
            probe_mark_lock: Mutex::new(()),
            probe_cursor: AtomicUsize::new(0),
            probe_failure_threshold,
            probe_unhealthy_cooldown: Duration::from_secs(probe_unhealthy_cooldown_secs),
        }
    }
}

// =====================================================================
// OPT-R18 P2-1：以下子模块按职责拆分 `RouterEngine` 的 `impl` 块。
// 每个子模块含一组 `impl RouterEngine` 方法（纯搬移，零行为变更）。
//
// **不做 `pub(crate) use x::*` 再导出**：这些方法都是**固有方法**（inherent
// method），调用方写 `engine.foo()` 即可，Rust 不要求为固有方法 import——
// 再导出会让 `unused_imports` 报警（`use super::*` 的 glob-of-glob 不算"使用"）。
// 唯一需要跨模块可见的**自由函数**是 `select::pick_weighted_streaming`，
// 由 `router::tests` 直接 `use super::select::pick_weighted_streaming;` 引入。
// =====================================================================
mod buckets;
mod health;
mod node_mgmt;
mod quarantine;
mod select;

#[cfg(test)]
fn pick_weighted(candidates: &[Arc<ProxyNode>], rng: &mut impl Rng) -> Option<Arc<ProxyNode>> {
    if candidates.is_empty() {
        return None;
    }
    let total: u64 = candidates.iter().map(|n| n.weight as u64).sum();
    if total == 0 {
        // 防御分支：正常走不到（matches 已滤 0），退化均匀避免 503 误杀。
        // R2-6：`&[Arc]` 上 `choose` 得 `Option<&Arc>`，一次 `cloned` 即 `Arc`。
        return candidates.choose(rng).cloned();
    }
    let mut roll = rng.gen_range(0..total);
    for n in candidates {
        let w = n.weight as u64;
        if roll < w {
            return Some(Arc::clone(n));
        }
        roll -= w;
    }
    // 整除边界兜底（理论不可达）：返回最后一个。
    candidates.last().cloned()
}

#[cfg(test)]
mod health_tests;

#[cfg(test)]
mod tests;
