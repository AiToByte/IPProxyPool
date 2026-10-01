#!/usr/bin/env python3
"""
free 池「抓取＋验证」可重复探针（OPT-R14 A 组）。

# 为什么需要它

2026-09-30 我手工验证过一次 free 池：Geonode 抓到 11275 个候选、intake 截到
4000、`tcp_fail` 2649、**30 个真实免费代理验证通过并入池**。但那一次的成本是
**约 40 分钟人工盯日志**，且结论散落在聊天里，无法被别人复现。

本脚本把那次过程收敛成**一条命令**，并输出**结构化判定**。

# 刻意不做的事：不断言节点数阈值

这是本脚本最重要的设计约束。公网免费代理的存活率是**天然churn**的量：实测
两轮分别有 30 / 22 个通过验证，且逐轮波动。任何形如"healthy >= N"的断言
都会**周期性假红**，而一个周期性假红的门禁会被运维学会忽略——那是比没有门
更糟的结果（本仓 R11 已吃过"并发测试整轮不命中就永远绿"的教训）。

故本脚本**只报告事实并分类判定**，把"多少个算正常"的判断权交回给人：

    FETCH_FAIL    没有任何源产出候选           → 抓取链路坏了（值得查）
    DEGRADED_ZERO 抓到了但 0 个通过验证         → 公网常态，**不是缺陷**
    VERIFIED      有节点通过验证并入池           → 链路完好
    INCONCLUSIVE  进程中途退出/指标不可达/源全挂 → **不下结论**

`INCONCLUSIVE` 是刻意保留的一等公民：本机实测中网关会在约 5~6 分钟后自行退出
（见 EXEC_LOG 步骤 38 补充，关联那个"异步上下文内 drop tokio runtime"的 P0），
此时若脚本硬报 FAIL，等于用环境问题污染代码结论。

# 它**不能**回答的问题：重分布

本探针只覆盖 free 池的**供给侧**（抓取→验证→入池）。
**bandit 在真实 free 池规模下的重分布仍未验证**——那需要把每个请求归因到
具体出口节点，而本机 Docker/ClickHouse 不可用 ⇒ 遥测降级 ⇒ 无法归因。
本脚本对此**不提供任何间接结论**。

# 用法

    # 探一个已在跑的网关
    python tools/probe_free_pool.py --metrics http://127.0.0.1:9221/metrics

    # 顺带把网关拉起来（Detached），等一个抓取周期后自动判定
    python tools/probe_free_pool.py --launch --wait 420

# 退出码

- 0：`VERIFIED` 或 `DEGRADED_ZERO`（供给侧链路可用）
- 1：`FETCH_FAIL`（抓取链路坏了）
- 2：`INCONCLUSIVE`（环境/进程问题，**不下代码结论**）
"""

import argparse
import collections
import re
import subprocess
import sys
import time
import urllib.request
from pathlib import Path

REPO = Path(__file__).resolve().parent.parent

# verify 结果分类：与 free_pool.rs 的 note_free_verify 调用点保持一致。
VERIFY_RESULTS = ("pass", "tcp_fail", "full_fail", "geo_fail")


def parse_prometheus(text: str) -> dict[str, list[tuple[dict, float]]]:
    """把 Prometheus 文本格式解析成 {metric: [(labels, value), ...]}。"""
    out: dict[str, list[tuple[dict, float]]] = collections.defaultdict(list)
    for line in text.split("\n"):
        line = line.strip()
        if not line or line.startswith("#"):
            continue
        m = re.match(r"^([a-zA-Z_:][a-zA-Z0-9_:]*)(\{(.*)\})?\s+(.+)$", line)
        if not m:
            continue
        name, _, labelblob, value = m.groups()
        try:
            val = float(value)
        except ValueError:
            continue
        labels: dict = {}
        if labelblob:
            for pair in re.findall(r'(\w+)="([^"]*)"', labelblob):
                labels[pair[0]] = pair[1]
        out[name].append((labels, val))
    return out


