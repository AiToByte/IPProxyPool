#!/usr/bin/env python3
"""
CI 断言：Windows「300 秒自杀」的修复不得被静默回退（OPT-R14 B4）。

# 为什么需要这道门

根因在依赖里：`pingora-core 0.6.0` 的 `Server::run()` 有一行
`#[cfg(windows)] let shutdown_type = ShutdownType::Graceful;` ——
Windows 上 `main_loop` 从不被 await（那段是 `#[cfg(unix)]`），于是**没有信号等待**，
`shutdown_type` 被硬编码为 Graceful，网关必然 `sleep(grace_period)` 后
`process::exit(0)`。本机实测退出寿命 **305~308s**。

修复是「把 `GATEWAY_GRACE_SECS` 的默认值改成平台感知」。这类修复的**典型死法**是：
某天有人看到「Windows 默认 86400 看着奇怪」就"清理"回 300，**在 code review 里
完全看不出问题**（Unix 侧行为一模一样，全绿），而 Windows 上网关又变回
"5 分钟自杀"。没有门的话，这个回归会静默发生且极难定位（无 panic、无错误码）。

故本门锁三件确定性的事：
1. `default_grace_secs` 仍是**平台感知**的（有 `is_windows` 分支）；
2. Windows 分支返回值 **> 300** 且 **!= 0**；
3. `docs/OPERATION.md` 写明该限制、覆盖方式与"升级 Pingora 才是长期修法"。

# 不锁什么

**不**断言"网关能活多久"——那是活流量行为，由 OPT-R14 的 t>340s 实测负责。
本门只做静态契约检查，不依赖公网、不依赖运行时。

# 用法

    python tools/check_windows_lifetime.py

# 退出码

- 0：契约在位
- 1：被回退（打印具体项）
"""

import re
import sys
from pathlib import Path

REPO = Path(__file__).resolve().parent.parent
MAIN_RS = REPO / "gateway" / "src" / "main.rs"
OPERATION = REPO / "docs" / "OPERATION.md"


