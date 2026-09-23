"""FreeProxy 大样本直探（20并发，只读，不进池）。
Usage: python log/probe_free_big.py log/free_big_sample.json
allowlist：仅快照候选 ip:port＋https://httpbin.org。
"""
import json, socket, sys, time, urllib.request
from concurrent.futures import ThreadPoolExecutor

BASE = "https://httpbin.org"
MARKER = "freepool-canary"
DISCLOSURE = {"forwarded", "x-forwarded-for", "x-real-ip", "client-ip", "via", "proxy-connection", "x-proxy-id"}
TCP_TIMEOUT = 3
HTTP_TIMEOUT = 5
WORKERS = 20

def tcp_ok(ip, port):
    try:
        s = socket.create_connection((ip, int(port)), timeout=TCP_TIMEOUT)
        s.close()
        return True
    except Exception:
        return False

def via_proxy(ip, port, path):
    req = urllib.request.Request(BASE + path, headers={"User-Agent": "FreePool-probe/1.0"})
    proxy = f"http://{ip}:{port}"
    opener = urllib.request.build_opener(urllib.request.ProxyHandler({"http": proxy, "https": proxy}))
    try:
        with opener.open(req, timeout=HTTP_TIMEOUT) as r:
            return json.loads(r.read().decode("utf-8", "ignore"))
    except Exception as e:
        return {"_err": str(e)[:120]}

def classify(baseline, exit_ip, headers):
    if not exit_ip:
        return "Unknown"
    if exit_ip.strip() == (baseline or "").strip():
        return "Transparent"
    low = {str(k).lower() for k in (headers or {})}
    return "Anonymous" if (low & DISCLOSURE) else "Elite"

def probe_one(baseline, x):
    ip, port, country = x["ip"], str(x["port"]), x["country"]
    t0 = time.time()
    if not tcp_ok(ip, port):
        return (ip, port, country, "tcp_fail", "-", "-", int((time.time() - t0) * 1000))
    t1 = time.time()
    ip_b = via_proxy(ip, port, "/ip")
    h_b = via_proxy(ip, port, "/headers")
    c_b = via_proxy(ip, port, f"/anything/{MARKER}")
    fwd_ms = int((time.time() - t1) * 1000)
    exit_ip = (ip_b.get("origin") or "").split(",")[0].strip() if isinstance(ip_b, dict) else ""
    headers = h_b.get("headers", {}) if isinstance(h_b, dict) else {}
    canary = (c_b.get("url", "") if isinstance(c_b, dict) else "")
    if MARKER not in str(canary) or "_err" in ip_b:
        return (ip, port, country, "full_fail", "-", "-", fwd_ms)
    return (ip, port, country, "pass", classify(baseline, exit_ip, headers), exit_ip, fwd_ms)

def main():
    src = sys.argv[1] if len(sys.argv) > 1 else "log/free_big_sample.json"
    d = json.load(open(src, encoding="utf-8"))
    try:
        # VPN-IMMUNE：同 probe_free.py（空 ProxyHandler 禁系统代理，真直连基线）。
        direct = urllib.request.build_opener(urllib.request.ProxyHandler({}))
        base = json.loads(direct.open(BASE + "/ip", timeout=5).read().decode("utf-8", "ignore")).get("origin", "")
    except Exception as e:
        base = ""
        print(f"baseline unreachable ({str(e)[:80]})")
    cands = []
    for x in d.get("data", [])[:100]:
        protos = [str(v).lower() for v in (x.get("protocols") or [])]
        if not any(p in ("http", "https") for p in protos):
            continue
        try:
            octs = str(x.get("ip")).split(".")
            assert len(octs) == 4 and all(0 <= int(o) <= 255 for o in octs) and 0 < int(str(x.get("port") or "0")) < 65536
        except Exception:
            continue
        cands.append({"ip": x.get("ip"), "port": str(x.get("port")), "country": x.get("country") or "ZZ"})
    rows = []
    with ThreadPoolExecutor(max_workers=WORKERS) as ex:
        for r in ex.map(lambda x: probe_one(base, x), cands):
            rows.append(r)
    npass = sum(1 for r in rows if r[3] == "pass")
    nelite = sum(1 for r in rows if len(r) > 4 and r[4] == "Elite")
    print(f"candidates={len(rows)} pass={npass} elite={nelite} rate={npass/len(rows) if rows else 0:.1%}")
    for r in rows:
        print(" | ".join(map(str, r)))
    with open("log/free_big_report.md", "w", encoding="utf-8") as f:
        f.write(f"# FreeProxy 大样本直探报告（{time.strftime('%Y-%m-%d %H:%M')}）\n\n")
        f.write(f"基址 {BASE}，基线 {base or 'unreachable'}，候选 {len(rows)}，通过 {npass}，Elite {nelite}。\n\n")
        f.write("| ip | port | country | result | anon | exit | latency_ms |\n|---|---|---|---|---|---|---|\n")
        for r in rows:
            f.write("| " + " | ".join(map(str, r)) + " |\n")

if __name__ == "__main__":
    main()
