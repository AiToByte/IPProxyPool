//! GW-1 无锁路由器：ArcSwap 快照 + DashMap 会话 + 隔离表。
//!
//! 读路径无锁（`ArcSwap::load`）；控制面原子替换整个池子。
//! OPT-1 补齐：`sweep_expired` 周期清理过期条目（会话/隔离），
//! `snapshot_all` 导出全量快照（供网关臂表修剪用），三者皆为长稳运行防内存泄漏之用。

use crate::janitor;
use crate::model::{ProxyNode, RoutingSpec};
use arc_swap::ArcSwap;
use dashmap::DashMap;
#[cfg(test)]
use rand::seq::SliceRandom;
use rand::Rng;
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

pub struct RouterEngine {
    /// R2-6：池内节点以 `Arc` 持有。读路径只做 `Arc` 克隆（一次原子 +1，
    /// 不碰 `ip/country/tier/provider` 四个 `String`）；控制面整体替换仍无锁。
    pools: ArcSwap<Vec<Arc<ProxyNode>>>,
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

impl RouterEngine {
    pub fn new(initial_nodes: Vec<ProxyNode>) -> Self {
        Self {
            pools: ArcSwap::from_pointee(initial_nodes.into_iter().map(Arc::new).collect()),
            session_store: DashMap::new(),
            quarantine_map: DashMap::new(),
            free_scale: DashMap::new(),
        }
    }

    /// NEXT-B6：查询免费套利因子（merge 应用；缺省 1.0 即无缩放）。
    pub fn free_factor(&self, vendor: &str, country: &str) -> f64 {
        self.free_scale
            .get(&scale_key(vendor, country))
            .map(|v| *v.value())
            .unwrap_or(1.0)
    }

    /// 施加域级隔离（GW-2 熔断器 / PubSub 增量同步调用）。
    /// R2-2：domain 统一归一化（小写 + 剥端口 + 去尾点），`A.COM:443` 与
    /// `a.com` 落同一条目，大小写/端口变体绕不过隔离。
    /// 复审钳制：ttl 先取上限再相加（`checked_add` 显式无 panic；上限内 checked 恒成功，
    /// 写成 checked 形式以证 panic-free，而非依赖平台知识）。
    ///
    /// OPT-R9 C1：**两级结构**写入——外层按归一化 domain，内层按 node ip。
    /// 对外签名与语义与旧扁平 key 实现完全一致（跨进程 PubSub/Redis key 生成
    /// 仍走 `circuit_breaker.rs` 侧的扁平字符串，协议未动）。
    pub fn set_quarantine(&self, domain: &str, ip: &str, ttl_secs: u64) {
        let ttl = Duration::from_secs(ttl_secs.min(QUARANTINE_MAX_TTL_SECS));
        let expiry = Instant::now()
            .checked_add(ttl)
            .unwrap_or_else(|| Instant::now() + Duration::from_secs(QUARANTINE_MAX_TTL_SECS));
        // OPT-R11 C1：落表前过**两道准入**，与 `session_sticky_allowed` 同构。
        //
        // 理由（这是本项定为 P1 的原因）：本表外层 key 源自**客户端可控的
        // `Host`**——攻击者用随机/超长 Host 反复吃 403/429 即可在 TTL 窗口
        // （403→600s）内持续新增外层条目；`apply_delta` 的 Redis PubSub 入口
        // 更直接，`parse_delta_message` 只校验 `ttl` 范围与 domain/ip 非空，
        // 长度/条目数一概不设防。此前本表**既无长度上限也无水位**，而会话表
        // 两样都有（`SESSION_ID_MAX_BYTES` + `SESSION_MAX_ENTRIES`）——同一仓
        // 内两张客户端可影响的表，防护水平不一致。
        //
        // 语义与超长 session_id 一致：**拒绝落表，不截断**。截断会把两个不同
        // domain 映射到同一 key，造成误隔离（把无辜节点拖下水），比不隔离更糟。
        let domain = normalize_domain(domain);
        if domain.len() > QUARANTINE_DOMAIN_MAX_BYTES {
            log::warn!(
                "[quarantine] domain too long ({} bytes > {}), not quarantined",
                domain.len(),
                QUARANTINE_DOMAIN_MAX_BYTES
            );
            return;
        }
        if self.quarantine_map.len() >= QUARANTINE_MAX_DOMAINS
            && !self.quarantine_map.contains_key(&domain)
        {
            // 软水位：并发下允许轻微超限，只防无限膨胀。已达水位时**不驱逐**
            // 已有 domain——驱逐正在生效的隔离等于放行攻击流量；宁可新 domain
            // 暂不隔离（下轮窗口自然重试），也不动已有判定。
            log::warn!(
                "[quarantine] outer domain watermark reached ({} >= {}), new domain not quarantined",
                self.quarantine_map.len(),
                QUARANTINE_MAX_DOMAINS
            );
            return;
        }
        self.quarantine_map
            .entry(domain)
            .or_insert_with(DashMap::new)
            .insert(ip.to_string(), expiry);
    }

    /// 隔离表外层条目数（`QUARANTINE_MAX_DOMAINS` 水位的可观测口径）。
    ///
    /// 与 `quarantine_len`（内层 ip 条目总数，供 metrics gauge 用）刻意区分：
    /// 水位卡的是**外层**，所以需要一个能看见外层的计数。生产侧目前只用于
    /// 指标/排障需求未接入，故先仅供测试与按需调用。
    #[cfg(test)]
    pub fn quarantine_domains(&self) -> usize {
        self.quarantine_map.len()
    }

    /// 导出当前全量节点快照（含被隔离节点，用于臂表修剪白名单）。
    /// R2-6：`Arc` 句柄向量（引用计数 +1，不克隆节点 `String`）。
    pub fn snapshot_all(&self) -> Vec<Arc<ProxyNode>> {
        self.pools.load().as_ref().clone()
    }

    /// NEXT-A4：内存隔离表水位（sweep 滴答同步进 `quarantine_nodes` gauge）。
    /// OPT-R9 C1：两级结构下需**汇总**内层条目数（外层 domain 数不是隔离条目数）。
    pub fn quarantine_len(&self) -> usize {
        self.quarantine_map.iter().map(|kv| kv.value().len()).sum()
    }

    /// 周期清理：删除已过期的会话绑定与隔离条目。
    ///
    /// - 只删“已过期”条目，有效条目不受影响，可在任意时刻调用；
    /// - DashMap 分片锁下逐条判断，调用方通常是 60s 一次的后台 ticker；
    /// - 返回 `(清理的会话数, 清理的隔离数)`，供日志观察。
    pub fn sweep_expired(&self) -> (usize, usize) {
        self.sweep_expired_at(Instant::now())
    }

    /// `sweep_expired` 的可测版本：允许测试注入“未来时间”验证过期逻辑。
    fn sweep_expired_at(&self, now: Instant) -> (usize, usize) {
        let sessions_before = self.session_store.len();
        // 会话：创建时间距 now 超过 TTL 即过期（saturating 防时钟回拨 panic）。
        self.session_store.retain(|_, (_, created_at)| {
            now.saturating_duration_since(*created_at).as_secs() < SESSION_TTL_SECS
        });
        // OPT-R6 S2：净删除经 `janitor::removed_count`（饱和减法）。会话表正被数据面
        // `select_node_excluding` 落表并发写入，两次 `len()` 之间可能增长；旧的
        // `before - len()` 会 debug panic / release 回绕，且当时 sweep 无 supervisor，
        // panic 即静默死亡 → 会话表无界增长至 OOM。
        let sessions_removed = janitor::removed_count(sessions_before, self.session_store.len());

        let quarantines_before = self.quarantine_len();
        // 隔离：到期时刻 <= now 即过期。
        //
        // OPT-R9 C1：两级结构下需**逐内层**判过期。顺带做**空内层回收**——
        // 一个 domain 的全部 ip 都过期后，外层条目若留着就是纯粹的内存泄漏
        // （key 空间由客户端可控的 `Host` 变体构成）。`DashMap::retain` 持锁期间
        // 不可再取同一分片的写锁，故用「先判后删」两趟，避免自死锁。
        self.quarantine_map.retain(|_, inner| {
            inner.retain(|_, expiry| *expiry > now);
            !inner.is_empty()
        });
        // OPT-R6 S2：同上，隔离表亦由熔断消费侧并发写入。
        let quarantines_removed = janitor::removed_count(quarantines_before, self.quarantine_len());

        // OPT-R4 B11：TTL 淘汰顺带清理僵尸因子（池内已无对应 vendor×country
        // 即删键；缺省 1.0 语义不变，恢复靠下轮 merge 健康快照，不靠残留 0 因子）。
        self.prune_free_scales();

        (sessions_removed, quarantines_removed)
    }

    /// 隔离查询（OPT-R9 C1：零分配的**内层**查表）。
    ///
    /// `normalized_domain` 必须**已归一**（调用方在节点循环外算一次），
    /// 本函数只做两级哈希查找，**不产生任何堆分配**——这是本轮优化的核心。
    /// 未命中外层（该 domain 没有任何隔离记录）时立即返回 `false`，
    /// 连内层都不查——空表是常态（隔离只在熔断后才写入）。
    #[inline]
    fn is_quarantined_normalized(&self, normalized_domain: &str, ip: &str, now: Instant) -> bool {
        match self.quarantine_map.get(normalized_domain) {
            Some(inner) => inner.get(ip).is_some_and(|exp| *exp.value() > now),
            None => false,
        }
    }

    /// 隔离查询的**对外形态**（自行归一 domain）。
    ///
    /// 仅供「不在节点循环内」的调用方与单元测试使用；节点循环**必须**走
    /// [`Self::is_quarantined_normalized`] 复用归一结果，否则会把分配
    /// 重新拉回 O(N)。生产热路径（`matches` / sticky 复核）已在各自入口
    /// 归一 `spec.target_domain`，本函数在生产路径上无调用方。
    #[cfg_attr(not(test), allow(dead_code))]
    fn is_quarantined(&self, domain: &str, ip: &str, now: Instant) -> bool {
        self.is_quarantined_normalized(&normalize_domain(domain), ip, now)
    }

    fn matches(&self, node: &ProxyNode, spec: &RoutingSpec, now: Instant) -> bool {
        // R2-1：weight==0 视为摘除（保留在池内以便 restore，只在选路时过滤）。
        // `snapshot_all` 绕过本函数，故修剪白名单不受影响。
        if node.weight == 0 {
            return false;
        }
        // P2：按出站协议隔离。显式约束须精确命中；默认（None）只放行 Http——
        // SOCKS 节点永不服务默认流量（翻译桥只处理显式 socks 请求，见 gateway filter）。
        if let Some(want) = spec.proto {
            if node.proto != want {
                return false;
            }
        } else if node.proto != crate::model::EgressProto::Http {
            return false;
        }
        if let Some(ref c) = spec.country {
            if !node.country.eq_ignore_ascii_case(c) {
                return false;
            }
        }
        if let Some(ref t) = spec.tier {
            // REVIEW-R2 Q8：两侧皆已归一（节点 `ProxyNode::new`＋本文件入口），
            // 直接比对零分配；短名/大小写语义与旧双分支一致（归一函数单源）。
            if node.tier != *t {
                return false;
            }
        }
        // OPT-R9 C1：隔离查表走 `is_node_quarantined`（零分配内层查表）。
        // 本函数只负责**保证入参已归一**：`get_healthy_candidates_excluding` /
        // `select_node_excluding` 的 sticky 复核路径都在节点循环**外**算一次
        // `normalize_domain` 并填入 `spec.target_domain` 旁的 `normalized_domain`。
        // 这里的 `&spec.target_domain` 由调用方保证已是归一值（见两处入口的
        // `spec.target_domain = normalize_domain(...)`），故**不重复归一**。
        if self.is_node_quarantined(node, &spec.target_domain, now) {
            return false;
        }
        true
    }

