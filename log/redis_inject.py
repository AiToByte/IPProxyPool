"""Inject a forged 403 telemetry event via raw RESP (avoids Windows CLI quote stripping).

Usage: redis_inject.py -> prints +XADD_ID:<id> then polls
quarantine:quarantine-test.example:127.0.0.1 until BANNED (max ~8s).
"""
import json
import socket
import time

HOST, PORT = "127.0.0.1", 6379
DOMAIN, OUT_IP = "quarantine-test.example", "127.0.0.1"


def resp(*args: bytes) -> bytes:
    out = f"*{len(args)}\r\n".encode()
    for a in args:
        out += f"${len(a)}\r\n".encode() + a + b"\r\n"
    return out


def roundtrip(s: socket.socket, *args: bytes) -> bytes:
    s.sendall(resp(*args))
    chunks = []
    s.settimeout(5.0)
    try:
        while True:
            data = s.recv(4096)
            if not data:
                break
            chunks.append(data)
            if data.endswith(b"\r\n"):
                break
    except socket.timeout:
        pass
    return b"".join(chunks)


payload = json.dumps(
    {
        "client_ip": "t",
        "target_domain": DOMAIN,
        "out_ip": OUT_IP,
        "provider": "mock-a",
        "tier": "residential",
        "country": "US",
        "status_code": 403,
        "latency_ms": 5,
        "transferred_bytes": 0,
        "retry_count": 0,
        "tenant_id": None,
        "error_type": None,
        "timestamp": 1,
    },
    separators=(",", ":"),
).encode()

s = socket.create_connection((HOST, PORT), timeout=5.0)
print(roundtrip(s, b"XADD", b"stream:proxy:telemetry", b"*",
                b"payload", payload, b"domain", DOMAIN.encode(), b"status", b"403").decode().strip())
key = f"quarantine:{DOMAIN}:{OUT_IP}".encode()
for _ in range(16):
    time.sleep(0.5)
    reply = roundtrip(s, b"GET", key)
    if b"BANNED" in reply:
        print("QUARANTINE_KEY:BANNED")
        break
else:
    print("QUARANTINE_KEY:MISSING")
s.close()
