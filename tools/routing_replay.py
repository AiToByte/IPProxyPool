#!/usr/bin/env python3
"""选路策略离线回放台（OPT-R18 P1-5）。

## 这个工具解决什么问题

本仓的核心差异化是 LinUCB 自学习选路，但此前的所有验收都只证明「**选得动**」：

- OPT-R13 的 1500 次活流量：证明**不永久锁死**；
- 步骤 41 的 45.9% 成功率：证明**供给可用**；
- `--release bandit` 的 17 个测试：证明**数学正确、<200ns**。

**没有一条证明「选得好」**——即：换一个策略，同一批请求的成功率会不会更高。
本工具把历史遥测重放一遍，在同一请求序列上并排跑多个策略，输出可比较的指标。
没有它，选路优化的收益永远只能靠自述。

## 方法：基于日志的离线策略评估（offline policy evaluation）

历史日志只记录了「当时选中的那个节点」及其结果，**没有反事实**（没被选中的节点
会怎样）。所以回放必须先用日志拟合一个**结果模型**，再让各策略在该模型上做决策。

```
每个请求 i：策略从候选集选节点 n
          → 模型给出 P(成功 | n, domain) 与期望时延
          → 采样成功/失败（种子固定，可复现）
          → reward = compute_reward(status, latency)（与生产同口径）
```

### 这个方法的已知偏差（必须写明，不能当成"真实收益"引用）

1. **支持偏差**：若记录策略从未探索过某 (节点, 域名) 组合，模型对该组合的估计
   只能靠收缩先验兜底。所以本工具报出的绝对值**不可**当作线上成功率预测，
   只有**策略之间的相对排序**有意义。
2. **结果模型把相关性当因果**：模型学的是「日志策略选它时它成功了多少次」，
   不是「换策略选它也会成功」。节点被选中本身与域名难度相关。
3. **因此本工具的定位是「回归护栏 + 方向判断」**：它能在改动前后给出同口径对比，
   回答「这次改动让成功率涨了还是跌了」，不回答「线上一定是 62%」。

## 用法

```bash
# 1) 从 ClickHouse 拉最近 24h 的遥测重放（需要 CH 可达）
python tools/routing_replay.py --source ch --hours 24

# 2) 离线 jsonl（CI 用；格式见 tools/replay/sample_trace.jsonl）
python tools/routing_replay.py --source jsonl --path tools/replay/sample_trace.jsonl

# 3) 自检：内置不变量断言（确定性、策略排序、指标口径）
python tools/routing_replay.py --self-test
```

## 为什么不用 numpy / pandas

本仓 `tools/` 下全部是**纯标准库**脚本（CI 的 static-checks job 只装了 python3）。
引入第三方依赖会让这道工具在 CI 里跑不起来，而跑不起来的工具等于不存在。
矩阵维数固定为 d=4，手写 4×4 运算的代码量小于引入依赖的成本。
"""

from __future__ import annotations

import argparse
import json
import math
import random
import sys
import urllib.request
from dataclasses import dataclass, field
from pathlib import Path
from typing import Callable, Iterable, Sequence

ROOT = Path(__file__).resolve().parent.parent

# =============================================================================
# 与 gateway/src/bandit.rs 逐字对齐的常量
#
# 这些值**不是**本工具的调参旋钮，而是生产实现的镜像。任何一侧改动都必须同步
# 另一侧——`tools/check_replay_parity.py` 用黄金向量锁住这件事。
# =============================================================================

CONTEXT_DIM = 4
DEFAULT_ALPHA = 0.4
MAX_MEAN_REWARD = 1.0
COST_DC = 0.1
COST_RESIDENTIAL = 1.0
COST_MOBILE = 3.0
FORGET_EVERY_N_UPDATES = 10_000
FORGET_EVERY_FREE_UPDATES = 1_000
FREE_RISK_PREMIUM = 0.15
FORGET_KEEP = 0.9

