//! GW-1 无锁路由器：ArcSwap 快照 + DashMap 会话 + 隔离表。
//!
//! 读路径无锁（`ArcSwap::load`）；控制面原子替换整个池子。
//! OPT-1 补齐：`sweep_expired` 周期清理过期条目（会话/隔离），
//! `snapshot_all` 导出全量快照（供网关臂表修剪用），三者皆为长稳运行防内存泄漏之用。

use crate::model::{ProxyNode, RoutingSpec};
use arc_swap::ArcSwap;
use dashmap::DashMap;
use rand::seq::SliceRandom;
use rand::Rng;
use std::sync::Arc;
use std::time::{Duration, Instant};

/// 会话粘性有效期（秒）：超时后会话绑定失效，可被清理。
const SESSION_TTL_SECS: u64 = 600;

/// 隔离 TTL 上限（秒，24h）：`set_quarantine` 与 PubSub 解析共用。
/// 复审结论：`Instant + Duration` 会溢出 panic——u64::MAX 级输入（毒报文/非法 env）
/// 必须钳制；上限远超业务 TTL（60/600s），钳制无行为影响。
pub const QUARANTINE_MAX_TTL_SECS: u64 = 86400;

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
    quarantine_map: DashMap<String, Instant>,
}

impl RouterEngine {
    pub fn new(initial_nodes: Vec<ProxyNode>) -> Self {
        Self {
            pools: ArcSwap::from_pointee(initial_nodes.into_iter().map(Arc::new).collect()),
            session_store: DashMap::new(),
            quarantine_map: DashMap::new(),
        }
    }

    /// 施加域级隔离（GW-2 熔断器 / PubSub 增量同步调用）。
    /// R2-2：domain 统一归一化（小写 + 剥端口 + 去尾点），`A.COM:443` 与
    /// `a.com` 落同一 key，大小写/端口变体绕不过隔离。
    /// 复审钳制：ttl 先取上限再相加（`checked_add` 显式无 panic；上限内 checked 恒成功，
    /// 写成 checked 形式以证 panic-free，而非依赖平台知识）。
    pub fn set_quarantine(&self, domain: &str, ip: &str, ttl_secs: u64) {
        let key = format!("{}:{ip}", normalize_domain(domain));
        let ttl = Duration::from_secs(ttl_secs.min(QUARANTINE_MAX_TTL_SECS));
        let expiry = Instant::now()
            .checked_add(ttl)
            .unwrap_or_else(|| Instant::now() + Duration::from_secs(QUARANTINE_MAX_TTL_SECS));
        self.quarantine_map.insert(key, expiry);
    }

    /// 导出当前全量节点快照（含被隔离节点，用于臂表修剪白名单）。
    /// R2-6：`Arc` 句柄向量（引用计数 +1，不克隆节点 `String`）。
    pub fn snapshot_all(&self) -> Vec<Arc<ProxyNode>> {
        self.pools.load().as_ref().clone()
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
        let sessions_removed = sessions_before - self.session_store.len();

        let quarantines_before = self.quarantine_map.len();
        // 隔离：到期时刻 <= now 即过期。
        self.quarantine_map.retain(|_, expiry| *expiry > now);
        let quarantines_removed = quarantines_before - self.quarantine_map.len();

        (sessions_removed, quarantines_removed)
    }

    fn is_quarantined(&self, domain: &str, ip: &str, now: Instant) -> bool {
        let key = format!("{}:{ip}", normalize_domain(domain));
        self.quarantine_map
            .get(&key)
            .is_some_and(|exp| *exp.value() > now)
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
        if self.is_node_quarantined(node, &spec.target_domain, now) {
            return false;
        }
        true
    }

    /// R3-2：节点级隔离判定（代理入口 ip＋真 egress exit_ip 双检）。
    /// CB 消费遥测 `out_ip`（R3-2 起为真 egress），隔离条目可能落在任一地址上；
    /// 双检保证代理级/出口级隔离都不静默失效（http 节点 exit 恒 None，行为冻结）。
    fn is_node_quarantined(&self, node: &ProxyNode, domain: &str, now: Instant) -> bool {
        self.is_quarantined(domain, &node.ip, now)
            || node
                .exit_ip
                .as_deref()
                .is_some_and(|e| self.is_quarantined(domain, e, now))
    }

    /// 按条件过滤健康候选节点（GW-3 LinUCB / 预热器 / 套利审计的统一入口）。
    /// R2-6：返回 `Arc` 句柄（零 `String` 克隆；调用方按需再解引用）。
    pub fn get_healthy_candidates(&self, spec: &RoutingSpec) -> Vec<Arc<ProxyNode>> {
        self.get_healthy_candidates_excluding(spec, &[])
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

        // 1. Sticky session fast path (skip quarantined/derated/excluded bindings).
        if let Some(ref session_id) = spec.session_id {
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
                if !excluded_hit
                    && now.saturating_duration_since(*created_at).as_secs() < SESSION_TTL_SECS
                    && still_live
                    && !self.is_node_quarantined(node, &spec.target_domain, now)
                {
                    return Some(Arc::clone(node));
                }
            }
        }

        // 2. Filter snapshot.
        let guard = self.pools.load();
        // REVIEW-R2 Q8：同候选入口，tier 请求侧归一一次。
        let mut spec = spec.clone();
        spec.tier = spec.tier.map(|t| crate::model::canonical_tier(&t));
        let candidates: Vec<Arc<ProxyNode>> = guard
            .iter()
            .filter(|n| !excluded.iter().any(|e| e == &n.addr))
            .filter(|n| self.matches(n, &spec, now))
            .cloned()
            .collect();

        // 3. Weighted random pick.
        let mut rng = rand::thread_rng();
        let selected = pick_weighted(&candidates, &mut rng);

        // 4. Bind new session.
        if let (Some(ref session_id), Some(ref node)) = (&spec.session_id, &selected) {
            self.session_store
                .insert(session_id.clone(), (Arc::clone(node), now));
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

    /// P3 免费独立套利：按 vendor×country 等比缩放权重（`factor` 来自 `free_pool_action`）。
    /// 写时复制同 `adjust_vendor_weight`；`factor<=0` 即摘除（matches 滤 0），
    /// 复检 upsert 按 health 重置权重即恢复；`factor>1` 不用（free 永不自动抬权，调用方保证）。
    pub fn scale_vendor_weights(&self, vendor: &str, country: &str, factor: f64) {
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
mod tests {
    use super::*;
    use crate::model::ProxyNode;

    #[test]
    fn quarantine_ttl_clamped_no_overflow() {
        // 复审 FLAG：极端 ttl（u64::MAX，PubSub 毒报文形态）不得 panic，
        // 按上限钳制；条目仍生效且可 sweep 清理。
        let r = RouterEngine::new(vec![]);
        r.set_quarantine("x.example", "10.0.0.1", u64::MAX);
        let exp = r
            .quarantine_map
            .get("x.example:10.0.0.1")
            .map(|e| *e.value())
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
}
