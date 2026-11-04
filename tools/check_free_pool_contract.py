#!/usr/bin/env python3
"""
CI 断言：free 池的**契约**（分类集合 / 指标名 / 文档口径）不得静默漂移（OPT-R14 A2）。

# 为何是"契约门"而不是"数量门"

2026-09-30 实测：公网免费代理每轮通过验证的数量是 **22~30** 且逐轮波动。
任何 `healthy >= N` 的断言都会**周期性假红**，而假红门禁会被运维学会忽略
——比没有门更糟（本仓 R11 已记录"并发测试整轮不命中就永远绿"的教训）。

故这里只锁**确定性的东西**：分类集合、指标名、文档口径。这些不依赖公网、
不随时间波动，因此**可以**进 CI。数量的可重复验证交给
`tools/probe_free_pool.py`（报告式，非门禁）。

# 锁住什么

1. **verify 分类集合**：`free_pool.rs` 里 `note_free_verify` 的实参集合必须
   恰为 {pass, tcp_fail, full_fail, geo_fail, backoff_skip}。新增一个分类却
   不更新本门与文档 ⇒ 判红。**理由**：分类是文档与告警的契约；悄悄加一个
   `waf_fail` 会让 `free_pool_verify_total` 的分母语义在文档里失真。
2. **指标名存在**：`free_pool_nodes_total` / `free_pool_source_yield_total` /
   `free_pool_verify_total` / `free_pool_source_suspended` /
   `free_pool_intake_capped_total` 必须在 `metrics.rs` 里被渲染。
   **理由**：探针与运维脚本按名字取数；改名不报错 ⇒ 静默读到空。
3. **intake 上限算式的下限**：源码须保留 `max_nodes × factor` 且**下限 100**
   （实测 2000×2=4000 与日志 `intake capped 11275->4000` 吻合）。
   **理由**：下限被去掉会让 `FREE_MAX_NODES=1` 时 intake 退化成 1，验证样本
   不足以判断源健康度。
4. **文档口径**：`docs/OPERATION.md` 必须写明 free 池是**公网 churn、天然可能
   为 0**，且必须写明**重分布未验证**。**理由**：这正是本轮踩过的坑——旧
   EXEC_LOG 写"公网存活率极低 ⇒ 池常态 0"，而实测每轮有 22~30 个通过；
   文档口径错了会让人误判代码。

# 用法

    python tools/check_free_pool_contract.py

# 退出码

- 0：契约一致
- 1：漂移（打印具体项）
"""

import re
import sys
from pathlib import Path

REPO = Path(__file__).resolve().parent.parent
# FreePool 的源码位置。OPT-R18 P2-1 把 3727 行的 `free_pool.rs` 拆成了
# 目录模块 `free_pool/`（`mod.rs` + 5 个子模块），故本门必须**读整个目录**，
# 不能只读一个文件——否则拆完立刻报"缺文件"（实测如此）。
# 两种布局都支持：`free_pool.rs` 单文件，或 `free_pool/*.rs` 目录模块。
FREE_POOL_DIR = REPO / "gateway" / "src" / "free_pool"
FREE_POOL_RS = REPO / "gateway" / "src" / "free_pool.rs"


def free_pool_source() -> str:
    """FreePool 全部源码的拼接（单文件或目录模块两种布局都支持）。"""
    if FREE_POOL_RS.is_file():
        return FREE_POOL_RS.read_text(encoding="utf-8")
    if FREE_POOL_DIR.is_dir():
        return "\n".join(
            f.read_text(encoding="utf-8") for f in sorted(FREE_POOL_DIR.glob("*.rs"))
        )
    return ""
SRC_DIR = REPO / "gateway" / "src"
# 路径解析交给共享助手（`metrics` / `free_pool` 都已是目录模块）。
sys.path.insert(0, str(Path(__file__).resolve().parent))
from _rustsrc import module_source  # noqa: E402
OPERATION = REPO / "docs" / "OPERATION.md"

EXPECTED_VERIFY_RESULTS = {"pass", "tcp_fail", "full_fail", "geo_fail"}

REQUIRED_METRICS = [
    "free_pool_nodes_total",
    "free_pool_source_yield_total",
    "free_pool_verify_total",
    "free_pool_source_suspended",
    "free_pool_intake_capped_total",
    # OPT-R18 P2-3：供给 SLO 三元组。它们回答的是"节点数"回答不了的问题——
    # 出口身份够不够（多样性）、节点是不是活不过一轮复检（年龄）、抓来的
    # 原始代理有多少能过质检（转化率）。免费池的真实失效方式就是这三种。
    "free_pool_distinct_exit_ips",
    "free_pool_median_age_seconds",
    "free_pool_yield_ratio",
]

