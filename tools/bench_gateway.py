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


def clean_env(extra: dict | None = None) -> dict:
    """给子进程用的环境：显式剥离代理变量。

    # 为什么（OPT-R16 B 排查结论）
    Go 的 http.Client 在自建 Transport 时默认直连（Proxy=nil），不读环境——
    所以本轮的反常曲线**不是**代理造成的（已证伪）。但压测数据若经过用户的
    前置代理（如 Clash），测出的就是代理的产能而非被测组件，**必须**在源头
    掐掉这种可能，而不是靠"当前 shell 恰好没设"这种运气。
    urllib 则相反：默认读环境代理，故 urllib 探针必须显式 ProxyHandler({})。
    """
    env = dict(os.environ)
    for k in ("HTTP_PROXY", "HTTPS_PROXY", "http_proxy", "https_proxy",
              "ALL_PROXY", "all_proxy"):
        env.pop(k, None)
    # 127.0.0.1/localhost 永不走代理（双保险：客户端已直连，这里防其它工具）。
    cur = env.get("NO_PROXY", env.get("no_proxy", ""))
    need = {"localhost", "127.0.0.1"}
    have = {x.strip() for x in cur.split(",") if x.strip()}
    env["NO_PROXY"] = ",".join(sorted(have | need))
    env.pop("no_proxy", None)
    if extra:
        env.update(extra)
    return env


def ports_in_use(ports: list[int]) -> list[tuple[int, int]]:
    """返回 [(port, pid)]：这些端口已被占用。压测前必须全空。"""
    import socket as _socket
    busy = []
    for p in ports:
        s = _socket.socket(_socket.AF_INET, _socket.SOCK_STREAM)
        try:
            s.bind(("127.0.0.1", p))
        except OSError:
            busy.append((p, _port_pid(p)))
        finally:
            s.close()
    return busy


def _port_pid(port: int) -> int:
    try:
        import subprocess as _sp
        out = _sp.run(
            ["netstat", "-ano", "-p", "TCP"],
            capture_output=True, text=True, timeout=15).stdout
        for line in out.splitlines():
            if f"127.0.0.1:{port}" in line and "LISTENING" in line:
                parts = line.split()
                return int(parts[-1])
    except Exception:
        pass
    return -1


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
            env=clean_env(),
            creationflags=subprocess.CREATE_NEW_PROCESS_GROUP))
    time.sleep(1.5)
    for i in range(N_UP):
        try:
            # urllib 默认读环境代理，必须显式绕过（见 clean_env 文档）。
            nop = urllib.request.build_opener(urllib.request.ProxyHandler({}))
            with nop.open(f"http://127.0.0.1:{UP_BASE+i}/", timeout=4) as r:
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
    env = clean_env({
        "TEST_POOL_NODES": nodes,
        "GATEWAY_ADDR": f"127.0.0.1:{GW_PORT}",
        "METRICS_ADDR": f"127.0.0.1:{METRICS_PORT}",
        "API_KEY": "bench-key",
        "REQUIRE_API_KEY": "1",
        "FREE_ENABLED": "0",
        "GATEWAY_GRACE_SECS": "86400",
    })
    # 上一次的网关日志先归档，避免"失败现场被下一次运行覆盖"——
    # 本轮排查时就因为 gw.err 被覆写，丢掉了关键 runs 的拒因日志。
    try:
        if os.path.isfile(GW_LOG):
            prev = GW_LOG + ".prev"
            if os.path.isfile(prev):
                os.remove(prev)
            os.rename(GW_LOG, prev)
    except Exception:
        pass
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
        nop = urllib.request.build_opener(urllib.request.ProxyHandler({}))
        with nop.open(f"http://127.0.0.1:{METRICS_PORT}/metrics", timeout=2) as r:
            r.read()
            return True
    except Exception:
        return False


def run_client(target_port, host_port, conc, n, country=None, label="",
               api_key="bench-key"):
    """跑一次压测客户端。target_port 决定连谁，host_port 决定 Host 头。

    # 血的教训（OPT-R16 B）
    第一版把 `-target` 硬编码成网关端口，导致"地板测量"实际走的也是网关，
    测出的 floor=0 毫无意义，还误导了后续判断。此后 target 必须显式传入：
      - 地板：target=上游端口（直连，不经过网关）
      - 端到端：target=网关端口（经网关转发）
    """
    args = [BENCH_EXE, "client",
            "-target", f"http://127.0.0.1:{target_port}/",
            "-host-header", f"127.0.0.1:{host_port}",
            "-c", str(conc), "-n", str(n),
            "-mode", "host",
            "-label", label or f"conc={conc}"]
    if api_key:
        args += ["-api-key", api_key]
    if country:
        args += ["-country", country]
    env = clean_env()
    r = subprocess.run(args, capture_output=True, text=True, encoding="utf-8",
                       errors="replace", timeout=1800, env=env)
    return parse(r.stdout or "")