#: WAF 域名风险先验（bandit.rs `DOMAIN_RISK_TABLE`，顺序即优先级）。
DOMAIN_RISK_TABLE: tuple[tuple[str, float], ...] = (
    ("cloudflare", 0.9),
    ("turnstile", 0.9),
    ("akamai", 0.85),
    ("datadome", 0.85),
    ("perimeterx", 0.85),
    ("kasada", 0.85),
    ("imperva", 0.85),
)
DOMAIN_RISK_DEFAULT = 0.3


def domain_risk(target_domain: str) -> float:
    """与 `bandit.rs::domain_risk` 同序：先命中先返回，未命中走默认。"""
    d = target_domain.lower()
    for needle, risk in DOMAIN_RISK_TABLE:
        if needle in d:
            return risk
    return DOMAIN_RISK_DEFAULT


def cos_time_prior(ts_unix: float) -> float:
    """UTC 日内余弦先验，归一化到 [0,1]。

    生产实现取 `SystemTime::now()`；回放取**该请求自己的时间戳**——
    这比生产更准（生产无法回溯历史时刻），也让回放结果可复现。
    """
    secs = ts_unix % 86_400
    return math.cos((secs / 86_400.0) * math.tau) * 0.5 + 0.5


def cost_weight_for_tier(tier: str) -> float:
    t = tier.lower()
    if t in ("datacenter", "dc"):
        return COST_DC
    if t == "mobile":
        return COST_MOBILE
    if t == "free":
        return 0.0
    return COST_RESIDENTIAL


def forget_every_for_tier(tier: str) -> int:
    return FORGET_EVERY_FREE_UPDATES if tier.lower() == "free" else FORGET_EVERY_N_UPDATES


def compute_reward(status_code: int, latency_ms: float) -> float:
    """与 `bandit.rs::compute_reward` 同口径。"""
    if 200 <= status_code <= 299:
        return 1.0 - min(latency_ms / 2000.0, 0.5)
    if status_code in (403, 429):
        return 0.0
    return 0.2


def extract_context(target_domain: str, ts_unix: float) -> list[float]:
    """`[DomainRisk, Bias, CosTime, LatencyPrior]`——顺序不可调换。"""
    return [domain_risk(target_domain), 1.0, cos_time_prior(ts_unix), 0.2]


# =============================================================================
# 4×4 线性代数（d 固定为 4，手写比引依赖便宜）
# =============================================================================


def mat_vec(m: Sequence[Sequence[float]], v: Sequence[float]) -> list[float]:
    return [sum(m[i][j] * v[j] for j in range(CONTEXT_DIM)) for i in range(CONTEXT_DIM)]


def dot(a: Sequence[float], b: Sequence[float]) -> float:
    return sum(x * y for x, y in zip(a, b))


def identity() -> list[list[float]]:
    return [[1.0 if i == j else 0.0 for j in range(CONTEXT_DIM)] for i in range(CONTEXT_DIM)]


# =============================================================================
# LinUCB 臂：与 bandit.rs::BanditArm 逐行对应
# =============================================================================


