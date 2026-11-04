//! 主动健康（步骤 54/55）单元测试。
//!
//! 独立成文件而非塞进 `tests.rs`，原因：这块的测试**互相之间有语义耦合**
//! （安全阀的"最后节点"语义依赖池中节点数），聚在一起便于对照阅读；
//! 同时避免 `tests.rs` 继续膨胀。
//!
//! 覆盖的能力（每条都对应一个**可被变异打红**的断言）：
//!
//! | 测试 | 锁定的性质 |
//! |---|---|
//! | `active_health_threshold_excludes_and_success_recovers` | 阈值未达不摘除；达标摘除；成功即恢复 |
//! | `active_health_never_removes_last_eligible_node` | 安全阀：绝不摘到全池 0 |
//! | `active_health_safety_valve_counts_only_weighted_nodes` | 安全阀的"可用节点"只算 `weight>0` |
//! | `active_health_migrates_sticky_session` | 摘除必须同时踢掉粘滞绑定 |
//! | `active_health_cooldown_expires_and_refailure_remarks` | 冷却到期放行；再失败**立即**重摘 |
//! | `active_health_pruned_on_pool_replacement` | 换池即清僵尸健康条目 |
//! | `probe_candidates_include_unhealthy_for_recovery` | 探测候选必须含被摘节点（否则永不恢复） |
//! | `active_health_ignores_stale_probe_for_removed_node` | 对已删节点的迟到探测不落状态 |
//! | `opt_r18_active_health_gate_matches_table_under_concurrency` | 并发摘除下"表即真相" |
//! | `opt_r18_probe_sampling_covers_free_pool_in_bounded_ticks` | 免费池抽样在有限 tick 内全覆盖 |
//!
//! **为什么要有这组测试**：主动健康摘除是一条**会直接把节点摘出数据面**的
//! 路径。它有三个天然危险面——① 误摘（一次抖动就摘）② 摘光（全池 503）
//! ③ 摘了不恢复（永久损失）。三者都不会 panic，只会让线上"莫名 503"或
//! "莫名变慢"，属于最难靠日志定位的故障类别。故此处对每条危险面都设
//! 至少一个可打红的断言。

use super::*;
use crate::model::RoutingSpec;
use std::sync::Arc;

// =====================================================================
// 夹具
// =====================================================================

/// 造一个节点（`ip:port` 即 `addr`）。
fn node(ip: &str, port: u16, country: &str, provider: &str, weight: u32) -> ProxyNode {
    ProxyNode::new(
        ip.to_string(),
        port,
        None,
        None,
        country.to_string(),
        "residential".to_string(),
        provider.to_string(),
        weight,
    )
}

/// 造一个 `weight>0` 的普通节点。
fn up(ip: &str, port: u16, country: &str) -> ProxyNode {
    node(ip, port, country, "mock-provider", 100)
}

/// 造一个免费池节点（`provider` 前缀命中 `free-` ⇒ 走抽样路径）。
fn free(ip: &str, port: u16) -> ProxyNode {
    node(ip, port, "US", "free-provider", 100)
}

/// 任意 spec（不限 tier/country/proto，只要求能选到 Http 节点）。
fn any_spec() -> RoutingSpec {
    RoutingSpec {
        session_id: None,
        ..Default::default()
    }
}

/// 该地址当前是否被主动健康摘除（走公开判定的**同一份表**）。
fn is_unhealthy(r: &RouterEngine, addr: &str) -> bool {
    r.is_active_unhealthy(addr, Instant::now())
}

// =====================================================================
// ① 阈值语义：未达标不摘、达标摘、成功恢复
// =====================================================================

