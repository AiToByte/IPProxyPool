#!/usr/bin/env python3
"""
CI 断言：活文档里的测试数字必须与权威基线一致（OPT-R10 D1）。

# 为何需要这道门（不是"保持整洁"，是防对外失实）

测试数字散落在 10+ 个文件里，每轮迭代手改一遍，漂移必然发生。实测漂移过：

- `docs/FEATURES.md` 停在 **209** 单测（OPT-R5 当时的值），而实际已 237 ——
  落后 5 轮；
- `README.md` 停在 **201**（OPT-R4 当时的值），落后 7 轮；
- `CHANGELOG.md` 里 OPT-R4-C 条目写着 **201** —— 那个是**对的**，因为它是
  步骤 26 当时的史实。

最后一条是关键：**同一批数字里，有的是"当前值"（该更新），有的是"历史值"
（改了就是伪造记录）**。人肉分辨必然出错——要么漏改活文档，要么误改史实。

本门用**白名单**把两类文件物理隔开：

- **活文档**（ALLOWLIST 之外）⇒ 数字必须等于 `docs/QUALITY_BASELINE.md`；
- **历史文件**（HISTORICAL）⇒ **一个字节都不许门禁碰**，因为它们记录的是
  某轮收官时的真实状态。

# 权威来源

`docs/QUALITY_BASELINE.md` 表格里的「单元测试通过数 / 忽略测试数」。本门
解析该表格，再扫活文档比对。

# 用法

    python tools/check_doc_consistency.py

# 退出码

- 0：活文档与基线一致
- 1：某个活文档写了与基线不同的数字（打印文件、行号、双方数值）
"""

import re
import sys
from pathlib import Path

REPO = Path(__file__).resolve().parent.parent
BASELINE = REPO / "docs" / "QUALITY_BASELINE.md"

# 活文档：这些文件对读者声称"当前质量水平"，数字必须准。
ALLOWLIST = [
    "README.md",
    "docs/FEATURES.md",
    "docs/ARCHITECTURE.md",
    "docs/USAGE.md",
    "docs/DATAFLOW.md",
    "docs/QUALITY_BASELINE.md",
    # 能力复核报告：§6 规模指标表声明"以 QUALITY_BASELINE 为准"，
    # 故必须被门禁守住，否则该表会静默漂移。
    "docs/CAPABILITY-AUDIT.md",
    "docs/SUPPLY_CHAIN.md",
    # 选路基线：它的**主要数字**（策略成功率等）不归本门管，由
    # `tools/check_routing_baseline.py` 逐格比对回放台输出。登记到本门的
    # 唯一目的是：若有人在这份文档里顺手写「338 单测」，那个数字也得准。
    # 两份门各守一类数字，互不重叠（详见该文档 §1 与 §6）。
    "docs/ROUTING_BASELINE.md",
    # 组件表：模块集合与每行单测数由 `check_components_docs.py` 守。登记到本门
    # 是为了守住"顺手写下的总数"——该文件此前写着「156 通过＋4 ignored」，
    # 而基线早已是 338/8，且它不在任何门禁范围内，于是自由漂移了十几轮。
    "docs/COMPONENTS.md",
    # 技术说明：§7 门禁定义里写着单测总数。此前它**不在任何门禁范围内**，
    # 于是中英两版分别漂到 338 与 156（差 182 条）而无人察觉——一份文档自己
    # 跟自己矛盾了两百条测试。登记后两处口径都归本门守。
    "docs/TECHNICAL.md",
    # 覆盖率基线文档：它的**覆盖率数字**由 `tools/check_coverage_baseline.py`
    # 逐字比对 `docs/coverage_baseline.json` 守着（文档↔JSON 双向）。
    # 登记到本门的唯一目的与上面几个文件同类——守住"顺手写下的总数"
    # （例如有人在 §4 里写「单测 355 个」，那个数字也得准）。
    "docs/COVERAGE_BASELINE.md",
]

# 历史文件：记录的是**某轮收官时的真实状态**。
#
# 门禁**不扫描**这些文件，也不该扫描——OPT-R4 收官时确实是 201 单测，把它
# 改成 237 等于伪造执行记录。这些文件的正确性由"append-only 纪律"保证，
# 而非由本门保证。
HISTORICAL_NOTE = (
    "CHANGELOG.md / TASK_PLAN.md / plan/*.md / EXEC_LOG.md 的历史数字"
    "是史实（如 OPT-R4 收官确为 201），门禁不扫描、也不得改写"
)

# 匹配「N 单测」「N 真 live」「N unit tests」「N live」这类断言。
COUNT_RE = re.compile(r"(\d{3})\s*(单测|真\s*live|真live|unit\s+tests?\b|live\b)")

