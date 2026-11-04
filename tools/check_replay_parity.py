#!/usr/bin/env python3
r"""回放台 LinUCB 与生产实现的跨语言一致性门。

## 为什么需要这道门

`tools/routing_replay.py` 里的 `Arm` 是 `gateway/src/bandit.rs::BanditArm` 的
**镜像实现**。镜像的风险是单向的、而且极其隐蔽：

- 生产改了公式（比如调 `FREE_RISK_PREMIUM`、改探索项、动遗忘节拍），
  回放台不知道 ⇒ 报告里的「LinUCB 成功率」算的是**另一个策略**；
- 报告看起来完全正常，数字也会动，**没有任何东西会变红**；
- 而这份报告正是用来决定「选路改动值不值得合」的。

所以必须有东西在两侧之间做仲裁。仲裁基准由**生产实现自己生成**
（`cargo test -- --ignored --nocapture print_replay_parity_vectors`），
镜像侧只能去对齐它。

## 门做什么

1. 读 `tools/replay/linucb_parity.json`（Rust 生成的黄金向量）；
2. 用 `routing_replay` 的 `Arm` 逐案重算，逐位比对（容差 1e-12）；
3. 断言 fixture 覆盖了关键分支（free 溢价、遗忘、裁剪、大 t）；
4. **变异自测**：把 alpha 扰动 1e-3，必须判红——证明这道门不是恒绿的摆设。

## 用法

```bash
python3 tools/check_replay_parity.py
python3 tools/check_replay_parity.py --fixture other.json   # 变异自测
```

重新生成黄金向量（改公式后必须做）：

```bash
cd gateway && cargo test -- --ignored --nocapture print_replay_parity_vectors \
  | sed -n '/^{/,/^}$/p' > ../tools/replay/linucb_parity.json
```
"""

from __future__ import annotations

import argparse
import json
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
sys.path.insert(0, str(ROOT / "tools"))

# 复用回放台的实现，而不是在这里抄一份——否则就变成"两份镜像互相校验"，
# 两边同时错仍然全绿，正是本门要防的事。
from routing_replay import DEFAULT_ALPHA, Arm  # noqa: E402

DEFAULT_FIXTURE = ROOT / "tools" / "replay" / "linucb_parity.json"
TOLERANCE = 1e-12
#: 必须出现的关键分支；缺任何一条都说明 fixture 退化了（覆盖变窄而门仍绿）。
REQUIRED_CASES = {
    "fresh_dc",
    "fresh_free",
    "clamped_theta",
    "free_forgetting_at_1k",
    "large_t",
}


def expand_rewards(v) -> list[float]:
    """与 Rust 侧 `expand_rewards` 同语义：数组或 `{"value","times"}`。"""
    if isinstance(v, list):
        return [float(x) for x in v]
    if isinstance(v, dict):
        return [float(v["value"])] * int(v["times"])
    raise SystemExit(f"不支持的 rewards 形态：{type(v).__name__}")


def check(fixture: Path, alpha_override: float | None = None) -> int:
    doc = json.loads(fixture.read_text(encoding="utf-8"))
    alpha = float(doc["alpha"])
    if alpha_override is not None:
        alpha = alpha_override
    cases = doc["cases"]
    names = {c["name"] for c in cases}

    problems: list[str] = []
    missing = REQUIRED_CASES - names
    if missing:
        problems.append(f"fixture 缺少关键场景：{sorted(missing)}")
    if len(cases) < 6:
        problems.append(f"fixture 只有 {len(cases)} 个场景，至少 6 个")

    worst = 0.0
    worst_case = ""
    for c in cases:
        ctx = [float(x) for x in c["context"]]
        arm = Arm("10.0.0.1:8080", c["tier"])
        for r in expand_rewards(c["rewards"]):
            arm.update(ctx, r)
        got = arm.score(ctx, alpha, int(c["t"]))
        want = float(c["score"])
        diff = abs(got - want)
        if diff > worst:
            worst, worst_case = diff, c["name"]
        if diff > TOLERANCE:
            problems.append(f"{c['name']}: python={got:.17g} rust={want:.17g} diff={diff:.3e}")

    if problems:
        print("[check-replay-parity] FAIL", file=sys.stderr)
        for p in problems:
            print(f"  - {p}", file=sys.stderr)
        print(
            "  修法：让 Python 侧对齐生产公式，或（若生产确实改了）重新生成黄金向量：\n"
            "    cd gateway && cargo test -- --ignored --nocapture print_replay_parity_vectors \\\n"
            "      | sed -n '/^{/,/^}$/p' > ../tools/replay/linucb_parity.json",
            file=sys.stderr,
        )
        return 1

    print(
        f"[check-replay-parity] OK: {len(cases)} cases, alpha={alpha}, "
        f"max|diff|={worst:.2e} (worst `{worst_case}`)"
    )
    return 0


def main() -> int:
    ap = argparse.ArgumentParser()
    ap.add_argument("--fixture", type=Path, default=DEFAULT_FIXTURE)
    ap.add_argument(
        "--self-test",
        action="store_true",
        help="变异自测：扰动 alpha 必须判红（证明门有鉴别力）",
    )
    args = ap.parse_args()

    if not args.fixture.exists():
        print(f"[check-replay-parity] FAIL: 找不到 {args.fixture}", file=sys.stderr)
        return 1

    rc = check(args.fixture)
    if not args.self_test:
        return rc
    if rc != 0:
        print("[check-replay-parity] 基线已红，变异自测无意义", file=sys.stderr)
        return 1
    # 扰动 alpha：任何真实公式差异都应被抓到；若仍绿，说明比对根本没生效。
    mutated = check(args.fixture, alpha_override=DEFAULT_ALPHA + 1e-3)
    if mutated == 0:
        print(
            "[check-replay-parity] FAIL: 变异自测未判红——alpha 扰动 1e-3 仍通过，"
            "说明比对链路失效（门是摆设）",
            file=sys.stderr,
        )
        return 1
    print("[check-replay-parity] self-test OK: 扰动 alpha 后正确判红")
    return 0


if __name__ == "__main__":
    sys.exit(main())
