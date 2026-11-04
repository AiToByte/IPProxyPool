#!/usr/bin/env python3
r"""覆盖率基线门（OPT-R18 P2-5）。

## 为什么需要这道门

此前本仓有 **355 个单测**但**没有任何覆盖率度量**——单测数量只说明
「有多少个测试函数」，不说明「它们覆盖了多少条生产代码路径」。于是两类
问题都不可见：

1. **有大段代码从未被任何测试触及**（改了它也不知道会炸在哪）；
2. **覆盖率其实是下降的**（新增了 200 行生产代码 + 0 个测试，单测数从
   340 涨到 355，全绿——但覆盖率可能掉了 3 个百分点）。

第 2 类是**最阴险**的：单测数上涨与覆盖率下降可以同时发生，而只看单测
数的门禁完全看不出来。

## 本门做什么（与「不设阈值」纪律的调和）

计划文档（`plan/2026年10月27日-OPT-R18高质量迭代方向评估.md` §3 P2-5）
明确写道：「先跑 `cargo-llvm-cov` 出基线**再**设地板；不要一上来卡阈值
（会逼出"为覆盖而覆盖"的测试）」。

本门照此执行，**分两层**：

| 层 | 内容 | 是否进 CI |
| -- | ---- | -------- |
| **基线记录** | `docs/COVERAGE_BASELINE.md` 里的逐模块覆盖率数字，与 `docs/coverage_baseline.json`（机器可读）逐字一致 | ✅ 每次 CI 都跑 |
| **回退检测** | 当前实测覆盖率**低于**基线某个幅度 ⇒ 判红 | ✅ 每次 CI 都跑（需 `cargo-llvm-cov`） |

**关键设计：只卡「回退」，不卡「达标」。**

- 本门**不要求**覆盖率必须达到某个绝对数字（那会逼出为覆盖率而写的测试）。
- 本门**只要求**：覆盖率**不得比基线显著下降**。默认容差 **1.0 个百分点**——
  低于它判红。因为基线本身会随代码演进上下浮动，而「下降超过 1pp」几乎总是
  「新增了没被测到的代码」而非「重构导致统计口径变化」。

这个方向的选择很重要：**向上没有门槛，向下有门槛**。向上无门槛 ⇒ 不逼出
凑覆盖率的测试；向下有门槛 ⇒ 真正的覆盖退化会被抓住。

## 为什么不用 `cargo-llvm-cov` 的默认输出做判据

`cargo llvm-cov` 的文本输出含绝对路径与平台分隔符，直接 grep 判据极脆。
本门读它导出的 **JSON**（`cargo llvm-cov --json`），用 `path` 字段的**末段
文件名**做键——与 `cargo llvm-cov` 的文本表格同源，但不含临时目录与平台
路径差异。

## 用法

```bash
# 门禁本体（读 JSON + 读基线文件，比对）
python3 tools/check_coverage_baseline.py --json <cargo llvm-cov 的 JSON>

# 刷新基线（改代码后重新测量并回写 json + md）
python3 tools/check_coverage_baseline.py --refresh --json <新 JSON>

# 变异自测：证明本门有判别力
python3 tools/check_coverage_baseline.py --self-test
```

## 退出码

- 0：基线一致且无显著回退
- 1：基线漂移 / 覆盖率回退 / 自测未判红
"""

from __future__ import annotations

import argparse
import json
import re
import subprocess
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
BASELINE_JSON = ROOT / "docs" / "coverage_baseline.json"
BASELINE_MD = ROOT / "docs" / "COVERAGE_BASELINE.md"

#: 允许的覆盖率回退幅度（百分点）。超过即判红。
#:
#: 为什么是 1.0 而不是 0：`cargo-llvm-cov` 的统计会随编译器版本、泛型单态化
#: 数量等**非本仓代码**因素小幅浮动；0 容差会周期性假红，而假红门禁会被
#: 学会忽略（比没有门更糟）。为什么不是 5：5pp 的容差足以放过"新增 800 行
#: 生产代码、0 测试"这种真实退化。1pp 是「噪声」与「退化」之间的位置。
TOLERANCE_PP = 1.0


