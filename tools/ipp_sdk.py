"""IPProxyPool Python SDK（USE-便捷落地 U2，stdlib 零依赖）。

网关是反向式 egress 路由：请求打到网关地址，真实上游放 Host 头。
本 SDK 做最小封装：URL 拆分、粘滞 session、tier/proto 选择、503 延迟重试。

Usage:
    from ipp_sdk import IPPClient
    c = IPPClient("http://127.0.0.1:8916", session="job-42")
    status, body = c.get("http://httpbin.org/ip")
Self-test:
    python tools/ipp_sdk.py --self-test   # 普通200＋粘滞＋坏Key403
"""
import sys
import time
import urllib.request
import urllib.error
from urllib.parse import urlsplit


class IPPClient:
    def __init__(self, gateway="http://127.0.0.1:8916", api_key=None,
                 session=None, country=None, tier=None, proto=None,
                 timeout=10):
        parts = urlsplit(gateway)
        self.gw_host = parts.hostname or "127.0.0.1"
        self.gw_port = parts.port or 80
        self.base = f"http://{self.gw_host}:{self.gw_port}"
        self.api_key = api_key
        self.session = session
        self.country = country
        self.tier = tier
        self.proto = proto
        self.timeout = timeout

    def _headers_for(self, target_host):
        # 头名与网关 parse_routing_spec 严格同名（X-Proxy-Session/Country…；
        # 误名头会被网关静默忽略，A5 live 复验抓获过 X-Session-Id 版）。
        h = {"Host": target_host, "User-Agent": "ipp-sdk/1.0"}
        if self.api_key:
            h["X-Api-Key"] = self.api_key
        if self.session:
            h["X-Proxy-Session"] = self.session
        if self.country:
            h["X-Proxy-Country"] = self.country
        if self.tier:
            h["X-Proxy-Tier"] = self.tier
        if self.proto:
            h["X-Proxy-Proto"] = self.proto
        return h

    def get(self, url, extra_headers=None, retries=1):
        """经网关 GET 公网 URL。返回 (status, body_bytes)。503 延迟 1s 重试一次。"""
        t = urlsplit(url)
        if t.scheme != "http":
            raise ValueError("only plain http targets are supported (no CONNECT tunneling)")
        target_host = t.netloc
        path = t.path or "/"
        if t.query:
            path += "?" + t.query
        headers = self._headers_for(target_host)
        headers.update(extra_headers or {})
        last = (0, b"")
        for attempt in range(retries + 1):
            req = urllib.request.Request(self.base + path, headers=headers, method="GET")
            try:
                with urllib.request.urlopen(req, timeout=self.timeout) as r:
                    return (r.status, r.read())
            except urllib.error.HTTPError as e:
                last = (e.code, e.read())
                if e.code == 503 and attempt < retries:
                    time.sleep(1)
                    continue
                return last
            except Exception as e:
                last = (0, str(e)[:200].encode())
                if attempt < retries:
                    time.sleep(1)
                    continue
                return last
        return last


def self_test():
    gw = "http://127.0.0.1:8916"
    c = IPPClient(gw)
    s1, b1 = c.get("http://127.0.0.1:8888/")
    assert s1 == 200, f"plain expect 200, got {s1} {b1[:80]}"
    assert b"mock-" in b1, f"body must carry mock marker, got {b1[:80]}"
    # 粘滞确定性证明：country=US 约束下同 session 两次必中 mock-a-us。
    s = IPPClient(gw, session="sdk-selftest-1", country="US")
    _, b2 = s.get("http://127.0.0.1:8888/")
    _, b3 = s.get("http://127.0.0.1:8888/")
    assert b2 == b3 == b"mock-a-us", f"sticky must pin mock-a-us, got {b2[:16]!r} {b3[:16]!r}"
    bad = IPPClient(gw, api_key="bad")
    s4, _ = bad.get("http://127.0.0.1:8888/")
    assert s4 == 403, f"bad key expect 403, got {s4}"
    print(f"self-test OK: plain={s1} sticky={b2!r} badkey={s4}")


if __name__ == "__main__":
    if len(sys.argv) > 1 and sys.argv[1] == "--self-test":
        self_test()
    else:
        print(__doc__)
