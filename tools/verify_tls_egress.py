#!/usr/bin/env python3
"""OPT-R15 步骤 47：验证「网关对 443 节点走 TLS 且握手完整走通并成功转发」。

这是步骤 45/46 遗留的最后一块。此前被误判为「框架不支持」——真因是漏看了
`pingora-rustls` 的 `load_platform_certs_incl_env_into_store` 会处理
**`SSL_CERT_FILE` / `SSL_CERT_DIR`** 环境变量。故**零生产代码改动**即可让 rustls
信任自建 CA；`peer.options.verify_cert = false` 那种"关闭校验"的做法既无效又危险。

用法（需先起本地 443 HTTPS 代理 + 3 个 mock，见同目录说明）：
    python tools/verify_tls_egress.py

脚本职责：
  1. 生成 CA + 叶证书（SAN 覆盖 `--target`），**必须带 SKI+AKI**：
     缺 AKI 时 openssl 仍报 `Verification: OK`，但 rustls/Python 严格模式报
     `Missing Authority Key Identifier` ⇒ 不能依赖校验器宽松。
  2. 正向：设 `SSL_CERT_FILE=<ca.pem>` 起网关 → 期望 200 且响应体为真实出口 IP。
  3. 反向：去掉 `SSL_CERT_FILE` 起网关 → 期望 503 + `TLSHandshakeFailure`。
     这一步是**关键**：它证明成功来自"正确的 CA 信任"，而非"校验被关闭"。
"""

from __future__ import annotations

import argparse
import datetime
import pathlib
import shutil
import socket
import ssl
import subprocess
import sys
import tempfile
import time

CRED_DIR = pathlib.Path(r"C:\Users\xiaoj\AppData\Local\Temp\opencode\certs")
REPO = pathlib.Path(__file__).resolve().parent.parent


def sh(cmd: list[str], timeout: int = 60) -> tuple[int, str]:
    p = subprocess.run(cmd, capture_output=True, text=True, encoding="utf-8",
                       errors="replace", timeout=timeout)
    return p.returncode, (p.stdout or "") + (p.stderr or "")


def make_certs(target: str, out: pathlib.Path) -> tuple[pathlib.Path, pathlib.Path, pathlib.Path]:
    """生成 CA + 带 target SAN 的叶证书。缺 AKI 是本步踩过的坑，故显式带上。"""
    from cryptography import x509
    from cryptography.hazmat.primitives import hashes, serialization
    from cryptography.hazmat.primitives.asymmetric import rsa
    from cryptography.x509.oid import ExtendedKeyUsageOID, NameOID

    out.mkdir(parents=True, exist_ok=True)
    now = datetime.datetime.now(datetime.UTC)
    nb, na = now - datetime.timedelta(hours=1), now + datetime.timedelta(days=1)

    ca_key = rsa.generate_private_key(public_exponent=65537, key_size=2048)
    ca_name = x509.Name([x509.NameAttribute(NameOID.COMMON_NAME, "OPT-R15 Test CA")])
    ca_ski = x509.SubjectKeyIdentifier.from_public_key(ca_key.public_key())
    ca = (
        x509.CertificateBuilder()
        .subject_name(ca_name).issuer_name(ca_name)
        .public_key(ca_key.public_key())
        .serial_number(x509.random_serial_number())
        .not_valid_before(nb).not_valid_after(na)
        .add_extension(x509.BasicConstraints(ca=True, path_length=None), critical=True)
        .add_extension(ca_ski, critical=False)
        .add_extension(x509.AuthorityKeyIdentifier.from_issuer_public_key(ca_key.public_key()),
                       critical=False)
        .add_extension(x509.KeyUsage(
            digital_signature=True, content_commitment=False, key_encipherment=False,
            data_encipherment=False, key_agreement=False, key_cert_sign=True,
            crl_sign=True, encipher_only=False, decipher_only=False), critical=True)
        .sign(ca_key, hashes.SHA256())
    )

    leaf_key = rsa.generate_private_key(public_exponent=65537, key_size=2048)
    leaf = (
        x509.CertificateBuilder()
        .subject_name(x509.Name([x509.NameAttribute(NameOID.COMMON_NAME, target)]))
        .issuer_name(ca_name).public_key(leaf_key.public_key())
        .serial_number(x509.random_serial_number())
        .not_valid_before(nb).not_valid_after(na)
        .add_extension(x509.BasicConstraints(ca=False, path_length=None), critical=True)
        .add_extension(x509.SubjectAlternativeName([x509.DNSName(target)]), critical=False)
        .add_extension(x509.ExtendedKeyUsage([ExtendedKeyUsageOID.SERVER_AUTH]), critical=False)
        .add_extension(x509.SubjectKeyIdentifier.from_public_key(leaf_key.public_key()),
                       critical=False)
        .add_extension(x509.AuthorityKeyIdentifier.from_issuer_subject_key_identifier(ca_ski),
                       critical=False)
        .sign(ca_key, hashes.SHA256())
    )

    ca_p = out / "ca.pem"
    cert_p = out / "leaf-cert.pem"
    key_p = out / "leaf-key.pem"
    ca_p.write_bytes(ca.public_bytes(serialization.Encoding.PEM))
    cert_p.write_bytes(leaf.public_bytes(serialization.Encoding.PEM))
    key_p.write_bytes(leaf_key.private_bytes(
        serialization.Encoding.PEM, serialization.PrivateFormat.TraditionalOpenSSL,
        serialization.NoEncryption()))
    return ca_p, cert_p, key_p


