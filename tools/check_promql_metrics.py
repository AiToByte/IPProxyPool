#!/usr/bin/env python3
"""CI 断言：Prometheus 规则里的指标名与标签名必须与 `metrics.rs` 的实际渲染一致。

# 为什么需要这道门（OPT-R9 A2，有实测依据）

`ci.yml` 原本只跑 `yaml.safe_load` 验规则**语法**。OPT-R9 A1 补的 `promtool check
rules` 验的是 **PromQL 静态类型**（能抓 `histogram_quantile(0.99, "str")` 这类
参数类型错），但**抓不到标签集不匹配**。

**实测证据（OPT-R9 A1 交付时）**：把 OPT-R7 B2 的原始错误（分母写成
`sum by (provider) (rate(proxy_requests_total[10m]))`，而该指标只渲染
`{status="..."}`、没有 `provider` 标签）注入后跑 `promtool check rules`，结果是
`SUCCESS: 12 rules found` —— **完全放过**。而这条表达式在运行期返回空集，
per-provider 封禁比永远算不出结果。

所以需要本门做 `promtool` 做不到的事：**把规则里用到的「指标名 + 聚合标签」
与 `gateway/src/metrics.rs` 的实际渲染逐一对账**。这正是 OPT-R7 B2 手工做过、
但没固化的核对。

# 数据来源（两侧都从源码提取，不硬编码）

- **指标名**：`metrics.rs` 的 `# HELP <name>` 清单。
- **标签名**：`metrics.rs` 里形如 `"<metric>{{<label>=\"...\"}}` 的渲染行。
  这是判定「某指标是否带某标签」的唯一可靠依据（注释与实际渲染曾有过不一致）。

# 判定口径

- Prometheus 内置序列（`up`）→ 放行。
- 规则里的标签：只看出现在 `sum by (...)` / `sum without (...)` / `by (...)`
  里的**聚合标签**——那才是会与指标标签集不匹配的地方。
  `{}` 内的选择器标签（如 `{status="2xx"}`、`{worker="x"}`）是**匹配条件**，
  与聚合标签不同语义，不参与本门校验。
- 聚合标签若在**任何**被聚合的指标上都不存在 → 报错（附具体指标与标签名）。

# 用法

```bash
python tools/check_promql_metrics.py
```

# 退出码

- 0：一致
- 1：发现不一致（逐条打印文件/规则/指标/标签）
- 2：读取失败
"""

from __future__ import annotations

import re
import sys
from pathlib import Path

REPO = Path(__file__).resolve().parent.parent
RULES = REPO / "deploy" / "prometheus" / "rules.yml"
METRICS_RS = REPO / "gateway" / "src" / "metrics.rs"

# Prometheus 内置序列（由 Prometheus 自身产生，网关不渲染）。
BUILTIN_SERIES = {"up", "scrape_duration_seconds", "scrape_samples_scraped"}

# 外部 exporter 提供的序列（网关自身不渲染，但可被规则引用）。
# redis_exporter 由 compose 的 `observability` profile 提供（profile-gated，
# 未跑起时规则静默 inactive）。
EXTERNAL_SERIES = {
    "redis_stream_length",       # redis_exporter（CHECK_SINGLE_KEYS 模式）
    "redis_up", "redis_connected_clients", "redis_memory_used_bytes",
}

# 显式豁免的「聚合标签 vs 指标标签集」组合。
#
# `ProviderForbiddenRatioHigh`（OPT-R7 B2 交付）：分子
# `proxy_requests_forbidden_total` 带 `provider` 标签，分母
# `proxy_requests_total` **只有 `status` 标签**（metrics.rs:404-416 已实证），
# 因此算不出真正的 per-provider 封禁比。该限制**已在 rules.yml 的注释与
# annotation 中如实标注**，分母已退化为「全网总量」并把阈值下调到 5%。
#
# 这不是疏忽而是**当前 exposition 能力的上限**——要改成真 per-provider 比率，
# 需先给 `proxy_requests_total` 增加 `provider` 标签（属 `metrics.rs` 变更，
# 不在本轮范围）。故在此显式豁免，并要求豁免时**必须已有注释说明**：
# 若将来给该指标补上 `provider` 标签，本条豁免应同步删除（届时本门会
# 立刻对该规则恢复正常校验）。
EXEMPT_AGGREGATIONS: dict[tuple[str, str, str], str] = {
    (
        "ProviderForbiddenRatioHigh",
        "proxy_requests_total",
        "provider",
    ): "proxy_requests_total 无 provider 标签（metrics.rs:404-416）；"
       "分母已退化为全网总量、阈值下调至 5%，限制已在 rules.yml 注释与 "
       "annotation 中标注。补上 provider 标签后请删除本豁免。",
}

