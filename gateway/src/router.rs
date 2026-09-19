//! GW-1 无锁路由器：ArcSwap 快照 + DashMap 会话 + 隔离表。
//!
//! 读路径无锁（`ArcSwap::load`）；控制面原子替换整个池子。
//! OPT-1 补齐：`sweep_expired` 周期清理过期条目（会话/隔离），
//! `snapshot_all` 导出全量快照（供网关臂表修剪用），三者皆为长稳运行防内存泄漏之用。

use crate::model::{ProxyNode, RoutingSpec};
use arc_swap::ArcSwap;
use dashmap::DashMap;
use rand::seq::SliceRandom;
use std::sync::Arc;
use std::time::{Duration, Instant};

/// 会话粘性有效期（秒）：超时后会话绑定失效，可被清理。
const SESSION_TTL_SECS: u64 = 600;

pub struct RouterEngine {
    pools: ArcSwap<Vec<ProxyNode>>,
    session_store: DashMap<String, (ProxyNode, Instant)>,
    /// Key = `{domain}:{ip}`, value = quarantine expiry.
    quarantine_map: DashMap<String, Instant>,
}

impl RouterEngine {
    pub fn new(initial_nodes: Vec<ProxyNode>) -> Self {
        Self {
            pools: ArcSwap::from_pointee(initial_nodes),
            session_store: DashMap::new(),
            quarantine_map: DashMap::new(),
        }
    }

    /// 施加域级隔离（GW-2 熔断器 / PubSub 增量同步调用）。
    pub fn set_quarantine(&self, domain: &str, ip: &str, ttl_secs: u64) {
        let key = format!("{domain}:{ip}");
        self.quarantine_map
            .insert(key, Instant::now() + Duration::from_secs(ttl_secs));
    }

    /// 导出当前全量节点快照（含被隔离节点，用于臂表修剪白名单）。
    pub fn snapshot_all(&self) -> Vec<ProxyNode> {
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
        let key = format!("{domain}:{ip}");
        self.quarantine_map
            .get(&key)
            .is_some_and(|exp| *exp.value() > now)
    }

    fn matches(&self, node: &ProxyNode, spec: &RoutingSpec, now: Instant) -> bool {
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
    pub fn get_healthy_candidates(&self, spec: &RoutingSpec) -> Vec<ProxyNode> {
        let now = Instant::now();
        let guard = self.pools.load();
        guard
            .iter()
            .filter(|n| self.matches(n, spec, now))
            .cloned()
            .collect()
    }

    /// Nanosecond-scale route selection with sticky-session support.
    pub fn select_node(&self, spec: &RoutingSpec) -> Option<ProxyNode> {
        let now = Instant::now();

        // 1. Sticky session fast path (skip quarantined bindings).
        if let Some(ref session_id) = spec.session_id {
            if let Some(entry) = self.session_store.get(session_id) {
                let (node, created_at) = entry.value();
                if created_at.elapsed().as_secs() < SESSION_TTL_SECS
                    && !self.is_quarantined(&spec.target_domain, &node.ip, now)
                {
                    return Some(node.clone());
                }
            }
        }

        // 2. Filter snapshot.
        let guard = self.pools.load();
        let candidates: Vec<&ProxyNode> = guard
            .iter()
            .filter(|n| self.matches(n, spec, now))
            .collect();

        // 3. Random pick (GW-3 upgrades this to LinUCB; keep uniform here).
        let mut rng = rand::thread_rng();
        let selected = candidates.choose(&mut rng).cloned().cloned();

        // 4. Bind new session.
        if let (Some(ref session_id), Some(ref node)) = (&spec.session_id, &selected) {
            self.session_store
                .insert(session_id.clone(), ((*node).clone(), now));
        }

        selected
    }

    /// 全量原子替换池子（稳定 API：控制面热更新，零停机）。
    /// 生产流量走快照读；测试与运维手工调用。allow 保留供控制面演进。
    #[allow(dead_code)]
    pub fn reload_nodes(&self, new_nodes: Vec<ProxyNode>) {
        self.pools.store(Arc::new(new_nodes));
    }

    /// 供应商套利调权（0=摘除熔断，100=恢复满权）。
    /// GW-1 实现为 retain/remove 垫片以稳定签名；真加权选路在 GW-R2 落地。
    pub fn adjust_vendor_weight(&self, vendor: &str, country: &str, weight: u32) {
        let guard = self.pools.load();
        let mut next: Vec<ProxyNode> = guard.as_ref().clone();
        if weight == 0 {
            next.retain(|n| !(n.provider == vendor && n.country.eq_ignore_ascii_case(country)));
        } else {
            for n in &mut next {
                if n.provider == vendor && n.country.eq_ignore_ascii_case(country) {
                    n.weight = weight;
                }
            }
        }
        self.pools.store(Arc::new(next));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::ProxyNode;

    fn fixtures() -> Vec<ProxyNode> {
        vec![
            ProxyNode {
                ip: "10.0.0.1".to_string(),
                port: 8080,
                username: None,
                password: None,
                country: "US".to_string(),
                tier: "residential".to_string(),
                provider: "mock-a".to_string(),
                weight: 100,
            },
            ProxyNode {
                ip: "10.0.0.2".to_string(),
                port: 8080,
                username: None,
                password: None,
                country: "JP".to_string(),
                tier: "datacenter".to_string(),
                provider: "mock-b".to_string(),
                weight: 80,
            },
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
}
