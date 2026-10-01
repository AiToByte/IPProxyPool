#!/usr/bin/env python3
"""
CI 断言：5 个 SDK 的**错误契约**必须一致（OPT-R10 C1/C2）。

# 为何需要这道门（不是"统一风格"，是修 bug）

5 个 SDK 各自演进，错误处理一度分裂成三种。实测发现的两个真实缺陷：

1. **.NET 函数内自相矛盾**：`GetAsync` 里 url 解析失败走 `return (0, msg)`，
   而 scheme 非 http 走 `throw ArgumentException`。同一个函数、同一类
   "调用方传错了" 的错误，两个不同通道 —— 调用方无法区分"我传错了"和
   "网关挂了"，只能一律 try/catch 兜住，于是参数错误被静默。

   更糟：`catch (Exception e)` 会把 SDK **自身的编程错误**
   （如误用 `HttpClient` API 抛的 `InvalidOperationException`）也吞成
   `(0, message)`，**真 bug 被伪装成网络故障**。这是把诊断信息反向丢弃。

2. **Go 静默降级**：`Get` 对非 http target 返回 `(0, "only plain http...")`，
   与真正的网络失败（连接失败、超时）**返回值完全同形**。调用方无法
   区分"我传错了 https"和"网关暂时不可用"，只会一律重试 —— 而重试
   一个永远不可能成功的请求只是浪费配额。

# 统一后的契约（语义等价，通道形态按语言惯例）

| 失败类型         | 含义                     | 通道                                  |
| ---------------- | ------------------------ | ------------------------------------- |
| **编程错误**     | 调用方把参数用错了       | 抛异常 / Go `return err`（status 恒 0） |
| **网络/网关失败** | 瞬时、可预期            | **不抛**，返回 status=0 + body 诊断    |

各语言的具体形态（"抛异常" 在 Go 里等价于 `return error`，这是语言惯例
差异，不是契约不一致）：

| SDK        | 文件                       | 编程错误通道           |
| ---------- | -------------------------- | ---------------------- |
| Python     | `tools/ipp_sdk.py`         | `raise ValueError`     |
| Java       | `tools/ipp_sdk_java.java`  | `IllegalArgumentException` |
| Node.js    | `tools/ipp_sdk_node.js`    | `throw Error`          |
| Go         | `tools/ipp_sdk_go.go`      | `return err`（第三返回值） |
| .NET       | `tools/ipp_sdk_dotnet.cs`  | `throw ArgumentException` |

# 为何是静态检查而非编译检查

`check_go_sdk.py` / `check_jvm_sdk.py` / `check_node_sdk.py` 都做真编译，
但 **`.NET` 在 CI 镜像里未必有 SDK**，且历史上 .NET 是 5 个语言里唯一
"改了没人编译"的那个 —— 这正是缺陷 1 能长期存活的原因。本门**不依赖任何
编译器**，纯文本断言，因此无论 CI 有没有 .NET SDK 都能拦住回归。

# 用法

    python tools/check_sdk_contract.py

# 退出码

- 0：5 个 SDK 契约齐全
- 1：任一 SDK 缺失契约要素（打印缺哪个文件、缺什么）
"""

import sys
from pathlib import Path

REPO = Path(__file__).resolve().parent.parent
TOOLS = REPO / "tools"

# 编程错误通道：文件 → 至少命中一个的模式（正则）
PROGRAMMING_ERROR_PATTERNS: dict[str, list[str]] = {
    "ipp_sdk.py": [r"raise ValueError", r"scheme\s*!=\s*[\"']http[\"']"],
    "ipp_sdk_java.java": [r"throw new IllegalArgumentException", r"![\"']http[\"']\.equals"],
    # Node：protocol 是 `http:`（含冒号），不是 `http`
    "ipp_sdk_node.js": [r"throw new Error", r"protocol\s*!==\s*[\"']http:[\"']"],
    # Go：编程错误必须走第三个返回值 err，且**不能**与网络失败同形。
    "ipp_sdk_go.go": [r"return 0, nil, fmt\.Errorf", r"Scheme\s*!=\s*\"http\""],
    # .NET：编程错误必须抛 ArgumentException，且 URL 解析不得再走 return 通道。
    "ipp_sdk_dotnet.cs": [r"throw new ArgumentException", r"Uri\.TryCreate"],
}