# PromQL 函数/关键字，不是指标名。
FUNCS = {
    "sum", "rate", "increase", "delta", "max", "min", "avg", "count", "topk", "bottomk",
    "histogram_quantile", "clamp_min", "clamp_max", "max_over_time", "min_over_time",
    "absent", "absent_over_time", "and", "or", "unless", "on", "ignoring",
    "group_left", "group_right", "offset", "bool", "by", "without", "le", "quantile",
}

# 指标名形态：小写字母/下划线/冒号，以字母开头。
METRIC_RE = re.compile(r"\b([a-z][a-z0-9_]*(?::[a-z][a-z0-9_]*)?)\b")
# `by (a, b)` / `without (a, b)` / `on (a)` 里的标签列表。
BY_RE = re.compile(r"\b(?:by|without|on)\s*\(([^)]*)\)")


def load_metric_labels() -> tuple[set[str], dict[str, set[str]]]:
    """从 metrics.rs 提取 (指标名集合, {指标名: 该指标渲染时用的标签集合})。

    渲染行有两种写法，**都要认**：
      ① `out.push_str("foo_total{{provider=\"{p}\"}} {n}\\n")` —— 直���字面量，
         Rust 字符串里花括号被转义成 `{{` `}}`（格式串要输出字面花括号时须转义）；
      ② `format!("foo_total{{provider=\"{p}\"}} {n}\\n")` —— 同上，本质一样。
    另有 `# HELP foo_bar ...` 里**不带标签**的裸序列（如 `quarantine_nodes`），
    它们在 labels 里没有条目，聚合校验时按「无标签」处理。
    """
    text = METRICS_RS.read_text(encoding="utf-8")

    names: set[str] = set(re.findall(r"#\s+HELP\s+([a-zA-Z_0-9]+)", text))

    # 形态 ①/②：`"<metric>{{label=\"...\" ...}}`（转义双花括号）。
    #
    # 正则要点：标签体内含 **Rust 转义引号** `\"`，其后面跟着格式串的 `}`。
    # 若用 `[^}]*` 会在 `\"}` 处被 `}` 截断（只抓到 `provider=\"` 就断），
    # 导致标签名提取失败 → 聚合校验误报。改用 `.*?` 非贪婪 + 锚定 `}}` 收尾。
    labels: dict[str, set[str]] = {}
    for m in re.finditer(r'"([a-z_0-9]+)\{\{(.*?)\}\}', text, re.S):
        metric, body = m.group(1), m.group(2)
        # 标签名 = `name=` 的 name 部分（允许 `\"` 等转义，不影响取名）。
        found = set(re.findall(r"([a-zA-Z_0-9]+)\s*=", body))
        if found:
            labels.setdefault(metric, set()).update(found)
    return names, labels


def metric_label_sets(names: set[str], labels: dict[str, set[str]]) -> dict[str, set[str]]:
    """把 `# HELP` 里的裸指标名也补进 labels（空标签集），供聚合校验使用。"""
    out: dict[str, set[str]] = {n: set(labels.get(n, set())) for n in names}
    out.update({k: v for k, v in labels.items() if k not in out})
    return out


def rules_exprs() -> list[tuple[str, str]]:
    """返回 [(规则名, expr)]。用 yaml 解析以正确处理多行表达式。"""
    try:
        import yaml
    except ImportError:  # pragma: no cover
        print("[check-promql] ERROR: PyYAML required", file=sys.stderr)
        sys.exit(2)
    doc = yaml.safe_load(RULES.read_text(encoding="utf-8"))
    out: list[tuple[str, str]] = []
    for group in doc.get("groups", []):
        for rule in group.get("rules", []):
            if "alert" in rule:
                out.append((rule["alert"], rule.get("expr", "")))
    return out


def strip_selectors(expr: str) -> str:
    """去掉选择器、字面量与**函数名**，只留下「指标名」序列。

    必须剥掉的部分（否则会把标签名/job 名误判成指标名）：
      - `{...}` 选择器：如 `{status="2xx"}`、`{key="stream:proxy:telemetry"}`、
        `{job="pingora-gateway"}`；
      - 字符串字面量：如 `"oops"`；
      - **函数名**（OPT-R15 修）：`time()`、`timestamp()`、`clamp()` 等任何
        `标识符(` 形式。

    函数名为什么不能靠 `FUNCS` 白名单兜底：那张表是手维护的，加函数就得同步加，
    漏一个就把函数名当成指标名 → 报"metrics.rs 没渲染该指标"的**假阳性**。
    实测踩过：`TelemetrySinkStale` 里的 `time()` 曾把门判红。
    用语法判定（`标识符` 紧跟 `(`）可一次性覆盖所有 PromQL 函数与将来新增的。
    `FUNCS` 里剩下的 `and/or/by/without/bool/offset/group_left` 是**运算符与关键字**，
    不是函数调用，仍需白名单兜底。
    """
    expr = re.sub(r"\{[^{}]*\}", " ", expr)
    expr = re.sub(r'"[^"]*"', " ", expr)
    # 函数/聚合器名：替换成空格，参数原样保留（嵌套调用靠 findall 逐个命中）。
    expr = re.sub(r"\b[a-zA-Z_][a-zA-Z_0-9:]*\s*\(", " ", expr)
    return expr


