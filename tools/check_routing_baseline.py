#!/usr/bin/env python3
r"""文档里发布的选路基线表，必须等于回放台的**可复现输出**。

## 为什么需要这道门（而不是把文档塞进 check_doc_consistency.py）

`docs/ROUTING_BASELINE.md` §4 发布了一张策略对比表（轮询 / 加权 / LinUCB /
oracle 的成功率、平均奖励、可避免失败、P50/P99）。这张表是本仓**唯一**
回答「选路选得好不好」的量化材料，因此它有两个特殊风险：

1. **它不会因为代码错误而变红。** 改了 `bandit.rs` 的公式或
   `routing_replay.py` 的结果模型后，表里的数字就**过期**了，但没有任何测试
   会失败——文档仍然理直气壮地声称「LinUCB 52.2%」。这正是本仓反复踩过的
   「对外表述 vs 实际行为」类失实（见 `check_observability_truth.py` 的注记）。
2. **手抄数字必漂。** 与 `check_doc_consistency.py` 的结论同源：靠人眼同步的
   数字一定会漂，且漂了没人知道。

所以这里不做"扫描 + 断言某个孤立数字"，而是**把文档整张表与工具输出逐格比对**。

## 与 check_doc_consistency.py 的分工

- `check_doc_consistency.py` 守的是**测试数**（`QUALITY_BASELINE.md` 是权威源）；
- 本门守的是**决策质量数字**（`routing_replay.py` 的输出是权威源）。

两者都遵循同一条纪律：**权威值只能来自可执行的东西**，不能来自另一份文档。

## 比对方式为什么是"逐格"而不是"整块字符串相等"

整块比对（`doc_table == rendered_table`）会在一个人只想调整列宽时给出
"整张表都错了"的噪声。逐格比对能精确报出「`linucb` 的成功率：文档 52.2%，
实测 54.1%」，让人一眼知道该改哪一格。格式化仍然只有一份实现
（`routing_replay.render_markdown`），本门只是把它渲染出来再解析回来比对
——所以不会出现"门自己算了一套数字"的双份真相。

## 用法

```bash
python3 tools/check_routing_baseline.py
python3 tools/check_routing_baseline.py --self-test   # 变异自测
```

更新表的方法（**改完选路逻辑必须做**）：

```bash
python3 tools/routing_replay.py --source jsonl \
  --path tools/replay/sample_trace.jsonl --seed 20261027
# 把输出的表格原样替换 docs/ROUTING_BASELINE.md §4 的表格
```

## 退出码

- 0：文档表格与实测输出逐格一致
- 1：某格不一致 / 缺行 / 文档表格结构被破坏 / 自测未判红
"""

from __future__ import annotations

import argparse
import re
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
sys.path.insert(0, str(ROOT / "tools"))

# 复用回放台的实现与渲染器，**不在这里抄第二份**——否则门与被测对象各自
# 维护一套格式，两边同时漂移仍然全绿（与 check_replay_parity.py 同一条纪律）。
from routing_replay import (  # noqa: E402
    LinUCBPolicy,
    Metrics,
    OraclePolicy,
    OutcomeModel,
    Policy,
    RequestRow,
    RoundRobinPolicy,
    WeightedPolicy,
    load_jsonl,
    render_markdown,
    replay,
)

DOC = ROOT / "docs" / "ROUTING_BASELINE.md"
SAMPLE = ROOT / "tools" / "replay" / "sample_trace.jsonl"

#: §4 表格里必须出现的策略。缺任何一行都算退化：删掉 `oracle` 就等于把
#: 「离上界还差多远」这个最有价值的数字悄悄抹掉，而门若只比对"存在的行"
#: 会照样全绿。
REQUIRED_POLICIES = ("round_robin", "weighted", "linucb", "oracle")

#: 表格列名 ⇒ 比对用的键。列顺序可以变（解析按名取），但列名不能改名，
#: 否则读表的人会误解（例如把「可避免失败」写成「失败」）。
COLUMNS = (
    "策略",
    "成功",
    "成功率",
    "平均奖励",
    "可避免失败",
    "P50 时延",
    "P99 时延",
    "命中节点数",
)