#[test]
fn active_health_threshold_excludes_and_success_recovers() {
    // 阈值 2：连续失败 2 次才摘除。冷却 300s 足够长（测试内不依赖时间推进）。
    let r = RouterEngine::new_with_probe_policy(
        vec![up("10.0.0.1", 8080, "US"), up("10.0.0.2", 8080, "JP")],
        2,
        300,
    );
    let addr = "10.0.0.1:8080";

    // 第 1 次失败：只累计，未达阈值 ⇒ 节点**仍可选**。
    assert_eq!(
        r.record_probe_failure(addr),
        ProbeFailureOutcome::BelowThreshold,
        "未达阈值不得摘除"
    );
    assert_eq!(r.probe_consecutive_failures(addr), 1);
    assert!(!is_unhealthy(&r, addr), "未达阈值不得进入摘除态");
    assert!(
        r.get_healthy_candidates(&any_spec())
            .iter()
            .any(|n| n.addr == addr),
        "未达阈值的节点必须继续承接流量"
    );

    // 第 2 次失败：达阈值 ⇒ 摘除，数据面立刻看不到它。
    assert_eq!(
        r.record_probe_failure(addr),
        ProbeFailureOutcome::MarkedUnhealthy
    );
    assert!(is_unhealthy(&r, addr), "达阈值必须进入摘除态");
    assert_eq!(r.active_unhealthy_len(), 1);
    assert!(
        !r.get_healthy_candidates(&any_spec())
            .iter()
            .any(|n| n.addr == addr),
        "被摘节点必须从选路候选中消失（否则摘除只影响指标不影响流量）"
    );

    // **已在摘除中**再失败 ⇒ 只刷新冷却截止时间，**不重复累计失败次数**、
    // 也不重复执行安全阀（否则会对同一节点反复跑一遍全池计数）。
    // 这条分支的判别点：返回 `RefreshedUnhealthy` 而非 `MarkedUnhealthy`。
    assert_eq!(
        r.record_probe_failure(addr),
        ProbeFailureOutcome::RefreshedUnhealthy,
        "已在摘除中的节点再次失败必须只刷新冷却，不得重复摘除"
    );
    assert_eq!(r.active_unhealthy_len(), 1, "刷新冷却不得改变摘除节点数");
    assert_eq!(
        r.probe_consecutive_failures(addr),
        2,
        "已在摘除中时不得再累计失败次数（阈值判定已通过，无需重复计数）"
    );

    // 探测成功 ⇒ 恢复（记录被整条清空）。
    assert!(r.record_probe_success(addr), "成功必须清掉该节点的健康记录");
    assert_eq!(r.probe_consecutive_failures(addr), 0);
    assert_eq!(r.active_unhealthy_len(), 0);
    assert!(
        r.get_healthy_candidates(&any_spec())
            .iter()
            .any(|n| n.addr == addr),
        "恢复后必须重新参与选路"
    );
}

// =====================================================================
// ② 安全阀：绝不把池摘空
// =====================================================================

#[test]
fn active_health_never_removes_last_eligible_node() {
    // 单节点池：任何摘除都会导致 503。
    let r = RouterEngine::new_with_probe_policy(vec![up("10.0.0.1", 8080, "US")], 1, 300);
    let addr = "10.0.0.1:8080";

    assert_eq!(
        r.record_probe_failure(addr),
        ProbeFailureOutcome::SuppressedLastNode,
        "安全阀必须抑制「摘到最后一个可用节点」"
    );
    assert!(!is_unhealthy(&r, addr), "被抑制的节点不得进入摘除态");
    assert_eq!(r.active_unhealthy_len(), 0);
    assert!(
        r.get_healthy_candidates(&any_spec())
            .iter()
            .any(|n| n.addr == addr),
        "安全阀生效时必须仍有节点可服务"
    );

    // 再失败仍然被抑制（计数继续累计，但状态不落）。
    assert_eq!(
        r.record_probe_failure(addr),
        ProbeFailureOutcome::SuppressedLastNode
    );
    assert_eq!(r.active_unhealthy_len(), 0);
}