def wait_ready(timeout_s=90) -> bool:
    """就绪门：/metrics 可达 ≠ 可服务。

    # 为什么需要
    start_gateway 只等 /metrics 可达就返回，但网关的 8916 监听、候选池连接、
    Prober 首轮验证都可能还没完成。若阶梯立即开跑，前几档测的是"启动中"
    而非"稳态"，数据不可用。本轮就出现过"低并发全失败、高并发恢复"的
    反常曲线，事后无法区分是启动窗口还是真问题——因为没有就绪证据。

    # 做法
    用与正式压测完全相同的路径（Go 客户端 → 网关 → 上游）打小批量，
    连续 3 次全成功才放行；超时则打印网关日志尾部并失败。
    """
    log("  --- readiness: same path as the bench, small batches ---")
    deadline = time.time() + timeout_s
    streak = 0
    attempt = 0
    while time.time() < deadline:
        attempt += 1
        d = run_client(GW_PORT, UP_BASE, 2, 10,
                       country=UP_COUNTRY, label=f"ready#{attempt}")
        ok, tot = d.get("ok", 0), d.get("ok", 0) + d.get("err", 0)
        status = d.get("status", {})
        if tot == 10 and ok == 10:
            streak += 1
            log(f"    attempt {attempt}: 10/10 ok (streak {streak}/3)")
            if streak >= 3:
                log("    READY")
                return True
        else:
            log(f"    attempt {attempt}: {ok}/{tot} ok  status={status}  "
                f"errors={d.get('errors', [])[:1]}")
            streak = 0
        time.sleep(2.0)
    log(f"  [FAIL] gateway not ready within {timeout_s}s; tail of {GW_LOG}:")
    try:
        with open(GW_LOG, "r", encoding="utf-8", errors="replace") as f:
            for line in f.readlines()[-12:]:
                log(f"    {line.rstrip()}")
    except Exception:
        pass
    return False


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
    # 状态码分布与错误采样：失败诊断的第一手证据（见 main.go 注释）。
    m = re.search(r"status\s*=\s*map\[([^\]]*)\]", out)
    d["status"] = m.group(1).strip() if m else ""
    errs = re.findall(r"err\[\d+\]\s*=\s*(.+)", out)
    d["errors"] = [e.strip()[:160] for e in errs[:3]]
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

    # 启动前断言端口全空：残留的上游/网关会让新实例绑端口失败（或 urllib
    # 自检连到旧实例），而失败是静默的——随后所有数据都不可信。
    # 本轮排查时就遇到过"自检连到残留进程"的情形。
    need = [GW_PORT, METRICS_PORT] + [UP_BASE + i for i in range(N_UP)]
    busy = ports_in_use(need)
    if busy:
        log("  [FAIL] ports already in use; stop the stale processes first:")
        for port, pid in busy:
            log(f"    127.0.0.1:{port}  pid={pid}")
        return 1
    log(f"  ports {GW_PORT},{METRICS_PORT},{UP_BASE}..{UP_BASE+N_UP-1} all free")
    log()

    n = start_upstreams()
    log(f"  upstream: {n}/{N_UP} ready on {UP_BASE}..{UP_BASE+N_UP-1}")
    if n == 0:
        log("  [FAIL] no upstream reachable")
        return 1
    log()

    if not args.skip_floor:
        log("  --- floor: client -> upstream DIRECTLY (no gateway) ---")
        log("  （注意：第一版这里误把 target 写成网关端口，导致 floor 实际")
        log("   走的也是网关，floor=0 毫无意义。现已修正为直连上游。）")
        floors = []
        for conc in (64, 256):
            d = run_client(UP_BASE, UP_BASE, conc, conc * 200,
                           country=None, api_key="",
                           label=f"floor conc={conc}")
            if "qps" in d and d["qps"] > 0:
                floors.append(d["qps"])
                log(f"    conc={conc:4d}  QPS={d['qps']:10.1f}  "
                    f"ok={d.get('ok', 0)}  p99={d.get('p99', '-')}")
            else:
                log(f"    conc={conc:4d}  FAILED  status={d.get('status','')}  "
                    f"errors={d.get('errors', [])[:1]}")
        floor = max(floors) if floors else 0.0
        log(f"    floor = {floor:,.0f} QPS")
        if floor <= 0:
            log("  [FAIL] floor is zero: the client/upstream path itself is "
                "broken; e2e numbers would be meaningless. fix that first.")
            return 1
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
    log(f"  gateway up on 127.0.0.1:{GW_PORT} (/metrics reachable)")
    log()
    if not wait_ready():
        return 1
    log()

    log("  --- end-to-end: client -> gateway -> upstream ---")
    log(f"  (X-Proxy-Country: {UP_COUNTRY} pins routing to the injected pool;")
    log(f"   without it the gateway also picks its hard-coded dead nodes 8888-8890")
    log(f"   and 1.5s connect timeouts x3 retries dominate the measurement)")
    log()
    rows = []
    for conc in [int(x) for x in args.ladder.split(",")]:
        d = run_client(GW_PORT, UP_BASE, conc, conc * args.per_conc,
                       country=UP_COUNTRY, label=f"e2e conc={conc}")
        if "qps" not in d:
            log(f"  conc={conc:4d}  (no parsable output; gateway down?)  "
                f"status={d.get('status','')}  errors={d.get('errors', [])[:1]}")
            continue
        rows.append((conc, d))
        tot = d.get("ok", 0) + d.get("err", 0)
        rate = 100.0 * d["ok"] / tot if tot else 0.0
        extra = ""
        if rate < 100.0:
            extra = f"  status={d.get('status','')}  errors={d.get('errors', [])[:1]}"
        log(f"  conc={conc:4d}  QPS={d['qps']:9.1f}  success={rate:6.2f}%  "
            f"({d.get('ok',0)}/{tot})  p50={d.get('p50','-'):>9}  "
            f"p99={d.get('p99','-'):>9}{extra}")

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