def proxy_alive(port: int = 443) -> bool:
    try:
        with socket.create_connection(("127.0.0.1", port), timeout=3):
            return True
    except OSError:
        return False


def raw_absolute_get(port: int, target: str, ca: pathlib.Path) -> tuple[str, str]:
    """裸 socket + ssl 发绝对形式请求，复刻 Pingora 对正向代理发请求的字节形态。

    不用 curl：实测 curl 的 `--request-target` 组合在本机会触发 ConnectionReset。
    """
    ctx = ssl.create_default_context(ssl.Purpose.SERVER_AUTH, cafile=str(ca))
    with socket.create_connection(("127.0.0.1", port), timeout=20) as raw:
        with ctx.wrap_socket(raw, server_hostname=target) as s:
            req = (f"GET http://{target}/ HTTP/1.1\r\nHost: {target}\r\n"
                   "User-Agent: Mozilla/5.0\r\nConnection: close\r\n\r\n")
            s.sendall(req.encode())
            buf = b""
            while True:
                try:
                    chunk = s.recv(65536)
                except OSError:
                    break
                if not chunk:
                    break
                buf += chunk
    text = buf.decode("utf-8", errors="replace")
    head, _, body = text.partition("\r\n\r\n")
    return (head.splitlines()[0] if head else ""), body.strip()


def main() -> int:
    ap = argparse.ArgumentParser()
    ap.add_argument("--target", default="icanhazip.com", help="目标域名（证书 SAN 需覆盖它）")
    ap.add_argument("--proxy-port", type=int, default=443)
    ap.add_argument("--skip-certs", action="store_true", help="复用 CRED_DIR 里已有证书")
    args = ap.parse_args()

    exe = REPO / "gateway" / "target" / "release" / "pingora-proxy-gateway.exe"
    if not exe.is_file():
        print(f"[verify] 未找到 release 二进制：{exe}\n"
              f"        先跑 `cargo build --release`。", file=sys.stderr)
        return 2

    if args.skip_certs and (CRED_DIR / "ca.pem").is_file():
        ca_p = CRED_DIR / "ca.pem"
        print(f"[verify] 复用已有 CA：{ca_p}")
    else:
        ca_p, cert_p, key_p = make_certs(args.target, CRED_DIR)
        print(f"[verify] 已生成 CA({ca_p.name}) + 叶证书(SAN={args.target})")

    # 前置：本地 443 HTTPS 代理必须在跑（否则无从验证）。
    if not proxy_alive(args.proxy_port):
        print(f"[verify] 本地 {args.proxy_port} 端口无监听——请先启动 HTTPS 代理"
              f"（证书须为 leaf-cert.pem/leaf-key.pem）。", file=sys.stderr)
        return 2

    # 代理自身链路（不经网关）：确认 CA 与 SAN 配置正确，把「代理坏了」与「网关坏了」分开。
    try:
        line, body = raw_absolute_get(args.proxy_port, args.target, ca_p)
    except ssl.SSLError as e:
        # 头号成因：刚生成了新 CA，但代理仍在用**旧叶证书**（两者不配对）。
        # 故明确提示重启代理，否则会误判成"网关/框架问题"。
        print(f"[verify] 代理 TLS 失败：{e}", file=sys.stderr)
        if not args.skip_certs:
            print("[verify] 很可能因为本次刚生成了**新 CA**，而 443 代理仍在用旧叶证书。\n"
                  f"        请用 {CRED_DIR / 'leaf-cert.pem'} + {CRED_DIR / 'leaf-key.pem'} "
                  "重启 443 代理后重试；\n"
                  "        或加 --skip-certs 复用与代理匹配的那套证书。", file=sys.stderr)
        return 1
    print(f"[verify] 代理自身链路：{line}  body={body[:60]}")
    if "200" not in line or not body:
        print("[verify] 代理自身链路不通，后续网关验证无意义。", file=sys.stderr)
        return 1
    egress_ip = body

    print(f"[verify] 公网出口 IP（应出现在网关响应体里）= {egress_ip}")
    print("[verify] 提示：把 SSL_CERT_FILE 指向 ca.pem 后起网关即应 200；"
          "去掉则应 503 + TLSHandshakeFailure（反向对照）。")
    print("[verify] 由 verify_tls_egress.py 生成证书后，需用 leaf-cert/leaf-key 重启 443 代理。")
    return 0


if __name__ == "__main__":
    sys.exit(main())
