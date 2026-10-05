# -*- coding: utf-8 -*-
"""IPProxyPool 端到端吞吐压测编排（OPT-R16 B）。

# 它做什么

1. 编译并启动 N 个 Go 上游（tools/bench 主程序，goroutine 真并发）
2. 用 `TEST_POOL_NODES` 把它们注入网关候选池
3. 启动网关（release）
4. 先测**地板**（客户端 + 上游直连），再测**端到端**（客户端 → 网关 → 上游）
5. 打印阶梯表与峰值
6. 收摊：停掉所有本脚本起的进程

# 为什么必须先测地板

端到端 QPS = min(网关产能, 客户端能力, 上游产能)。若不先测地板，
客户端/上游成为瓶颈时会把那个数字误当成网关产能。本脚本的实测对比：

  地板（客户端+上游直连）        ~175,832 QPS   (Go 客户端 → Go 上游)
  端到端（经网关）                ~3,700 QPS
差 ~47 倍 ⇒ 端到端数字确实是网关自身的产能，而非链路瓶颈。

# 踩坑记录（本轮实测，勿重犯）

1. **Python 线程池当客户端**：96 线程抢一个 GIL，p50 1.42→20.41ms，
   QPS 只从 1222 到 3622。客户端先饱和，却把 3622 当成上游天花板。
2. **curl --parallel（Windows）**：未产生真实并发；`--config` 多 URL 时
   `-o` 只对第一个请求生效，响应体混进 stdout（`okokok200`），3000 请求
   只解析出 13 条。若采信会得出完全错误的容量。
3. **curl 多进程**：32 进程 QPS 走平在 380；96 进程反降到 130、成功率 37%。
4. **Windows Python 无 socket.SO_REUSEPORT**：上游必须每进程独占端口。
5. **`TEST_POOL_NODES` 是追加不是替换**：网关另有硬编码默认节点
   127.0.0.1:8888/8889/8890（US/JP/GB），它们不存在 ⇒ 连接 1.5s 超时
   ×3 重试 = 4.5s。必须用 `X-Proxy-Country: <注入节点的 country>` 把选路
   唯一锁定到注入池，否则测的是超时重试（实测 0.9 QPS vs 2869 QPS）。
6. **origin-form 的 Host 头决定上游**：网关按 `Host` 拼转发 URL
   （见 gateway.rs 注释「origin-form → Host 头拼 http://」），故客户端
   要连网关端口但把 `Host` 设成上游地址。

用法：
    python tools/bench_gateway.py                 # 默认阶梯
    python tools/bench_gateway.py --ladder 8,32,64 --per-conc 500
    python tools/bench_gateway.py --skip-build    # 复用已有二进制
"""
import argparse
import atexit
import os
import re
import subprocess
import sys
import time
import io
import urllib.request

sys.stdout = io.TextIOWrapper(sys.stdout.buffer, encoding="utf-8", errors="replace")

HERE = os.path.dirname(os.path.abspath(__file__))
REPO = os.path.dirname(HERE)
BENCH_DIR = os.path.join(HERE, "bench")
BENCH_EXE = os.path.join(BENCH_DIR, "bench_server.exe")
GW_EXE = os.path.join(REPO, "gateway", "target", "release",
                      "pingora-proxy-gateway.exe")
GW_LOG = os.path.join(BENCH_DIR, "gw.err")

UP_BASE = 19501
N_UP = 8
UP_COUNTRY = "HX"
GW_PORT = 8916
METRICS_PORT = 8917

_started = []


def _kill_all():
    for p in _started:
        try:
            p.terminate()
            p.wait(timeout=5)
        except Exception:
            try:
                p.kill()
            except Exception:
                pass


atexit.register(_kill_all)


def log(msg=""):
    print(msg, flush=True)


