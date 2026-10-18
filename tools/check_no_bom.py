#!/usr/bin/env python3
"""
CI 断言：配置类文本文件**不得**携带 UTF-8 BOM（OPT-R17）。

# 为什么需要这道门（与 check_ps1_bom.py 恰好相反）

本仓已有 `tools/check_ps1_bom.py`，它要求 `tools/*.ps1` **必须**有 BOM
（PS 5.1 读无 BOM 的 .ps1 会按 ANSI 代码页解析中文注释）。本门是它的**对偶**：
要求配置类文件**不得**有 BOM。二者方向相反、理由不同，故必须分两个文件 ——
合成一个「按扩展名决定要不要 BOM」的门，会让规则变得不可读。

# 曾经的真实问题（OPT-R17 发现）

`gateway/Cargo.toml` 带 UTF-8 BOM（EF BB BF），导致 CI 的 supply-chain job
输出：

    Error parsing Cargo.toml manifest, fallback to caching entire file:
    Error: Invalid TOML document: only letter, numbers, dashes and underscores
    are allowed in keys
    1:  [package]

`Swatinem/rust-cache` 用 TOML 解析器读 manifest，解析失败 ⇒ **回退到「缓存
整个文件」**，丢掉了按 `Cargo.lock` 哈希做细粒度 key 的能力（本仓 ci.yml 的
OPT-R9 A4 正是为此引入 rust-cache）。即：**以为开了缓存提速，实际退化成了
整文件缓存。**

**成因不是 git，而是编辑器/PowerShell**：git 会剥离 UTF-8 BOM
（`.gitattributes` 的 OPT-R7 D1 段已实测记录该行为无法用 attribute 改变），
所以 BOM 只能从工作区侧被重新写回（IDE 选「UTF-8 with BOM」、
`Set-Content -Encoding UTF8` in PS 5.1 等）。CI 只拿到工作区快照，
故必须在提交前断言。

# 为什么不把 BOM 断言并进 check_ps1_bom.py

那个文件的**全部**逻辑都围绕「必须有 BOM」组织（函数名、常量、报错文案）。
加一个「必须没有 BOM」的反向分支进去，会让同一个文件同时表达两条方向相反
的规则，读者需要反复确认当前走的是哪条。分开更安全。

# 覆盖范围

只锁**本仓的**配置类文件（清单固定，不用 glob 自动发现）。理由：自动 glob 会
把 `log/` 下的一次性产物、第三方 vendor 文件一并卷进来，那些文件本项目不负责。

刻意**不锁**：
- `tools/*.ps1` —— 那批文件**要** BOM（见 `check_ps1_bom.py`）
- `.gitattributes` / `.gitignore` —— git 自身的配置文件，内容是 ASCII 规则，
  实践中不会被 BOM 化；若将来真出问题，git 会有更响的报错
- 任何 `.md` —— Markdown 渲染器普遍容忍 BOM，且文档常有中文

# 用法

    python tools/check_no_bom.py

# 退出码

- 0：全部无 BOM
- 1：某文件带 BOM（打印文件路径）

# 双向验证记录（OPT-R17）

- 给 `gateway/Cargo.toml` 加回 BOM → 门红并指出该文件
- 还原（剥离 BOM）→ 门绿
"""

from __future__ import annotations

import sys
from pathlib import Path

REPO = Path(__file__).resolve().parent.parent

# UTF-8 BOM 字节序列。
UTF8_BOM = b"\xef\xbb\xbf"

# 必须**无** BOM 的文件清单。
#
# 选它们的共同理由：都是被外部工具以严格解析器读取的配置，解析器对
# BOM 零容忍（或会退化）——TOML/YAML/JSON 规范都把 BOM 视为可选前置，
# 但**实现**层面常常不容忍（rust-cache 的 TOML 解析器就是实证）。
NO_BOM_FILES = [
    REPO / "gateway" / "Cargo.toml",
    REPO / "gateway" / "deny.toml",
    REPO / "docker-compose.yml",
    REPO / "deploy" / "alertmanager.yml",
    REPO / "deploy" / "prometheus" / "prometheus.yml",
    REPO / "deploy" / "prometheus" / "rules.yml",
    REPO / "deploy" / "grafana" / "dashboard.json",
]


def has_bom(path: Path) -> bool:
    """前 3 字节是否为 UTF-8 BOM（文件短于 3 字节视为无 BOM）。"""
    try:
        with path.open("rb") as fh:
            return fh.read(3) == UTF8_BOM
    except OSError as exc:  # pragma: no cover - 读失败按无 BOM 报，附原因
        print(f"[check-no-bom] ERROR: cannot read {path}: {exc}", file=sys.stderr)
        return False


def main() -> int:
    failures: list[str] = []
    checked = 0

    for p in NO_BOM_FILES:
        if not p.is_file():
            print(f"[check-no-bom] ERROR: missing file {p}", file=sys.stderr)
            return 1
        checked += 1
        if has_bom(p):
            rel = p.relative_to(REPO).as_posix()
            failures.append(
                f"{rel} 携带 UTF-8 BOM（EF BB BF）。"
                f"严格解析器会拒绝或退化（实测：Swatinem/rust-cache 因 BOM 解析"
                f"Cargo.toml 失败，回退到整文件缓存，丢掉按 Cargo.lock 哈希的"
                f"细粒度 key）。用编辑器存为「UTF-8 无 BOM」即可修复。"
            )

    if failures:
        print("[check-no-bom] FAIL:", file=sys.stderr)
        for f in failures:
            print(f"  - {f}", file=sys.stderr)
        return 1

    print(
        f"[check-no-bom] OK: {checked} 个配置文件均无 UTF-8 BOM"
    )
    return 0


if __name__ == "__main__":
    sys.exit(main())
