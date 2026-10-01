#!/usr/bin/env python3
"""
OPT-R9 A3：Go SDK 编译验证脚本（`go vet` + `go build`）。

用途：让 CI 能真正**编译** `tools/ipp_sdk_go.go`——此前该文件从未被任何构建
系统碰过（仓库既无 `go.mod`，CI 也只跑 `python3 compileall`）。

# 为何需要它

`ipp_sdk_go.go` 是 `package main` 的单文件程序。Go 工具链**要求源文件位于某个
模块目录内**才会纳入构建，而 `tools/` 不是模块（仓库根无 `go.mod`）。审阅还
发现其文件头写 `import ipp "path/to/ipp_sdk_go"`——Go **禁止 import 一个
package main**，那种示例 100% 编译失败（OPT-R9 A3 已修正该文档）。

# 做法

把 SDK 源文件**复制**进临时目录，配上 `go.mod`，再 `go vet` + `go build`。
不为何不用软链：Windows 建 symlink 需管理员权限，CI 与本地行为要一致。

# 用法

    python tools/check_go_sdk.py

# 退出码

- 0：vet 与 build 均通过
- 1：编译/校验失败（打印 go 原始输出）
- 2：环境问题（找不到 `go`）
"""

"""
OPT-R9 A3：Go SDK 编译验证脚本（`go vet` + `go build`）。

用途：让 CI 能真正**编译** `tools/ipp_sdk_go.go`——此前该文件从未被任何构建
系统碰过（仓库既无 `go.mod`，CI 也只跑 `python3 compileall`）。

# 为何需要它

`ipp_sdk_go.go` 是 `package main` 的单文件程序。Go 工具链**要求源文件位于某个
模块目录内**才会纳入构建，而 `tools/` 不是模块（仓库根无 `go.mod`）。审阅还
发现其文件头写 `import ipp "path/to/ipp_sdk_go"`——Go **禁止 import 一个
package main**，那种示例 100% 编译失败（OPT-R9 A3 已修正该文档）。

# 做法

把 SDK 源文件**复制**进临时目录，配上 `go.mod`，再 `go vet` + `go build`。
不为何不用软链：Windows 建 symlink 需管理员权限，CI 与本地行为要一致。

# 用法

    python tools/check_go_sdk.py

# 退出码

- 0：vet 与 build 均通过
- 1：编译/校验失败（打印 go 原始输出）
- 2：环境问题（找不到 `go`）
"""

import shutil
import subprocess
import sys
import tempfile
from pathlib import Path

REPO = Path(__file__).resolve().parent.parent
SDK_SRC = REPO / "tools" / "ipp_sdk_go.go"
# 放 .gitignore 的构建壳（见 tools/sdk-go/go.mod）；本脚本仍复制源文件到临时
# 目录，因为 Go 要求源文件在模块目录内。
GOMOD_TEMPLATE = "module ippproxypool/sdk\n\ngo 1.21\n"


def main() -> int:
    if not SDK_SRC.is_file():
        print(f"[check-go-sdk] ERROR: SDK source missing: {SDK_SRC}", file=sys.stderr)
        return 2

    go = shutil.which("go")
    if not go:
        # CI 的 ubuntu-latest 自带 Go；本地缺失时明确报「环境问题」而不是
        # 「编译失败」——两者的处置完全不同。
        print(
            "[check-go-sdk] ERROR: `go` not found on PATH (install Go, or run this "
            "check on CI where the toolchain is preinstalled)",
            file=sys.stderr,
        )
        return 2

    with tempfile.TemporaryDirectory(prefix="ipp-go-sdk-") as tmp:
        workdir = Path(tmp)
        # 复制源文件（保持与 CI 一致：Windows 建 symlink 需管理员）。
        shutil.copy2(SDK_SRC, workdir / SDK_SRC.name)
        (workdir / "go.mod").write_text(GOMOD_TEMPLATE, encoding="utf-8")

        results: list[tuple[str, subprocess.CompletedProcess[str]]] = []
        for label, cmd in [
            ("go vet", [go, "vet", "./..."]),
            ("go build", [go, "build", "-o", str(workdir / "sdk-check"), "."]),
        ]:
            proc = subprocess.run(
                cmd, cwd=workdir, capture_output=True, text=True, timeout=300
            )
            results.append((label, proc))
            if proc.returncode != 0:
                print(f"[check-go-sdk] FAIL: {label} failed", file=sys.stderr)
                if proc.stdout:
                    print(proc.stdout, file=sys.stderr)
                if proc.stderr:
                    print(proc.stderr, file=sys.stderr)
                return 1

    for label, proc in results:
        note = f" ({proc.stdout.strip()})" if proc.stdout.strip() else ""
        print(f"[check-go-sdk] {label}: OK{note}")
    print("[check-go-sdk] Go SDK compiles and vets clean")
    return 0


if __name__ == "__main__":
    sys.exit(main())
