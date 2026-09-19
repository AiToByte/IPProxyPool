"""GW-5a end-to-end load probe (Windows dev-box numbers, NOT prod claims).

Hammers the gateway with N concurrent workers, records status mix,
per-request latency percentiles, and aggregate QPS. Stdlib only.

Usage: load_probe.py [workers] [requests_per_worker]
  prints: statuses, P50/P99/max latency ms, elapsed s, QPS
"""
import statistics
import sys
import time
import urllib.request
from concurrent.futures import ThreadPoolExecutor

URL = "http://127.0.0.1:8080/"
WORKERS = int(sys.argv[1]) if len(sys.argv) > 1 else 32
PER_WORKER = int(sys.argv[2]) if len(sys.argv) > 2 else 100


def one(_):
    req = urllib.request.Request(URL, method="GET")
    t0 = time.perf_counter()
    try:
        with urllib.request.urlopen(req, timeout=15) as r:
            code = r.status
    except Exception as e:
        code = f"ERR:{type(e).__name__}"
    return code, (time.perf_counter() - t0) * 1000.0


def pct(data, p):
    if not data:
        return float("nan")
    s = sorted(data)
    return s[min(len(s) - 1, int(p / 100 * len(s)))]


total = WORKERS * PER_WORKER
codes: dict = {}
lat = []
t0 = time.perf_counter()
with ThreadPoolExecutor(max_workers=WORKERS) as ex:
    for code, ms in ex.map(one, range(total)):
        codes[code] = codes.get(code, 0) + 1
        lat.append(ms)
dt = time.perf_counter() - t0

print(f"requests={total} workers={WORKERS} elapsed_s={dt:.2f} qps={total / dt:.1f}")
print(f"status={codes}")
print(
    f"lat_ms p50={pct(lat, 50):.2f} p99={pct(lat, 99):.2f} "
    f"max={max(lat):.2f} mean={statistics.mean(lat):.2f}"
)