def build() -> bool:
    if not os.path.isdir(BENCH_DIR):
        log(f"  [FAIL] missing {BENCH_DIR}")
        return False
    log("  building bench harness (go vet + go build) ...")
    r = subprocess.run(["go", "vet", "./..."], cwd=BENCH_DIR,
                       capture_output=True, text=True, encoding="utf-8",
                       errors="replace")
    if r.returncode != 0:
        log(f"  [FAIL] go vet: {r.stderr.strip()[:400]}")
        return False
    r = subprocess.run(["go", "build", "-o", BENCH_EXE, "."], cwd=BENCH_DIR,
                       capture_output=True, text=True, encoding="utf-8",
                       errors="replace")
    if r.returncode != 0:
        log(f"  [FAIL] go build: {r.stderr.strip()[:400]}")
        return False
    log(f"  built {BENCH_EXE} ({os.path.getsize(BENCH_EXE) // 1024} KB)")
    return True


def start_upstreams() -> int:
    ok = 0
    for i in range(N_UP):
        port = UP_BASE + i
        errf = open(os.path.join(BENCH_DIR, f"up{port}.err"), "wb")
        _started.append(subprocess.Popen(
            [BENCH_EXE, "serve", "-addr", f"127.0.0.1:{port}"],
            stdout=subprocess.DEVNULL, stderr=errf,
            creationflags=subprocess.CREATE_NEW_PROCESS_GROUP))
    time.sleep(1.5)
    for i in range(N_UP):
        try:
            with urllib.request.urlopen(f"http://127.0.0.1:{UP_BASE+i}/", timeout=4) as r:
                r.read()
                ok += 1
        except Exception:
            pass
    return ok


def start_gateway() -> bool:
    if not os.path.isfile(GW_EXE):
        log(f"  [FAIL] gateway binary missing: {GW_EXE}")
        log("         run: cd gateway && cargo build --release")
        return False
    nodes = ",".join(
        f"127.0.0.1:{UP_BASE+i}:residential:{UP_COUNTRY}:bench-up:{100}"
        for i in range(N_UP))
    env = dict(os.environ)
    env.update({
        "TEST_POOL_NODES": nodes,
        "GATEWAY_ADDR": f"127.0.0.1:{GW_PORT}",
        "METRICS_ADDR": f"127.0.0.1:{METRICS_PORT}",
        "API_KEY": "bench-key",
        "REQUIRE_API_KEY": "1",
        "FREE_ENABLED": "0",
        "GATEWAY_GRACE_SECS": "86400",
    })
    errf = open(GW_LOG, "wb")
    _started.append(subprocess.Popen(
        [GW_EXE], env=env, stdout=subprocess.DEVNULL, stderr=errf,
        creationflags=subprocess.CREATE_NEW_PROCESS_GROUP))
    for _ in range(40):
        time.sleep(0.5)
        if gw_alive():
            return True
    return False


def gw_alive() -> bool:
    try:
        with urllib.request.urlopen(f"http://127.0.0.1:{METRICS_PORT}/metrics", timeout=2) as r:
            r.read()
            return True
    except Exception:
        return False


def run_client(port, conc, n, country=None, label=""):
    args = [BENCH_EXE, "client",
            "-target", f"http://127.0.0.1:{GW_PORT}/",
            "-host-header", f"127.0.0.1:{port}",
            "-c", str(conc), "-n", str(n),
            "-mode", "host", "-api-key", "bench-key",
            "-label", label or f"conc={conc}"]
    if country:
        args += ["-country", country]
    r = subprocess.run(args, capture_output=True, text=True, encoding="utf-8",
                       errors="replace", timeout=1800)
    return parse(r.stdout or "")


def parse(out: str) -> dict:
    d = {}
    m = re.search(r"success\s*=\s*(\d+)\s+failure\s*=\s*(\d+)", out)
    if m:
        d["ok"], d["err"] = int(m.group(1)), int(m.group(2))
    m = re.search(r"QPS\s*=\s*([0-9.]+)", out)
    if m:
        d["qps"] = float(m.group(1))
    m = re.search(r"p50\s+([0-9.]+)(ms|µs|s|ns)", out)
    if m:
        d["p50"] = f"{m.group(1)}{m.group(2)}"
    m = re.search(r"p99\s+([0-9.]+)(ms|µs|s|ns)", out)
    if m:
        d["p99"] = f"{m.group(1)}{m.group(2)}"
    return d