def coverage_key(filename: str) -> str:
    """覆盖率报告的 `filename`（绝对路径）→ 稳定、无歧义的键。

    **取 `src/` 之后的相对路径**（`.../gateway/src/router/health.rs` →
    `router/health.rs`；`.../gateway/src/main.rs` → `main.rs`）。

    为什么不能只用末段文件名：早期实现就是 `Path(f["filename"]).name`，当时
    只有一个目录模块（`router/`），凑巧没撞——但它有**两处**必然出问题：
      ① 目录模块的父文件叫 `mod.rs`，作键毫无信息量（哪个模块的 `mod.rs`？）；
      ② 第二个目录模块一出现就撞键：`router/tests.rs` 与 `free_pool/tests.rs`
         都归约成 `tests.rs`，后写的那份**静默覆盖**前一份，覆盖率数字直接错，
         而且不报任何错。
    OPT-R18 P2-1 拆 `free_pool` 时正好触发第 ② 类风险，故改为相对路径。
    绝对路径本身仍不可作键（含临时目录与平台分隔符，跨机必假红）。
    """
    p = Path(filename)
    parts = p.parts
    # 从后往前找 `src` 段，取其后的部分；找不到就退回末段文件名。
    for i in range(len(parts) - 1, -1, -1):
        if parts[i] == "src":
            return "/".join(parts[i + 1 :])
    return p.name


def load_json_cov(path: Path) -> dict:
    """读 `cargo llvm-cov --json` 的输出，返回 `{文件名: {...}}` 与 totals。

    `cargo llvm-cov --json` 的实际形状是
    `{"data": [{"files": [...], "totals": {...}}], ...}`——
    `data` 是**列表**（当前只有一项），每项里有 `files` 与 `totals`。
    早期版本直接是 dict，这里两种都接受。
    """
    raw = json.loads(path.read_text(encoding="utf-8"))
    data = raw.get("data", raw)
    if isinstance(data, list):
        if not data:
            raise ValueError("cargo llvm-cov JSON 的 data 为空")
        data = data[0]
    files = {}
    for f in data.get("files", []):
        files[coverage_key(f["filename"])] = f.get("summary", {})
    totals = data.get("totals", {})
    return {"files": files, "totals": totals}


def baseline_entries() -> dict:
    """读基线 JSON，返回 `{文件名: {"lines": pct, "regions": pct, ...}}`。"""
    data = json.loads(BASELINE_JSON.read_text(encoding="utf-8"))
    return data["files"]


def baseline_meta() -> dict:
    """读基线 JSON 的元信息（工具链、平台、容差）。

    为什么需要：**覆盖率数字不是跨工具链可比的**。同一份源码在
    rustc 1.99.1 与更新的编译器上，单态化数量、内联决策、`cfg` 分支
    的展开都可能不同——实测这类差异足以超过 1pp 的容差。若不区分，
    CI 上换个编译器版本就会**假红**，而假红门禁会被学会忽略（比没有门更糟）。
    """
    data = json.loads(BASELINE_JSON.read_text(encoding="utf-8"))
    return {
        "toolchain": data.get("_toolchain", ""),
        "platform": data.get("_platform", ""),
    }


def detect_toolchain() -> tuple[str, str]:
    """探测当前工具链，返回 `(rustc 版本串, 目标三元组)`。

    探测失败时返回 `("", "")` —— 调用方据此判定「不可比」，
    **而不是**默认当成可比（默认可比 ⇒ 静默放行真实退化）。
    """
    try:
        ver = subprocess.run(
            ["rustc", "--version"], capture_output=True, text=True, timeout=30
        )
        if ver.returncode != 0:
            return ("", "")
        version = ver.stdout.strip()
        host = subprocess.run(
            ["rustc", "-vV"], capture_output=True, text=True, timeout=30
        )
        triple = ""
        if host.returncode == 0:
            for line in host.stdout.splitlines():
                if line.startswith("host:"):
                    triple = line.split(":", 1)[1].strip()
                    break
        return (version, triple)
    except (OSError, subprocess.SubprocessError):
        return ("", "")


