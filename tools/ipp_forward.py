"""IPProxyPool 本地前置适配器（USE-便捷落地 U1，只跑 localhost，不进生产）。

背景：网关是反向式 egress 路由（origin-form＋Host 定上游），标准代理形态
（absolute-URI）被 Pingora 以 400 拒绝——浏览器/系统代理/标准 HTTP_PROXY
直连网关不可用。本适配器做最小翻译：
  absolute-URI（GET http://host:port/path）→ origin-form（GET /path＋Host: host:port）→ 网关 :8080
  origin-form 直转（补 Host  unchanged）
  转发表头白名单：X-Api-Key/X-Session-Id/X-Tenant-Country/X-Proxy-Tier/X-Proxy-Proto
  （网关选择语义）＋Content-Type/Length/Host/Authorization 等常规头透传。
诚实限制：CONNECT（HTTPS 隧道）→ 501；分块请求体（chunked）→ 501；
  请求体>10MB → 413；只服务 127.0.0.1（禁远程）。

Usage: python tools/ipp_forward.py [listen_port] [gateway_host] [gateway_port]
  default: 127.0.0.1:18080 -> 127.0.0.1:8080
验证：curl.exe -x http://127.0.0.1:18080 http://127.0.0.1:8888/  # 经网关命中 mock
"""
import sys
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from urllib.parse import urlsplit
import http.client

LISTEN_PORT = int(sys.argv[1]) if len(sys.argv) > 1 else 18080
GW_HOST = sys.argv[2] if len(sys.argv) > 2 else "127.0.0.1"
GW_PORT = int(sys.argv[3]) if len(sys.argv) > 3 else 8080
MAX_BODY = 10 * 1024 * 1024

HOP_BY_HOP = {
    "connection", "keep-alive", "proxy-authenticate", "proxy-authorization",
    "te", "trailer", "transfer-encoding", "upgrade", "proxy-connection",
}
PASS_HEADERS = {
    "x-api-key", "x-session-id", "x-tenant-country",
    "x-proxy-tier", "x-proxy-proto",
}


class H(BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"

    def _send_error(self, code, msg):
        body = msg.encode()
        self.send_response(code)
        self.send_header("Content-Type", "text/plain; charset=utf-8")
        self.send_header("Content-Length", str(len(body)))
        self.send_header("Connection", "close")
        self.end_headers()
        try:
            self.wfile.write(body)
        except (BrokenPipeError, ConnectionResetError):
            pass

    def _handle(self):
        if self.command == "CONNECT":
            # 网关不支持 CONNECT 隧道（Phase 2 显式 out）：诚实 501。
            self._send_error(501, "CONNECT not supported by IPProxyPool gateway; use plain HTTP or app-level integration (see docs/USAGE.md)")
            return
        if self.headers.get("Transfer-Encoding", "").lower() == "chunked":
            self._send_error(501, "chunked request body not supported by adaptor; resend with Content-Length")
            return
        raw_path = self.path
        if raw_path.startswith("http://") or raw_path.startswith("https://"):
            # 标准代理形态：拆出目标 Host＋path。
            parts = urlsplit(raw_path)
            host = parts.netloc
            path = parts.path or "/"
            if parts.query:
                path += "?" + parts.query
            if not host:
                self._send_error(400, "bad absolute URI")
                return
        else:
            # origin-form：Host 即上游（网关原生形态）。
            host = self.headers.get("Host", "")
            path = raw_path
            if not host:
                self._send_error(400, "missing Host")
                return
        try:
            length = int(self.headers.get("Content-Length") or 0)
        except ValueError:
            self._send_error(400, "bad Content-Length")
            return
        if length > MAX_BODY:
            self._send_error(413, "body over 10MB cap")
            return
        body = self.rfile.read(length) if length > 0 else None
        out = {"Host": host}
        for k, v in self.headers.items():
            kl = k.lower()
            if kl in HOP_BY_HOP or kl == "host":
                continue
            if kl in PASS_HEADERS or kl in ("content-type", "content-length", "authorization", "user-agent", "accept"):
                out[k] = v
        try:
            conn = http.client.HTTPConnection(GW_HOST, GW_PORT, timeout=30)
            conn.request(self.command, path, body=body, headers=out)
            resp = conn.getresponse()
            data = resp.read()
        except Exception as e:
            self._send_error(502, f"gateway unreachable: {str(e)[:160]}")
            return
        try:
            self.send_response(resp.status, resp.reason)
            for k, v in resp.getheaders():
                if k.lower() in HOP_BY_HOP:
                    continue
                self.send_header(k, v)
            if resp.getheader("Content-Length") is None:
                self.send_header("Content-Length", str(len(data)))
            self.send_header("Connection", "close")
            self.end_headers()
            self.wfile.write(data)
        except (BrokenPipeError, ConnectionResetError):
            pass

    do_GET = _handle
    do_POST = _handle
    do_PUT = _handle
    do_DELETE = _handle
    do_HEAD = _handle
    do_OPTIONS = _handle
    do_PATCH = _handle
    do_CONNECT = _handle

    def log_message(self, *a):
        sys.stderr.write(f"[adaptor] {self.command} {self.path} -> {GW_HOST}:{GW_PORT}\n")


if __name__ == "__main__":
    ThreadingHTTPServer(("127.0.0.1", LISTEN_PORT), H).serve_forever()