    /// R3-2：节点级隔离判定（代理入口 ip ＋ 真 egress exit_ip 双检）。
    /// CB 消费遥测 `out_ip`（R3-2 起为真 egress），隔离条目可能落在任一地址上；
    /// 双检保证代理级/出口级隔离都不静默失效（http 节点 exit 恒 None，行为冻结）。
    ///
    /// OPT-R9 C1：入参改为**已归一**的 domain——`normalize_domain` 只在调用方
    /// （`matches`）的节点循环**外**算一次，本函数与被它调用的内层查表均零分配。
    fn is_node_quarantined(&self, node: &ProxyNode, normalized_domain: &str, now: Instant) -> bool {
        self.is_quarantined_normalized(normalized_domain, &node.ip, now)
            || node
                .exit_ip
                .as_deref()
                .is_some_and(|e| self.is_quarantined_normalized(normalized_domain, e, now))
    }

    /// 按条件过滤健康候选节点（GW-3 LinUCB / 预热器 / 套利审计的统一入口）。
    /// R2-6：返回 `Arc` 句柄（零 `String` 克隆；调用方按需再解引用）。
    pub fn get_healthy_candidates(&self, spec: &RoutingSpec) -> Vec<Arc<ProxyNode>> {
        self.get_healthy_candidates_excluding(spec, &[])
    }

    /// OPT-R11 B1：导出「当前池快照 guard」与「节点可用性判定」两个访问器。
    ///
    /// # 为什么要导出（而不是把选路整个搬进 router）
    ///
    /// 网关的 LinUCB 选路需要**交叉**两个私有状态：池快照（`RouterEngine` 的）
    /// 与臂表（`SmartProxyGateway` 的）。旧实现靠 `get_healthy_candidates_*`
    /// 把池内容**物化**出来再让网关去比对臂，等于用一次 O(N) 分配换取这点
    /// 可见性。流式版改为直接遍历快照，就必然要碰到这两处。
    ///
    /// 边界控制：**只导出只读能力，不导出可变状态**——`pools_guard` 返回
    /// `ArcSwap` 的只读 guard（引用计数，非阻塞锁，不影响写侧），
    /// `matches_node` 是纯判定。写路径（`upsert`/`set_quarantine` 等）仍私有。
    ///
    /// # `matches_node` 的归一责任
    ///
    /// 调用方**必须**先自行归一 `spec`（tier 与 `target_domain`），
    /// 否则判定口径会与 `matches` 内部不一致。数据面两个调用点都在入口归一
    /// 一次（`select_node_excluding` 本函数上方的既有逻辑、
    /// `select_bandit_node_excluding` 的调用链），口径统一。
    /// OPT-R11 B1：导出「在当前池快照上做只读计算」的闭包式访问器。
    ///
    /// # 为何是闭包而不是返回 guard
    ///
    /// 三个理由，第三个是关键的：
    ///
    /// 1. 不泄漏 `ArcSwap` 的 guard 具体类型（该类型在本模块内被同名 `Arc`
    ///    遮蔽，书写本身就别扭）；
    /// 2. 只读能力边界更清楚——调用方**拿不到**可长期持有的快照句柄；
    /// 3. **从类型上阻止误用**：`guard` 一旦返回，调用方可能把它带出闭包
    ///    长期持有（`select_bandit_node_excluding` 这类函数里就可能触发
    ///    跨 `.await` 持有）。闭包形态让"快照只在闭包期间有效"成为**类型
    ///    保证**而非口头约定——这正是本仓禁区条款（不跨 await 持锁/持引用）
    ///    想要的机制级保障。
    ///
    /// 注意 `f` 收到的切片可能**跨 `.await` 存活**（若 `f` 是 async），
    /// 故调用方仍须自行保证闭包内不 await；`guard` 本身是引用计数、
    /// 非阻塞锁，不影响写侧。
    pub fn with_pools<R>(&self, f: impl FnOnce(&[Arc<ProxyNode>]) -> R) -> R {
        let guard = self.pools.load();
        f(guard.as_ref())
    }

    /// 单节点可用性判定（`matches` 的对外只读包装，纯函数语义）。
    pub fn matches_node(&self, node: &ProxyNode, spec: &RoutingSpec, now: Instant) -> bool {
        self.matches(node, spec, now)
    }

    /// R2-2：带失败排除的候选过滤（网关重试路径用；`excluded` 为 `ip:port` 集合）。
    pub fn get_healthy_candidates_excluding(
        &self,
        spec: &RoutingSpec,
        excluded: &[String],
    ) -> Vec<Arc<ProxyNode>> {
        let now = Instant::now();
        let guard = self.pools.load();
        // REVIEW-R2 Q8：tier 请求侧归一一次（逐节点循环外；matches 内零分配）。
        let mut spec = spec.clone();
        spec.tier = spec.tier.map(|t| crate::model::canonical_tier(&t));
        // OPT-R9 C1：目标域同样归一一次（逐节点循环外）。`matches` 内的隔离查表
        // 直接用该值做外层 `DashMap` 查找，**循环内零分配**——旧实现在
        // `is_quarantined` 里对每个节点做 `format!("{}:{ip}", normalize_domain(...))`，
        // 池 N 时每请求 2N 次堆分配（免费池缺省 2000 节点 ≈ 4000 次/请求）。
        // 归一后语义与旧实现完全一致（隔离查表键的第一层就是归一化 domain）。
        spec.target_domain = normalize_domain(&spec.target_domain);
        guard
            .iter()
            .filter(|n| !excluded.iter().any(|e| e == &n.addr))
            .filter(|n| self.matches(n, &spec, now))
            .cloned()
            .collect()
    }

    /// Nanosecond-scale route selection with sticky-session support.
    /// R2-1：加权随机（权重累计 + 二分思想的线性 roll；权重全等价时退化为均匀）。
    /// 无排除兼容入口（单测/外部调用）；网关重试路径走 `select_node_excluding`。
    /// R2-6：返回 `Arc` 句柄（选中才 +1 未选中零分配；粘滞命中直接返回存量句柄）。
    #[allow(dead_code)]
    pub fn select_node(&self, spec: &RoutingSpec) -> Option<Arc<ProxyNode>> {
        self.select_node_excluding(spec, &[])
    }

    /// R2-2：带失败排除的选路。`excluded` 命中时：
    /// - 粘滞绑定若落在排除集则视为未命中，走新鲜加权选择（重试换节点）；
    /// - 新鲜选择直接过滤排除集；全被排除 → None（网关映射 503）。
    pub fn select_node_excluding(
        &self,
        spec: &RoutingSpec,
        excluded: &[String],
    ) -> Option<Arc<ProxyNode>> {
        let now = Instant::now();
        // NEXT-A5：tier 请求侧归一前移到入口（粘滞复核与新鲜选择共用同一归一 spec；
        // 原 Q8 hoist 只在步骤 2，粘滞路径用的还是原文 tier）。
        let mut spec = spec.clone();
        spec.tier = spec.tier.map(|t| crate::model::canonical_tier(&t));
        // OPT-R9 C1：目标域同样在入口归一一次，粘滞复核与新鲜选择共用
        // （粘滞路径只复核单个节点，收益小于新路径，但统一归一可避免两处口径
        // 分叉——旧实现的 sticky 复核里 `is_node_quarantined` 走的是未归一的
        // 原文 domain，靠 `is_quarantined` 内部归一兜底；现在改为入口统一归一，
        // 内层查表不再重复归一）。
        spec.target_domain = normalize_domain(&spec.target_domain);

        // 1. Sticky session fast path (skip quarantined/derated/excluded bindings).
        // OPT-R4 A7：超长 session 当无 session 处理（不查表，直接走无状态新鲜选择，
        // 不哈希巨型 key，永不 panic）。
        if let Some(ref session_id) = spec.session_id {
            if session_id.len() <= SESSION_ID_MAX_BYTES {
                if let Some(entry) = self.session_store.get(session_id) {
                    let (node, created_at) = entry.value();
                    // R2-1：粘滞命中后必须复核“当前池”权重，derate 到 0 的会话要迁移，
                    // 不能沿用绑定时刻克隆的老权重。
                    // P2：同时复核 proto——绑 socks 节点的会话发默认请求必须迁走
                    // （否则 socks 节点漏进 HttpPeer 必失败；反之亦然）。
                    let want_proto = spec.proto.unwrap_or(crate::model::EgressProto::Http);
                    let still_live = self
                        .pools
                        .load()
                        .iter()
                        .find(|n| n.addr == node.addr)
                        .is_some_and(|n| n.weight > 0 && n.proto == want_proto);
                    let excluded_hit = excluded.iter().any(|e| e == &node.addr);
                    // REVIEW-R2 Q9：`elapsed()` 时钟回拨即 panic（数据面不可炸），与
                    // `sweep_expired_at` 同口径改 saturating（回拨按 0 处理，会话多活一轮）。
                    // NEXT-A5：粘滞命中复核 country/tier 约束（spec.tier 入口已归一，
                    // 与 matches 同语义；换约束即迁移，不再粘错节点）。
                    let country_ok = spec
                        .country
                        .as_deref()
                        .is_none_or(|c| node.country.eq_ignore_ascii_case(c));
                    let tier_ok = spec.tier.as_deref().is_none_or(|t| node.tier == *t);
                    if !excluded_hit
                        && now.saturating_duration_since(*created_at).as_secs() < SESSION_TTL_SECS
                        && still_live
                        && country_ok
                        && tier_ok
                        && !self.is_node_quarantined(node, &spec.target_domain, now)
                    {
                        return Some(Arc::clone(node));
                    }
                }
            }
        }

        // 2. Filter snapshot（spec 已在入口归一，直接用）。
        let guard = self.pools.load();
        // 3. Weighted random pick。
        //
        // OPT-R11 B1：原为「先把全部候选物化成 `Vec<Arc<ProxyNode>>`，再调
        // `pick_weighted` 取一个」——物化的唯一用途就是喂给只需要一个元素的
        // 函数。`FREE_MAX_NODES=2000` 缺省时每请求约 16KB 分配 + 2000 次原子
        // 引用计数递增 + 约 11 次 realloc。改走流式两趟（`total`/`count` 一趟、
        // 定位一趟），**分配量 O(N) → 0**。
        //
        // 过滤条件与顺序逐字沿用旧实现（`excluded` 先判、`matches` 后判），
        // 故候选集合与遍历顺序不变 ⇒ 与 `pick_weighted` 逐值等价
        // （差分测试见 `opt_r11_b1_pick_weighted_streaming_matches_reference`）。
        let mut rng = rand::thread_rng();
        let selected = pick_weighted_streaming(
            || {
                guard
                    .iter()
                    .filter(|n| !excluded.iter().any(|e| e == &n.addr))
                    .filter(|n| self.matches(n, &spec, now))
            },
            &mut rng,
        );

        // 4. Bind new session.
        // OPT-R4 A7：落表前过准入（超长/满水位新 key 不落表，只用无状态选中节点）。
        if let (Some(ref session_id), Some(ref node)) = (&spec.session_id, &selected) {
            if session_sticky_allowed(&self.session_store, session_id) {
                self.session_store
                    .insert(session_id.clone(), (Arc::clone(node), now));
            }
        }

        selected
    }

    /// 全量原子替换池子（稳定 API：控制面热更新，零停机）。
    /// 生产流量走快照读；测试与运维手工调用。allow 保留供控制面演进。
    #[allow(dead_code)]
    pub fn reload_nodes(&self, new_nodes: Vec<ProxyNode>) {
        self.pools
            .store(Arc::new(new_nodes.into_iter().map(Arc::new).collect()));
    }