def fetch_metrics(url: str, timeout: float = 6.0) -> str | None:
    try:
        with urllib.request.urlopen(url, timeout=timeout) as r:
            return r.read().decode("utf-8", "replace")
    except Exception:
        return None


def summarize(metrics: dict) -> dict:
    """把关心的 free 池指标压成一份可读摘要。"""
    s: dict = {}
    g = metrics.get("free_pool_nodes_total", [])
    s["nodes_total"] = g[0][1] if g else 0.0

    yld = metrics.get("free_pool_source_yield_total", [])
    s["yield_by_source"] = {lb.get("source", "?"): v for lb, v in yld}
    s["yield_total"] = sum(s["yield_by_source"].values())

    ver = metrics.get("free_pool_verify_total", [])
    s["verify"] = collections.Counter({lb.get("result", "?"): v for lb, v in ver})
    s["verify_seen"] = set(s["verify"].keys())
    s["verify_total"] = sum(s["verify"].values())

    susp = metrics.get("free_pool_source_suspended", [])
    s["suspended"] = {lb.get("source", "?"): v for lb, v in susp if v > 0}

    cap = metrics.get("free_pool_intake_capped_total", [])
    s["intake_capped"] = cap[0][1] if cap else 0.0
    return s


def classify(s: dict, alive: bool) -> tuple[str, str]:
    if not alive:
        return "INCONCLUSIVE", "指标端点不可达（网关未运行或已退出）——不下代码结论"
    if s["yield_total"] <= 0:
        return (
            "FETCH_FAIL",
            "没有任何源产出候选。区分「本机无公网」与「源全挂」："
            f"被暂停的源={sorted(s['suspended']) or '无'}",
        )
    if s["nodes_total"] > 0:
        return (
            "VERIFIED",
            f"有 {s['nodes_total']:.0f} 个节点通过验证并入池 —— 供给侧链路完好",
        )
    if s["verify_total"] <= 0:
        return (
            "DEGRADED_ZERO",
            f"已抓到 {s['yield_total']:.0f} 个候选，但**验证还没开始**"
            f"（verify 计数为 0）。验证需数分钟，请加 --wait 重跑；"
            "此时不应把它当作结论",
        )
    return (
        "DEGRADED_ZERO",
        f"抓到 {s['yield_total']:.0f} 个候选、验证 {s['verify_total']:.0f} 次"
        f"（{dict(s['verify'])}）但 0 个入池。公网免费代理存活率天然 churn，"
        "**这不是缺陷**，不要据此改代码",
    )


def launch_gateway(metrics_port: int, gw_port: int) -> subprocess.CompletedProcess:
    """按仓库铁律经 log/launch_detached.py 拉起网关（禁 Get-NetTCPConnection）。"""
    launcher = REPO / "log" / "launch_detached.py"
    if not launcher.is_file():
        return subprocess.CompletedProcess([], 1, "", f"缺 {launcher}")
    exe = REPO / "gateway" / "target" / "release" / "pingora-proxy-gateway.exe"
    if not exe.is_file():
        return subprocess.CompletedProcess([], 1, "", f"缺 {exe}（先 cargo build --release）")
    return subprocess.run(
        [sys.executable, str(launcher), str(exe),
         str(REPO / "log" / "freeprobe.out"), str(REPO / "log" / "freeprobe.err")],
        capture_output=True, text=True,
        env={**__import__("os").environ,
             "METRICS_ADDR": f"127.0.0.1:{metrics_port}",
             "GATEWAY_ADDR": f"127.0.0.1:{gw_port}",
             "FREE_ENABLED": "1"},
    )


