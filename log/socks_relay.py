"""P2-8 本地确定性 E2E 之 SOCKS5 relay stub（只跑 localhost，不进生产）。
行为：greeting -> 回 no-auth -> 读 CONNECT -> 直连目标 -> 回成功 -> 双向管道。
Usage: socks_relay.py <listen_port>
"""
import socket
import struct
import threading
import sys

PORT = int(sys.argv[1]) if len(sys.argv) > 1 else 1099


def recvn(c, n):
    buf = b""
    while len(buf) < n:
        chunk = c.recv(n - len(buf))
        if not chunk:
            raise ConnectionError("eof")
        buf += chunk
    return buf


def pipe(a, b):
    try:
        while True:
            d = a.recv(65536)
            if not d:
                break
            b.sendall(d)
    except OSError:
        pass
    finally:
        try:
            a.shutdown(socket.SHUT_RD)
        except OSError:
            pass
        try:
            b.shutdown(socket.SHUT_WR)
        except OSError:
            pass


def handle(c):
    try:
        ver, nmethods = struct.unpack("!BB", recvn(c, 2))
        recvn(c, nmethods)
        assert ver == 5, f"bad ver {ver}"
        c.sendall(b"\x05\x00")
        ver, cmd, rsv, atyp = struct.unpack("!BBBB", recvn(c, 4))
        assert (ver, cmd) == (5, 1), f"bad req {ver} {cmd}"
        if atyp == 1:
            raw = recvn(c, 6)
            host = socket.inet_ntoa(raw[:4])
            port = struct.unpack("!H", raw[4:])[0]
        elif atyp == 3:
            ln = recvn(c, 1)[0]
            host = recvn(c, ln).decode()
            port = struct.unpack("!H", recvn(c, 2))[0]
        else:
            c.sendall(b"\x05\x08\x00\x01\x00\x00\x00\x00\x00\x00")
            return
        try:
            up = socket.create_connection((host, port), timeout=10)
        except OSError:
            c.sendall(b"\x05\x05\x00\x01\x00\x00\x00\x00\x00\x00")
            return
        c.sendall(b"\x05\x00\x00\x01\x00\x00\x00\x00\x00\x00")
        t1 = threading.Thread(target=pipe, args=(c, up), daemon=True)
        t2 = threading.Thread(target=pipe, args=(up, c), daemon=True)
        t1.start()
        t2.start()
        t1.join()
        t2.join()
    except (OSError, ConnectionError, AssertionError) as e:
        print(f"[relay] conn failed: {e}", flush=True)
    finally:
        c.close()


srv = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
srv.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
srv.bind(("127.0.0.1", PORT))
srv.listen(64)
print(f"[relay] listening 127.0.0.1:{PORT}", flush=True)
while True:
    c, _ = srv.accept()
    threading.Thread(target=handle, args=(c,), daemon=True).start()