#[test]
fn active_health_safety_valve_counts_only_weighted_nodes() {
    // 三节点，其中 C 的 weight=0（等价于已被摘，保留在池内以便 restore）。
    // ⇒ 「可用节点」= A、B 两个，不是三个。
    let r = RouterEngine::new_with_probe_policy(
        vec![
            up("10.0.0.1", 8080, "US"),
            up("10.0.0.2", 8080, "JP"),
            node("10.0.0.3", 8080, "GB", "mock-provider", 0),
        ],
        1,
        300,
    );
    let a = "10.0.0.1:8080";
    let b = "10.0.0.2:8080";
    let c = "10.0.0.3:8080";

    // A 摘得掉（还有 B 顶上）。
    assert_eq!(
        r.record_probe_failure(a),
        ProbeFailureOutcome::MarkedUnhealthy
    );

    // B 摘不得（摘了就没人了）——**这正是「weight=0 不算可用」的判别点**：
    // 若把 C 误计为可用，可用数会是 3、摘除数 1，差值 2 > 1 ⇒ B 会被放行摘除。
    assert_eq!(
        r.record_probe_failure(b),
        ProbeFailureOutcome::SuppressedLastNode,
        "weight=0 的节点不得计入安全阀的「可用节点」"
    );
    assert!(!is_unhealthy(&r, b));

    // C 权重为 0 ⇒ 连「可用」都不是，探测失败不落任何状态。
    assert_eq!(
        r.record_probe_failure(c),
        ProbeFailureOutcome::NodeGone,
        "weight=0 的节点不属于「池中且可用」，迟到探测不落状态"
    );
    assert_eq!(r.probe_consecutive_failures(c), 0);
}

// =====================================================================
// ③ 粘滞会话必须随摘除一起失效
// =====================================================================

#[test]
fn active_health_migrates_sticky_session() {
    // 三个**全部 weight>0** 的节点：安全阀需要「还有别人顶上」才允许摘除。
    let r = RouterEngine::new_with_probe_policy(
        vec![
            up("10.0.0.1", 8080, "US"),
            up("10.0.0.2", 8080, "JP"),
            up("10.0.0.3", 8080, "GB"),
        ],
        1,
        300,
    );
    let a = "10.0.0.1:8080";
    let session = "sess-active-health";

    // 第一次选路：按国家约束锁定到 A（确定性，不依赖随机权重）。
    let spec = RoutingSpec {
        session_id: Some(session.to_string()),
        country: Some("US".to_string()),
        ..Default::default()
    };
    let picked = r.select_node_excluding(&spec, &[]).expect("必须选到节点");
    assert_eq!(picked.addr, a, "夹具前提：应当选中 US 节点 A");
    assert!(
        r.session_store.contains_key(session),
        "夹具前提：会话必须已落表"
    );

    // A 被摘除。
    assert_eq!(
        r.record_probe_failure(a),
        ProbeFailureOutcome::MarkedUnhealthy
    );

    // 关键断言：粘滞绑定必须同时被清掉——
    // 否则「已摘除却仍在发流量」的漏网路径成立（会话把流量钉回坏节点）。
    assert!(
        !r.session_store.contains_key(session),
        "摘除节点时必须一并失效其粘滞会话绑定"
    );

    // 再用同一 session 选路：不得再返回 A。
    let again = r.select_node_excluding(&spec, &[]);
    assert!(
        again.as_ref().is_none_or(|n| n.addr != a),
        "被摘节点不得再被任何会话选中，实测={:?}",
        again.map(|n| n.addr.clone())
    );
}

// =====================================================================
// ④ 冷却到期 → 放行；再失败 → 立即重摘（不重新累计阈值）
// =====================================================================

#[test]
fn active_health_cooldown_expires_and_refailure_remarks() {
    // 冷却 1s：测试里睡 1.1s 即可到期（不引入长时间等待）。
    let r = RouterEngine::new_with_probe_policy(
        vec![up("10.0.0.1", 8080, "US"), up("10.0.0.2", 8080, "JP")],
        1,
        1,
    );
    let a = "10.0.0.1:8080";

    assert_eq!(
        r.record_probe_failure(a),
        ProbeFailureOutcome::MarkedUnhealthy
    );
    assert!(is_unhealthy(&r, a));

    std::thread::sleep(std::time::Duration::from_millis(1100));

    // sweep：清掉过期的摘除标记，但**保留连续失败次数**。
    r.sweep_active_health();
    assert!(!is_unhealthy(&r, a), "冷却到期后必须放行");
    assert_eq!(r.active_unhealthy_len(), 0);
    assert_eq!(
        r.probe_consecutive_failures(a),
        1,
        "冷却到期不得清零连续失败次数（否则节点会在摘除—放行—再摘之间反复横跳）"
    );

    // 再次失败：阈值判定必须**立即**通过（累计已 ≥ 阈值），不再从 0 数起。
    assert_eq!(
        r.record_probe_failure(a),
        ProbeFailureOutcome::MarkedUnhealthy,
        "冷却到期后的再失败必须立即重摘，而不是重新累计阈值"
    );
    assert!(is_unhealthy(&r, a));
}