def main() -> int:
    ap = argparse.ArgumentParser(description="free 池抓取+验证可重复探针")
    ap.add_argument("--metrics", default=None,
                    help="网关 metrics 端点（默认由 --metrics-port 推导）")
    ap.add_argument("--launch", action="store_true", help="先拉起网关再探")
    ap.add_argument("--wait", type=int, default=0, help="拉起后轮询等待秒数")
    ap.add_argument("--metrics-port", type=int, default=9221)
    ap.add_argument("--gw-port", type=int, default=8942)
    args = ap.parse_args()

    # 【自坑记录】首版把「网关监听端口」(--metrics-port) 与「探针轮询的 URL」
    # (--metrics) 设成两个独立参数且各带默认值，于是 `--metrics-port 9230`
    # 只改了被拉起网关的监听口，探针仍去轮询默认的 9221 ⇒ 必然 INCONCLUSIVE。
    # 教训：**同一条信息的两个表述必须联动**，否则用户改了一个却不知道另一个
    # 没跟着改——这类缺陷不报错、只是安静地给出错误答案，比崩溃更糟。
    if args.metrics is None:
        args.metrics = f"http://127.0.0.1:{args.metrics_port}/metrics"

    if args.launch:
        r = launch_gateway(args.metrics_port, args.gw_port)
        print(f"[freeprobe] launch -> {r.stdout.strip() or r.stderr.strip()}")
        print(f"[freeprobe] 轮询 {args.metrics}")
        if r.returncode != 0:
            print("[freeprobe] INCONCLUSIVE: 网关拉起失败", file=sys.stderr)
            return 2

    # 等到「**有实质数据**」再判定，而不是指标一上线就取样。
    #
    # 【自坑记录】首版只等「端口可连」，于是网关刚启动的第一个空样本就会被
    # 当成结论 → DEGRADED_ZERO（0 产出 / 0 验证）。这会把「还没开始抓」误报成
    # 「抓不到」。判据：**产出或验证至少有一项 > 0**，即真的跑过了。
    need_data = args.wait > 0
    deadline = time.time() + max(args.wait, 0)
    text, s = None, None
    while True:
        raw = fetch_metrics(args.metrics)
        if raw is not None:
            cand = summarize(parse_prometheus(raw))
            # 非空即接受（覆盖「已跑过一轮、结果为 0」这一有意义的 DEGRADED_ZERO）
            if (not need_data) or cand["yield_total"] > 0 or cand["verify_total"] > 0:
                text, s = raw, cand
                break
        if time.time() >= deadline:
            if raw is not None:
                text, s = raw, summarize(parse_prometheus(raw))
            break
        time.sleep(10)

    if text is None or s is None:
        print("[freeprobe] 指标端点不可达（网关未运行/已退出）", file=sys.stderr)
        print("[freeprobe] 判定 INCONCLUSIVE —— 不下代码结论"
              "（注意：本机已实测网关约 5~6 分钟后自行退出，见 EXEC_LOG 步骤 38 补充）",
              file=sys.stderr)
        return 2

    verdict, why = classify(s, True)

    print("=" * 66)
    print("free 池供给侧探针（抓取 → 验证 → 入池）")
    print("=" * 66)
    print(f"  源产出候选总数        : {s['yield_total']:.0f}  {s['yield_by_source'] or '（无）'}")
    print(f"  intake 截断累计      : {s['intake_capped']:.0f}")
    print(f"  验证次数             : {s['verify_total']:.0f}  {dict(s['verify']) or '（无）'}")
    print(f"  看到的 verify 分类   : {sorted(s['verify_seen']) or '（无）'}")
    print(f"  被暂停的源           : {sorted(s['suspended']) or '（无）'}")
    print(f"  **入池节点数**       : {s['nodes_total']:.0f}")
    print()
    print(f"  判定: {verdict}")
    print(f"  理由: {why}")
    print()
    print("  本探针**刻意不断言节点数阈值**（公网代理 churn 天然，任何数量门都会假红）。")
    print("  本探针**不覆盖** bandit 重分布（需按出口节点归因，依赖 ClickHouse 遥测）。")
    print(f"  参考：verify 分类应为 {list(VERIFY_RESULTS)} 之一。")

    if verdict == "FETCH_FAIL":
        return 1
    if verdict == "INCONCLUSIVE":
        return 2
    return 0


if __name__ == "__main__":
    sys.exit(main())