#: 文档里那行样本摘要：`样本 600 条 / 4 节点 / 3 域名 / 全局成功率 29.7% / seed 20261027`
SAMPLE_RE = re.compile(
    r"样本\s*(\d+)\s*条\s*/\s*(\d+)\s*节点\s*/\s*(\d+)\s*域名\s*/\s*"
    r"结果模型全局成功率\s*([\d.]+)%\s*/\s*seed\s*(\d+)"
)


def _cells(line: str) -> list[str] | None:
    """把一行 markdown 表格拆成单元格；不是表格行则返回 None。"""
    s = line.strip()
    if not s.startswith("|") or not s.endswith("|"):
        return None
    return [c.strip() for c in s[1:-1].split("|")]


def _is_separator(cells: list[str]) -> bool:
    return all(re.fullmatch(r":?-{3,}:?", c) for c in cells if c)


def parse_table(text: str) -> dict[str, dict[str, str]]:
    """从文档里解析 §4 的策略表，返回 `{策略名: {列名: 值}}`。

    只认「表头含 `策略` 且表头含 `成功率`」的那张表，避免误抓 §1 的
    「已有验收」表或 §6 的「两侧各有一把锁」表（它们同样是 markdown 表格）。
    """
    lines = text.split("\n")
    for i, line in enumerate(lines):
        header = _cells(line)
        if not header or "策略" not in header or "成功率" not in header:
            continue
        # 表头下面必须紧跟分隔行，否则这不是真表格。
        if i + 1 >= len(lines):
            continue
        sep = _cells(lines[i + 1])
        if not sep or not _is_separator(sep):
            continue

        rows: dict[str, dict[str, str]] = {}
        for raw in lines[i + 2 :]:
            cells = _cells(raw)
            if cells is None:
                break
            if len(cells) != len(header):
                break
            record = dict(zip(header, cells))
            name = record.get("策略", "").strip("`").strip()
            if not name:
                break
            rows[name] = record
        return rows
    return {}


def parse_sample_line(text: str) -> tuple[int, int, int, float, int] | None:
    m = SAMPLE_RE.search(text)
    if not m:
        return None
    return int(m.group(1)), int(m.group(2)), int(m.group(3)), float(m.group(4)), int(m.group(5))


def build_policies(rows: list[RequestRow], model: OutcomeModel, names: list[str]) -> list[Policy]:
    """与 `routing_replay.main` 的策略构造保持一致（含 tier 的取法）。"""
    tiers: dict[str, str] = {}
    for r in rows:
        tiers[r.out_ip] = r.tier
    out: list[Policy] = []
    for name in names:
        if name == "linucb":
            out.append(LinUCBPolicy(tiers))
        elif name == "weighted":
            out.append(WeightedPolicy())
        elif name == "round_robin":
            out.append(RoundRobinPolicy())
        elif name == "oracle":
            out.append(OraclePolicy(model))
        else:
            raise SystemExit(f"[check-routing-baseline] 文档里出现未知策略：{name}")
    return out


def measure(rows: list[RequestRow], model: OutcomeModel, names: list[str], seed: int) -> dict[str, dict[str, str]]:
    """跑一遍回放，用**工具自己的渲染器**产出期望表格，再解析回结构。

    走「渲染 → 解析」而不是手写格式化，是为了让 `render_markdown` 成为
    格式的唯一实现：文档要改列名/精度时，只需改那一处，本门自动跟随。
    """
    results: list[Metrics] = []
    for policy in build_policies(rows, model, names):
        results.append(replay(rows, model, policy, seed=seed))
    md = render_markdown(results, len(rows), model)
    return parse_table(md)


def diff(
    doc: dict[str, dict[str, str]],
    got: dict[str, dict[str, str]],
) -> list[str]:
    """逐格比对，返回人类可读的差异列表。"""
    problems: list[str] = []

    missing = [p for p in REQUIRED_POLICIES if p not in doc]
    if missing:
        problems.append(
            f"§4 表格缺少这些策略行：{missing}"
            "（删行会让门静默变窄，故视为失败）"
        )

    extra = sorted(set(doc) - set(got))
    if extra:
        problems.append(f"§4 表格有回放台不认识的策略：{extra}")

    for name in REQUIRED_POLICIES:
        if name not in doc or name not in got:
            continue
        d_row, g_row = doc[name], got[name]
        for col in COLUMNS:
            if col == "策略":
                continue
            want = g_row.get(col)
            have = d_row.get(col)
            if want is None:
                # 列名被改名了：读表的人会误解，直接报出来。
                problems.append(f"`{name}` 行缺少列「{col}」（文档实际列：{list(d_row)}）")
                continue
            if have != want:
                problems.append(f"`{name}` 的「{col}」：文档 {have!r} / 实测 {want!r}")
    return problems