// =====================================================================
// ⑤ 换池即清僵尸
// =====================================================================

#[test]
fn active_health_pruned_on_pool_replacement() {
    let r = RouterEngine::new_with_probe_policy(
        vec![up("10.0.0.1", 8080, "US"), up("10.0.0.2", 8080, "JP")],
        5,
        300,
    );
    let a = "10.0.0.1:8080";

    // 阈值 5 ⇒ 这次失败只累计（留下健康条目），便于观察清理行为。
    assert_eq!(
        r.record_probe_failure(a),
        ProbeFailureOutcome::BelowThreshold
    );
    assert_eq!(r.probe_consecutive_failures(a), 1);

    // 换池：A 不在新池中 ⇒ 它的健康条目是僵尸，必须被清掉。
    r.reload_nodes(vec![up("10.0.0.9", 8080, "DE")]);
    assert_eq!(
        r.probe_consecutive_failures(a),
        0,
        "池替换后指向已不存在节点的健康条目必须被清理（否则表无限膨胀且指标失真）"
    );
    assert!(
        !r.active_health.contains_key(a),
        "僵尸条目必须从健康表中移除"
    );
}

// =====================================================================
// ⑥ 探测候选必须含被摘节点（否则永不恢复）
// =====================================================================

#[test]
fn probe_candidates_include_unhealthy_for_recovery() {
    let r = RouterEngine::new_with_probe_policy(
        vec![up("10.0.0.1", 8080, "US"), up("10.0.0.2", 8080, "JP")],
        1,
        300,
    );
    let a = "10.0.0.1:8080";
    assert_eq!(
        r.record_probe_failure(a),
        ProbeFailureOutcome::MarkedUnhealthy
    );

    // 100% 抽样：付费节点全在候选里，**包括已被摘除的 A**。
    let cands = r.probe_candidates(100);
    assert!(
        cands.iter().any(|n| n.addr == a),
        "探测候选必须包含被摘除的节点——否则它永远得不到探测结果、永远无法恢复"
    );
    assert_eq!(cands.len(), 2, "weight>0 的节点必须全部进入候选");
}

// =====================================================================
// ⑦ 迟到探测（节点已被删）不落状态
// =====================================================================

#[test]
fn active_health_ignores_stale_probe_for_removed_node() {
    let r = RouterEngine::new_with_probe_policy(
        vec![up("10.0.0.1", 8080, "US"), up("10.0.0.2", 8080, "JP")],
        1,
        300,
    );
    let a = "10.0.0.1:8080";

    // A 被控制面删除。
    assert!(r.remove_node(a).is_some());
    assert!(r.get_node_by_addr(a).is_none());

    // 探针在途的失败结果此刻才回来 ⇒ 必须被识别为「节点已不在」。
    assert_eq!(
        r.record_probe_failure(a),
        ProbeFailureOutcome::NodeGone,
        "对已删除节点的迟到探测结果不得落健康状态"
    );
    assert_eq!(r.probe_consecutive_failures(a), 0);
    assert_eq!(r.active_unhealthy_len(), 0);
}

// =====================================================================
// ⑧ 并发下「表即真相」：计数与逐条判定必须一致，且安全阀不许摘光
// =====================================================================

