#!/usr/bin/env python3
"""
OPT-R9 A3：.NET 与 Java SDK **编译**验证脚本（CI 侧执行）。

用途：让 CI 能真正编译 `tools/ipp_sdk_dotnet.cs` 与 `tools/ipp_sdk_java.java`
——这两个文件此前从未被任何构建系统碰过（仓库既无 `.csproj`，CI 也只跑
`python3 compileall`，只覆盖 `.py`）。

# 为何值得做（能力远强于 Node 的语法检查）

- **C#**：`dotnet build` 做**完整编译**，能抓类名/签名/using/语法/类型错误。
  审阅发现的历史破损——`ipp_sdk_dotnet.cs` 文件头写 `dotnet run -- --self-test`，
  而无 `.csproj` 时该命令直接报错；有了 `tools/sdk-dotnet/` 这个最小工程壳，
  编译期问题在 CI 就被挡住。
- **Java**：`javac` 同样做**完整编译**（语法 + 类型 + 符号解析）。

# 本机与 CI 的分工（如实说明）

本机（开发用 Windows）**未安装** JDK 与 .NET SDK，故这两个检查在本机会返回
`2`（环境问题）并被跳过；**CI 的 ubuntu-latest runner 预装两者**
（`actions/setup-java` / `actions/setup-dotnet` 会在 workflow 里显式固定版本），
检查在那里真正执行。因此「本机未跑」不等于「门禁无效」，但**本轮无法在本地
实证这两个门禁能抓住错误**——这一点如实登记，不假装已验证。

# 用法

    python tools/check_jvm_sdk.py          # 两个都查（缺工具链则各自跳过）
    python tools/check_jvm_sdk.py --dotnet # 只查 .NET
    python tools/check_jvm_sdk.py --java    # 只查 Java

# 退出码

- 0：已安装的工具链全部编译通过（未安装的按「跳过」处理，不算失败）
- 1：某个已安装的工具链**编译失败**（打印原始输出）
"""

import shutil
import subprocess
import sys
import tempfile
from pathlib import Path

REPO = Path(__file__).resolve().parent.parent
DOTNET_SRC = REPO / "tools" / "ipp_sdk_dotnet.cs"
DOTNET_PROJ = REPO / "tools" / "sdk-dotnet" / "ipp_sdk.csproj"
JAVA_SRC = REPO / "tools" / "ipp_sdk_java.java"


def check_dotnet() -> int:
    """编译 .NET SDK。返回 0/1；2 表示工具链缺失（上层按「跳过」处理）。"""
    if not DOTNET_SRC.is_file() or not DOTNET_PROJ.is_file():
        print(f"[check-jvm-sdk] ERROR: .NET SDK or project shell missing", file=sys.stderr)
        return 1
    dotnet = shutil.which("dotnet")
    if not dotnet:
        print("[check-jvm-sdk] dotnet: SKIPPED (not installed on this machine)")
        return 2
    proc = subprocess.run(
        [dotnet, "build", str(DOTNET_PROJ), "--nologo", "-v", "quiet"],
        cwd=REPO,
        capture_output=True,
        # OPT-R16 E1：`dotnet build` 的输出含中文与本地代码页字符，
        # `text=True` 会用 locale 默认编码解码；在中文 Windows 上是 GBK，
        # 遇到 UTF-8 字节直接抛 UnicodeDecodeError。该异常发生在
        # `_readerthread` 里，**吞掉了真实的编译错误**，随后 `proc.stdout`
        # 变成 None，再触发 `TypeError: 'NoneType' object is not
        # subscriptable` ——两层异常叠加，门禁报出的信息完全不可读。
        # 显式指定 errors="replace" 让解码永不失败，保住真实报错。
        encoding="utf-8",
        errors="replace",
        timeout=600,
    )
    if proc.returncode != 0:
        print("[check-jvm-sdk] FAIL: dotnet build failed", file=sys.stderr)
        # `proc.stdout` 理论上可为 None（异常路径），用 or "" 兜底，
        # 避免门禁自身崩溃而掩盖真正的编译错误。
        print((proc.stdout or "")[-4000:], file=sys.stderr)
        print((proc.stderr or "")[-4000:], file=sys.stderr)
        return 1
    print("[check-jvm-sdk] dotnet build: OK")
    return 0


def check_java() -> int:
    """编译 Java SDK（javac 需要输出目录，输出到临时目录避免污染仓库）。"""
    if not JAVA_SRC.is_file():
        print("[check-jvm-sdk] ERROR: Java SDK missing", file=sys.stderr)
        return 1
    javac = shutil.which("javac")
    if not javac:
        print("[check-jvm-sdk] javac: SKIPPED (not installed on this machine)")
        return 2
    with tempfile.TemporaryDirectory(prefix="ipp-java-sdk-") as tmp:
        proc = subprocess.run(
            [javac, "-d", tmp, str(JAVA_SRC)],
            cwd=REPO,
            capture_output=True,
            # OPT-R16 E1：同 check_dotnet 的理由——javac 在中文 Windows 上
            # 也输出本地代码页字节，text=True 会按 GBK 解码而抛
            # UnicodeDecodeError，进而让门禁报不出真正的编译错误。
            encoding="utf-8",
            errors="replace",
            timeout=300,
        )
    if proc.returncode != 0:
        print("[check-jvm-sdk] FAIL: javac failed", file=sys.stderr)
        print((proc.stdout or "")[-4000:], file=sys.stderr)
        print((proc.stderr or "")[-4000:], file=sys.stderr)
        return 1
    print("[check-jvm-sdk] javac: OK")
    return 0


def main() -> int:
    args = set(sys.argv[1:])
    want_dotnet = not args or "--dotnet" in args
    want_java = not args or "--java" in args

    results: list[tuple[str, int]] = []
    if want_dotnet:
        results.append((".NET", check_dotnet()))
    if want_java:
        results.append(("Java", check_java()))

    failed = [name for name, code in results if code == 1]
    if failed:
        print(f"[check-jvm-sdk] FAILED: {', '.join(failed)}", file=sys.stderr)
        return 1

    skipped = [name for name, code in results if code == 2]
    if skipped:
        print(
            f"[check-jvm-sdk] PASS (toolchains absent, skipped: {', '.join(skipped)}) — "
            "these checks really run on CI where the SDKs are preinstalled"
        )
    else:
        print("[check-jvm-sdk] all installed toolchains compile clean")
    return 0


if __name__ == "__main__":
    sys.exit(main())