def check(doc_text: str, *, doc_override: dict | None = None) -> list[str]:
    rows = load_jsonl(SAMPLE)
    model = OutcomeModel(rows)

    sample = parse_sample_line(doc_text)
    problems: list[str] = []
    if sample is None:
        problems.append(
            "找不到 §4 的样本摘要行（`样本 N 条 / N 节点 / N 域名 / "
            "结果模型全局成功率 X% / seed Y`）——没有它就无法确定用什么种子复现"
        )
        return problems

    n_rows, n_nodes, n_domains, global_rate, seed = sample
    actual_nodes = len({r.out_ip for r in rows})
    actual_domains = len({r.target_domain for r in rows})
    if n_rows != len(rows):
        problems.append(f"样本条数：文档 {n_rows} / 实际 {len(rows)}")
    if n_nodes != actual_nodes:
        problems.append(f"节点数：文档 {n_nodes} / 实际 {actual_nodes}")
    if n_domains != actual_domains:
        problems.append(f"域名数：文档 {n_domains} / 实际 {actual_domains}")
    if f"{global_rate:.1f}%" != f"{model.global_rate:.1%}":
        problems.append(
            f"结果模型全局成功率：文档 {global_rate}% / 实测 {model.global_rate:.1%}"
        )

    doc_table = doc_override if doc_override is not None else parse_table(doc_text)
    if not doc_table:
        problems.append(
            "解析不到 §4 的策略表（表头需含「策略」与「成功率」两列，"
            "且表头下必须紧跟 `| --- |` 分隔行）"
        )
        return problems

    got = measure(rows, model, list(REQUIRED_POLICIES), seed)
    problems.extend(diff(doc_table, got))
    return problems


def main() -> int:
    ap = argparse.ArgumentParser()
    ap.add_argument("--doc", type=Path, default=DOC)
    ap.add_argument(
        "--self-test",
        action="store_true",
        help="变异自测：篡改文档表格的一格，必须判红（证明门有鉴别力）",
    )
    args = ap.parse_args()

    if not args.doc.is_file():
        print(f"[check-routing-baseline] FAIL: 找不到 {args.doc}", file=sys.stderr)
        return 1

    text = args.doc.read_text(encoding="utf-8")
    problems = check(text)

    if problems:
        print(
            "[check-routing-baseline] FAIL: 文档发布的选路基线表与回放台实测不一致。",
            file=sys.stderr,
        )
        for p in problems:
            print(f"  - {p}", file=sys.stderr)
        print(
            "  修法：跑 `python3 tools/routing_replay.py --source jsonl "
            "--path tools/replay/sample_trace.jsonl --seed <文档里的 seed>`，"
            "把输出的表格原样替换 docs/ROUTING_BASELINE.md §4 的表格。",
            file=sys.stderr,
        )
        return 1

    print("[check-routing-baseline] OK: §4 表格与回放台输出逐格一致。")

    if not args.self_test:
        return 0

    # 变异：把 linucb 的成功率改掉 0.1pp。任何真实漂移都应被抓到；
    # 若仍绿，说明比对链路根本没生效（门是摆设）。
    table = parse_table(text)
    if "linucb" not in table:
        print(
            "[check-routing-baseline] FAIL: 自测无法变异——表格里没有 linucb 行",
            file=sys.stderr,
        )
        return 1
    mutated = {k: dict(v) for k, v in table.items()}
    original = mutated["linucb"]["成功率"]
    num = float(original.rstrip("%"))
    mutated["linucb"]["成功率"] = f"{num + 0.1:.1f}%"
    if check(text, doc_override=mutated) == []:
        print(
            "[check-routing-baseline] FAIL: 变异自测未判红——篡改成功率 0.1pp "
            "仍通过，说明逐格比对失效",
            file=sys.stderr,
        )
        return 1
    print(
        f"[check-routing-baseline] self-test OK: linucb 成功率 "
        f"{original} → {mutated['linucb']['成功率']} 后正确判红。"
    )
    return 0


if __name__ == "__main__":
    sys.exit(main())