def baseline_total() -> float:
    """基线的**总体**行覆盖率（加权重算值，不是逐文件百分比的平均）。

    为什么必须存真值而不是现算：逐文件百分比的**算术平均** ≠ 总体覆盖率。
    总体覆盖率是 `Σ covered / Σ count`（按行数加权）。`main.rs` 有 1737 行、
    `fingerprint.rs` 有 73 行，前者占 50% 的代码量——用算术平均会让小文件的
    高覆盖率与高覆盖率的大文件等权，直接算错。
    """
    data = json.loads(BASELINE_JSON.read_text(encoding="utf-8"))
    return float(data["_totals"]["lines"])


def pct(summary: dict, key: str) -> float:
    """从 `cargo llvm-cov` 的 summary 结构里取某指标的百分比。

    注意：这里处理的是**实测**数据（形状为
    `{"lines": {"count": n, "covered": m, "percent": p}}`）。
    **基线**数据的形状不同（`{"lines": p}`，裸浮点数），由
    `base_pct()` 处理——两者形状不可混用。
    """
    return float(summary.get(key, {}).get("percent", 0.0))


def base_pct(entry: dict, key: str) -> float:
    """从**基线**条目里取某指标的百分比（基线里存的是裸浮点数）。

    与 `pct()` 分开是因为两者数据形状不同：实测是
    `{"lines": {"percent": p}}`，基线是 `{"lines": p}`。早期把两者混用会
    抛 `AttributeError: 'float' object has no attribute 'get'`——
    这正是门禁自身先崩掉、而不是给出判红结论的原因。
    """
    return float(entry.get(key, 0.0))


def compare(
    current: dict, base: dict, tolerance: float, numeric_comparable: bool = True
) -> list[str]:
    """比对实测与基线，返回问题列表（空 = 全绿）。

    两类检查：
    A. **基线漂移**——基线里有、实测里没有的文件（文件被删/改名），
       或反之（新增文件未更新基线）。后者只**提示**不判红：新增文件通常
       是正常开发，只有实测覆盖率**低于**基线时才算退化。
    B. **回退**——同一文件的实测覆盖率比基线低 `tolerance` 个百分点以上。

    `numeric_comparable=False` 时**跳过全部 [B] 数值判定**（但仍做 [A] 文件集
    判定）。用于「工具链/平台与基线不同」的情形：此时数字不可比，硬判必然
    假红。跳过时**必须显式打印原因**——静默放行等于没有门。
    """
    problems: list[str] = []
    cur_files = current["files"]
    base_files = base

    for name in sorted(set(base_files) - set(cur_files)):
        problems.append(
            f"[A] 基线里有 `{name}`，但本次实测的覆盖率报告里没有它"
            "（文件被删或改名 ⇒ 基线该刷新了）"
        )
    for name in sorted(set(cur_files) - set(base_files)):
        problems.append(
            f"[A] 本次实测新增了 `{name}`，但基线里没有它"
            "（新文件未并入基线 ⇒ 覆盖率被低估或高估，需刷新基线）"
        )

    if not numeric_comparable:
        return problems

    for name in sorted(set(cur_files) & set(base_files)):
        cur = pct(cur_files[name], "lines")
        bas = base_pct(base_files[name], "lines")
        if cur < bas - tolerance:
            problems.append(
                f"[B] `{name}` 行覆盖率从基线 {bas:.2f}% 回退到 {cur:.2f}%"
                f"（超过容差 {tolerance:.1f}pp ⇒ 新增了未被测到的代码路径）"
            )

    # 总量：只用**行**覆盖率（按行数加权重算的真值），不看 functions
    # （后者受泛型单态化与内联决策影响，噪声大得多）。
    cur_total = float(current["totals"].get("lines", {}).get("percent", 0.0))
    base_total = baseline_total()
    if cur_total < base_total - tolerance:
        problems.append(
            f"[B] 总体行覆盖率 {cur_total:.2f}% 低于基线 {base_total:.2f}%"
            f"超过容差 {tolerance:.1f}pp"
        )
    return problems