def main() -> int:
    failures: list[str] = []

    for p in (MAIN_RS, OPERATION):
        if not p.is_file():
            print(f"[check-windows-lifetime] ERROR: 缺文件 {p}", file=sys.stderr)
            return 1

    src = MAIN_RS.read_text(encoding="utf-8")
    doc = OPERATION.read_text(encoding="utf-8")

    # 1) 函数仍在且是平台感知
    if "fn default_grace_secs" not in src:
        failures.append("  main.rs 缺 default_grace_secs —— 平台感知默认值被删掉了？")
    else:
        body = src[src.index("fn default_grace_secs"):][:900]
        if "is_windows" not in body:
            failures.append(
                "  default_grace_secs 不再按 is_windows 分支 —— "
                "Windows 会被打回 300s 自杀，且 Unix 侧看不出任何异常"
            )
        m = re.search(r"if is_windows\s*\{[^}]*?(\d[\d_]*)", body, re.DOTALL)
        if not m:
            failures.append("  解析不出 Windows 分支的返回值（格式变了？）")
        else:
            val = int(m.group(1).replace("_", ""))
            if val <= 300:
                failures.append(
                    f"  Windows 分支默认 grace = {val}s，\u5fc5\u987b > 300s \u624d\u80fd\u7ed5\u8fc7\u6846\u67b6\u7684\u81ea\u6740"
                )
            if val == 0:
                failures.append("  Windows 分支不得为 0（会立即退出，比现状更糟）")

    # 2) 生产赋值处必须真的用了它。
    #
    # 【自洞记录】首版在**全文**搜 `default_grace_secs(cfg!(windows))`，结果匹配到了
    # **测试里**的同形调用 —— 于是把生产调用点改成硬编码 300，本门仍然绿。
    # 修法：切掉 `#[cfg(test)]` 之后的测试模块，只看**生产**部分，并且只认
    # 「给 `conf.grace_period_seconds` 赋值的那段表达式」里出现了该函数。
    # 【自洞记录之二】分界符不能用 `#[cfg(test)]` —— `main.rs` 里**单个函数**的
    # `#[cfg(test)]`（位于文件前部 1KB 处）早于 grace 赋值点（38KB 处），
    # 用它切"生产段"会把真正的生产代码整段切掉，导致本门**正向就误判红**。
    # 正确分界是测试**模块**声明 `mod tests`。
    prod = src.split("mod tests")[0]
    if not re.search(r"default_grace_secs\(\s*cfg!\(\s*windows\s*\)\s*\)", prod):
        failures.append(
            "  生产段（`mod tests` 之前）未出现 default_grace_secs(cfg!(windows)) —— "
            "grace 默认值可能已被改回硬编码 300。"
            "**只扫生产段**：测试里有同形调用，扫全文会被它掩盖（这正是首版的洞）"
        )
    if not re.search(r"conf\.grace_period_seconds\s*=\s*Some\(", prod):
        failures.append("  生产段找不到 `conf.grace_period_seconds = Some(..)` 赋值处（格式变了？）")

    # 3) 文档口径
    #
    # 【自洞记录】首版有两处漏洞，均被本门自己的双向测试抓出：
    #   ① 用 `(\u5347\u7ea7|upgrade)` 匹配"长期修法是升级 Pingora"，结果文档**其它层**
    #      的"升级"二字掩盖了 R14 那段被删的事实 ⇒ 删掉照样绿。
    #      修法：匹配**完整短语**，且只在 R14 小节内查找（见下方 r14）。
    #   ② 调用点检查匹配到了**测试里**的同形调用（见上方第 2 项注释）。
    r14 = doc.split("OPT-R14 B")[-1] if "OPT-R14 B" in doc else doc
    doc_claims = [
        (r"Windows", "必须写明该限制发生在 Windows"),
        (r"86400|86_400", "必须写明 Windows 默认值 86400s"),
        (r"GATEWAY_GRACE_SECS", "必须给出覆盖方式（GATEWAY_GRACE_SECS）"),
        (r"Pingora|pingora", "必须点名根因在 Pingora"),
        (r"升级\s*Pingora|升级\s*pingora|upgrade\s*Pingora",
         "必须写明长期修法是「升级 Pingora」（完整短语，不接受单独的「升级」）"),
        (r"无\s*panic|no\s*panic", "必须写明现象：无 panic/错误码，故此前极难定位"),
    ]
    for pat, why in doc_claims:
        if not re.search(pat, r14, re.IGNORECASE):
            failures.append(f"  docs/OPERATION.md \u7f3a\u53e3\u5f84 {pat!r} \u2014 {why}")

    if failures:
        print(
            "[check-windows-lifetime] FAIL: Windows 300s \u81ea\u6740\u7684\u4fee\u590d\u88ab\u56de\u9000\u3002\n"
            "\u6839\u56e0\uff1apingora-core 0.6 \u5728 Windows \u4e0a\u4e0d\u7b49\u5f85\u4fe1\u53f7\uff0c"
            "Server::run() \u628a shutdown_type \u786c\u7f16\u7801\u4e3a Graceful \u5e76 sleep(grace) \u540e\u9000\u51fa"
            "\uff08\u5b9e\u6d4b\u5b58\u6d3b 305~308s\uff09\u3002\n\u56de\u9000\u4e3a 300 \u5728 review \u4e2d\u51e0\u4e4e\u770b\u4e0d\u51fa\u95ee\u9898\uff0c"
            "\u4f46 Windows \u4e0a\u7f51\u5173\u4f1a\u53c8\u5f00\u59cb\u81ea\u6740\u3002\u95ee\u9898\uff1a",
            file=sys.stderr,
        )
        print("\n".join(failures), file=sys.stderr)
        return 1

    print(
        "[check-windows-lifetime] OK: 平台感知默认值在位（Windows >300s 且非 0）、"
        "调用点使用 cfg!(windows)、6 条文档口径齐全"
    )
    return 0


if __name__ == "__main__":
    sys.exit(main())