# 每个 SDK 都必须出现「编程错误」这四个概念之一（防止只改了实现、
# 忘了写文档，调用方无从得知契约 —— 那等于契约不存在）
CONCEPT_MARKERS: dict[str, list[str]] = {
    "ipp_sdk.py": [r"编程错误|programming error"],
    "ipp_sdk_java.java": [r"编程错误|programming error"],
    "ipp_sdk_node.js": [r"编程错误|programming error"],
    "ipp_sdk_go.go": [r"编程错误|programming error"],
    "ipp_sdk_dotnet.cs": [r"编程错误|programming error"],
}

# 反向断言：这些是**曾经存在过的缺陷形态**，出现即判红。
FORBIDDEN: list[tuple[str, str, str]] = [
    # .NET 曾用 catch(Exception) 把 SDK 自身编程错误吞成 (0, message)。
    (
        "ipp_sdk_dotnet.cs",
        r"catch\s*\(\s*Exception\s+\w+\s*\)\s*\{\s*lastStatus\s*=\s*0",
        "无过滤的 catch(Exception) 会把 SDK 编程错误伪装成网络故障；"
        "只允许 `when (e is HttpRequestException || e is OperationCanceledException)`",
    ),
    # .NET 曾用 new Uri(url) + catch 兜解析失败 → 走 return 通道（与 throw 矛盾）。
    (
        "ipp_sdk_dotnet.cs",
        r"catch\s*\(\s*Exception\s+\w+\s*\)\s*\{\s*return\s*\(0,\s*Cut\(",
        "URL 解析失败曾走 return (0, msg)，与 scheme 校验的 throw 矛盾；"
        "必须统一为 throw ArgumentException",
    ),
]


def main() -> int:
    import re

    failures: list[str] = []

    for fname, patterns in PROGRAMMING_ERROR_PATTERNS.items():
        path = TOOLS / fname
        if not path.is_file():
            failures.append(f"  {fname}: 文件缺失")
            continue
        text = path.read_text(encoding="utf-8")
        for pat in patterns:
            if not re.search(pat, text):
                failures.append(f"  {fname}: 编程错误通道缺失 — 未匹配 {pat!r}")

        # 契约必须**写在文件里**（文档即契约的一部分）。
        for pat in CONCEPT_MARKERS.get(fname, []):
            if not re.search(pat, text, re.IGNORECASE):
                failures.append(
                    f"  {fname}: 未文档化错误契约（无 '编程错误' 说明）— "
                    "只改实现不写文档，调用方无从得知契约"
                )

    # 反向断言
    for fname, pat, why in FORBIDDEN:
        path = TOOLS / fname
        if not path.is_file():
            continue
        if re.search(pat, path.read_text(encoding="utf-8"), re.DOTALL):
            failures.append(f"  {fname}: 检出已修复的缺陷形态 — {why}")

    if failures:
        print(
            "[check-sdk-contract] FAIL: SDK 错误契约不一致。\n"
            "统一要求：编程错误走异常/Go error（status 恒 0），"
            "网络失败不抛、返回 status=0 + body 诊断。\n问题：",
            file=sys.stderr,
        )
        print("\n".join(failures), file=sys.stderr)
        return 1

    print(
        f"[check-sdk-contract] OK: {len(PROGRAMMING_ERROR_PATTERNS)} 个 SDK 错误契约一致"
        "（编程错误走异常/Go error；网络失败返回 status=0 + 诊断）"
    )
    return 0


if __name__ == "__main__":
    sys.exit(main())
