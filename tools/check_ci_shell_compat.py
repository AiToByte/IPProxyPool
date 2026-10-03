# -*- coding: utf-8 -*-
"""CI workflow 静态门：shell 兼容性。

# 为什么需要这个门（OPT-R16 E6 的由来）

`ci.yml` 里 Windows job 的两个步骤声明 `shell: powershell`，即 **Windows
PowerShell 5.1**。GitHub 把 `run:` 的内容写成临时 `.ps1` 交给它执行，而该
文件以 runner 的代码页写出。**只要 run 块里出现任何非 ASCII 字节**（中英文
注释、em dash U+2014、全角标点），就可能在字符串中途被截断。

实测（run 37117250483 step #4）：一个 `Write-Error` 字符串里的 em dash 被
截断 → 字符串未闭合 → 后续所有行被吞进去 → PS 报出

    Missing closing ')' in expression.
    The string is missing the terminator: ".

而报错位置指向 runner 生成的 `D:\\a\\_temp\\<uuid>.ps1`，**不是仓库里任何
文件**，本机完全无法复现（按 HEAD 原样在本机跑同一命令，6 个脚本全过）。

所以必须在提交前静态拦住。判定规则：
  - `shell: powershell`（Windows PowerShell 5.1）→ run 块必须纯 ASCII
  - `shell: pwsh`（PS 7，原生 UTF-8）→ 不作要求
  - 无 shell（Linux 默认 bash）→ 不作要求

用法：`python tools/check_ci_shell_compat.py`
退出码：0 = 通过；1 = 有违规。
"""
import pathlib
import re
import sys

REPO = pathlib.Path(__file__).resolve().parent.parent
WF_DIR = REPO / ".github" / "workflows"

# 这些 shell 由 Windows PowerShell 5.1 执行，对非 ASCII 敏感
PS51_SHELLS = {"powershell", "pwsh -command", "powershell -command"}


def scan(path: pathlib.Path) -> list[str]:
    lines = path.read_text(encoding="utf-8").splitlines()
    problems: list[str] = []

    # 先把文件切成 step 块，再逐块判定——单遍状态机容易出闭包捕获的坑。
    steps: list[dict] = []
    cur: dict | None = None
    in_run = False

    for i, line in enumerate(lines):
        m = re.match(r"(\s*)- name:\s*(.+)", line)
        if m:
            cur = {"name": m.group(2).strip(), "shell": None,
                   "run": None, "line": i + 1}
            steps.append(cur)
            in_run = False
            continue

        if cur is None:
            continue

        m = re.match(r"\s*shell:\s*(\S+)", line)
        if m:
            cur["shell"] = m.group(1).strip()
            continue

        m = re.match(r"(\s*)run:\s*\|", line)
        if m:
            cur["run"] = {"indent": len(m.group(1)), "start": i + 1}
            in_run = True
            continue

        if in_run and cur["run"] is not None:
            indent = len(line) - len(line.lstrip())
            if line.strip() and indent <= cur["run"]["indent"]:
                # 块标量结束
                cur["run"]["end"] = i
                in_run = False
                continue
            cur["run"]["end"] = i + 1

    for st in steps:
        if st["shell"] not in PS51_SHELLS or st["run"] is None:
            continue
        r = st["run"]
        end = r.get("end", len(lines))
        for i in range(r["start"], min(end, len(lines))):
            body = lines[i]
            if not body.strip():
                continue
            # 注意：这里**不能**豁免以 # 开头的行。YAML 块标量内部的 #
            # 是 PowerShell 注释，PS 5.1 会把它当脚本内容读入并按代码页
            # 写出，同样会被截断。只有 run: 块**上方**的 YAML 注释
            # （缩进小于 run: 的缩进）才是安全的。
            bad = sorted({ch for ch in body if ord(ch) > 127})
            if not bad:
                continue
            shown = " ".join(f"U+{ord(c):04X}({c})" for c in bad[:6])
            kind = "注释行" if body.strip().startswith("#") else "代码行"
            problems.append(
                f"  {path.name}:{i + 1}: [{st['name']}] shell={st['shell']} "
                f"{kind}含非 ASCII：{shown}" + (" ..." if len(bad) > 6 else "") + "\n"
                f"      行内容: {body.strip()[:100]}\n"
                f"      修法：把这些文字移到 run: 块**上方**的 YAML 注释里"
                f"（缩进比 run: 少一级），或改用 shell: pwsh（PS 7 原生 UTF-8）。"
                f"run 块内部（含 # 注释行）只能是 ASCII。"
            )
    return problems


def main() -> int:
    if not WF_DIR.is_dir():
        print(f"[check-ci-shell-compat] ERROR: 缺 {WF_DIR}", file=sys.stderr)
        return 1

    files = sorted(list(WF_DIR.glob("*.yml")) + list(WF_DIR.glob("*.yaml")))
    if not files:
        print("[check-ci-shell-compat] ERROR: 未找到任何 workflow", file=sys.stderr)
        return 1

    problems: list[str] = []
    for wf in files:
        problems.extend(scan(wf))

    if problems:
        print(
            "[check-ci-shell-compat] FAIL: shell: powershell 的 run 块含非 ASCII 字符。\n"
            "Windows PowerShell 5.1 会把 run 块按 runner 代码页写成临时 .ps1，\n"
            "非 ASCII 字节可能在字符串中途被截断，导致「字符串未闭合」级联解析错误，\n"
            "且报错指向 runner 的临时文件，本机无法复现。\n"
            "实测：run 37117250483 step #4 因此失败（Missing closing ')'）。\n"
            "问题：",
            file=sys.stderr,
        )
        print("\n".join(problems), file=sys.stderr)
        return 1

    print(f"[check-ci-shell-compat] OK: {len(files)} 个 workflow 中 "
          f"shell: powershell 的 run 块均为纯 ASCII")
    return 0


if __name__ == "__main__":
    sys.exit(main())