def regenerate_md() -> None:
    """从基线 JSON 重写 `COVERAGE_BASELINE.md` 的**数字部分**（表格 + §1 三行）。

    为什么由 `--refresh` 一并调用、而不是让人手动改文档：
    刷新基线后文档必然失同步，而 `check_md_matches_json()` 会立刻判红。
    若把「改文档」留给人工，就有两个必然的坏结果：
      ① 人漏了 → 门禁红，但红的原因是「文档没跟上」而非「覆盖率退化」，
         噪声会稀释门禁的可信度；
      ② 人懒得逐个改 29 行 → 干脆手改一个大概的数 → **文档开始骗人**，
         而这正是本门存在的理由。
    故：机器生成的数字由机器回写，人只维护论证文字（§3/§4/§5）。

    **只替换数字，不动文字**：表头之前的说明、§3 的容差论证、§4 的读法、
    §5 的门禁说明全部原样保留——那些是判断，不能自动生成。
    """
    if not BASELINE_MD.is_file():
        return
    data = json.loads(BASELINE_JSON.read_text(encoding="utf-8"))
    files = data["files"]
    totals = data["_totals"]
    counts = data.get("_counts", {})

    rows = sorted(files.items(), key=lambda kv: kv[1]["lines"])
    table = [
        "| 模块 | 行覆盖率 | region 覆盖率 | 函数覆盖率 |",
        "|------|---------|-------------|-----------|",
    ]
    for name, e in rows:
        table.append(
            f"| `{name}` | {e['lines']:.2f}% | {e['regions']:.2f}% | {e['functions']:.2f}% |"
        )

    text = BASELINE_MD.read_text(encoding="utf-8")

    # §2 表格：从表头行起，消费连续的 `|` 行。
    header = "| 模块 | 行覆盖率 | region 覆盖率 | 函数覆盖率 |"
    if header in text:
        start = text.index(header)
        consumed = 0
        for ln in text[start:].split("\n"):
            if ln.startswith("|"):
                consumed += len(ln) + 1
            else:
                break
        text = text[:start] + "\n".join(table) + "\n" + text[start + consumed :]

    # §1 的总体三行。带绝对量（读者可核对分母）；缺 `_counts` 时退回纯百分比。
    def _row(label: str, key: str) -> str:
        pct_v = totals[key]
        c = counts.get(key)
        if c:
            return f"| {label} | {pct_v:.2f}%（{c['count']} 中 {c['covered']} 被覆盖） |"
        return f"| {label} | {pct_v:.2f}% |"

    text = re.sub(
        r"\| 总体行覆盖率 \| .*? \|",
        f"| 总体行覆盖率 | **{totals['lines']:.2f}%**"
        + (
            f"（{counts['lines']['count']} 行中 {counts['lines']['covered']} 行被覆盖） |"
            if counts.get("lines")
            else " |"
        ),
        text,
        count=1,
    )
    text = re.sub(
        r"\| 总体 region 覆盖率 \| .*? \|", _row("总体 region 覆盖率", "regions"), text, count=1
    )
    text = re.sub(
        r"\| 总体函数覆盖率 \| .*? \|", _row("总体函数覆盖率", "functions"), text, count=1
    )

    BASELINE_MD.write_text(text, encoding="utf-8")