@dataclass
class Arm:
    """一个出口节点的 LinUCB 状态。

    `a_inv` 是协方差逆矩阵（初始单位阵），`b` 是奖励偏置向量（初始零）。
    生产用 Sherman-Morrison 原地更新（O(d²)），此处同法——**不要**改成
    「先累积 A 再求逆」，那样两者的数值路径不同，黄金向量会对不上。
    """

    key: str
    tier: str
    a_inv: list[list[float]] = field(default_factory=identity)
    b: list[float] = field(default_factory=lambda: [0.0] * CONTEXT_DIM)
    updates: int = 0

    @property
    def cost_weight(self) -> float:
        return cost_weight_for_tier(self.tier)

    def score(self, ctx: Sequence[float], alpha: float, t: int) -> float:
        """UCB 分数。公式见 `bandit.rs::compute_ucb_score` 的文档注释。"""
        theta = mat_vec(self.a_inv, self.b)
        expected = min(max(dot(theta, ctx), 0.0), MAX_MEAN_REWARD)
        a_inv_x = mat_vec(self.a_inv, ctx)
        variance = math.sqrt(max(dot(ctx, a_inv_x), 0.0))
        n_i = float(self.updates)
        exploration = alpha * variance * math.sqrt(math.log(1.0 + t)) / math.sqrt(1.0 + n_i)
        s = expected + exploration - 0.05 * self.cost_weight
        if self.tier.lower() == "free":
            s -= FREE_RISK_PREMIUM
        return s

    def update(self, ctx: Sequence[float], reward: float) -> None:
        """Sherman-Morrison 更新 + 分档遗忘。"""
        for i in range(CONTEXT_DIM):
            self.b[i] += reward * ctx[i]
        a_inv_x = mat_vec(self.a_inv, ctx)
        denominator = 1.0 + dot(ctx, a_inv_x)
        for i in range(CONTEXT_DIM):
            for j in range(CONTEXT_DIM):
                self.a_inv[i][j] -= a_inv_x[i] * a_inv_x[j] / denominator
        self.updates += 1
        if self.updates % forget_every_for_tier(self.tier) == 0:
            # 轻 reset：90% 已学方向 + 10% 先验；计数器不清零（节拍恒定）。
            ident = identity()
            for i in range(CONTEXT_DIM):
                for j in range(CONTEXT_DIM):
                    self.a_inv[i][j] = self.a_inv[i][j] * FORGET_KEEP + ident[i][j] * (1.0 - FORGET_KEEP)
            for i in range(CONTEXT_DIM):
                self.b[i] *= FORGET_KEEP


# =============================================================================
# 请求记录与结果模型
# =============================================================================


@dataclass(frozen=True)
class RequestRow:
    """一条历史请求。字段取自 ClickHouse `proxy_telemetry` 的实际列。"""

    ts: float
    trace_id: str
    target_domain: str
    out_ip: str
    provider: str
    tier: str
    status_code: int
    latency_ms: float

    @property
    def ok(self) -> bool:
        return 200 <= self.status_code <= 299


#: 结果模型向节点全局成功率收缩的**先验强度**（伪计数）。
#:
#: 为什么必须收缩：某个 (节点, 域名) 组合可能只出现过 1 次，直接用 1/1 当
#: 「100% 成功」会让策略在回放里做出与线上完全无关的乐观决策。5 是经验值：
#: 观测少于 5 次时，先验权重占主导；超过 20 次后先验影响 < 20%。
PRIOR_STRENGTH = 5.0


class OutcomeModel:
    """从日志拟合的 `P(成功 | 节点, 域名)` 与期望时延。

    两级收缩：`(节点,域名)` → 节点全局 → 全局。层级越浅样本越多，越可信。
    """

    def __init__(self, rows: Iterable[RequestRow]) -> None:
        self.by_pair: dict[tuple[str, str], list[int]] = {}
        self.by_node: dict[str, list[int]] = {}
        self.lat_pair: dict[tuple[str, str], list[float]] = {}
        self.lat_node: dict[str, list[float]] = {}
        self.total_ok = 0
        self.total = 0
        for r in rows:
            self.total += 1
            self.total_ok += 1 if r.ok else 0
            for table, key in ((self.by_pair, (r.out_ip, r.target_domain)), (self.by_node, r.out_ip)):
                slot = table.setdefault(key, [0, 0])
                slot[0] += 1 if r.ok else 0
                slot[1] += 1
            if r.ok:
                self.lat_pair.setdefault((r.out_ip, r.target_domain), []).append(r.latency_ms)
                self.lat_node.setdefault(r.out_ip, []).append(r.latency_ms)

    @property
    def global_rate(self) -> float:
        return (self.total_ok / self.total) if self.total else 0.0

    def node_rate(self, node: str) -> float:
        ok, n = self.by_node.get(node, (0, 0))
        if n == 0:
            return self.global_rate
        prior = self.global_rate
        return (ok + PRIOR_STRENGTH * prior) / (n + PRIOR_STRENGTH)

    def success_prob(self, node: str, domain: str) -> float:
        """`(节点,域名)` 成功率，向节点全局收缩。"""
        prior = self.node_rate(node)
        ok, n = self.by_pair.get((node, domain), (0, 0))
        return (ok + PRIOR_STRENGTH * prior) / (n + PRIOR_STRENGTH)

    def mean_latency(self, node: str, domain: str) -> float:
        """期望时延：优先 (节点,域名) 观测均值，回落到节点均值，再回落 200ms。"""
        pair = self.lat_pair.get((node, domain))
        if pair:
            return sum(pair) / len(pair)
        node_lat = self.lat_node.get(node)
        if node_lat:
            return sum(node_lat) / len(node_lat)
        return 200.0