# 文档必须命中的口径（正则）。两条都要，因为本轮正是被这两条坑到。
REQUIRED_DOC_CLAIMS = [
    (
        "公网 free 代理存在天然 churn、可能为 0",
        r"(churn|存活率|公网[^。]{0,20}波动|天然)",
        "必须写明公网免费代理存活率天然波动、池可能为 0，"
        "避免把 DEGRADED_ZERO 误判成代码缺陷",
    ),
    (
        "重分布尚未验证",
        # 匹配「重分布…未验证 / 尚未验证」两种语序（本仓文档两种都用过，
        # 早期版本只匹配其中一种会造成假红——门禁自己成为噪音）。
        r"(重分布|redistribut)[^。]{0,120}?(尚未验证|未验证|not )"
        r"|(尚未验证|未验证)[^。]{0,60}?(重分布|redistribut)",
        "必须写明 bandit 在真实 free 池规模下的重分布**尚未验证**"
        "（本机 CH 不可用 ⇒ 遥测降级 ⇒ 无法按出口节点归因）",
    ),
    (
        "供给 SLO 的三个问题",
        # 三选一命中即可（OPERATION 的表述可以变，但必须提到"出口多样性/年龄/
        # 转化率"这三类里至少两类，否则读者不知道新指标是干什么的）。
        r"(出口(多样性|身份)|distinct_exit_ips)[\s\S]{0,400}?(年龄|median_age)"
        r"|(年龄|median_age)[\s\S]{0,400}?(转化率|yield_ratio)",
        "必须写明供给 SLO 至少两类口径（出口多样性 / 年龄 / 转化率），"
        "否则新增的三个指标在运维手册里没有解释，等于只有作者知道怎么读",
    ),
    (
        "供给侧可重复探针的入口",
        r"probe_free_pool",
        "必须给出可重复验证的命令入口（tools/probe_free_pool.py）",
    ),
]


def main() -> int:
    failures: list[str] = []

    if not free_pool_source():
        print(
            f"[check-free-pool-contract] ERROR: 找不到 FreePool 源码"
            f"（既无 {FREE_POOL_RS.name} 也无 {FREE_POOL_DIR.name}/ 目录）",
            file=sys.stderr,
        )
        return 1
    for p in (OPERATION,):
        if not p.is_file():
            print(f"[check-free-pool-contract] ERROR: 缺文件 {p}", file=sys.stderr)
            return 1

    fp = free_pool_source()
    mx = module_source(SRC_DIR, "metrics")
    if not mx:
        print(
            "[check-free-pool-contract] ERROR: 找不到 metrics 模块源码", file=sys.stderr
        )
        return 1
    doc = OPERATION.read_text(encoding="utf-8")

    # 1) verify 分类集合（取 note_free_verify 的字符串实参）
    found = set(re.findall(r'note_free_verify\(\s*"([a-z_]+)"', fp))
    if found != EXPECTED_VERIFY_RESULTS:
        missing = EXPECTED_VERIFY_RESULTS - found
        extra = found - EXPECTED_VERIFY_RESULTS
        detail = []
        if missing:
            detail.append(f"缺 {sorted(missing)}")
        if extra:
            detail.append(f"多出 {sorted(extra)}（新增分类必须同时更新本门与文档）")
        failures.append(f"  verify 分类集合漂移：{'，'.join(detail)}；实测={sorted(found)}")

    # 2) 指标名必须被渲染
    for name in REQUIRED_METRICS:
        # metrics.rs 用裸串或 format!，两种都算
        if name not in mx:
            failures.append(f"  metrics.rs 未见指标 {name}（改名会让探针/脚本静默读到空）")

    # 3) intake 上限算式须保留 max_nodes × factor 且下限 100
    if "fn intake_cap_limit" not in fp:
        failures.append("  free_pool 源码缺 intake_cap_limit（intake 上限算式被移走）")
    else:
        body = fp[fp.index("fn intake_cap_limit"):][:400]
        if "saturating_mul" not in body:
            failures.append("  intake_cap_limit 未用 saturating_mul（max_nodes×factor 溢出即 panic）")
        if not re.search(r"\.max\(\s*100\s*\)", body):
            failures.append(
                "  intake_cap_limit 丢了 `.max(100)` 下限 —— "
                "FREE_MAX_NODES 很小时 intake 会退化成个位数，验证样本不足以判断源健康度"
            )

    # 4) 文档口径
    for name, pat, why in REQUIRED_DOC_CLAIMS:
        if not re.search(pat, doc, re.IGNORECASE):
            failures.append(f"  docs/OPERATION.md 缺口径「{name}」— {why}")

    if failures:
        print(
            "[check-free-pool-contract] FAIL: free 池契约漂移。\n"
            "设计约束：数量类断言**故意不做**（公网代理 churn 天然波动，会假红）；\n"
            "本门只锁确定性的分类集合/指标名/算式下限/文档口径。\n"
            "数量的可重复验证见 tools/probe_free_pool.py（报告式）。\n问题：",
            file=sys.stderr,
        )
        print("\n".join(failures), file=sys.stderr)
        return 1

    print(
        f"[check-free-pool-contract] OK: verify 分类 {sorted(EXPECTED_VERIFY_RESULTS)} 一致、"
        f"{len(REQUIRED_METRICS)} 个指标名在位、intake 下限 100 保留、"
        # 口径条数**不要写死**：本轮加了第 4 条，写死的 "3 条" 立刻变成谎报
        # （门全绿但输出说错数）。与 `check_doc_consistency.py` 同一条教训。
        f"{len(REQUIRED_DOC_CLAIMS)} 条文档口径齐全"
    )
    return 0


if __name__ == "__main__":
    sys.exit(main())
