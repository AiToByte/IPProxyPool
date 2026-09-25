"""IPProxyPool 本地前置适配器（USE-便捷落地 U1，只跑 localhost，不进生产）。

背景：网关是反向式 egress 路由（origin-form＋Host 定上游），标准代理形态
（absolute-URI）被 Pingora 以 400 拒绝——浏览器/系统代理/标准 HTTP_PROXY
直连网关不可用。本适配器做最小翻译：
  absolute-URI（GET http://host:port/path）→ origin-form（GET /path＋Host: host:port）→ 网关 :8916
  origin-form 直转（补 Host  unchanged）
  转发表头白名单：X-Api-Key/X-Proxy-Country/X-Proxy-Session/X-Proxy-Tier/X-Proxy-Proto
  （与网关 parse_routing_spec 同名；误名头会被网关忽略）
  （网关选择语义）＋Content-Type/Length/Host/Authorization 等常规头透传。
诚实限制：CONNECT→501；chunked 分块读透传（算不出总长仍 501 注明原因）；超上限（默认 10MB，env IPP_MAX_BODY_BYTES 可配）→ 413；只服务 127.0.0.1。

Usage: python tools/ipp_forward.py [listen_port] [gateway_host] [gateway_port]
  default: 127.0.0.1:18080 -> 127.0.0.1:8916
验证：curl.exe -x http://127.0.0.1:18080 http://127.0.0.1:8888/  # 经网关命中 mock
"""
import os
import sys
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from urllib.parse import urlsplit
import http.client

LISTEN_PORT = int(sys.argv[1]) if len(sys.argv) > 1 else 18080
GW_HOST = sys.argv[2] if len(sys.argv) > 2 else "127.0.0.1"
GW_PORT = int(sys.argv[3]) if len(sys.argv) > 3 else 8916
# 请求体上限 env 可配（默认 10MB，保持缺省行为不变）。
MAX_BODY = int(os.environ.get("IPP_MAX_BODY_BYTES", str(10 * 1024 * 1024)))

HOP_BY_HOP = {
    "connection", "keep-alive", "proxy-authenticate", "proxy-authorization",
    "te", "trailer", "transfer-encoding", "upgrade", "proxy-connection",
}
# 头名以网关 parse_routing_spec 为准（X-Proxy-Country/Session/Tier/Proto＋X-Api-Key；
# 曾误用 X-Session-Id/X-Tenant-Country（网关不认），A5 live 复验抓获，已修正）。
PASS_HEADERS = {
    "x-api-key", "x-proxy-country", "x-proxy-session",
    "x-proxy-tier", "x-proxy-proto",
}


def _read_chunked_body(rfile, max_bytes):
    # chunked 分块读透传：按块读 rfile（hex 长度行＋块数据＋CRLF，0 块后吞 trailer）。
    # 全量缓冲后才能算出总长（内存安全：累计超限即抛 OverflowError，外层转 413）。
    chunks = []
    total = 0
    while True:
        line = rfile.readline(8192)
        if not line:
            raise ValueError("truncated chunk-size line")
        s = line.decode("iso-8859-1").strip().split(";", 1)[0].strip()
        if s == "":
            continue
        try:
            size = int(s, 16)
        except ValueError:
            raise ValueError(f"bad chunk-size {s!r}")
        if size == 0:
            # 吞掉 trailer 头直到空行（界定块结束）。
            while True:
                t = rfile.readline(8192)
                if not t or t in (b"\r\n", b"\n", b""):
                    break
            break
        if total + size > max_bytes:
            raise OverflowError(f"body over {max_bytes} bytes cap")
        data = rfile.read(size)
        if len(data) < size:
            raise ValueError("truncated chunk data")
        chunks.append(data)
        total += size
        crlf = rfile.read(2)
        if crlf != b"\r\n":
            raise ValueError("missing CRLF after chunk")
    return b"".join(chunks)


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
        te = self.headers.get("Transfer-Encoding", "")
        is_chunked = "chunked" in te.lower()
        chunked_body = None
        if is_chunked:
            # chunked 分块读透传：按块读 rfile，全量缓冲后算出总长再转 Content-Length。
            # 算不出总长（截断/坏块/CRLF 缺失）仍诚实 501 并注明原因；超限转 413。
            try:
                chunked_body = _read_chunked_body(self.rfile, MAX_BODY)
            except OverflowError:
                self._send_error(413, f"body over {MAX_BODY} bytes cap")
                return
            except ValueError as e:
                self._send_error(501, f"chunked decode failed ({e}); resend with Content-Length")
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
        if chunked_body is not None:
            # chunked 已全量缓冲，能算出总长则设 Content-Length 转给网关。
            body = chunked_body
        else:
            # 非 chunked：Content-Length 未知（缺失）时不主动读 rfile，
            # 以 Connection: close 界定连接结束（避免粘包/阻塞）；有长度则按长度读。
            try:
                length = int(self.headers.get("Content-Length") or 0)
            except ValueError:
                self._send_error(400, "bad Content-Length")
                return
            if length > MAX_BODY:
                # 请求体超上限（默认 10MB，env IPP_MAX_BODY_BYTES 可配）：诚实 413。
                self._send_error(413, f"body over {MAX_BODY} bytes cap")
                return
            body = self.rfile.read(length) if length > 0 else None
        out = {"Host": host}
        for k, v in self.headers.items():
            kl = k.lower()
            if kl in HOP_BY_HOP or kl == "host":
                continue
            if kl in PASS_HEADERS or kl in ("content-type", "content-length", "authorization", "user-agent", "accept"):
                out[k] = v
        if chunked_body is not None:
            out["Content-Length"] = str(len(body))
        try:
            conn = http.client.HTTPConnection(GW_HOST, GW_PORT, timeout=30)
            conn.request(self.command, path, body=body, headers=out)
            resp = conn.getresponse()
            # 响应侧保持整包读现状（resp.read 全量后转发）：请求侧改动已够 E4，流式改响应收益小且超 60 行才动，故不动。
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