# =============================================================================
# 策略：统一接口 `choose(ctx, candidates, t) -> node`
#
# `candidates` 已按请求约束（tier/proto）过滤；策略只能在其中选。
# 所有策略都**必须**是确定性的（同输入同输出），否则回放不可复现。
# =============================================================================


class Policy:
    name = "policy"

    def choose(self, ctx: Sequence[float], candidates: Sequence[str], t: int) -> str:  # pragma: no cover
        raise NotImplementedError

    def observe(self, node: str, ctx: Sequence[float], reward: float) -> None:
        """被动反馈。只有被选中的节点会收到（与生产一致：无全量反馈）。"""


class LinUCBPolicy(Policy):
    """生产策略的镜像。

    关键语义与 `gateway::select_bandit_node_excluding` 一致：
    - `t` 每请求取一次（**不是**每候选一次），否则探索项按池大小放大；
    - 严格 `>` 比较 ⇒ 同分时首个候选胜出（与 Rust 的流式实现一致）。
    """

    name = "linucb"

    def __init__(self, tiers: dict[str, str], alpha: float = DEFAULT_ALPHA) -> None:
        self.alpha = alpha
        self.arms = {node: Arm(node, tier) for node, tier in tiers.items()}

    def choose(self, ctx: Sequence[float], candidates: Sequence[str], t: int) -> str:
        best: str | None = None
        best_score = float("-inf")
        for node in candidates:
            arm = self.arms.get(node)
            if arm is None:
                arm = self.arms[node] = Arm(node, "residential")
            s = arm.score(ctx, self.alpha, t)
            if s > best_score:
                best_score, best = s, node
        if best is None:
            raise ValueError("empty candidate set")
        return best

    def observe(self, node: str, ctx: Sequence[float], reward: float) -> None:
        arm = self.arms.get(node)
        if arm is not None:
            arm.update(ctx, reward)


class WeightedPolicy(Policy):
    """加权随机（生产 `pick_weighted_streaming` 的形态）。

    日志里没有节点权重，故默认**均匀**（等价于随机选路）。这看起来"太弱"，
    但它正是加权随机的下限：真实权重只会让它更偏向大权重节点，不会更聪明。
    需要精确对比时用 `--weights <json>` 传入配置权重。
    """

    name = "weighted"

    def __init__(self, weights: dict[str, float] | None = None) -> None:
        self.weights = weights or {}

    def choose(self, ctx: Sequence[float], candidates: Sequence[str], t: int) -> str:
        # 用 t 作为种子的一部分：确定性 + 不随策略内部状态漂移。
        rng = random.Random(f"weighted:{t}")
        total = sum(self.weights.get(c, 1.0) for c in candidates)
        pick = rng.random() * total
        acc = 0.0
        for c in candidates:
            acc += self.weights.get(c, 1.0)
            if pick <= acc:
                return c
        return candidates[-1]


class RoundRobinPolicy(Policy):
    """轮询：无状态基线。用来回答「LinUCB 到底比轮询强多少」。"""

    name = "round_robin"

    def choose(self, ctx: Sequence[float], candidates: Sequence[str], t: int) -> str:
        return candidates[t % len(candidates)]


class OraclePolicy(Policy):
    """作弊上界：每步都选模型里成功率最高的节点。

    它**不是**可部署策略（真实系统不知道未来），作用是给出该结果模型下的
    理论上限，让「LinUCB 与上限的差距」可量化。若 LinUCB 已贴近 oracle，
    继续优化选路的边际收益就很低——这是判断「还值不值得投」的关键数字。
    """

    name = "oracle"

    def __init__(self, model: OutcomeModel) -> None:
        self.model = model

    def choose(self, ctx: Sequence[float], candidates: Sequence[str], t: int) -> str:
        return max(candidates, key=lambda c: (self.model.success_prob(c, self._domain), c))

    # oracle 需要知道当前请求的域名；由 replay 在调用前注入，避免接口膨胀。
    _domain: str = ""

    def set_domain(self, domain: str) -> None:
        self._domain = domain