def check_md_matches_json() -> list[str]:
    """基线 markdown 里的逐模块数字必须与 JSON 逐字一致。

    为什么要有这道：`docs/COVERAGE_BASELINE.md` 是读者看的，
    `docs/coverage_baseline.json` 是机器读的。两边不一致时，
    **至少有一边在骗人**——而读者只会看 markdown 那边。
    """
    if not BASELINE_MD.is_file():
        return [f"[C] 找不到基线文档 {BASELINE_MD.name}"]
    text = BASELINE_MD.read_text(encoding="utf-8")
    base = baseline_entries()
    problems: list[str] = []
    for name, entry in sorted(base.items()):
        want = entry.get("lines")
        if want is None:
            continue
        # markdown 行形如 `| `router.rs` | 118 | 92.37% | ... |`
        needle = f"{want:.2f}%"
        if needle not in text:
            problems.append(
                f"[C] `{BASELINE_MD.name}` 里找不到 `{name}` 的行覆盖率 {needle}"
                "（文档与 JSON 不一致 ⇒ 至少一边是假的）"
            )
    return problems


def self_test() -> int:
    """变异自测：三处注入必须全部判红。

    - ① 回退：把某文件的实测覆盖率压低 5pp ⇒ 必须判红；
    - ② 幽灵文件：基线里有、实测里没有 ⇒ 必须判红；
    - ③ markdown/JSON 不一致 ⇒ 必须判红。
    """
    base = baseline_entries()
    cur_files = {}
    for name, entry in base.items():
        cur_files[name] = {
            "lines": {"count": 100, "covered": 80, "percent": entry["lines"]},
            "regions": {"count": 200, "covered": 160, "percent": entry.get("regions", 0.0)},
            "functions": {"count": 50, "covered": 40, "percent": entry.get("functions", 0.0)},
        }
    # 总量按**基线的真值**取（不是现算的平均），否则注入 ① 的"总量回退"
    # 分支会因为基线总量口径不同而测不到自己。
    totals = {
        "lines": {
            "count": 100 * len(cur_files),
            "covered": 80 * len(cur_files),
            "percent": baseline_total(),
        }
    }

    probes: list[tuple[str, list[str]]] = []

    # ① 回退注入：把第一个文件的覆盖率压到基线 - 5pp
    victim = sorted(base)[0]
    regressed = {k: dict(v) for k, v in cur_files.items()}
    regressed[victim] = dict(regressed[victim])
    regressed[victim]["lines"] = dict(regressed[victim]["lines"])
    regressed[victim]["lines"]["percent"] = base[victim]["lines"] - 5.0
    regressed_total = dict(totals["lines"])
    regressed_total["percent"] = regressed_total["percent"] - 5.0
    probes.append(
        (
            "覆盖率回退",
            compare({"files": regressed, "totals": {"lines": regressed_total}}, base, TOLERANCE_PP),
        )
    )

    # ② 幽灵文件注入：基线里有、实测里没有
    missing = {k: v for k, v in cur_files.items() if k != sorted(base)[-1]}
    probes.append(
        (
            "基线文件缺失",
            compare({"files": missing, "totals": totals}, base, TOLERANCE_PP),
        )
    )

    # ③ markdown/JSON 不一致：临时把 JSON 里的一个数字改成别的
    saved = BASELINE_JSON.read_text(encoding="utf-8")
    tampered = json.loads(saved)
    first = sorted(tampered["files"])[0]
    tampered["files"][first]["lines"] = 1.23
    try:
        BASELINE_JSON.write_text(
            json.dumps(tampered, ensure_ascii=False, indent=2), encoding="utf-8"
        )
        probes.append(("markdown/JSON 不一致", check_md_matches_json()))
    finally:
        BASELINE_JSON.write_text(saved, encoding="utf-8")

    # ④ 工具链/平台不可比 ⇒ 数值判定被跳过，但**文件集判定仍生效**。
    # 这条注入锁的是「跳过」这个行为本身：它必须**只**跳过 [B]，
    # 不能顺带把 [A] 也跳过（否则「工具链不同」就成了绕过整道门的万能钥匙）。
    # 反向断言：注入同样的回退，在不可比模式下**不应**产生 [B] 问题。
    regressed_again = {k: dict(v) for k, v in cur_files.items()}
    regressed_again[victim] = dict(regressed_again[victim])
    regressed_again[victim]["lines"] = dict(regressed_again[victim]["lines"])
    regressed_again[victim]["lines"]["percent"] = base[victim]["lines"] - 5.0
    skipped = compare(
        {"files": regressed_again, "totals": {"lines": regressed_total}},
        base,
        TOLERANCE_PP,
        numeric_comparable=False,
    )
    if any(p.startswith("[B]") for p in skipped):
        probes.append(
            (
                "不可比模式未跳过数值判定",
                ["[B] 不可比模式下**不应**产生数值回退问题——跳过逻辑失效"],
            )
        )
    # 不可比模式下，文件集漂移仍必须判红（[A] 不受跳过影响）
    ghost = {k: v for k, v in cur_files.items() if k != sorted(base)[-1]}
    skipped_ghost = compare(
        {"files": ghost, "totals": totals},
        base,
        TOLERANCE_PP,
        numeric_comparable=False,
    )
    if not any(p.startswith("[A]") for p in skipped_ghost):
        probes.append(
            (
                "不可比模式误跳过文件集判定",
                ["[A] 不可比模式下文件集漂移**仍须**判红——跳过逻辑越界了"],
            )
        )

    green = [name for name, problems in probes if not problems]
    if green:
        print(
            f"[check-coverage-baseline] FAIL: 变异自测未判红——这些注入仍然通过：{green}",
            file=sys.stderr,
        )
        return 1
    print(
        "[check-coverage-baseline] self-test OK: 覆盖率回退 / 基线文件缺失 / "
        "markdown-JSON 不一致 / 不可比模式只跳过数值判定 四处注入全部正确判红。"
    )
    return 0


