#!/usr/bin/env python3
"""
OPT-R9 A3：Node SDK 语法验证脚本（`node --check`）。

用途：让 CI 能真正**解析** `tools/ipp_sdk_node.js`——此前该文件从未被任何
构建/检查系统碰过（CI 只跑 `python3 compileall`，只覆盖 `.py`）。

# 能力边界（如实标注，不要误以为这是类型检查）

`node --check` 只做**语法**解析，**不解析类型、不解析依赖**。所以：

- 能抓：括号/引号不配对、缺分号导致的 ASI 歧义、`const`/`let` 误用等语法错；
- 抓不到：调用不存在的方法、类型不匹配、拼错的内置 API 名。

**JavaScript 没有独立的「只做类型检查且零配置」的命令**（`tsc` 需要 TS 与配置
或 `// @ts-check`，那是另一套工具链）。故本轮给出语法级保证并**如实标注
边界**，而不是假装做了类型检查。真正的运行时验证靠 SDK 自带的
`--self-test`（需网关在线，属手动/集成范畴，不进 CI 快速门）。

# 用法

    python tools/check_node_sdk.py

# 退出码

- 0：语法检查通过
- 1：语法错误（打印 node 原始输出）
- 2：环境问题（找不到 `node`）
"""

import shutil
import subprocess
import sys
from pathlib import Path

REPO = Path(__file__).resolve().parent.parent
SDK_SRC = REPO / "tools" / "ipp_sdk_node.js"


def main() -> int:
    if not SDK_SRC.is_file():
        print(f"[check-node-sdk] ERROR: SDK source missing: {SDK_SRC}", file=sys.stderr)
        return 2

    node = shutil.which("node")
    if not node:
        # CI 的 ubuntu-latest 自带 Node（actions/setup-node 之前即已内置），
        # Windows/macOS runner 同样自带。缺失即环境问题，不是代码问题。
        print(
            "[check-node-sdk] ERROR: `node` not found on PATH (install Node, or run "
            "this check on CI where the runtime is preinstalled)",
            file=sys.stderr,
        )
        return 2

    proc = subprocess.run(
        [node, "--check", str(SDK_SRC)],
        capture_output=True,
        text=True,
        timeout=120,
    )
    if proc.returncode != 0:
        print("[check-node-sdk] FAIL: node --check reported a syntax error", file=sys.stderr)
        if proc.stdout:
            print(proc.stdout, file=sys.stderr)
        if proc.stderr:
            print(proc.stderr, file=sys.stderr)
        return 1

    ver = subprocess.run(
        [node, "--version"], capture_output=True, text=True, timeout=30
    ).stdout.strip()
    print(f"[check-node-sdk] node --check: OK ({ver})")
    print(
        "[check-node-sdk] note: syntax-level only — JS has no config-free type check; "
        "runtime verification lives in the SDK's own --self-test (needs a live gateway)"
    )
    return 0


if __name__ == "__main__":
    sys.exit(main())