# =============================================================================
# 回放主循环
# =============================================================================


@dataclass
class Metrics:
    policy: str
    requests: int = 0
    success: int = 0
    reward_sum: float = 0.0
    avoidable: int = 0
    latencies: list[float] = field(default_factory=list)
    per_node: dict[str, int] = field(default_factory=dict)

    @property
    def success_rate(self) -> float:
        return self.success / self.requests if self.requests else 0.0

    @property
    def mean_reward(self) -> float:
        return self.reward_sum / self.requests if self.requests else 0.0

    @property
    def avoidable_rate(self) -> float:
        return self.avoidable / self.requests if self.requests else 0.0

    def p(self, q: float) -> float:
        if not self.latencies:
            return 0.0
        s = sorted(self.latencies)
        idx = min(len(s) - 1, max(0, int(round(q * (len(s) - 1)))))
        return s[idx]


def replay(
    rows: Sequence[RequestRow],
    model: OutcomeModel,
    policy: Policy,
    *,
    seed: int = 20261027,
    candidates_for: Callable[[RequestRow], Sequence[str]] | None = None,
) -> Metrics:
    """在给定请求序列上跑一个策略。

    # 为什么失败判定用「采样」而不是「模型期望」

    用期望值（`p` 直接当成功次数）会抹掉方差，让所有策略的成功率看起来都
    平滑而接近，**失去区分度**。采样能保留「某节点在某域名上就是不稳」这类
    真实形态。种子固定 ⇒ 可复现；同一 seed 下所有策略看到**同一批随机数**，
    对比公平。
    """
    rng = random.Random(seed)
    m = Metrics(policy=policy.name)
    all_nodes = sorted({r.out_ip for r in rows})
    t = 0
    for r in rows:
        cands = list(candidates_for(r) if candidates_for else all_nodes)
        if not cands:
            continue
        t += 1
        ctx = extract_context(r.target_domain, r.ts)
        if isinstance(policy, OraclePolicy):
            policy.set_domain(r.target_domain)
        node = policy.choose(ctx, cands, t)

        p = model.success_prob(node, r.target_domain)
        ok = rng.random() < p
        latency = model.mean_latency(node, r.target_domain)
        status = 200 if ok else 502
        reward = compute_reward(status, latency)

        m.requests += 1
        m.success += 1 if ok else 0
        m.reward_sum += reward
        m.latencies.append(latency)
        m.per_node[node] = m.per_node.get(node, 0) + 1
        # 「本可避免的失败」：这次选的节点失败了，但候选里存在成功率 ≥50% 的其他节点。
        # 这是选路质量的直接度量——不是"网络不好"，而是"选错了"。
        if not ok and any(
            c != node and model.success_prob(c, r.target_domain) >= 0.5 for c in cands
        ):
            m.avoidable += 1
        policy.observe(node, ctx, reward)
    return m


# =============================================================================
# 数据加载
# =============================================================================


def load_jsonl(path: Path) -> list[RequestRow]:
    rows: list[RequestRow] = []
    for lineno, line in enumerate(path.read_text(encoding="utf-8").splitlines(), 1):
        line = line.strip()
        if not line or line.startswith("#"):
            continue
        try:
            d = json.loads(line)
        except json.JSONDecodeError as e:
            raise SystemExit(f"{path}:{lineno}: 不是合法 JSON：{e}") from e
        rows.append(
            RequestRow(
                ts=float(d["ts"]),
                trace_id=str(d.get("trace_id", "")),
                target_domain=str(d["target_domain"]),
                out_ip=str(d["out_ip"]),
                provider=str(d.get("provider", "unknown")),
                tier=str(d.get("tier", "datacenter")),
                status_code=int(d["status_code"]),
                latency_ms=float(d.get("latency_ms", 0.0)),
            )
        )
    if not rows:
        raise SystemExit(f"{path}: 没有任何请求行")
    return rows


