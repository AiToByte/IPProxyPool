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
    pub fn set_quarantine(&self, domain: &str, ip: &str, ttl_secs: u64) {
        let key = format!("{}:{ip}", normalize_domain(domain));
        self.quarantine_map
            .insert(key, Instant::now() + Duration::from_secs(ttl_secs));
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
        if let Some(ref c) = spec.country {
            if !node.country.eq_ignore_ascii_case(c) {
                return false;
            }
        }
        if let Some(ref t) = spec.tier {
            // Accept both short (`res`) and long (`residential`) tier names.
            let want = t.to_ascii_lowercase();
            let have = node.tier.to_ascii_lowercase();
            let want_norm = match want.as_str() {
                "res" => "residential",
                "dc" => "datacenter",
                _ => want.as_str(),
            };
            if have != want_norm && have != want {
                return false;
            }
        }
        if self.is_quarantined(&spec.target_domain, &node.ip, now) {
            return false;
        }
        true
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
        guard
            .iter()
            .filter(|n| !excluded.iter().any(|e| e == &n.addr))
            .filter(|n| self.matches(n, spec, now))
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
                let still_live = self
                    .pools
                    .load()
                    .iter()
                    .find(|n| n.addr == node.addr)
                    .is_some_and(|n| n.weight > 0);
                let excluded_hit = excluded.iter().any(|e| e == &node.addr);
                if !excluded_hit
                    && created_at.elapsed().as_secs() < SESSION_TTL_SECS
                    && still_live
                    && !self.is_quarantined(&spec.target_domain, &node.ip, now)
                {
                    return Some(Arc::clone(node));
                }
            }
        }

        // 2. Filter snapshot.
        let guard = self.pools.load();
        let candidates: Vec<Arc<ProxyNode>> = guard
            .iter()
            .filter(|n| !excluded.iter().any(|e| e == &n.addr))
            .filter(|n| self.matches(n, spec, now))
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
        });
        assert!(blocked.is_none());
        let other = r.select_node(&RoutingSpec {
            country: None,
            session_id: None,
            tier: None,
            target_domain: "b.com".to_string(),
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
        });
        assert!(blocked.is_none());
        let other = r.select_node(&RoutingSpec {
            country: None,
            session_id: None,
            tier: None,
            target_domain: "b.com".to_string(),
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
        };
        let first = r.select_node(&spec).expect("bind");
        assert_eq!(first.ip, "10.0.0.1");
        r.reload_nodes(us_pair());
        let excluded = vec!["10.0.0.1:8080".to_string()];
        let second = r.select_node_excluding(&spec, &excluded).expect("migrate");
        assert_eq!(second.ip, "10.0.0.2");
    }
}
