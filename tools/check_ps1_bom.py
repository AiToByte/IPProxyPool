#!/usr/bin/env python3
"""CI 断言：`tools/*.ps1` 工作区副本必须带 UTF-8 BOM（OPT-R7 D1）。

# 背景

Windows PowerShell 5.1 按**系统 ANSI 代码页**（简体中文为 GBK/936）解析无 BOM 的
`.ps1` 文件。仓库运维脚本含中文注释，一旦以无 BOM 形态执行，会出现乱码乃至
解析失败。事故记录见 `docs/FREE_BASELINE.md` 与 `docs/SYSTEM-ARCHITECTURE.md`。

# 为什么需要这道断言（git 配置无解）

实测（git 2.52 / Windows / `core.autocrlf=true`）证明 **git 会剥离 UTF-8 BOM**，
且该行为不受 `.gitattributes` 任何组合控制（详见 `.gitattributes` 中 OPT-R7 D1
的实测记录表）。因此：

- 仓库入库的 blob 必然无 BOM；
- 但**开发者本机的工作区文件必须带 BOM**才能正确执行；
- clone 出来的工作区文件若无 BOM，PS 5.1 会在运行时报错。

故 CI 在**每个 PR/push** 上校验工作区副本：一旦有人提交了无 BOM 的 `.ps1`（例如
用 `Set-Content -Encoding utf8NoBOM`、或 IDE 存成 "UTF-8 without BOM"），
或有人 clone 后误改，本断言会立即红并给出修复命令。

# 用法

```bash
python tools/check_ps1_bom.py            # 检查 tools/ 下全部 .ps1
python tools/check_ps1_bom.py <file>...  # 只检查指定文件
```

# 退出码

- 0：全部通过
- 1：存在缺 BOM 的文件（逐个打印修复命令）
- 2：用法错误（传入的文件不存在）

# 修复命令

PowerShell（5.1）：

```powershell
Get-Content -Raw -Encoding UTF8 tools/ipp.ps1 |
    Set-Content -Encoding UTF8 tools/ipp.ps1
```

（`Set-Content -Encoding UTF8` 在 Windows PowerShell 5.1 下**带 BOM**；
PowerShell 7+ 的 `UTF8` 是无 BOM，需显式用 `utf8BOM`。）
"""

from __future__ import annotations

import sys
from pathlib import Path

# UTF-8 BOM 的字节序列（EF BB BF）。
UTF8_BOM = b"\xef\xbb\xbf"

# PowerShell 5.1 写 BOM 的编码名。
PS51_ENCODING = "UTF8"


def find_scripts(paths: list[str] | None = None) -> list[Path]:
    """收集待检查的 `.ps1`：未指定路径时递归仓库 `tools/`。"""
    if paths:
        found: list[Path] = []
        for raw in paths:
            p = Path(raw)
            if not p.is_file():
                print(f"[check-ps1-bom] ERROR: no such file: {raw}", file=sys.stderr)
                sys.exit(2)
            found.append(p)
        return found

    tools_dir = Path(__file__).resolve().parent
    return sorted(tools_dir.rglob("*.ps1"))


def has_bom(path: Path) -> bool:
    """前 3 字节是否为 UTF-8 BOM（文件短于 3 字节视为缺失）。"""
    try:
        with path.open("rb") as fh:
            return fh.read(3) == UTF8_BOM
    except OSError as exc:  # pragma: no cover - 读失败按缺 BOM 报，附原因
        print(f"[check-ps1-bom] ERROR: cannot read {path}: {exc}", file=sys.stderr)
        return False


def main() -> int:
    scripts = find_scripts(sys.argv[1:] or None)
    if not scripts:
        print("[check-ps1-bom] no .ps1 files found — nothing to check")
        return 0

    missing = [p for p in scripts if not has_bom(p)]
    checked = len(scripts)

    if not missing:
        print(f"[check-ps1-bom] OK: {checked} script(s) all carry a UTF-8 BOM")
        return 0

    print(
        f"[check-ps1-bom] FAIL: {len(missing)}/{checked} script(s) missing UTF-8 BOM",
        file=sys.stderr,
    )
    print(
        "[check-ps1-bom] Windows PowerShell 5.1 parses BOM-less .ps1 using the "
        "system ANSI code page (GBK on zh-CN), which corrupts the Chinese "
        "comments and can cause parse failures.",
        file=sys.stderr,
    )
    print("", file=sys.stderr)
    for p in missing:
        rel = p.as_posix()
        print(f"  MISSING BOM: {rel}", file=sys.stderr)
        # 修复命令：用 UTF8 编码重写自身即可补上 BOM（PS 5.1 下 UTF8 == 带 BOM）。
        print(
            f"    fix (PowerShell 5.1): "
            f"Get-Content -Raw -Encoding UTF8 {rel} | "
            f"Set-Content -Encoding UTF8 {rel}",
            file=sys.stderr,
        )
        print(
            f"    fix (editor): save {rel} as 'UTF-8 with BOM'",
            file=sys.stderr,
        )
    return 1


if __name__ == "__main__":
    sys.exit(main())
