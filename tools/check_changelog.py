#!/usr/bin/env python3
"""
OPT-R16 D：CHANGELOG 与 Cargo.toml 版本一致性门。

用途：版本号 bump 时必须同步加 CHANGELOG 小节，否则版本号与发布记录脱节，
几个月后没人知道 0.2.0 里到底装了什么。

# 规则（确定性，无例外）

`gateway/Cargo.toml` 的 `version` 必须在 `CHANGELOG.md` 里以
`## [<version>]` 小节形式出现。否则失败。

# 为什么只做这么少

- 不检查小节内容是否详实（那是 review 的事，机器判不了）。
- 不检查 Unreleased 是否为空（允许积压，发布时再归档）。
- 不碰 git history（tag 是否打了由发布流程保证，不是每次提交的门）。

# 用法

    python tools/check_changelog.py

# 退出码

- 0：一致
- 1：Cargo.toml 的版本在 CHANGELOG 里没有对应小节
"""
import re
import sys
from pathlib import Path

REPO = Path(__file__).resolve().parent.parent
CARGO = REPO / "gateway" / "Cargo.toml"
CHANGELOG = REPO / "CHANGELOG.md"


def main() -> int:
    if not CARGO.is_file():
        print(f"[check-changelog] ERROR: 缺 {CARGO}", file=sys.stderr)
        return 1
    if not CHANGELOG.is_file():
        print(f"[check-changelog] ERROR: 缺 {CHANGELOG}", file=sys.stderr)
        return 1

    m = re.search(r'^version\s*=\s*"([^"]+)"',
                  CARGO.read_text(encoding="utf-8"), re.M)
    if not m:
        print("[check-changelog] ERROR: Cargo.toml 里找不到 version",
              file=sys.stderr)
        return 1
    version = m.group(1)

    sections = set(re.findall(r"^## \[([0-9]+\.[0-9]+\.[0-9]+[^\]]*)\]",
                             CHANGELOG.read_text(encoding="utf-8"), re.M))
    if version not in sections:
        print(f"[check-changelog] FAIL: Cargo.toml version={version}，",
              file=sys.stderr)
        print(f"  但 CHANGELOG.md 里没有 '## [{version}]' 小节。",
              file=sys.stderr)
        print("  修法：bump 版本时同步加小节（把 Unreleased 的相关条目搬过去）。",
              file=sys.stderr)
        print(f"  现有小节：{sorted(sections) if sections else '(无)'}",
              file=sys.stderr)
        return 1

    print(f"[check-changelog] OK: version={version} 在 CHANGELOG 有对应小节")
    return 0


if __name__ == "__main__":
    sys.exit(main())