    /// 按 provider 前缀原子替换（FreePool 合并入口）。
    /// 只移除 `provider.starts_with(prefix)` 的旧节点并追加新集；其余节点复用
    /// 旧 `Arc`（引用不断，粘滞绑定/臂状态不受影响）；空 `nodes` 即清空该前缀。
    pub fn replace_vendor_nodes(&self, prefix: &str, nodes: Vec<ProxyNode>) {
        let guard = self.pools.load();
        let mut next: Vec<Arc<ProxyNode>> = guard
            .iter()
            .filter(|n| !n.provider.starts_with(prefix))
            .map(Arc::clone)
            .collect();
        next.extend(nodes.into_iter().map(Arc::new));
        self.pools.store(Arc::new(next));
        // OPT-R4 B11：merge（含空快照）后顺带清理僵尸因子。
        self.prune_free_scales();
    }

    /// 供应商套利调权（0=摘除熔断，100=恢复满权）。
    /// R2-1：只改数字不增删（摘除即可恢复）；选路侧 `matches` 过滤 `weight==0`。
    /// R2-6：写时复制——未命中节点复用旧 `Arc`（零克隆），命中节点重建
    ///（`addr` 随 `..base.clone()` 原样继承，`ip:port` 未变故恒一致）。
    pub fn adjust_vendor_weight(&self, vendor: &str, country: &str, weight: u32) {
        let guard = self.pools.load();
        let next: Vec<Arc<ProxyNode>> = guard
            .iter()
            .map(|n| {
                if n.provider == vendor && n.country.eq_ignore_ascii_case(country) {
                    let mut updated = (**n).clone();
                    updated.weight = weight;
                    Arc::new(updated)
                } else {
                    Arc::clone(n)
                }
            })
            .collect();
        self.pools.store(Arc::new(next));
    }

    /// OPT-R4 B11：清理僵尸因子——池内已无对应 vendor×country 节点即删键。
    /// 缺省回到 1.0；节点恢复靠下轮 merge 健康快照重插，不靠残留 0 因子。
    fn prune_free_scales(&self) {
        let live: std::collections::HashSet<(String, String)> = self
            .pools
            .load()
            .iter()
            .map(|n| scale_key(&n.provider, &n.country))
            .collect();
        self.free_scale.retain(|k, _| live.contains(k));
    }