# 各活文档允许出现的口径：哪些键必须与基线一致。
#   tests  => 「N 单测」/「N unit tests」   须等于 baseline.tests
#   live   => 「N 真 live」/「N live」     须等于 baseline.live
EXPECTED_KEYS = {
    "README.md": {"tests", "live"},
    "docs/FEATURES.md": {"tests", "live"},
    "docs/ARCHITECTURE.md": {"tests", "live"},
    "docs/USAGE.md": {"tests"},
    "docs/DATAFLOW.md": {"tests"},
    "docs/QUALITY_BASELINE.md": {"tests", "live"},
    # 必须与 ALLOWLIST 同时登记：只加 ALLOWLIST 而不加本表，等于"被扫描但
    # 不做任何断言"，会给出虚假的安全感（曾实测：把该文件改成 999 单测，
    # 门禁仍 exit=0）。§5 取证纪律那节直接以「284 单测」作反面教材，
    # 该数字若漂移，本文件自身就失去说服力。
    "docs/CAPABILITY-AUDIT.md": {"tests"},
    "docs/SUPPLY_CHAIN.md": {"tests"},
    # 只声明 `tests`：本文件不声称 live 数。它的策略成功率/时延由
    # `check_routing_baseline.py` 负责，本门不重复断言（也断言不了——
    # 那需要跑回放）。
    "docs/ROUTING_BASELINE.md": {"tests"},
    # 组件表按模块列单测数（1~2 位），不声称总量与 live 数；只守 tests 口径。
    "docs/COMPONENTS.md": {"tests"},
    # 只声明 `tests`：§7 的中英两处都写单测总数，但不写「N 真 live」
    # （那 4 条是 `--ignored` 手工跑的，本门不重复断言）。
    "docs/TECHNICAL.md": {"tests"},
    # 只声明 `tests`：本文件不声称 live 数，也不出现「N 单测」以外的
    # 数字断言（覆盖率数字由 check_coverage_baseline.py 负责）。
    "docs/COVERAGE_BASELINE.md": {"tests"},
}


def parse_baseline() -> dict[str, int]:
    """从 docs/QUALITY_BASELINE.md 解析权威数字。"""
    if not BASELINE.is_file():
        raise SystemExit(f"[check-doc-consistency] ERROR: 缺权威基线 {BASELINE}")
    text = BASELINE.read_text(encoding="utf-8")

    def grab(label: str) -> int:
        m = re.search(rf"\|\s*{label}\s*\|\s*\*\*(\d+)\*\*", text)
        if not m:
            raise SystemExit(
                f"[check-doc-consistency] ERROR: 基线表格缺「{label}」行，"
                "或数字没加 ** 粗体。请修 docs/QUALITY_BASELINE.md。"
            )
        return int(m.group(1))

    return {
        "tests": grab("单元测试通过数"),
        "live": grab("其中：真 live 测试"),
        # 「忽略测试数」本门不比对（活文档不声称它），但
        # `check_components_docs.py` 需要它来断言「按模块统计的单测之和 ==
        # 通过数 + 忽略数」。放在这里是为了让**解析基线这件事只有一份实现**。
        "ignored": grab("忽略测试数"),
    }


def main() -> int:
    baseline = parse_baseline()
    failures: list[str] = []
    checked: list[str] = []

    # 两表必须同步：ALLOWLIST 里登记了 EXPECTED_KEYS 没有的条目 ⇒ 该文件被
    # 扫描却不做任何断言，等于虚假安全感（实测：把 CAPABILITY-AUDIT 的
    # 「284 单测」改成 999，门禁仍 exit=0）。这是结构性坑，靠人记不住，
    # 故在此硬断言。
    unasserted = sorted(set(ALLOWLIST) - set(EXPECTED_KEYS))
    if unasserted:
        print(
            "[check-doc-consistency] FAIL: ALLOWLIST 与 EXPECTED_KEYS 不同步。\n"
            f"这些文件被扫描但不做任何数字断言：{unasserted}\n"
            "修法：给它们补上 EXPECTED_KEYS 条目（哪怕只声明实际用到的口径），"
            "或从 ALLOWLIST 移除。",
            file=sys.stderr,
        )
        return 1

    for rel in ALLOWLIST:
        path = REPO / rel
        if not path.is_file():
            continue

        keys = EXPECTED_KEYS.get(rel, set())
        for lineno, line in enumerate(path.read_text(encoding="utf-8").split("\n"), 1):
            for num, unit in COUNT_RE.findall(line):
                if unit.startswith("单测") or unit.startswith("unit"):
                    key = "tests"
                else:
                    key = "live"

                # 该文件不声称这个口径 ⇒ 不管（例如 docs/USAGE.md 提 live 数）。
                if key not in keys:
                    continue

                want = baseline[key]
                if int(num) != want:
                    failures.append(
                        f"  {rel}:{lineno}: 写着 {num}，但基线是 {want}（{key}）\n"
                        f"      行内容：{line.strip()[:120]}"
                    )
        checked.append(rel)

    if failures:
        print(
            "[check-doc-consistency] FAIL: 活文档的测试数字与权威基线不一致。\n"
            f"权威基线：{BASELINE.relative_to(REPO)} — tests={baseline['tests']}, live={baseline['live']}\n"
            "修法：跑 `cargo test --locked --workspace` 取实测值，"
            "改 docs/QUALITY_BASELINE.md，再同步活文档。\n"
            f"注意：{HISTORICAL_NOTE}。\n问题：",
            file=sys.stderr,
        )
        print("\n".join(failures), file=sys.stderr)
        return 1

    print(
        f"[check-doc-consistency] OK: {len(checked)} 个活文档与基线一致"
        f"（tests={baseline['tests']}, live={baseline['live']}）。"
        f"历史文件不扫描——{HISTORICAL_NOTE}。"
    )
    return 0


if __name__ == "__main__":
    sys.exit(main())
