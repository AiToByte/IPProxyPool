import sys
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

port = int(sys.argv[1])
body = sys.argv[2].encode()


class H(BaseHTTPRequestHandler):
    def do_GET(self):
        self.send_response(200)
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)

    def log_message(self, *a):
        pass


# Threaded: single-threaded BaseHTTPServer serializes concurrent gateway
# requests and pollutes tail-latency probes (GW-5a lesson).
ThreadingHTTPServer(("127.0.0.1", port), H).serve_forever()
