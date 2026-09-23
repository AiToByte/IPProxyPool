"""FreeProxy 直探对照（只读，不进池，不进生产）。
判据对齐 gateway/src/free_pool.rs Task 8：
TCP 建链 3s → 经代理 GET {base}/ip (/origin) ＋ /headers (7头) ＋ /anything/freepool-canary。
allowlist：仅 Geonode 候选 ip:port＋https://httpbin.org；其余一律不连。
Usage: python log/probe_free.py log/free_sample.json
"""
import json, socket, sys, time, urllib.request

BASE = "https://httpbin.org"
MARKER = "freepool-canary"
DISCLOSURE = {"forwarded", "x-forwarded-for", "x-real-ip", "client-ip", "via", "proxy-connection", "x-proxy-id"}
TIMEOUT = 3

def tcp_ok(ip, port):
    try:
        s = socket.create_connection((ip, int(port)), timeout=TIMEOUT)
        s.close()
        return True
    except Exception:
        return False

def via_proxy(ip, port, path):
    url = BASE + path
    req = urllib.request.Request(url, headers={"User-Agent": "FreePool-probe/1.0"})
    proxy = f"http://{ip}:{port}"
    opener = urllib.request.build_opener(urllib.request.ProxyHandler({"http": proxy, "https": proxy}))
    try:
        with opener.open(req, timeout=TIMEOUT + 2) as r:
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

def main():
    src = sys.argv[1] if len(sys.argv) > 1 else "log/free_sample.json"
    d = json.load(open(src, encoding="utf-8"))
    try:
        base = json.loads(urllib.request.urlopen(BASE + "/ip", timeout=5).read().decode("utf-8", "ignore")).get("origin", "")
    except Exception as e:
        base = ""
        print(f"baseline unreachable ({str(e)[:80]}), anon may be Unknown-first")
    rows = []
    for x in d.get("data", [])[:20]:
        protos = [str(v).lower() for v in (x.get("protocols") or [])]
        if not any(p in ("http", "https") for p in protos):
            continue
        ip, port, country = x.get("ip"), str(x.get("port") or ""), x.get("country") or "ZZ"
        try:
            octs = ip.split(".")
            assert len(octs) == 4 and all(0 <= int(o) <= 255 for o in octs) and 0 < int(port) < 65536
        except Exception:
            continue
        t0 = time.time()
        ok = tcp_ok(ip, port)
        tcp_ms = int((time.time() - t0) * 1000)
        if not ok:
            rows.append((ip, port, country, "tcp_fail", "-", "-", tcp_ms))
            continue
        t1 = time.time()
        ip_b = via_proxy(ip, port, "/ip")
        h_b = via_proxy(ip, port, "/headers")
        c_b = via_proxy(ip, port, f"/anything/{MARKER}")
        fwd_ms = int((time.time() - t1) * 1000)
        exit_ip = (ip_b.get("origin") or "").split(",")[0].strip() if isinstance(ip_b, dict) else ""
        headers = h_b.get("headers", {}) if isinstance(h_b, dict) else {}
        canary = (c_b.get("url", "") if isinstance(c_b, dict) else "")
        if MARKER not in str(canary):
            rows.append((ip, port, country, "full_fail(canary)", "-", "-", fwd_ms))
            continue
        if "_err" in ip_b:
            rows.append((ip, port, country, "full_fail", "-", "-", fwd_ms))
            continue
        anon = classify(base, exit_ip, headers)
        rows.append((ip, port, country, "pass", anon, exit_ip, fwd_ms))
    npass = sum(1 for r in rows if r[3] == "pass")
    print(f"candidates={len(rows)} pass={npass} rate={npass/len(rows) if rows else 0:.1%}")
    for r in rows:
        print(" | ".join(map(str, r)))
    with open("log/free_probe_report.md", "w", encoding="utf-8") as f:
        f.write(f"# FreeProxy 直探报告（{time.strftime('%Y-%m-%d %H:%M')}）\n\n")
        f.write(f"基址 {BASE}，基线 {base or 'unreachable'}，候选 {len(rows)}，通过 {npass}。\n\n")
        f.write("| ip | port | country | result | anon | exit | latency_ms |\n|---|---|---|---|---|---|---|\n")
        for r in rows:
            f.write("| " + " | ".join(map(str, r)) + " |\n")

if __name__ == "__main__":
    main()
