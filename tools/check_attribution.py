#!/usr/bin/env python3
"""
CI 断言：第三方**数据**署名必须完整存在（OPT-R10 A1）。

# 为何需要这道门（这是唯一有法律时效的检查）

GeoLite2 数据库采用 **CC BY-SA 4.0**。该许可 §3(a) **强制要求**署名，且署名
义务随「使用方实际使用该数据库」而发生——不是随代码分发。

本仓的实情：GeoLite2 数据库**不在仓库内**（需 MaxMind 账号 + license key 自取），
所以**部署方**才是署名的义务人。本仓的责任是**提供一份可复制、且不会被误删的
署名文本**，并在文档中指明「何时适用、怎么提供」。

# 曾经的真实缺口（OPT-R10 发现）

`docs/OPEN-SOURCE.md` 明确写「使用需署名（OPERATION 有署名行）」，但
`docs/OPERATION.md` 当时**只有 GeoLite2 的操作步骤、没有任何署名**——
文档在**两处**声称已履行，实际未履行。这不是笔误级别的问题：它是**对外的
不实陈述**＋**合规缺口**。

故本门断言 `OPERATION.md` 里存在完整署名，且四要素齐全（CC BY-SA 4.0 §3(a)）：

1. 提供者署名（`Copyright © MaxMind`）
2. 许可名称与链接（`CC BY-SA 4.0` + creativecommons.org 链接）
3. 免责声明（`AS IS` / 无担保）
4. 数据来源（`maxmind.com`）

# 用法

    python tools/check_attribution.py

# 退出码

- 0：署名完整
- 1：缺失任一要素（打印缺哪些、在哪个文件）
"""

import sys
from pathlib import Path

REPO = Path(__file__).resolve().parent.parent
OPERATION = REPO / "docs" / "OPERATION.md"
README = REPO / "README.md"
OPEN_SOURCE = REPO / "docs" / "OPEN-SOURCE.md"

# 署名四要素：键 → (必需子串, 说明)
REQUIRED_ELEMENTS: dict[str, tuple[str, str]] = {
    "提供者署名": ("Copyright © MaxMind", "MaxMind as the data provider"),
    "许可名称": ("CC BY-SA 4.0", "the license name"),
    "许可链接": ("creativecommons.org/licenses/by-sa/4.0", "the license URL"),
    "免责声明": ("AS IS", "the warranty disclaimer"),
    "数据来源": ("maxmind.com", "the data source"),
}


def main() -> int:
    if not OPERATION.is_file():
        print(f"[check-attribution] ERROR: {OPERATION} not found", file=sys.stderr)
        return 1

    text = OPERATION.read_text(encoding="utf-8")
    missing: list[str] = []
    for name, (needle, why) in REQUIRED_ELEMENTS.items():
        if needle not in text:
            missing.append(f"  {name} ({why}) — 缺: {needle!r}")

    if missing:
        print(
            "[check-attribution] FAIL: docs/OPERATION.md 的 GeoLite2 署名不完整。\n"
            "GeoLite2 采用 CC BY-SA 4.0，§3(a) 强制要求署名四要素。\n"
            "缺失要素：",
            file=sys.stderr,
        )
        print("\n".join(missing), file=sys.stderr)
        return 1

    # 交叉引用一致性：OPEN-SOURCE 声称「OPERATION 有署名行」，该陈述现在才成立。
    # 若有人删掉 OPERATION 的署名节而 OPEN-SOURCE 仍这么写，就是不实陈述。
    os_text = OPEN_SOURCE.read_text(encoding="utf-8") if OPEN_SOURCE.is_file() else ""
    if "OPERATION" in os_text and "署名" in os_text:
        if "第三方数据署名" not in os_text:
            print(
                "[check-attribution] FAIL: docs/OPEN-SOURCE.md 指向 OPERATION 的署名，"
                "但未指向具体章节（应链接 OPERATION.md#第三方数据署名）。",
                file=sys.stderr,
            )
            return 1

    # README 也应有一行指向——只在看门面的人也能找到署名要求。
    rd_text = README.read_text(encoding="utf-8") if README.is_file() else ""
    if "OPERATION" not in rd_text:
        print(
            "[check-attribution] note: README 未指向 OPERATION.md。"
            "建议加一行，让只看门面的人也能发现署名义务。",
            file=sys.stderr,
        )

    print(
        "[check-attribution] OK: GeoLite2 署名四要素齐全 "
        f"({len(REQUIRED_ELEMENTS)}/{len(REQUIRED_ELEMENTS)} in docs/OPERATION.md)"
    )
    return 0


if __name__ == "__main__":
    sys.exit(main())
