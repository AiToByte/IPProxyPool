"""P2-8 本地确定性 E2E 之免费源冒充（只跑 localhost，不进生产）。
GET /list.txt -> "127.0.0.1:<relay_port> socks5\\n"（一行一个 socks5 节点）。
Usage: free_list_server.py <listen_port> <relay_port>
"""
import sys
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

LISTEN = int(sys.argv[1]) if len(sys.argv) > 1 else 18080
RELAY = sys.argv[2] if len(sys.argv) > 2 else "1099"
BODY = f"127.0.0.1:{RELAY} socks5\n".encode()


class H(BaseHTTPRequestHandler):
    def do_GET(self):
        self.send_response(200)
        self.send_header("Content-Length", str(len(BODY)))
        self.end_headers()
        self.wfile.write(BODY)

    def log_message(self, *a):
        pass


ThreadingHTTPServer(("127.0.0.1", LISTEN), H).serve_forever()