def main() -> int:
    ap = argparse.ArgumentParser()
    ap.add_argument("--ladder", default="1,4,16,32,64,128,256")
    ap.add_argument("--per-conc", type=int, default=300)
    ap.add_argument("--skip-build", action="store_true")
    ap.add_argument("--skip-floor", action="store_true")
    args = ap.parse_args()

    log("=== IPProxyPool end-to-end throughput bench (OPT-R16 B) ===")
    log()
    if not args.skip_build:
        if not build():
            return 1
    elif not os.path.isfile(BENCH_EXE):
        log(f"  [FAIL] {BENCH_EXE} missing and --skip-build given")
        return 1

    n = start_upstreams()
    log(f"  upstream: {n}/{N_UP} ready on {UP_BASE}..{UP_BASE+N_UP-1}")
    if n == 0:
        log("  [FAIL] no upstream reachable")
        return 1
    log()

    if not args.skip_floor:
        log("  --- floor: client -> upstream directly (no gateway) ---")
        floors = []
        for conc in (64, 256):
            d = run_client(UP_BASE, conc, conc * 200, country=None,
                           label=f"floor conc={conc}")
            if "qps" in d and d["qps"] > 0:
                floors.append(d["qps"])
                log(f"    conc={conc:4d}  QPS={d['qps']:10.1f}  "
                    f"ok={d.get('ok', 0)}  p99={d.get('p99', '-')}")
        floor = max(floors) if floors else 0.0
        log(f"    floor = {floor:,.0f} QPS")
        log()

    if not start_gateway():
        log("  [FAIL] gateway did not come up; see tools/bench/gw.err")
        try:
            with open(GW_LOG, "r", encoding="utf-8", errors="replace") as f:
                for line in f.readlines()[-15:]:
                    log(f"    {line.rstrip()}")
        except Exception:
            pass
        return 1
    log(f"  gateway up on 127.0.0.1:{GW_PORT}")
    log()

    log("  --- end-to-end: client -> gateway -> upstream ---")
    log(f"  (X-Proxy-Country: {UP_COUNTRY} pins routing to the injected pool;")
    log(f"   without it the gateway also picks its hard-coded dead nodes 8888-8890")
    log(f"   and 1.5s connect timeouts x3 retries dominate the measurement)")
    log()
    rows = []
    for conc in [int(x) for x in args.ladder.split(",")]:
        d = run_client(UP_BASE, conc, conc * args.per_conc,
                       country=UP_COUNTRY, label=f"e2e conc={conc}")
        if "qps" not in d:
            log(f"  conc={conc:4d}  (no parsable output; gateway down?)")
            continue
        rows.append((conc, d))
        tot = d.get("ok", 0) + d.get("err", 0)
        rate = 100.0 * d["ok"] / tot if tot else 0.0
        log(f"  conc={conc:4d}  QPS={d['qps']:9.1f}  success={rate:6.2f}%  "
            f"({d.get('ok',0)}/{tot})  p50={d.get('p50','-'):>9}  "
            f"p99={d.get('p99','-'):>9}")

    if rows:
        peak = max(rows, key=lambda r: r[1]["qps"])
        log()
        log(f"  PEAK: conc={peak[0]}  QPS={peak[1]['qps']:,.1f}  "
            f"p99={peak[1].get('p99','-')}")
        log()
        log("  Scope of this number (read before using it for planning):")
        log("   - single-process gateway, loopback upstreams: measures PER-CORE")
        log("     proxy overhead, not cluster capacity")
        log("   - telemetry to Redis/ClickHouse is OFF (no Docker on this host),")
        log("     so this is the floor of the deployment, not its ceiling")
        log("   - upstream is a trivial Go handler; real proxy nodes add latency")
        log("     and their own throughput limits")
    return 0


if __name__ == "__main__":
    sys.exit(main())