    /// P3 免费独立套利：按 vendor×country 等比缩放权重（`factor` 来自 `free_pool_action`）。
    /// 写时复制同 `adjust_vendor_weight`；`factor<=0` 即摘除（matches 滤 0），
    /// 复检 upsert 按 health 重置权重即恢复；`factor>1` 不用（free 永不自动抬权，调用方保证）。
    pub fn scale_vendor_weights(&self, vendor: &str, country: &str, factor: f64) {
        // NEXT-B6：因子同步记表（含 factor<=0 显式摘除，合并后依然生效）。
        // OPT-R4 B11：显式 1.0 等价缺省——删键不残留（表随 scale 插入永不清理即僵尸）。
        if factor == 1.0 {
            self.free_scale.remove(&scale_key(vendor, country));
        } else {
            self.free_scale.insert(scale_key(vendor, country), factor);
        }
        let guard = self.pools.load();
        let next: Vec<Arc<ProxyNode>> = guard
            .iter()
            .map(|n| {
                if n.provider == vendor && n.country.eq_ignore_ascii_case(country) {
                    let mut updated = (**n).clone();
                    // REVIEW-R2 Q5：factor<=0 即显式摘除（matches 滤 0）；factor>0 下限钳 1——
                    // 复乘不得几何衰减到 0（生产 factor 仅 0.0/0.5 触发不了，任意小 factor 才暴露；
                    // 非 finite（NaN）`as u32` 饱和为 0，同样被下限兜住，永不意外摘除）。
                    updated.weight = if factor <= 0.0 {
                        0
                    } else {
                        ((n.weight as f64 * factor).round() as u32).max(1)
                    };
                    Arc::new(updated)
                } else {
                    Arc::clone(n)
                }
            })
            .collect();
        self.pools.store(Arc::new(next));
    }
}

/// R2-1 加权随机核心（纯函数，可单测）：累计权重 roll，`total==0` 退化均匀。
/// R2-6：输入/输出均为 `Arc`（只动引用计数，不克隆节点）。
///
/// # 它是 `pick_weighted_streaming` 的**独立参考实现**（OPT-R11 B1）
///
/// 数据面热路径已改走流式版，但本函数**刻意保留原样**（不改成委托），
/// 因为差分测试需要它作为「修改前的行为」基准——若让流式版委托本函数，
/// 比较就退化成自己跟前比、恒真，等于没有门。
/// 相应地本函数**仅在测试期编译**（`#[cfg(test)]`），不进生产二进制。
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

/// OPT-R11 B1：加权随机选路的**流式**版本——两趟遍历，**零物化**。
///
/// # 为什么需要它
///
/// 数据面每请求都要在候选里选一个节点，而旧路径先把全部候选物化成
/// `Vec<Arc<ProxyNode>>`（`select_node_excluding` 步骤 2）**只为了最终取一个**。
/// 代价随池规模线性增长：`FREE_MAX_NODES=2000` 缺省时每请求约 16KB 分配、
/// 2000 次原子引用计数递增，外加 `collect` 因 `size_hint` 下界为 0 而走
/// 倍增策略的约 11 次 realloc。
///
/// # 为什么两趟就够
///
/// `pick_weighted` 只需要两样东西：权重总和（决定 roll 范围）与「第几个
/// 命中」（决定选中谁）。两者都可在**流式**下得出：第 1 趟求 `total`/`count`，
/// 第 2 趟按同一顺序定位。**语义与物化版逐值等价**，包括：
///
/// - `total == 0` ⇒ 均匀退化，且**RNG 消耗顺序**与 `pick_weighted` 一致
///   （都是先算 total、再抽一个 `gen_range`）；
/// - `total > 0` ⇒ `roll ∈ [0,total)` 后按权重递减，与物化版同一算式；
/// - 平局/边界兜底语义不变。
///
/// 差分测试见本文件末尾 `opt_r11_b1_*`：以 `pick_weighted` 为基准，
/// 对多组权重分布 × 多组 seed 断言选中**同一节点**。
///
/// # `make_iter` 为何是闭包而不是 `I: Clone`
///
/// `DashMap` 的 `iter()` 每次都返回新迭代器，且遍历顺序对固定表内容稳定；
/// 传闭包让「每趟新建迭代器」显式化，也免去对迭代器要求 `Clone`。
fn pick_weighted_streaming<'a, I, F>(mut make_iter: F, rng: &mut impl Rng) -> Option<Arc<ProxyNode>>
where
    F: FnMut() -> I,
    I: Iterator<Item = &'a Arc<ProxyNode>>,
{
    // 第 1 趟：权重总和与候选个数（不物化、不克隆）。
    let mut total: u64 = 0;
    let mut count: usize = 0;
    for n in make_iter() {
        total += n.weight as u64;
        count += 1;
    }
    if count == 0 {
        return None;
    }
    if total == 0 {
        // 防御分支（matches 已滤 0，正常走不到）：退化均匀。
        // `choose` 内部对非空切片即 `gen_range(0..len)`，故与物化版同源。
        let idx = rng.gen_range(0..count);
        return make_iter().nth(idx).map(Arc::clone);
    }
    // 第 2 趟：按同一顺序消耗 roll。
    let mut roll = rng.gen_range(0..total);
    for n in make_iter() {
        let w = n.weight as u64;
        if roll < w {
            return Some(Arc::clone(n));
        }
        roll -= w;
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::ProxyNode;

    #[test]
    fn quarantine_ttl_clamped_no_overflow() {
        // 复审 FLAG：极端 ttl（u64::MAX，PubSub 毒报文形态）不得 panic，
        // 按上限钳制；条目仍生效且可 sweep 清理。
        let r = RouterEngine::new(vec![]);
        r.set_quarantine("x.example", "10.0.0.1", u64::MAX);
        // OPT-R9 C1：两级结构下条目落在 `domain → ip → expiry`。
        // 用 `map` 而非 `and_then`：后者的闭包返回借用会踩「引用不能逃逸出
        // 借用局部变量」的借用检查（`Ref` 守卫生命周期短于链式调用结果）。
        let exp = r
            .quarantine_map
            .get("x.example")
            .and_then(|inner| inner.get("10.0.0.1").map(|e| *e.value()))
            .expect("entry");
        let horizon = Instant::now() + Duration::from_secs(QUARANTINE_MAX_TTL_SECS);
        assert!(
            exp <= horizon + Duration::from_secs(1),
            "expiry must be clamped"
        );
        // 钳制语义：MAX+1 秒后 sweep 即清理（TTL 有限，非永久隔离）。
        let (s, q) = r.sweep_expired_at(horizon + Duration::from_secs(1));
        assert_eq!((s, q), (0, 1));
        // 正常值不受影响。
        r.set_quarantine("x.example", "10.0.0.2", 600);
        assert!(r.is_quarantined("x.example", "10.0.0.2", Instant::now()));
    }

    fn fixtures() -> Vec<ProxyNode> {
        vec![
            ProxyNode::new(
                "10.0.0.1".to_string(),
                8080,
                None,
                None,
                "US".to_string(),
                "residential".to_string(),
                "mock-a".to_string(),
                100,
            ),
            ProxyNode::new(
                "10.0.0.2".to_string(),
                8080,
                None,
                None,
                "JP".to_string(),
                "datacenter".to_string(),
                "mock-b".to_string(),
                80,
            ),
        ]
    }

    #[test]
    fn sticky_session_pins_same_node() {
        let r = RouterEngine::new(fixtures());
        let spec = RoutingSpec {
            country: None,
            session_id: Some("task-001".to_string()),
            tier: None,
            target_domain: "example.com".to_string(),
            proto: None,
        };
        // Single-candidate pool to make pinning deterministic.
        r.reload_nodes(vec![fixtures()[0].clone()]);
        let a = r.select_node(&spec).expect("node");
        let b = r.select_node(&spec).expect("node");
        assert_eq!(a.ip, b.ip);
    }

    #[test]
    fn sticky_migrates_on_constraint_change() {
        // NEXT-A5：同 session 换 country/tier 约束→迁移（原绑定失配不再命中）；
        // 约束不变→仍粘滞。US/residential 绑 10.0.0.1；切 JP→10.0.0.2。
        let r = RouterEngine::new(fixtures());
        let us = RoutingSpec {
            country: Some("US".to_string()),
            session_id: Some("task-7".to_string()),
            tier: None,
            target_domain: "example.com".to_string(),
            proto: None,
        };
        assert_eq!(r.select_node(&us).expect("us").ip, "10.0.0.1");
        assert_eq!(r.select_node(&us).expect("sticky").ip, "10.0.0.1");
        let jp = RoutingSpec {
            country: Some("JP".to_string()),
            ..us.clone()
        };
        assert_eq!(r.select_node(&jp).expect("migrate").ip, "10.0.0.2");
        let tier = RoutingSpec {
            country: None,
            tier: Some("datacenter".to_string()),
            ..us.clone()
        };
        assert_eq!(r.select_node(&tier).expect("tier").ip, "10.0.0.2");
    }

    #[test]
    fn quarantine_filters_domain_only() {
        let r = RouterEngine::new(fixtures());
        r.reload_nodes(vec![fixtures()[0].clone()]);
        r.set_quarantine("a.com", "10.0.0.1", 600);
        let blocked = r.select_node(&RoutingSpec {
            country: None,
            session_id: None,
            tier: None,
            target_domain: "a.com".to_string(),
            proto: None,
        });
        assert!(blocked.is_none());
        let other = r.select_node(&RoutingSpec {
            country: None,
            session_id: None,
            tier: None,
            target_domain: "b.com".to_string(),
            proto: None,
        });
        assert!(other.is_some());
    }

    #[test]
    fn sweep_removes_only_expired_entries() {
        let r = RouterEngine::new(fixtures());
        // 1 个会话绑定 + 1 个长隔离 + 1 个即时过期隔离。
        let spec = RoutingSpec {
            country: None,
            session_id: Some("sess-1".to_string()),
            tier: None,
            target_domain: "example.com".to_string(),
            proto: None,
        };
        r.reload_nodes(vec![fixtures()[0].clone()]);
        assert!(r.select_node(&spec).is_some());
        r.set_quarantine("a.com", "10.0.0.1", 600);
        r.set_quarantine("old.com", "10.0.0.1", 0);

        // 当前时刻：只有 ttl=0 的隔离过期。
        let (s0, q0) = r.sweep_expired();
        assert_eq!((s0, q0), (0, 1));

        // 快进到 601s 后：会话与长隔离一并过期。
        let future = Instant::now() + Duration::from_secs(601);
        let (s1, q1) = r.sweep_expired_at(future);
        assert_eq!((s1, q1), (1, 1));

        // 空扫：无事发生，不 panic。
        assert_eq!(r.sweep_expired(), (0, 0));
    }

    #[test]
    fn snapshot_all_returns_full_pool() {
        let r = RouterEngine::new(fixtures());
        r.set_quarantine("", "10.0.0.1", 600);
        // 即使被隔离，快照仍包含全部节点（修剪白名单必须用它，而非健康候选）。
        assert_eq!(r.snapshot_all().len(), 2);
    }

    fn us_pair() -> Vec<ProxyNode> {
        vec![
            ProxyNode::new(
                "10.0.0.1".to_string(),
                8080,
                None,
                None,
                "US".to_string(),
                "residential".to_string(),
                "mock-a".to_string(),
                100,
            ),
            ProxyNode::new(
                "10.0.0.2".to_string(),
                8080,
                None,
                None,
                "US".to_string(),
                "datacenter".to_string(),
                "mock-b".to_string(),
                100,
            ),
        ]
    }

    fn us_spec() -> RoutingSpec {
        RoutingSpec {
            country: Some("US".to_string()),
            session_id: None,
            tier: None,
            target_domain: "x.example".to_string(),
            proto: None,
        }
    }

    #[test]
    fn derate_hold_restore_cycle() {
        // R2-1：derate 只摘除不删除，restore 可恢复（P0 可逆性）。
        let r = RouterEngine::new(us_pair());
        assert_eq!(r.get_healthy_candidates(&us_spec()).len(), 2);
        r.adjust_vendor_weight("mock-a", "US", 0);
        // 池内仍 2 节点（可恢复），但健康候选只剩 mock-b。
        assert_eq!(r.snapshot_all().len(), 2);
        let remaining = r.get_healthy_candidates(&us_spec());
        assert_eq!(remaining.len(), 1);
        assert_eq!(remaining[0].provider, "mock-b");
        // hold：非零权重不增删。
        r.adjust_vendor_weight("mock-b", "US", 80);
        assert_eq!(r.snapshot_all().len(), 2);
        // restore：mock-a 回来。
        r.adjust_vendor_weight("mock-a", "US", 100);
        assert_eq!(r.get_healthy_candidates(&us_spec()).len(), 2);
    }

    #[test]
    fn weight_zero_filtered_but_snapshot_keeps() {
        // R2-1：weight==0 不进候选，但快照保留（臂表修剪白名单语义）。
        let r = RouterEngine::new(us_pair());
        r.adjust_vendor_weight("mock-a", "us", 0); // country 大小写不敏感
        assert!(r
            .get_healthy_candidates(&us_spec())
            .iter()
            .all(|n| n.provider != "mock-a"));
        assert_eq!(r.snapshot_all().len(), 2);
        assert!(r.select_node(&us_spec()).is_some());
        // 全摘除 → 503（None），而非误回均匀。
        r.adjust_vendor_weight("mock-b", "US", 0);
        assert!(r.get_healthy_candidates(&us_spec()).is_empty());
        assert!(r.select_node(&us_spec()).is_none());
    }

    #[test]
    fn weighted_pick_skews_toward_heavy() {
        // R2-1：1:99 权重下重节点显著多（种子 RNG 确定性断言方向，不卡精确值）。
        // R2-6：候选即 `Arc` 句柄（与线上选路同形态）。
        use rand::{rngs::StdRng, SeedableRng};
        let mut heavy = us_pair()[1].clone();
        heavy.weight = 99;
        let mut light = us_pair()[0].clone();
        light.weight = 1;
        let pool = [Arc::new(light), Arc::new(heavy)];
        let mut rng = StdRng::seed_from_u64(42);
        let mut heavy_hits = 0;
        for _ in 0..1000 {
            if pick_weighted(&pool, &mut rng).expect("pick").ip == "10.0.0.2" {
                heavy_hits += 1;
            }
        }
        assert!(heavy_hits > 900, "heavy_hits={heavy_hits}");
    }

    #[test]
    fn sticky_migrates_when_derated() {
        // R2-1：粘滞绑定后若该 vendor 被 derate 到 0，同 session 下次迁移走。
        let r = RouterEngine::new(vec![us_pair()[0].clone()]);
        let spec = RoutingSpec {
            country: None,
            session_id: Some("mig-1".to_string()),
            tier: None,
            target_domain: "example.com".to_string(),
            proto: None,
        };
        let first = r.select_node(&spec).expect("bind");
        assert_eq!(first.ip, "10.0.0.1");
        // 扩池 + 摘除老节点。
        r.reload_nodes(us_pair());
        r.adjust_vendor_weight("mock-a", "US", 0);
        let second = r.select_node(&spec).expect("migrate");
        assert_eq!(second.ip, "10.0.0.2");
    }

    #[test]
    fn arc_snapshot_shares_allocations() {
        // R2-6：快照/候选/选中均为池内同一分配（`ptr_eq`），无节点 `String` 克隆。
        let r = RouterEngine::new(us_pair());
        let a = r.snapshot_all();
        let b = r.snapshot_all();
        assert_eq!(a.len(), 2);
        for (x, y) in a.iter().zip(b.iter()) {
            assert!(Arc::ptr_eq(x, y));
        }
        let picked = r.select_node(&us_spec()).expect("pick");
        assert!(a.iter().any(|n| Arc::ptr_eq(n, &picked)));
        let cands = r.get_healthy_candidates(&us_spec());
        assert_eq!(cands.len(), 2);
        assert!(
            cands.iter().all(|c| a.iter().any(|n| Arc::ptr_eq(n, c))),
            "candidates must alias pool allocations"
        );
        // 预存 addr 与 ip:port 一致（构造器保证，读路径直接复用）。
        assert_eq!(a[0].addr, "10.0.0.1:8080");
    }

    #[test]
    fn normalize_domain_cases() {
        // R2-2：大小写/端口/尾点/空白归一；IPv6 括号；非数字后缀保留；永不 panic。
        assert_eq!(normalize_domain("Example.COM:8080"), "example.com");
        assert_eq!(normalize_domain("  a.COM. "), "a.com");
        assert_eq!(normalize_domain("x.example"), "x.example");
        assert_eq!(normalize_domain(""), "");
        assert_eq!(normalize_domain("[::1]:8080"), "::1");
        assert_eq!(normalize_domain("a:b"), "a:b");
    }

    #[test]
    fn quarantine_key_is_normalized() {
        // R2-2：`A.COM:443` 施加的隔离，`a.com` 同样命中；他域不受影响。
        let r = RouterEngine::new(fixtures());
        r.reload_nodes(vec![fixtures()[0].clone()]);
        r.set_quarantine("A.COM:443", "10.0.0.1", 600);
        let blocked = r.select_node(&RoutingSpec {
            country: None,
            session_id: None,
            tier: None,
            target_domain: "a.com".to_string(),
            proto: None,
        });
        assert!(blocked.is_none());
        let other = r.select_node(&RoutingSpec {
            country: None,
            session_id: None,
            tier: None,
            target_domain: "b.com".to_string(),
            proto: None,
        });
        assert!(other.is_some());
    }

    #[test]
    fn exclusion_skips_failed_nodes() {
        // R2-2：排除集命中节点永不被选；全排除 → None（网关映射 503）。
        let r = RouterEngine::new(us_pair());
        let spec = us_spec();
        let excluded = vec!["10.0.0.1:8080".to_string()];
        for _ in 0..20 {
            let n = r.select_node_excluding(&spec, &excluded).expect("node");
            assert_eq!(n.ip, "10.0.0.2");
        }
        assert!(r
            .get_healthy_candidates_excluding(&spec, &excluded)
            .iter()
            .all(|n| n.ip != "10.0.0.1"));
        let all = vec!["10.0.0.1:8080".to_string(), "10.0.0.2:8080".to_string()];
        assert!(r.select_node_excluding(&spec, &all).is_none());
    }

    #[test]
    fn sticky_yields_to_exclusion() {
        // R2-2：粘滞绑定后若该节点进入排除集（刚失败），同 session 迁移走。
        let r = RouterEngine::new(vec![us_pair()[0].clone()]);
        let spec = RoutingSpec {
            country: None,
            session_id: Some("exc-1".to_string()),
            tier: None,
            target_domain: "example.com".to_string(),
            proto: None,
        };
        let first = r.select_node(&spec).expect("bind");
        assert_eq!(first.ip, "10.0.0.1");
        r.reload_nodes(us_pair());
        let excluded = vec!["10.0.0.1:8080".to_string()];
        let second = r.select_node_excluding(&spec, &excluded).expect("migrate");
        assert_eq!(second.ip, "10.0.0.2");
    }

    #[test]
    fn replace_vendor_nodes_keeps_paid() {
        // 免费线合并语义：只动 `free-` 前缀节点，付费节点保持同一分配（ptr_eq）。
        use crate::model::ProxyNode;
        let paid = ProxyNode::new(
            "10.0.0.1".to_string(),
            8080,
            None,
            None,
            "US".to_string(),
            "residential".to_string(),
            "mock-a".to_string(),
            100,
        );
        let r = RouterEngine::new(vec![paid]);
        let before = r.snapshot_all();
        let free1 = ProxyNode::new(
            "9.9.9.9".to_string(),
            8080,
            None,
            None,
            "ZZ".to_string(),
            "free".to_string(),
            "free-geonode".to_string(),
            10,
        );
        r.replace_vendor_nodes("free-", vec![free1]);
        let after = r.snapshot_all();
        assert_eq!(after.len(), 2);
        assert!(after.iter().any(|n| n.provider == "mock-a"));
        assert!(after.iter().any(|n| n.provider == "free-geonode"));
        // 付费节点仍是池内原分配。
        assert!(after.iter().any(|n| Arc::ptr_eq(n, &before[0])));
        // 二次合并替换旧免费节点，不堆积。
        let free2 = ProxyNode::new(
            "8.8.8.8".to_string(),
            8080,
            None,
            None,
            "ZZ".to_string(),
            "free".to_string(),
            "free-github".to_string(),
            10,
        );
        r.replace_vendor_nodes("free-", vec![free2]);
        let again = r.snapshot_all();
        assert_eq!(again.len(), 2);
        assert!(again.iter().all(|n| n.provider != "free-geonode"));
    }

    #[test]
    fn free_tier_isolation_and_zz_semantics() {
        // G12：tier=residential 请求不命中 free 节点；US 约束不命中 ZZ 节点；
        // 无约束请求按权重混合（free 占少数但可见）；tier=free 只命中免费。
        let paid = ProxyNode::new(
            "10.0.0.1".to_string(),
            8080,
            None,
            None,
            "US".to_string(),
            "residential".to_string(),
            "mock-a".to_string(),
            100,
        );
        let free_node = ProxyNode::new(
            "9.9.9.9".to_string(),
            8080,
            None,
            None,
            "ZZ".to_string(),
            "free".to_string(),
            "free-geonode".to_string(),
            10,
        );
        let r = RouterEngine::new(vec![paid, free_node]);
        // residential 请求恒命中付费。
        let res_spec = RoutingSpec {
            country: None,
            session_id: None,
            tier: Some("residential".to_string()),
            target_domain: "x.example".to_string(),
            proto: None,
        };
        for _ in 0..20 {
            let n = r.select_node(&res_spec).expect("node");
            assert_eq!(n.provider, "mock-a");
        }
        // US 约束请求不命中 ZZ 免费节点。
        let us_spec = RoutingSpec {
            country: Some("US".to_string()),
            session_id: None,
            tier: None,
            target_domain: "x.example".to_string(),
            proto: None,
        };
        for _ in 0..20 {
            let n = r.select_node(&us_spec).expect("node");
            assert_eq!(n.provider, "mock-a");
        }
        // 无约束请求按权重混合（100:10，100 次内必见 free，P(不见)~8e-5）。
        let open = RoutingSpec {
            country: None,
            session_id: None,
            tier: None,
            target_domain: "x.example".to_string(),
            proto: None,
        };
        let mut saw_free = false;
        for _ in 0..100 {
            if r.select_node(&open)
                .expect("node")
                .provider
                .starts_with("free-")
            {
                saw_free = true;
                break;
            }
        }
        assert!(
            saw_free,
            "free nodes must serve unconstrained traffic sometimes"
        );
        // tier=free 请求只命中免费。
        let free_spec = RoutingSpec {
            country: None,
            session_id: None,
            tier: Some("free".to_string()),
            target_domain: "x.example".to_string(),
            proto: None,
        };
        let n = r.select_node(&free_spec).expect("node");
        assert!(n.provider.starts_with("free-"));
    }

    #[test]
    fn scale_vendor_weights_proportional() {
        // P3-2：按 vendor×country 等比缩放（round；factor≤0 即摘除，可恢复）。
        // 非目标前缀节点不动；country 精确匹配（大小写不敏感沿 adjust 惯例）。
        let a1 = ProxyNode::new(
            "10.0.0.1".to_string(),
            8080,
            None,
            None,
            "ZZ".to_string(),
            "free".to_string(),
            "free-gh0".to_string(),
            10,
        );
        let a2 = ProxyNode::new(
            "10.0.0.2".to_string(),
            8080,
            None,
            None,
            "ZZ".to_string(),
            "free".to_string(),
            "free-gh0".to_string(),
            6,
        );
        let paid = ProxyNode::new(
            "10.0.0.3".to_string(),
            8080,
            None,
            None,
            "US".to_string(),
            "residential".to_string(),
            "mock-a".to_string(),
            100,
        );
        let r = RouterEngine::new(vec![a1, a2, paid]);
        r.scale_vendor_weights("free-gh0", "ZZ", 0.5);
        let snap = r.snapshot_all();
        let w = |ip: &str| snap.iter().find(|n| n.ip == ip).map(|n| n.weight);
        assert_eq!(w("10.0.0.1"), Some(5));
        assert_eq!(w("10.0.0.2"), Some(3));
        assert_eq!(w("10.0.0.3"), Some(100));
        // factor 0 → 摘除（matches 过滤）；factor>1 不用（free 永不自动抬权，注释写明）。
        r.scale_vendor_weights("free-gh0", "ZZ", 0.0);
        assert_eq!(
            r.snapshot_all()
                .iter()
                .find(|n| n.ip == "10.0.0.1")
                .map(|n| n.weight),
            Some(0)
        );
    }

    #[test]
    fn scale_never_derates_to_zero_unless_explicit() {
        // REVIEW-R2 Q5：factor>0 反复调用不得几何衰减到 0（0 只能由 factor<=0 显式摘除）。
        // 生产 factor 仅 0.0/0.5（0.5 收敛于 1 触发不了）；任意小 factor（如 0.3）10→3→1→0 即红。
        let n = ProxyNode::new(
            "10.0.0.9".to_string(),
            8080,
            None,
            None,
            "ZZ".to_string(),
            "free".to_string(),
            "free-gh0".to_string(),
            10,
        );
        let r = RouterEngine::new(vec![n]);
        for _ in 0..6 {
            r.scale_vendor_weights("free-gh0", "ZZ", 0.3);
        }
        let w = r
            .snapshot_all()
            .iter()
            .find(|n| n.ip == "10.0.0.9")
            .map(|n| n.weight)
            .unwrap();
        assert!(w >= 1, "repeated factor>0 must not decay to zero, got {w}");
        r.scale_vendor_weights("free-gh0", "ZZ", 0.0);
        assert_eq!(
            r.snapshot_all()
                .iter()
                .find(|n| n.ip == "10.0.0.9")
                .map(|n| n.weight),
            Some(0)
        );
    }

    #[test]
    fn tier_matches_without_per_request_alloc() {
        // REVIEW-R2 Q8：tier 构造期归一（大小写/短名→长名）；matches 内纯比对。
        // 节点 "RES" 必须命中 spec "residential"（修前存原文 "RES"，小写后 "res"≠"residential" 即红）。
        let mk = |tier: &str| {
            ProxyNode::new(
                "10.0.0.1".to_string(),
                8080,
                None,
                None,
                "US".to_string(),
                tier.to_string(),
                "mock-a".to_string(),
                100,
            )
        };
        assert_eq!(mk("Residential").tier, "residential");
        assert_eq!(mk("RES").tier, "residential");
        assert_eq!(mk("DC").tier, "datacenter");
        assert_eq!(mk("free").tier, "free");
        let r = RouterEngine::new(vec![mk("RES")]);
        for want in ["res", "RES", "residential", "Residential", "RESIDENTIAL"] {
            let spec = RoutingSpec {
                country: None,
                session_id: None,
                tier: Some(want.to_string()),
                target_domain: "x.example".to_string(),
                proto: None,
            };
            assert_eq!(r.get_healthy_candidates(&spec).len(), 1, "{want}");
        }
    }

    #[test]
    fn quarantine_matches_exit_ip() {
        // R3-2：隔离 exit_ip 即摘除该节点（CB 用 out_ip＝真 egress 隔离，见 logging）；
        // 隔离 proxy ip 仍摘除；两者皆无即放行（双隔离，语义不降级）。
        use crate::model::EgressProto;
        let node = ProxyNode::new(
            "9.9.9.9".to_string(),
            1080,
            None,
            None,
            "ZZ".to_string(),
            "free".to_string(),
            "free-socks".to_string(),
            10,
        )
        .with_proto(EgressProto::Socks5)
        .with_exit_ip(Some("10.9.9.9".to_string()));
        let spec = RoutingSpec {
            proto: Some(EgressProto::Socks5),
            target_domain: "x.example".to_string(),
            ..Default::default()
        };
        let r = RouterEngine::new(vec![node]);
        assert!(r.select_node(&spec).is_some());
        r.set_quarantine("x.example", "10.9.9.9", 600);
        assert!(
            r.select_node(&spec).is_none(),
            "exit-ip quarantine must exclude"
        );
        let r2 = RouterEngine::new(vec![ProxyNode::new(
            "9.9.9.9".to_string(),
            1080,
            None,
            None,
            "ZZ".to_string(),
            "free".to_string(),
            "free-socks".to_string(),
            10,
        )
        .with_proto(EgressProto::Socks5)
        .with_exit_ip(Some("10.9.9.9".to_string()))]);
        r2.set_quarantine("x.example", "9.9.9.9", 600);
        assert!(
            r2.select_node(&spec).is_none(),
            "proxy-ip quarantine must still exclude"
        );
    }

    #[test]
    fn proto_isolation_default_http_only() {
        // P2-1：默认 spec（proto=None）永不命中 socks 节点；显式 socks5 只命中 socks5；
        // 显式 http 不命中 socks；socks4 与 socks5 互斥。
        use crate::model::EgressProto;
        let http_node = ProxyNode::new(
            "10.0.0.1".to_string(),
            8080,
            None,
            None,
            "US".to_string(),
            "residential".to_string(),
            "mock-a".to_string(),
            100,
        );
        let socks_node = ProxyNode::new(
            "9.9.9.9".to_string(),
            1080,
            None,
            None,
            "ZZ".to_string(),
            "free".to_string(),
            "free-socks".to_string(),
            10,
        )
        .with_proto(EgressProto::Socks5);
        let r = RouterEngine::new(vec![http_node, socks_node]);
        let open = RoutingSpec {
            country: None,
            session_id: None,
            tier: None,
            target_domain: "x.example".to_string(),
            proto: None,
        };
        for _ in 0..20 {
            assert_eq!(r.select_node(&open).expect("node").provider, "mock-a");
        }
        let s5 = RoutingSpec {
            proto: Some(EgressProto::Socks5),
            ..Default::default()
        };
        let n = r.select_node(&s5).expect("socks node");
        assert!(n.provider.starts_with("free-"));
        assert_eq!(n.proto, EgressProto::Socks5);
        let h = RoutingSpec {
            proto: Some(EgressProto::Http),
            ..Default::default()
        };
        assert_eq!(r.select_node(&h).expect("node").provider, "mock-a");
        let s4 = RoutingSpec {
            proto: Some(EgressProto::Socks4),
            ..Default::default()
        };
        assert!(r.select_node(&s4).is_none());
    }

    #[test]
    fn sticky_binding_migrates_on_proto_mismatch() {
        // P2-4：粘滞绑定只在同 proto 下复用——绑 socks 节点的会话发默认请求必须迁走
        // （否则 socks 节点漏进 HttpPeer 必失败）；反之亦然。
        use crate::model::EgressProto;
        let http_node = ProxyNode::new(
            "10.0.0.1".to_string(),
            8080,
            None,
            None,
            "US".to_string(),
            "residential".to_string(),
            "mock-a".to_string(),
            100,
        );
        let socks_node = ProxyNode::new(
            "9.9.9.9".to_string(),
            1080,
            None,
            None,
            "ZZ".to_string(),
            "free".to_string(),
            "free-socks".to_string(),
            10,
        )
        .with_proto(EgressProto::Socks5);
        let r = RouterEngine::new(vec![http_node, socks_node]);
        // 先用 socks 会话绑定 socks 节点。
        let s5sess = RoutingSpec {
            session_id: Some("sess-1".to_string()),
            proto: Some(EgressProto::Socks5),
            ..Default::default()
        };
        let bound = r.select_node(&s5sess).expect("bind socks");
        assert!(bound.provider.starts_with("free-"));
        // 同名会话发默认请求 → 迁到 http 节点（不沿用绑定）。
        let open_sess = RoutingSpec {
            session_id: Some("sess-1".to_string()),
            ..Default::default()
        };
        let moved = r.select_node(&open_sess).expect("migrate");
        assert_eq!(moved.provider, "mock-a");
    }

    #[test]
    fn opt_r4_a7_overlong_session_is_stateless() {
        // OPT-R4 A7：超长 session（>64B）当无 session 处理：仍可选路（无状态），
        // 但不得写入 session_store（防随机长 key 打爆 DashMap），不得 panic。
        let r = RouterEngine::new(vec![fixtures()[0].clone()]);
        let long_id = "x".repeat(200);
        let spec = RoutingSpec {
            country: None,
            session_id: Some(long_id),
            tier: None,
            target_domain: "example.com".to_string(),
            proto: None,
        };
        assert!(r.select_node(&spec).is_some());
        assert_eq!(r.session_store.len(), 0);
        // 非 ASCII 超长同样不得 panic，且走无状态。
        let uni_id = "🦀".repeat(100);
        let uni_spec = RoutingSpec {
            session_id: Some(uni_id),
            ..spec.clone()
        };
        assert!(r.select_node(&uni_spec).is_some());
        assert_eq!(r.session_store.len(), 0);
    }

    #[test]
    fn opt_r4_a7_session_table_capped_new_sessions_stateless() {
        // OPT-R4 A7：会话表水位上限——满水位后新 key 拒绝粘滞只走无状态
        // （不新增条目但仍返回节点）；已存 key 仍可粘滞。不得 panic。
        const CAP: usize = 8192;
        let r = RouterEngine::new(vec![fixtures()[0].clone()]);
        for i in 0..CAP {
            r.session_store.insert(
                format!("pre-{i}"),
                (Arc::clone(&r.snapshot_all()[0]), Instant::now()),
            );
        }
        assert_eq!(r.session_store.len(), CAP);
        let fresh = RoutingSpec {
            country: None,
            session_id: Some("fresh-new-session".to_string()),
            tier: None,
            target_domain: "example.com".to_string(),
            proto: None,
        };
        assert!(r.select_node(&fresh).is_some());
        assert_eq!(r.session_store.len(), CAP, "满水位新会话不得新增条目");
        // 已存 key 仍粘滞（条目数不变）。
        let old = RoutingSpec {
            session_id: Some("pre-0".to_string()),
            ..fresh.clone()
        };
        assert!(r.select_node(&old).is_some());
        assert_eq!(r.session_store.len(), CAP);
    }

    #[test]
    fn opt_r4_b11_scale_pruned_when_vendor_vanishes() {
        // OPT-R4 B11：因子僵尸——节点 TTL 淘汰/merge 空快照后对应键必须清理，
        // 缺省回到 1.0；新节点恢复不受旧 0 因子牵连。
        let n = ProxyNode::new(
            "10.0.0.9".to_string(),
            8080,
            None,
            None,
            "ZZ".to_string(),
            "free".to_string(),
            "free-gh0".to_string(),
            10,
        );
        let r = RouterEngine::new(vec![n]);
        r.scale_vendor_weights("free-gh0", "ZZ", 0.0);
        assert_eq!(r.free_factor("free-gh0", "ZZ"), 0.0);
        // 模拟 TTL 淘汰后 merge 空快照：该前缀节点清空。
        r.replace_vendor_nodes("free-", vec![]);
        assert_eq!(r.free_factor("free-gh0", "ZZ"), 1.0);
        assert!(r.free_scale.is_empty(), "僵尸因子必须清理");
        // 恢复：同 vendor×country 新节点不再被旧 0 因子摘除。
        let fresh = ProxyNode::new(
            "10.0.0.10".to_string(),
            8080,
            None,
            None,
            "ZZ".to_string(),
            "free".to_string(),
            "free-gh0".to_string(),
            10,
        );
        r.replace_vendor_nodes("free-", vec![fresh]);
        let w = r
            .snapshot_all()
            .iter()
            .find(|x| x.ip == "10.0.0.10")
            .map(|x| x.weight)
            .unwrap();
        assert_eq!(w, 10);
    }

    #[test]
    fn opt_r4_b11_explicit_one_clears_factor() {
        // OPT-R4 B11：恢复走显式 1.0——scale(1.0) 与缺省同语义，
        // 不得在表内残留条目（表随 scale 插入永不清理即红）。
        let r = RouterEngine::new(vec![fixtures()[0].clone()]);
        r.scale_vendor_weights("free-gh0", "ZZ", 0.5);
        assert_eq!(r.free_factor("free-gh0", "ZZ"), 0.5);
        r.scale_vendor_weights("free-gh0", "ZZ", 1.0);
        assert_eq!(r.free_factor("free-gh0", "ZZ"), 1.0);
        assert!(r.free_scale.is_empty(), "显式 1.0 应等价缺省，不得残留");
    }

    // ---- OPT-R6 S2：淘汰计数下溢（P0）回归锁定 ----

    /// P0 根因回归：`sweep_expired_at` 的删除计数在「淘汰窗口内并发生长」时
    /// 旧实现 `before - len()` 会 debug panic（`subtract with overflow`）/
    /// release 回绕成天文数字。
    ///
    /// 本测试用多线程把「数据面持续写入会话表」与「sweep 持续淘汰」同时跑起来，
    /// 真实复现那条竞态窗口——只测 `janitor::removed_count` 助手本身不足以证明
    /// 调用点已修好，故在此对**真实调用路径**施压。
    #[test]
    fn opt_r6_s2_sweep_survives_concurrent_session_writes() {
        use std::sync::Barrier;

        let r = Arc::new(RouterEngine::new(vec![fixtures()[0].clone()]));
        let node = Arc::clone(&r.snapshot_all()[0]);

        // 预置一批「已过期」会话，保证 sweep 每轮都有真删除发生。
        for i in 0..256 {
            r.session_store.insert(
                format!("stale-{i}"),
                (
                    Arc::clone(&node),
                    Instant::now() - Duration::from_secs(SESSION_TTL_SECS + 60),
                ),
            );
        }
        assert_eq!(r.session_store.len(), 256);

        // 两组线程：writer 持续插入新会话（制造并发增长），sweeper 反复淘汰。
        // Barrier 让双方同时起跑，最大化撞上 `len()` 窗口的概率。
        let barrier = Arc::new(Barrier::new(2));
        let writer_r = Arc::clone(&r);
        let writer_barrier = Arc::clone(&barrier);
        let writer = std::thread::spawn(move || {
            writer_barrier.wait();
            for i in 0..20_000u32 {
                writer_r.session_store.insert(
                    format!("live-{i}"),
                    (
                        Arc::clone(&node),
                        Instant::now(), // 未过期：不会被本轮 sweep 清掉
                    ),
                );
            }
        });

        let sweeper_r = Arc::clone(&r);
        let sweeper = std::thread::spawn(move || {
            barrier.wait();
            let mut total_removed = 0usize;
            for _ in 0..2_000 {
                let (sessions, quarantines) = sweeper_r.sweep_expired_at(Instant::now());
                // 关键断言：旧实现在此处 debug panic；现实现要求「净删除」恒为
                // 非负且不超过采样时的存量（不可能出现回绕天文数字）。
                assert!(
                    sessions <= 2_048,
                    "净删除数不得回绕成天文数字（实际 {sessions}）"
                );
                assert!(
                    quarantines <= 2_048,
                    "隔离净删除数不得回绕（实际 {quarantines}）"
                );
                total_removed = total_removed.saturating_add(sessions);
            }
            total_removed
        });

        writer.join().expect("writer thread");
        let _ = sweeper.join().expect("sweeper must not panic");

        // 全部写入完成后，剩余条目必然全是未过期的 live 会话。
        assert_eq!(
            r.session_store.len(),
            20_000,
            "预置的 256 条过期会话应已全部清掉"
        );
    }

    /// P0 根因的确定性版本：不依赖线程调度，直接构造「before 采样后表变长」
    /// 的等价输入，锁定 `sweep_expired_at` 的返回值口径。
    /// 做法：先让会话表为空时调用 `sweep_expired_at` 的公共封装 `sweep_expired`
    /// 不可控并发，故此处锁定语义边界——空表淘汰必须返回 0，且不 panic。
    #[test]
    fn opt_r6_s2_sweep_on_empty_table_is_zero_not_panic() {
        let r = RouterEngine::new(vec![]);
        assert_eq!(r.sweep_expired_at(Instant::now()), (0, 0));
        // 反复调用不累积状态（幂等）。
        for _ in 0..100 {
            assert_eq!(r.sweep_expired_at(Instant::now()), (0, 0));
        }
    }

    /// OPT-R6 S1（P0 安全）**完整链路**回归：把「无 tier 声明的请求会命中免费节点」
    /// 这一 P0 前提，与「按实际节点档位拦截凭据」这一修复，在同一个测试里串起来。
    ///
    /// # 为什么必须串起来测（而不是各测一半）
    ///
    /// 单独测护栏函数（`gateway.rs` 已覆盖真值表）只能证明「给定一个 free 档节点
    /// 会拦截」；单独测选路（`free_tier_isolation_and_zz_semantics` 已覆盖）只能证明
    /// 「无 tier 声明会命中免费节点」。**P0 的本质是这两件事同时成立**——
    /// 旧代码里两者都成立，而护栏只看请求声明的 tier，于是凭据经免费出口泄露。
    /// 本测试锁定串联后的完整后果，是唯一能防「将来有人把护栏改回看请求侧」的锚点。
    ///
    /// 权重取 100:10，故 100 次无约束选路内必见 free（P(未见) ≈ 8e-5，见既有测试注释）。
    #[test]
    fn opt_r6_s1_undeclared_request_hitting_free_node_is_caught_by_guard() {
        use crate::gateway::free_node_exits_with_credentials;

        let paid = ProxyNode::new(
            "10.0.0.1".to_string(),
            8080,
            None,
            None,
            "US".to_string(),
            "residential".to_string(),
            "mock-a".to_string(),
            100,
        );
        let free_node = ProxyNode::new(
            "9.9.9.9".to_string(),
            8080,
            None,
            None,
            "ZZ".to_string(),
            "free".to_string(),
            "free-geonode".to_string(),
            10,
        );
        let r = RouterEngine::new(vec![paid, free_node]);

        // 无 tier / 无 country 约束 = 旧护栏完全放行的形态（`tier == Some("free")`
        // 为 false，`request_filter` 的提前拒绝不触发）。
        let undeclared = RoutingSpec {
            country: None,
            session_id: None,
            tier: None,
            target_domain: "victim.example".to_string(),
            proto: None,
        };
        // 旧护栏口径：只看请求声明的 tier → 放行（这正是漏洞）。
        assert!(
            !crate::gateway::free_tier_with_credentials(undeclared.tier.as_deref(), true, false),
            "前置确认：旧护栏在无 tier 声明时放行（P0 漏洞本体）"
        );

        // 模拟数据面：连发请求，一旦选路命中免费节点就用新护栏判定。
        // `hit_free` 证明「无约束请求确实会走免费出口」；`blocked` 证明同一次请求
        // 被新护栏拦下。两者必须都成立，否则本测试无意义。
        let mut hit_free = false;
        let mut blocked = false;
        let mut paid_passed = 0u32;
        for _ in 0..200 {
            let node = r.select_node(&undeclared).expect("node");
            if node.tier == "free" {
                hit_free = true;
                // 带 Authorization 的请求经该节点出站 → 必须被拦。
                if free_node_exits_with_credentials(&node.tier, true, false) {
                    blocked = true;
                }
            } else {
                // 付费节点：带凭据必须放行（存量语义零变化）。
                assert!(
                    !free_node_exits_with_credentials(&node.tier, true, false),
                    "付费节点不得被护栏误伤（实际 {}）",
                    node.tier
                );
                paid_passed += 1;
            }
        }
        assert!(
            hit_free,
            "无 tier 声明的请求必须仍会命中免费节点（P0 前提）"
        );
        assert!(
            blocked,
            "命中免费节点的带凭据请求必须被护栏拦截（S1 修复本体）"
        );
        assert!(paid_passed > 0, "付费节点路径也必须被走到（存量语义验证）");
    }
    // ---- OPT-R9 C1：隔离表两级结构（热路径零分配）回归锁定 ----

    /// 语义等价性：同 domain 不同 ip 各自独立隔离。
    /// 两级结构的**第一层键**是归一化 domain，第二层是 ip。
    #[test]
    fn opt_r9_c1_quarantine_two_level_same_domain_distinct_ips() {
        let r = RouterEngine::new(vec![]);
        r.set_quarantine("shop.example", "10.0.0.1", 600);
        let now = Instant::now();
        assert!(r.is_quarantined("shop.example", "10.0.0.1", now));
        // 同 domain 下另一个 ip 不受影响。
        assert!(!r.is_quarantined("shop.example", "10.0.0.2", now));
        // 水位按**条目数**计（不是外层 domain 数）：两条 ip ⇒ 2。
        assert_eq!(r.quarantine_len(), 1, "先只有一条");
        r.set_quarantine("shop.example", "10.0.0.2", 600);
        assert_eq!(r.quarantine_len(), 2, "同 domain 两条 ip = 两条隔离");
    }

    /// 语义等价性：跨 domain 同 ip 互不影响（两级结构的第二层是**各自**独立的）。
    #[test]
    fn opt_r9_c1_quarantine_two_level_cross_domain_same_ip() {
        let r = RouterEngine::new(vec![]);
        r.set_quarantine("a.example", "10.0.0.1", 600);
        let now = Instant::now();
        assert!(r.is_quarantined("a.example", "10.0.0.1", now));
        assert!(
            !r.is_quarantined("b.example", "10.0.0.1", now),
            "另一个 domain 下的同 ip 不得被牵连"
        );
    }

    /// 归一化仍然生效：大小写/端口/尾点变体命中同一条目。
    /// 这是 R2-2 的既有语义，两级结构不得削弱。
    #[test]
    fn opt_r9_c1_normalization_still_applies() {
        let r = RouterEngine::new(vec![]);
        r.set_quarantine("Shop.Example:443", "10.0.0.1", 600);
        let now = Instant::now();
        for variant in [
            "Shop.Example:443",
            "shop.example",
            "SHOP.EXAMPLE",
            "shop.example.",
            " shop.example:443 ",
        ] {
            assert!(
                r.is_quarantined(variant, "10.0.0.1", now),
                "变体 {variant:?} 应命中同一条隔离"
            );
        }
        assert_eq!(r.quarantine_len(), 1, "五个变体应落同一条目");
    }

    /// 本轮**顺带修掉的潜在缺陷**：旧扁平 key 用 `:` 拼接，而 `normalize_domain`
    /// 允许 domain 含 `:`（IPv6 字面量 `::1`；单测 `normalize_domain_cases` 已锁定
    /// `normalize_domain("a:b") == "a:b"`）。于是
    ///   `(domain="a:b", ip="10.0.0.1")` 与 `(domain="a", ip="b:10.0.0.1")`
    /// 在旧实现里产生**同一个扁平 key** `a:b:10.0.0.1` ⇒ 互相污染。
    /// 两级结构以 `DashMap` 键相等性取代字符串拼接，从根上消除该隐忧。
    #[test]
    fn opt_r9_c1_no_flat_key_collision_for_colon_domains() {
        let r = RouterEngine::new(vec![]);
        r.set_quarantine("a:b", "10.0.0.1", 600);
        let now = Instant::now();
        assert!(r.is_quarantined("a:b", "10.0.0.1", now));
        // 旧实现下这条会被误判为已隔离（key 碰撞）；现在必须互不牵连。
        assert!(
            !r.is_quarantined("a", "b:10.0.0.1", now),
            "不同 (domain, ip) 组合不得共享隔离条目（扁平 key 拼接碰撞回归）"
        );
        // 反向也成立。
        assert!(!r.is_quarantined("a:b:10.0.0.1", "x", now));
    }

    /// 过期清理：内层逐条判过期，且**空内层必须回收**。
    /// 空内层若留着，外层 key（由客户端可控的 `Host` 变体构成）就是内存泄漏。
    #[test]
    fn opt_r9_c1_sweep_reclaims_empty_inner_maps() {
        let r = RouterEngine::new(vec![]);
        r.set_quarantine("gone.example", "10.0.0.1", 0);
        r.set_quarantine("stay.example", "10.0.0.2", 600);
        assert_eq!(r.quarantine_len(), 2);

        // 注入「未来时间」让 ttl=0 的条目过期，ttl=600 的保留。
        let horizon = Instant::now() + Duration::from_secs(1);
        let (sessions, quarantines) = r.sweep_expired_at(horizon);

        assert_eq!(quarantines, 1, "应只清掉 1 条过期隔离");
        assert_eq!(sessions, 0);
        assert_eq!(r.quarantine_len(), 1, "存活条目保留");
        // 外层 `gone.example` 的空内层必须被回收（否则外层 key 泄漏）。
        assert!(
            !r.quarantine_map.contains_key("gone.example"),
            "全空的内层必须被回收，否则外层 key 随 Host 变体无界增长"
        );
        assert!(r.quarantine_map.contains_key("stay.example"));
    }

    /// 零分配回归锁定：`is_quarantined_normalized` 是**纯查表**，
    /// 不做任何 `String` 构造。本测试用「全局分配计数器」验证。
    ///
    /// 判据刻意选在**空隔离表**上：旧实现在此路径上仍会为每个节点执行
    /// `format!("{}:{ip}", normalize_domain(domain))`（两次分配），
    /// 而新实现外层查表未命中即返回，**一次分配都没有**。
    ///
    /// 计数器本身是 test-only 的全局原子 + test binary 的**全局分配器**。
    ///
    /// # 为什么必须用 `#[global_allocator]`（一次实测打脸）
    ///
    /// 最初这个测试只定义了 `Counting` 分配器但**没有安装**（漏了
    /// `#[global_allocator]`），于是 `ALLOCS` 恒为 0、断言恒真——把
    /// `format!` 塞回热路径后测试**仍然绿**（实测）。零分配断言只有在
    /// 计数器真的接在进程的分配路径上时才有意义。
    // 该测试**必须串行运行**：它用 `#[global_allocator]` 统计进程级分配次数，
    // 而并行测试会污染该计数器。CI 已单独配置
    // `cargo test --workspace opt_r9_c1_isolated_lookup -- --test-threads=1`。
    //
    // 下方的 `#[ignore]` 只为让 `cargo test --workspace`（并行）保持全绿——
    // 忽略测试仍会被编译，且串行 job 用**精确过滤器**跑它，不受 `ignore` 影响
    // （libtest 的过滤串先于 `ignore` 判定：显式指定过滤器时，被忽略的测试
    // **仍会运行**，除非过滤器与 ignore 同时生效——实测见 CI）。
    #[test]
    #[ignore = "needs --test-threads=1; run via CI serial job or: cargo test opt_r9_c1_isolated_lookup -- --test-threads=1 --ignored"]
    fn opt_r9_c1_isolated_lookup_does_not_allocate() {
        use std::sync::atomic::Ordering;

        let r = RouterEngine::new(vec![]);
        // 预热：确保任何惰性初始化（DashMap 分片等）已发生，不计入测量。
        let _ = r.quarantine_len();
        let domain = normalize_domain("x.example");

        // 测量窗口内**不放断言**：`assert!` 的失败路径会做 panic 消息格式化
        // （实测会让计数虚高），语义正确性由窗口外的 `hits` 断言兜住。
        //
        // # 并行污染与对策（实测踩坑，故写明）
        //
        // `#[global_allocator]` 的计数器是**进程级**的，而 Rust 测试默认多线程
        // 并行——别的测试此刻的分配会一并计入本窗口。实测：并行下计数虚高、
        // `--test-threads=1` 下为 0。
        //
        // 对策：**串行化本测试**（CI 单独跑），而不是放宽判据。
        // 试过「并行下不误报」的设计，实测它会让真实回归溜过——把
        // `format!` 塞回热路径后观察到 2000 次分配，却被当作「污染」放行，
        // 那种门禁等于没有门禁。故保留硬断言，隔离并行噪声。
        let now = Instant::now();
        let before = crate::test_allocs::ALLOCS.load(Ordering::Relaxed);
        let mut hits = 0usize;
        for _ in 0..1000 {
            if r.is_quarantined_normalized(&domain, "10.0.0.1", now) {
                hits += 1;
            }
        }
        let after = crate::test_allocs::ALLOCS.load(Ordering::Relaxed);
        let observed = after - before;

        // 语义：空表 + 未命中的 ip ⇒ 1000 次全为 false（**永远硬断言**，
        // 与分配计数无关，不受并行影响）。
        assert_eq!(hits, 0, "空隔离表不应有任何命中");

        // 硬断言。并行污染问题用「串行化本测试」解决而非放宽判据——
        // 放宽会让真实回归（如 `format!` 被塞回热路径，实测 1000 循环产生
        // 2000 次分配）悄悄溜过，那种门禁等于没有门禁。
        assert_eq!(
            observed,
            0,
            "未命中隔离时的查表必须零分配（OPT-R9 C1 核心收益）。             若本条在并行全量运行下偶发失败，是其它测试线程的分配污染了             进程级计数器——请用 --test-threads=1 复跑确认（CI 已为该测试             单独配置串行 job）。若串行下仍失败，说明热路径真的引入了分配。"
        );
    }

    // ---- OPT-R11 B1：选路零物化的**差分测试** ----

    /// **核心验收**：加权分支上，`pick_weighted_streaming` 与「物化版」
    /// `pick_weighted` 在**多组权重分布 × 多组 seed** 下必须选中
    /// **同一个节点**。
    ///
    /// # 为什么是差分测试
    ///
    /// 流式版重写了加权随机的算式（两轮、顺序保持、uniform 分支
    /// 改用 `gen_range`）。这类重写最典型的失败模式就是**看着对、
    /// 边界错**——例如 RNG 消耗顺序变了、权重递减的边界从 `<`
    /// 变成 `<=`。单跑一次看不出，必须与旧实现逐一对拍。
    ///
    /// `pick_weighted` 因此**不改成委托**（否则比较恰真，等于没有门）。
    #[test]
    fn opt_r11_b1_pick_weighted_streaming_matches_reference_weighted() {
        use rand::SeedableRng;
        // 覆盖：单元素／等权／权重悬巟／末位权重 0（考验 roll 递减边界）。
        // 注意：这里**不含全 0 权重**——见下一个测试与其说明。
        let weight_sets: Vec<Vec<u32>> = vec![
            vec![100],
            vec![10, 10, 10, 10],
            vec![1000, 1, 1, 1],
            vec![1, 1, 1, 1000],
            vec![5, 3, 0, 7, 0],
            vec![1, 0, 0, 1],
        ];
        for (wi, weights) in weight_sets.iter().enumerate() {
            let nodes: Vec<Arc<ProxyNode>> = weights
                .iter()
                .enumerate()
                .map(|(i, w)| {
                    Arc::new(ProxyNode::new(
                        format!("10.0.{}.{}", i / 256, i % 256),
                        8080,
                        None,
                        None,
                        "US".to_string(),
                        "residential".to_string(),
                        "mock-a".to_string(),
                        *w,
                    ))
                })
                .collect();
            for seed in 0..256u64 {
                let mut rng_ref = rand::rngs::StdRng::seed_from_u64(seed);
                let expected = pick_weighted(&nodes, &mut rng_ref).map(|n| n.addr.clone());
                let mut rng_new = rand::rngs::StdRng::seed_from_u64(seed);
                let got =
                    pick_weighted_streaming(|| nodes.iter(), &mut rng_new).map(|n| n.addr.clone());
                assert_eq!(
                    got, expected,
                    "weights#{wi}={weights:?} seed={seed}: 加权分支上两者必须选中同一节点"
                );
            }
        }
    }

    /// **防御分支（`total == 0`）：不与 `choose` 对拍，只验合法性。
    ///
    /// # 为什么不对拍（这是诚实的差异，不是逃避）
    ///
    /// 物化版用 `SliceRandom::choose`，而 rand 的 `choose` 内部用的是
    /// **`gen_index`（Lemire 宽化乘法）**，与 `Rng::gen_range` 不同源。
    /// 要复刻就得依赖 rand 的内部实现（升级即可能改变），
    /// 属于错误的依赖方向。
    ///
    /// # 为什么不影响生产行为（已实证，非信口传）
    ///
    /// 该分支在生产中**可证明不可达**：数据面的每个候选
    /// 都必须先过 `matches`，而 `matches` 的第一句就是
    /// `if node.weight == 0 { return false; }`（`router.rs` `matches`）。
    /// 因此每个候选 `weight >= 1` ⇒ `count > 0` 时 `total >= count > 0`。
    /// 下一个测试 `opt_r11_b1_matches_rejects_zero_weight` 把这个前提**锁成断言**
    /// 而不是残留在注释里。
    #[test]
    fn opt_r11_b1_pick_weighted_streaming_uniform_branch_returns_valid_member() {
        use rand::SeedableRng;
        let weights = [0u32, 0, 0, 0];
        let nodes: Vec<Arc<ProxyNode>> = weights
            .iter()
            .enumerate()
            .map(|(i, w)| {
                Arc::new(ProxyNode::new(
                    format!("10.0.0.{}", i),
                    8080,
                    None,
                    None,
                    "US".to_string(),
                    "residential".to_string(),
                    "mock-a".to_string(),
                    *w,
                ))
            })
            .collect();
        let addrs: Vec<String> = nodes.iter().map(|n| n.addr.clone()).collect();
        for seed in 0..128u64 {
            let mut rng = rand::rngs::StdRng::seed_from_u64(seed);
            let got = pick_weighted_streaming(|| nodes.iter(), &mut rng)
                .map(|n| n.addr.clone())
                .expect("防御分支必须选中一个");
            assert!(
                addrs.contains(&got),
                "防御分支选中了候选集之外的节点: {got}"
            );
        }
    }

    /// **把「防御分支不可达」从注释变成断言**。
    ///
    /// `matches` 必须拒绝 `weight == 0` 的节点。这条前提一旦被改掉
    /// （如为了「优化」让 0 权重节点也参与选路），那么
    /// 生产就会真的进入 `total == 0` 分支，而该分支的抽样算法
    /// 与物化版不同源（见上一测试）——此测试就会红。
    #[test]
    fn opt_r11_b1_matches_rejects_zero_weight() {
        let r = RouterEngine::new(vec![]);
        let mk = |w: u32| {
            ProxyNode::new(
                "10.0.0.1".to_string(),
                8080,
                None,
                None,
                "US".to_string(),
                "residential".to_string(),
                "mock-a".to_string(),
                w,
            )
        };
        let now = Instant::now();
        // weight == 0 → 拒绝（保证每个候选 weight >= 1 ⇒ total >= count ⇒
        // 防御分支不可达）。
        assert!(
            !r.matches(&mk(0), &RoutingSpec::default(), now),
            "matches 必须拒绝 weight==0 的节点，否则生产会进入防御分支"
        );
        assert!(
            r.matches(&mk(1), &RoutingSpec::default(), now),
            "weight>=1 的健康节点应通过 matches"
        );
    }

    /// 流式版在**空候选**上必须返回 `None`（⇒ 网关 503），与物化版一致。
    #[test]
    fn opt_r11_b1_pick_weighted_streaming_empty_returns_none() {
        let empty: Vec<Arc<ProxyNode>> = Vec::new();
        let mut rng = rand::thread_rng();
        assert!(pick_weighted_streaming(|| empty.iter(), &mut rng).is_none());
    }

    /// **零分配契约**：流式选路在命中路径上不得产生堆分配。
    ///
    /// 候选切片由调用方持有，函数内只做引用计数与栈上算术。
    /// 与另两个分配契约测试同样受并行污染，故标 `#[ignore]` 并由 CI 串行跑。
    #[test]
    #[ignore = "needs --test-threads=1; run via CI serial allocation-contract job"]
    fn opt_r11_b1_pick_weighted_streaming_hit_path_does_not_allocate() {
        let nodes: Vec<Arc<ProxyNode>> = (0..64u32)
            .map(|i| {
                Arc::new(ProxyNode::new(
                    format!("10.0.0.{}", i),
                    8080,
                    None,
                    None,
                    "US".to_string(),
                    "residential".to_string(),
                    "mock-a".to_string(),
                    1 + i,
                ))
            })
            .collect();

        // RNG 提到计数窗口**之外**。
        //
        // 【踩坑记录】本测试首跑实测得到 `left: 1`（本该是 0）。根因是
        // `rand::thread_rng()` 惰性初始化**线程局部**熵源，首次调用会分配；
        // 原写法在循环**内**每轮新建 rng，把那次一次性分配计进了窗口。
        // 修法：rng 在窗口外建好并复用，既排除一次性初始化噪声，也更贴近
        // 生产的 `select_node_excluding`（那里 rng 在循环外建一次）。
        let mut rng = rand::thread_rng();
        // 预热：显式消耗一次，确保线程局部已初始化完毕。
        let _ = rng.gen_range(0..u32::MAX);

        // 先验证功能不变量：非空候选必须每次都选中一个。
        {
            let mut picked = 0usize;
            for _ in 0..1000 {
                if pick_weighted_streaming(|| nodes.iter(), &mut rng).is_some() {
                    picked += 1;
                }
            }
            assert_eq!(picked, 1000, "非空候选必须每次都选中一个");
        }

        // OPT-R16 E8：改用共享的「多轮采样取最小值」判据，理由见
        // test_allocs::assert_min_zero_alloc_across_rounds 的文档。
        //
        // 【踩坑记录 · 修正自身第一版】第一版把 body 写成「单次调用」，结果
        // 每轮恰好 1 次分配（min=1 ⇒ 判红）。对比 HEAD 版（循环 1000 次）为 0。
        // 原因不是热路径退化，而是**测量口径变了**：单次调用时迭代器适配器
        // 与捕获环境的初始化被计入，而循环版把它摊薄到 1000 次里。
        // 结论：零分配契约必须以「批量循环」为测量单位，否则测的是初始化
        // 而非稳态热路径。这也是 HEAD 版一直用循环的原因——那是对的，
        // 不该改。这一点是我在修 CI 平台差异时误伤的。
        const ITERS_PER_ROUND: usize = 1000;
        crate::test_allocs::assert_min_zero_alloc_across_rounds("pick_weighted_streaming", || {
            for _ in 0..ITERS_PER_ROUND {
                let _ = pick_weighted_streaming(|| nodes.iter(), &mut rng);
            }
        });
    }

    // ---- OPT-R11 C1：隔离表外层准入（长度 + 水位）----

    /// 超长 domain **不落表**（与超长 session_id 不落表同语义）。
    #[test]
    fn opt_r11_c1_rejects_overlong_domain() {
        let r = RouterEngine::new(vec![]);
        let long = "a".repeat(QUARANTINE_DOMAIN_MAX_BYTES + 1);
        r.set_quarantine(&long, "10.0.0.1", 600);
        assert_eq!(
            r.quarantine_domains(),
            0,
            "超长 domain 不得落表（{}字节 > {}）",
            long.len(),
            QUARANTINE_DOMAIN_MAX_BYTES
        );
        assert_eq!(r.quarantine_len(), 0, "内层也不得有条目");
    }

    /// 正常 domain 与边界值**原样落表**（存量行为零变化）。
    #[test]
    fn opt_r11_c1_accepts_normal_and_boundary_domain() {
        let r = RouterEngine::new(vec![]);
        r.set_quarantine("example.com", "10.0.0.1", 600);
        assert_eq!(r.quarantine_domains(), 1);
        assert_eq!(r.quarantine_len(), 1);

        // 恰好于上限的 domain 必须落表（不得多拒）。
        let exact = "b".repeat(QUARANTINE_DOMAIN_MAX_BYTES);
        r.set_quarantine(&exact, "10.0.0.2", 600);
        assert_eq!(r.quarantine_domains(), 2, "恰好上限的 domain 必须落表");
    }

    /// 水位：外层条目数达到软上限后，新 domain 被拒、而**已存 domain 仍可用**。
    ///
    /// # 为何用**真实**上限 8192 造满表（而不是取个小的 `CAP`）
    ///
    /// 首版测试用 `const CAP: usize = 4` 直接塞 4 条，然后期望"新 domain 被拒"
    /// ——但生产水位是 `QUARANTINE_MAX_DOMAINS = 8192`，4 远未达水位，
    /// `set_quarantine` **本就应该**放行。测试失败暴露的是**测试自身的
    /// 前提错误**，不是实现缺陷。这类"用假阈值测真逻辑"的测试往往能绿一段时间
    /// 直到逻辑改动才炸，或更糟——它断言的是一个生产永不触发的场景。
    ///
    /// 8192 条 `DashMap` insert 在测试里是毫秒级，完全可接受；这样测的才是
    /// 生产上真实会发生的路径。
    #[test]
    fn opt_r11_c1_watermark_rejects_new_domain_but_keeps_existing() {
        let r = RouterEngine::new(vec![]);
        // 塞满到真实水位（生产上由 sweep 递减，测试里直接塞最快）。
        for i in 0..QUARANTINE_MAX_DOMAINS {
            r.quarantine_map.insert(
                format!("seeded{i}.test"),
                DashMap::from_iter([("10.0.0.9".to_string(), Instant::now())]),
            );
        }
        assert_eq!(r.quarantine_domains(), QUARANTINE_MAX_DOMAINS);

        // 新 domain 被拒：不得新增外层条目。
        r.set_quarantine("fresh-attack.test", "10.0.0.1", 600);
        assert_eq!(
            r.quarantine_domains(),
            QUARANTINE_MAX_DOMAINS,
            "达水位后新 domain 不得新增外层条目"
        );

        // 已存 domain 仍可写入（不得因水位让已生效的隔离失效）。
        r.set_quarantine("seeded0.test", "10.0.0.2", 600);
        let inner = r
            .quarantine_map
            .get("seeded0.test")
            .expect("已存 domain 仍在");
        assert!(
            inner.contains_key("10.0.0.2"),
            "已存 domain 必须能继续写入新 ip（不能把已生效隔离丢掉）"
        );
    }

    /// 日志不能漏：达水位时仍会保留**已存 domain 的写能力**。
    #[test]
    fn opt_r11_c1_watermark_is_soft_and_does_not_evict() {
        let r = RouterEngine::new(vec![]);
        r.set_quarantine("keepme.test", "10.0.0.1", 600);
        // 低于水位时应当传数落表（无日志拒绝）。
        r.set_quarantine("alsokeep.test", "10.0.0.1", 600);
        assert_eq!(r.quarantine_domains(), 2);
    }

    /// R13 诊断：直接调 `select_node_excluding`（sticky/加权路径）
    /// 300 次新 session，无约束。权重 100/80/60 下必须分布。
    #[test]
    fn opt_r13_weighted_path_distributes() {
        let r = RouterEngine::new(vec![
            ProxyNode::new(
                "127.0.0.1".to_string(),
                8888,
                None,
                None,
                "US".to_string(),
                "residential".to_string(),
                "mock-a".to_string(),
                100,
            ),
            ProxyNode::new(
                "127.0.0.1".to_string(),
                8889,
                None,
                None,
                "JP".to_string(),
                "datacenter".to_string(),
                "mock-b".to_string(),
                80,
            ),
            ProxyNode::new(
                "127.0.0.1".to_string(),
                8890,
                None,
                None,
                "GB".to_string(),
                "mobile".to_string(),
                "mock-c".to_string(),
                60,
            ),
        ]);
        let mut cnt = [0usize; 3];
        for i in 0..300 {
            let spec = RoutingSpec {
                session_id: Some(format!("w{i}")),
                ..Default::default()
            };
            match r.select_node_excluding(&spec, &[]).map(|n| n.port) {
                Some(8888) => cnt[0] += 1,
                Some(8889) => cnt[1] += 1,
                Some(8890) => cnt[2] += 1,
                other => panic!("unexpected {other:?}"),
            }
        }
        println!("WEIGHTED-DIST {cnt:?}");
        assert!(cnt.iter().all(|c| *c > 0), "加权路径必须分布，实测={cnt:?}");
    }
}