#[test]
fn opt_r18_active_health_gate_matches_table_under_concurrency() {
    // 8 个节点、阈值 1：8 个线程同时摘除。
    let nodes: Vec<ProxyNode> = (1..=8)
        .map(|i| up(&format!("10.0.0.{i}"), 8080, "US"))
        .collect();
    let addrs: Vec<String> = nodes.iter().map(|n| n.addr.clone()).collect();
    let r = Arc::new(RouterEngine::new_with_probe_policy(nodes, 1, 300));

    let outcomes = std::thread::scope(|s| {
        let handles: Vec<_> = addrs
            .iter()
            .map(|a| {
                // 每个线程各自克隆一份 `Arc`：`Arc` 不实现 `Copy`，
                // 直接 move 会把 `r` 吃掉（外层还要用它做断言）。
                let engine = Arc::clone(&r);
                s.spawn(move || engine.record_probe_failure(a))
            })
            .collect();
        handles
            .into_iter()
            .map(|h| h.join().unwrap())
            .collect::<Vec<_>>()
    });

    // 每个线程只可能得到「摘除」或「被安全阀抑制」两种结局。
    for o in &outcomes {
        assert!(
            matches!(
                o,
                ProbeFailureOutcome::MarkedUnhealthy
                    | ProbeFailureOutcome::SuppressedLastNode
                    | ProbeFailureOutcome::NodeGone
            ),
            "并发摘除只允许三种结局，实测={o:?}"
        );
    }

    // 安全阀：并发下**绝不能**把池摘空 ⇒ 至少还剩 1 个可用节点。
    let (eligible, unhealthy) = r.count_eligible_and_unhealthy(Instant::now());
    assert_eq!(eligible, 8);
    assert!(
        eligible - unhealthy >= 1,
        "并发摘除后必须仍有可用节点（安全阀在并发下不得失效），unhealthy={unhealthy}"
    );

    // 「表即真相」：全表扫描出来的摘除数，必须与**逐条判定**的结果完全一致。
    // 这一条是主动健康的核心不变式——旧实现用增量计数器维护「被摘除数」，
    // 计数器低估会让安全阀放行「摘到全池 503」。
    let by_scan = r.active_unhealthy_len();
    assert_eq!(by_scan, unhealthy, "全表扫描与同快照计数必须自洽");
    let by_per_node = addrs
        .iter()
        .filter(|a| r.is_active_unhealthy(a, Instant::now()))
        .count();
    assert_eq!(
        by_per_node, by_scan,
        "逐条判定与全表扫描必须一致（表即真相）"
    );
}

// =====================================================================
// ⑨ 免费池抽样：有限 tick 内全覆盖
// =====================================================================

#[test]
fn opt_r18_probe_sampling_covers_free_pool_in_bounded_ticks() {
    // 8 个免费节点，25% 抽样 ⇒ 步长 4 ⇒ 每轮取 2 个，4 轮全覆盖。
    let nodes: Vec<ProxyNode> = (1..=8)
        .map(|i| free(&format!("10.1.0.{i}"), 8080))
        .collect();
    let r = RouterEngine::new_with_probe_policy(nodes, 1, 300);

    let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
    for _ in 0..4 {
        for c in r.probe_candidates(25) {
            seen.insert(c.addr.clone());
        }
    }
    assert_eq!(
        seen.len(),
        8,
        "游标抽样必须在 ceil(100/25)=4 轮内覆盖全部免费节点，实测覆盖={}",
        seen.len()
    );

    // 单轮只取 1/4 ⇒ 抽样确实生效（不是退化成全量）。
    let one_round = r.probe_candidates(25);
    assert_eq!(one_round.len(), 2, "25% 抽样单轮应取 8/4=2 个");

    // 付费节点不参与抽样：100% 必选。
    let r2 = RouterEngine::new_with_probe_policy(
        vec![free("10.1.0.1", 8080), up("10.2.0.1", 8080, "US")],
        1,
        300,
    );
    for _ in 0..4 {
        let cands = r2.probe_candidates(25);
        assert!(
            cands.iter().any(|n| n.addr == "10.2.0.1:8080"),
            "付费节点必须每轮都在探测候选中（SLA 主体不抽样）"
        );
    }
}