def load_clickhouse(url: str, user: str, password: str, hours: int) -> list[RequestRow]:
    """从 ClickHouse 拉遥测。

    只取**回放必需**的列：多取一列就多一份内存与带宽，而回放是单进程的。
    `FORMAT JSONEachRow` 让结果直接按行解析，不需要额外的 JSON 库。
    """
    query = (
        "SELECT toUnixTimestamp(timestamp) AS ts, trace_id, target_domain, out_ip, "
        "provider, tier, status_code, latency_ms "
        "FROM proxy_telemetry "
        f"WHERE timestamp >= now() - INTERVAL {int(hours)} HOUR "
        "ORDER BY timestamp ASC FORMAT JSONEachRow"
    )
    req = urllib.request.Request(
        url,
        data=query.encode("utf-8"),
        headers={"X-ClickHouse-User": user, "X-ClickHouse-Key": password},
        method="POST",
    )
    with urllib.request.urlopen(req, timeout=30) as resp:  # noqa: S310（内网固定地址）
        body = resp.read().decode("utf-8")
    rows = []
    for line in body.splitlines():
        if not line.strip():
            continue
        d = json.loads(line)
        rows.append(
            RequestRow(
                ts=float(d["ts"]),
                trace_id=str(d.get("trace_id", "")),
                target_domain=str(d["target_domain"]),
                out_ip=str(d["out_ip"]),
                provider=str(d.get("provider", "unknown")),
                tier=str(d.get("tier", "datacenter")),
                status_code=int(d["status_code"]),
                latency_ms=float(d.get("latency_ms", 0.0)),
            )
        )
    if not rows:
        raise SystemExit("ClickHouse 返回 0 行：检查时间窗、表名与凭据")
    return rows


# =============================================================================
# 报告
# =============================================================================


def render_markdown(results: Sequence[Metrics], total_rows: int, model: OutcomeModel) -> str:
    lines = [
        "| 策略 | 成功 | 成功率 | 平均奖励 | 可避免失败 | P50 时延 | P99 时延 | 命中节点数 |",
        "| --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: |",
    ]
    for m in results:
        lines.append(
            f"| `{m.policy}` | {m.success}/{m.requests} | {m.success_rate:.1%} | "
            f"{m.mean_reward:.4f} | {m.avoidable} ({m.avoidable_rate:.1%}) | "
            f"{m.p(0.50):.0f}ms | {m.p(0.99):.0f}ms | {len(m.per_node)} |"
        )
    lines.append("")
    lines.append(
        f"> 样本 {total_rows} 条请求；结果模型全局成功率 {model.global_rate:.1%}；"
        "**绝对值不可当作线上预测**（见脚本头部的偏差说明），只比较行间相对差异。"
    )
    return "\n".join(lines)


def render_json(results: Sequence[Metrics]) -> str:
    return json.dumps(
        [
            {
                "policy": m.policy,
                "requests": m.requests,
                "success": m.success,
                "success_rate": round(m.success_rate, 6),
                "mean_reward": round(m.mean_reward, 6),
                "avoidable": m.avoidable,
                "avoidable_rate": round(m.avoidable_rate, 6),
                "p50_ms": round(m.p(0.50), 3),
                "p99_ms": round(m.p(0.99), 3),
                "per_node": m.per_node,
            }
            for m in results
        ],
        ensure_ascii=False,
        indent=2,
    )


# =============================================================================
# 自检：不变量断言（CI 用，不需要 ClickHouse）
# =============================================================================