def metrics_in(expr: str, label_names: set[str]) -> set[str]:
    """表达式里引用到的指标名（过滤函数名、标签名、job 名等）。"""
    cleaned = strip_selectors(expr)
    raw = set(METRIC_RE.findall(cleaned))
    return {
        r
        for r in raw
        # 函数/关键字、数字
        if r not in FUNCS and not r.isdigit()
        # 标签名（从 metrics.rs 的标签清单汇总而来）
        and r not in label_names
    }


def main() -> int:
    if not METRICS_RS.is_file() or not RULES.is_file():
        print("[check-promql] ERROR: expected files not found", file=sys.stderr)
        return 2

    names, labels_raw = load_metric_labels()
    labels = metric_label_sets(names, labels_raw)
    # 全仓出现过的标签名（用于把表达式里的标签名从「指标名」候选里剔除）。
    label_names: set[str] = set()
    for s in labels.values():
        label_names |= s
    # Prometheus 惯用标签（不一定在 metrics.rs 渲染里，如 job/instance）。
    label_names |= {"job", "instance", "status", "provider", "worker", "source",
                    "proto", "level", "result", "exit_ip", "key", "le"}

    exprs = rules_exprs()
    if not exprs:
        print("[check-promql] no alert rules found — nothing to check")
        return 0

    problems: list[str] = []

    for alert, expr in exprs:
        # ① 指标名必须存在（内置序列、外部 exporter 序列放行）。
        for m in sorted(metrics_in(expr, label_names)):
            if m in BUILTIN_SERIES or m in EXTERNAL_SERIES:
                continue
            # Prometheus 直方图/摘要的 `_bucket` / `_sum` / `_count` 是
            # `<base>_bucket` 这类同族序列，metrics.rs 只对 base 名发 HELP。
            base = re.sub(r"_(bucket|sum|count)$", "", m)
            if m not in names and base in names:
                continue
            if m not in names:
                problems.append(
                    f"rule {alert!r}: metric {m!r} is not rendered by metrics.rs "
                    f"(typo, or metrics.rs renamed it)"
                )

        # ② 聚合标签必须存在于被聚合的每个指标上。
        #    例：sum by (provider) (rate(X)) 要求 X 带 provider 标签。
        by_labels: list[str] = []
        for grp in BY_RE.findall(expr):
            for tok in grp.split(","):
                tok = tok.strip().strip('"').strip("'")
                if tok and tok not in FUNCS and not tok.isdigit():
                    by_labels.append(tok)
        if not by_labels:
            continue

        used = metrics_in(expr, label_names)
        for metric in sorted(used):
            if metric in BUILTIN_SERIES or metric in EXTERNAL_SERIES:
                continue
            base = re.sub(r"_(bucket|sum|count)$", "", metric)
            key = metric if metric in labels else (base if base in labels else None)
            if key is None:
                continue  # ① 已报过「指标不存在」，此处不重复
            have = labels[key]
            for lab in by_labels:
                if lab in have:
                    continue
                exempt = EXEMPT_AGGREGATIONS.get((alert, metric, lab))
                if exempt:
                    print(
                        f"[check-promql] note: rule {alert!r} aggregating "
                        f"{metric!r} by {lab!r} is an acknowledged limitation "
                        f"({exempt})",
                        file=sys.stderr,
                    )
                    continue
                problems.append(
                    f"rule {alert!r}: aggregates by {lab!r} but metric "
                    f"{metric!r} only renders labels {sorted(have) or ['<none>']} "
                    f"-> expression yields an empty result set at runtime "
                    f"(promtool cannot catch this; it only type-checks)"
                )

    if not problems:
        print(
            f"[check-promql] OK: {len(exprs)} alert rule(s) consistent with "
            f"{len(names)} rendered metric(s)"
        )
        return 0

    print(
        f"[check-promql] FAIL: {len(problems)} inconsistency(ies) between "
        f"rules.yml and metrics.rs",
        file=sys.stderr,
    )
    for p in problems:
        print(f"  {p}", file=sys.stderr)
    return 1


if __name__ == "__main__":
    sys.exit(main())
