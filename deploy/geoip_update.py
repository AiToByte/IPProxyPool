"""GeoLite2-City.mmdb 更新脚本（P4-2，只跑运维侧，不进网关）。
流程：MAXMIND_LICENSE_KEY（环境必填）→ 下载 tar.gz（含校验）→ 内存解 tar →
提取 .mmdb → 写临时文件 → os.replace 原子替换 → 打印版本行。
更新后重启网关生效（热加载不做，见 OPERATION §6）。
零第三方依赖（urllib＋tarfile＋os＋sys＋argparse，标准库）。
Usage:
  set MAXMIND_LICENSE_KEY=<key> && python deploy/geoip_update.py --out-dir ./data
  python deploy/geoip_update.py --help
"""
import argparse
import io
import os
import sys
import tarfile
import urllib.request

USAGE = "usage: geoip-update.py [--edition GeoLite2-City] [--out-dir ./data]\n" \
        "  env MAXMIND_LICENSE_KEY is required (get one from a MaxMind account)"
DOWNLOAD_TMPL = ("https://download.maxmind.com/geoip/databases/{edition}/download"
                 "?license_key={key}&suffix=tar.gz")


def parse_args(argv):
    p = argparse.ArgumentParser(
        description="Download and atomically install a GeoLite2 .mmdb file.")
    p.add_argument("--edition", default="GeoLite2-City")
    p.add_argument("--out-dir", default="./data")
    return p.parse_args(argv)


def download(edition, key, opener=urllib.request.urlopen):
    url = DOWNLOAD_TMPL.format(edition=edition, key="***")
    real = DOWNLOAD_TMPL.format(edition=edition, key=key)
    req = urllib.request.Request(real, headers={"User-Agent": "ipproxy-geoip-update/1.0"})
    print(f"[geoip-update] downloading {url}", flush=True)
    with opener(req, timeout=60) as resp:
        if resp.status != 200:
            raise RuntimeError(f"download failed: HTTP {resp.status}")
        return resp.read()


def extract_mmdb(tar_bytes):
    with tarfile.open(fileobj=io.BytesIO(tar_bytes), mode="r:gz") as tf:
        for m in tf.getmembers():
            if m.isfile() and m.name.endswith(".mmdb"):
                f = tf.extractfile(m)
                if f is None:
                    continue
                return os.path.basename(m.name), f.read()
    raise RuntimeError("no .mmdb found in archive")


def atomic_install(out_dir, filename, data):
    os.makedirs(out_dir, exist_ok=True)
    final = os.path.join(out_dir, filename)
    tmp = final + ".tmp"
    with open(tmp, "wb") as f:
        f.write(data)
    os.replace(tmp, final)
    return final


def main(argv=None):
    args = parse_args(argv if argv is not None else sys.argv[1:])
    key = os.environ.get("MAXMIND_LICENSE_KEY", "")
    if not key:
        print(USAGE, file=sys.stderr)
        print("error: env MAXMIND_LICENSE_KEY is required", file=sys.stderr)
        return 2
    try:
        tar_bytes = download(args.edition, key)
        filename, data = extract_mmdb(tar_bytes)
        final = atomic_install(args.out_dir, filename, data)
    except Exception as e:  # noqa: BLE001 - ops script, report and exit nonzero
        print(f"error: {e}", file=sys.stderr)
        return 1
    print(f"[geoip-update] installed {final} ({len(data)} bytes); restart gateway to apply",
          flush=True)
    return 0


if __name__ == "__main__":
    sys.exit(main())
