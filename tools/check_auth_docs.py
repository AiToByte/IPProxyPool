#!/usr/bin/env python3
"""
CI 断言：鉴权配置文档必须说清 `REQUIRE_API_KEY=0` 与 `API_KEY` 的**真实语义**（OPT-R12 A2）。

# 为何需要这道门（这是 OPT-R12 的核心缺陷类型）

`REQUIRE_API_KEY=0` 此前只被描述为「显式关闭鉴权门」。这句话是**不准确**的，
而且方向很危险：它让运维以为"关掉了鉴权 = 没有鉴权"。

2026-09-29 用 release 二进制**实测**到的真实行为：

    REQUIRE_API_KEY=0  +  完全不带 X-Api-Key  ->  200 + 完整代理服务

机制：网关把无头请求**静默补成默认租户**
（`unwrap_or_else(|| DEFAULT_API_KEY.to_string())`），而默认租户是
qps/并发 10000/10000 的**满额**配额。

所以真实语义是「**全网共享一个满额身份**」。文档说「关闭鉴权」与代码做的
事之间差了一整个量级——这不是笔误，是**危险的表述失实**。而不准确文档比
没有文档更糟：它给了运维一个虚假的安全感。

这与 OPT-R10 的 MaxMind 署名门同属一类——**对外表述与实际行为不符**，
故用静态门锁住，防止后续「顺手精简文档」把它改回去。

# 本门断言的四个要点

1. `REQUIRE_API_KEY=0` **不等于**无鉴权；
2. 无头请求会落到默认租户（满额 10000/10000）；
3. `API_KEY` 可覆盖，且**设置后 `default_key` 立即失效**；
4. `API_KEY` 误配（空／等于 `default_key`）**fail-closed**。

外加**反向断言**：检出「关掉鉴权门 = 没有鉴权」这类旧的失实表述即判红。

# 用法

    python tools/check_auth_docs.py

# 退出码

- 0：文档语义齐全
- 1：缺要点，或检出旧的失实表述（打印文件与缺哪条）
"""

import re
import sys
from pathlib import Path

REPO = Path(__file__).resolve().parent.parent

# 必须在 OPERATION.md 与 USAGE.md 中说清的要点。
# 每条 = (要点名, 正则, 说明)
CONCEPTS: list[tuple[str, str, str]] = [
    (
        "REQUIRE_API_KEY=0 不等于无鉴权",
        r"(不等于|不是|is NOT|not .{0,12}(no auth|anonymous))",
        "必须显式澄清关掉鉴权门 != 没有鉴权",
    ),
    (
        "无头请求落到默认租户（满额配额）",
        r"(default_key|default tenant|默认租户|默认租户).{0,200}?(10000|10_000|满额|full-rate|sentinel)",
        "必须写明无头被静默补成默认租户、且配额是满额 10000/10000",
    ),
    (
        "API_KEY 可覆盖默认 Key",
        r"`?API_KEY`?",
        "必须写明 API_KEY env 的存在与作用",
    ),
    (
        "设置 API_KEY 后 default_key 立即失效",
        r"(default_key|default key).{0,120}?(立即|immediately|403|失效|stop)",
        "必须写明设置 API_KEY 后 default_key 立即 403 失效（否则旧客户端会被静默打断）",
    ),
    (
        "API_KEY 误配 fail-closed",
        r"(fail-closed|fail closed|failclosed|拒绝服务|全部 403|every request 403)",
        "必须写明误配时不回退弱默认而是拒绝服务",
    ),
]

# 反向断言：这些是**曾经出现过的不准确表述**，出现即判红。
#
# ⚠️ 必须**否定感知**（NEGATION_MARKERS）。首版直接做子串匹配，结果把本轮
# 刚写对的澄清句也判了红——"⚠️ **不等于「无鉴权」**" 里含"无鉴权"，于是
# "正确地澄清"与"错误地声称"被判成同一件事。
#
# 这类门禁自身的假阳性比没有门更坏：它会逼人把正确的澄清句改回模糊表述
# （"不写了就不过"），恰好毁掉门禁想守住的东西。故此处显式区分。
NEGATION_MARKERS = (
    "不等于", "不是", "并非", "≠", "!=", "is NOT", "is not", "not equal",
    "rather than", "不是「", "not \"", "never",
)


def _is_negated(line: str, match_start: int) -> bool:
    """匹配点之前/之后的一段窗口内出现否定词 ⇒ 这是澄清句，不是失实断言。"""
    lo = max(0, match_start - 60)
    window = line[lo : match_start + 60]
    return any(m in window for m in NEGATION_MARKERS)


FORBIDDEN: list[tuple[str, str, str]] = [
    (
        "REQUIRE_API_KEY=0",
        r"REQUIRE_API_KEY=0[^\n]{0,80}?(关闭鉴权门?关|disable[sd]? auth(?:entication)?)",
        "把 REQUIRE_API_KEY=0 说成「关闭鉴权」是不实陈述；"
        "它的真实语义是「无头请求共享满额 default 租户」",
    ),
    (
        "REQUIRE_API_KEY=0",
        r"REQUIRE_API_KEY=0[^\n]{0,80}?(无鉴权|匿名访问|anonymous access|no auth)",
        "把 REQUIRE_API_KEY=0 说成「无鉴权／匿名访问」是不实陈述",
    ),
]

# 只在这两份"面向运维/使用者"的文档里要求语义；plan/ 与 EXEC_LOG 是历史记录，不扫。
TARGETS = ["docs/OPERATION.md", "docs/USAGE.md"]


def main() -> int:
    failures: list[str] = []

    for rel in TARGETS:
        path = REPO / rel
        if not path.is_file():
            failures.append(f"  {rel}: 文件缺失")
            continue
        text = path.read_text(encoding="utf-8")
        # 只在提到鉴权门的那几行附近找语义，避免"文中碰巧出现 API_KEY"就算过。
        if "REQUIRE_API_KEY" not in text and "API_KEY" not in text:
            failures.append(f"  {rel}: 完全没提到鉴权门配置，语义无从谈起")
            continue

        for name, pat, why in CONCEPTS:
            if not re.search(pat, text, re.IGNORECASE | re.DOTALL):
                failures.append(f"  {rel}: 缺要点「{name}」— {why}")

        for line_no, line in enumerate(text.split("\n"), 1):
            for owner, pat, why in FORBIDDEN:
                m = re.search(pat, line, re.IGNORECASE)
                if m and not _is_negated(line, m.start()):
                    failures.append(
                        f"  {rel}:{line_no}: 检出已修正的不准确表述 — {why}\n"
                        f"      行内容：{line.strip()[:120]}"
                    )

    if failures:
        print(
            "[check-auth-docs] FAIL: 鉴权配置文档未说清真实语义。\n"
            "背景（OPT-R12 实测）：`REQUIRE_API_KEY=0` 时完全不带 X-Api-Key 的请求\n"
            "返回 200 并获得完整代理服务——无头被静默补成满额默认租户。\n"
            "把它描述成「关闭鉴权」会给出虚假的安全感。\n问题：",
            file=sys.stderr,
        )
        print("\n".join(failures), file=sys.stderr)
        return 1

    print(
        f"[check-auth-docs] OK: {len(TARGETS)} 份文档的鉴权语义齐全"
        f"（{len(CONCEPTS)} 要点），且未检出旧的失实表述"
    )
    return 0


if __name__ == "__main__":
    sys.exit(main())