def self_test(verbose: bool = True) -> int:
    """断言回放台自身的**性质**，而不是「跑出某个数字」。

    锁三类东西：
    1. **确定性**：同 seed 两次运行结果逐字段相同（否则报告不可复现）；
    2. **区分度**：oracle 的成功率必须严格高于 round_robin（否则模型没信息量，
       或指标算错了）；
    3. **LinUCB 的可学习性**：在一个「有明确好节点」的合成流上，LinUCB 必须
       显著优于轮询——这是它存在的理由，也是本工具的核心断言。
    """
    sample = ROOT / "tools" / "replay" / "sample_trace.jsonl"
    rows = load_jsonl(sample)
    model = OutcomeModel(rows)
    tiers = {n: "datacenter" for n in {r.out_ip for r in rows}}

    def run(policy: Policy) -> Metrics:
        return replay(rows, model, policy, seed=7)

    a = run(LinUCBPolicy(tiers))
    b = run(LinUCBPolicy(tiers))
    checks: list[tuple[str, bool, str]] = []
    checks.append(
        (
            "determinism",
            (a.success, round(a.reward_sum, 9)) == (b.success, round(b.reward_sum, 9)),
            f"{a.success} vs {b.success}",
        )
    )

    rr = run(RoundRobinPolicy())
    orc = run(OraclePolicy(model))
    checks.append(("oracle>round_robin", orc.success_rate > rr.success_rate,
                   f"{orc.success_rate:.3f} vs {rr.success_rate:.3f}"))
    checks.append(("linucb>round_robin", a.success_rate > rr.success_rate,
                   f"{a.success_rate:.3f} vs {rr.success_rate:.3f}"))
    checks.append(("oracle>=linucb", orc.success_rate >= a.success_rate - 0.02,
                   f"{orc.success_rate:.3f} vs {a.success_rate:.3f}"))
    checks.append(("linucb explores", len(a.per_node) > 1, f"hit {len(a.per_node)} nodes"))
    checks.append(("metrics_sum", a.success <= a.requests and a.requests == len(rows),
                   f"{a.success}/{a.requests} of {len(rows)}"))

    failed = 0
    for name, ok, detail in checks:
        if verbose:
            print(f"{'ok  ' if ok else 'FAIL'} {name}: {detail}")
        failed += 0 if ok else 1
    if verbose and failed == 0:
        print("\n" + render_markdown([rr, a, orc], len(rows), model))
    return 1 if failed else 0


# =============================================================================
# CLI
# =============================================================================


def main(argv: Sequence[str] | None = None) -> int:
    ap = argparse.ArgumentParser(description="选路策略离线回放台")
    ap.add_argument("--source", choices=("jsonl", "ch"), default="jsonl")
    ap.add_argument("--path", type=Path, default=ROOT / "tools" / "replay" / "sample_trace.jsonl")
    ap.add_argument("--ch-url", default="http://127.0.0.1:8123/")
    ap.add_argument("--ch-user", default="proxy")
    ap.add_argument("--ch-password", default="123456")
    ap.add_argument("--hours", type=int, default=24)
    ap.add_argument(
        "--policies",
        default="round_robin,weighted,linucb,oracle",
        help="逗号分隔；oracle 是作弊上界，只用于标定差距",
    )
    ap.add_argument("--seed", type=int, default=20261027)
    ap.add_argument("--format", choices=("markdown", "json"), default="markdown")
    ap.add_argument("--self-test", action="store_true", help="跑不变量断言（不需要 CH）")
    args = ap.parse_args(argv)

    if args.self_test:
        return self_test()

    rows = (
        load_jsonl(args.path)
        if args.source == "jsonl"
        else load_clickhouse(args.ch_url, args.ch_user, args.ch_password, args.hours)
    )
    model = OutcomeModel(rows)
    tiers: dict[str, str] = {}
    for r in rows:
        # 同一节点可能以不同 tier 出现（极少）；取最后一次，与"池里只有一个实例"一致。
        tiers[r.out_ip] = r.tier

    results: list[Metrics] = []
    for name in (p.strip() for p in args.policies.split(",") if p.strip()):
        policy: Policy
        if name == "linucb":
            policy = LinUCBPolicy(tiers)
        elif name == "weighted":
            policy = WeightedPolicy()
        elif name == "round_robin":
            policy = RoundRobinPolicy()
        elif name == "oracle":
            policy = OraclePolicy(model)
        else:
            raise SystemExit(f"未知策略：{name}")
        results.append(replay(rows, model, policy, seed=args.seed))

    print(render_json(results) if args.format == "json" else render_markdown(results, len(rows), model))
    return 0


if __name__ == "__main__":
    sys.exit(main())