def main() -> int:
    ap = argparse.ArgumentParser()
    ap.add_argument("--json", type=Path, help="cargo llvm-cov --json 的输出路径")
    ap.add_argument("--refresh", action="store_true", help="用实测 JSON 刷新基线")
    ap.add_argument("--self-test", action="store_true", help="变异自测")
    args = ap.parse_args()

    if args.self_test:
        return self_test()

    if not BASELINE_JSON.is_file():
        print(f"[check-coverage-baseline] FAIL: 找不到基线 {BASELINE_JSON}", file=sys.stderr)
        return 1

    base = baseline_entries()

    if args.refresh:
        if not args.json:
            print("[check-coverage-baseline] FAIL: --refresh 需要 --json", file=sys.stderr)
            return 1
        current = load_json_cov(args.json)
        new_files = {}
        for name, summary in current["files"].items():
            new_files[name] = {
                "lines": pct(summary, "lines"),
                "regions": pct(summary, "regions"),
                "functions": pct(summary, "functions"),
            }
        cur_ver, cur_triple = detect_toolchain()
        payload = {
            "_comment": (
                "OPT-R18 P2-5 覆盖率基线。由 tools/check_coverage_baseline.py --refresh "
                "从 `cargo llvm-cov --json` 生成，勿手改。"
            ),
            "_tolerance_pp": TOLERANCE_PP,
            "_metric": "lines",
            "_tool": (
                "cargo-llvm-cov 0.9.1 + llvm-tools-preview"
                "（rustc 1.99.1 / MSVC x86_64-pc-windows-msvc）"
            ),
            "_command": "cargo llvm-cov --locked --summary-only --json",
            "_platform": cur_triple or "unknown",
            "_toolchain": cur_ver or "unknown",
            "_totals": {
                "lines": round(
                    float(current["totals"].get("lines", {}).get("percent", 0.0)), 4
                ),
                "regions": round(
                    float(current["totals"].get("regions", {}).get("percent", 0.0)), 4
                ),
                "functions": round(
                    float(current["totals"].get("functions", {}).get("percent", 0.0)), 4
                ),
            },
            # 绝对量（count / covered）也存：文档要写「N 行中 M 行被覆盖」，
            # 只有百分比时这句话无法复现（读者没法核对分母）。
            # 单独一个 `_counts` 键而不是塞进 `_totals`：`_totals` 的语义是
            # 「百分比」，混进计数会让 `baseline_total()` 的读法含糊。
            "_counts": {
                m: {
                    "count": int(current["totals"].get(m, {}).get("count", 0)),
                    "covered": int(current["totals"].get(m, {}).get("covered", 0)),
                }
                for m in ("lines", "regions", "functions")
            },
            "_files_count": len(new_files),
            "files": dict(sorted(new_files.items())),
        }
        BASELINE_JSON.write_text(
            json.dumps(payload, ensure_ascii=False, indent=2) + "\n", encoding="utf-8"
        )
        print(f"[check-coverage-baseline] 已刷新基线：{len(new_files)} 个文件")
        # 一并回写文档的数字部分：否则刷新后文档必然失同步，门禁立刻判红，
        # 而那个红是「文档没跟上」而非「覆盖率退化」——噪声会稀释门禁可信度。
        regenerate_md()
        print(f"[check-coverage-baseline] 已同步 {BASELINE_MD.name} 的表格与总体数字")
        base = payload["files"]

    problems = check_md_matches_json()

    if args.json:
        current = load_json_cov(args.json)

        # 工具链/平台可比性判定。**不可比时必须显式说明**，否则等于静默放行。
        meta = baseline_meta()
        cur_ver, cur_triple = detect_toolchain()
        skip_reason = ""
        numeric_comparable = True
        if meta["toolchain"] and cur_ver and meta["toolchain"] != cur_ver:
            numeric_comparable = False
            skip_reason = (
                f"基线工具链 `{meta['toolchain']}` 与当前 `{cur_ver}` 不同"
            )
        if meta["platform"] and cur_triple and meta["platform"] != cur_triple:
            numeric_comparable = False
            skip_reason = (
                (skip_reason + "；" if skip_reason else "")
                + f"基线平台 `{meta['platform']}` 与当前 `{cur_triple}` 不同"
            )
        if not cur_ver:
            numeric_comparable = False
            skip_reason = "探测不到当前 rustc 版本"

        if numeric_comparable:
            problems += compare(current, base, TOLERANCE_PP)
        else:
            # 只做可比的 [A] 文件集判定；[B] 数值判定不可比，明确跳过。
            problems += compare(current, base, TOLERANCE_PP, numeric_comparable=False)

        cur_total = current["totals"].get("lines", {}).get("percent", 0.0)
        base_total = baseline_total()
        if numeric_comparable:
            print(
                f"[check-coverage-baseline] 实测行覆盖率 {cur_total:.2f}% / "
                f"基线 {base_total:.2f}%（加权重算值，容差 {TOLERANCE_PP:.1f}pp）"
            )
        else:
            print(
                f"[check-coverage-baseline] 实测行覆盖率 {cur_total:.2f}%（仅供参考）"
                f"；基线 {base_total:.2f}%。"
            )
            print(
                f"[check-coverage-baseline] **跳过数值回退判定**：{skip_reason}。"
                "覆盖率数字跨工具链/跨平台不可比，强行判定必然假红。"
                "本次仅校验文件集一致性（[A]）与文档一致性（[C]）。"
            )
    else:
        print(
            "[check-coverage-baseline] 未提供 --json：仅校验基线与文档一致性"
            "（未做回退检测）"
        )

    if problems:
        print("[check-coverage-baseline] FAIL:", file=sys.stderr)
        for p in problems:
            print(f"  - {p}", file=sys.stderr)
        return 1

    print("[check-coverage-baseline] OK")
    return 0


if __name__ == "__main__":
    sys.exit(main())
