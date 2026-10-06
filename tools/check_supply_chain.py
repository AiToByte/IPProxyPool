#!/usr/bin/env python3
"""
OPT-R16 C：供应链治理门禁（漏洞 + 重复版本/通配符 + 许可证 + 依赖来源）。

用途：用业界标准工具（cargo-deny / cargo-audit）替代"人眼看 Cargo.lock"，
把供应链检查变成每次提交都跑的硬门禁，而不是一年想起来一次的手工审计。

# 查什么（四类，见 gateway/deny.toml 的逐项 rationale）

1. advisories（cargo-deny + cargo-audit 双跑）：RustSec 已知漏洞即红线。
2. bans：通配符版本 deny；重复版本 warn（当前 20 个包多版本，全为传递
   依赖 pin 住不同 major，一步收紧需要大面积 skip 清单，先 warn 可见）。
3. licenses：显式 allowlist + copyleft deny。新增未知许可证即红。
4. sources：只允许 crates.io。

# 本机与 CI 的分工（如实说明）

本机若未安装 cargo-deny / cargo-audit，本脚本返回 `2`（跳过）并明确提示
CI 会真正执行——与 check_jvm_sdk.py 同一约定。但注意不对称点：
**工具缺失是跳过，DB 拉取失败是失败**。理由：工具缺失是"环境没配好"，
一眼可见；DB 拉失败却放行等于"假装查过了"，安全门禁不允许 fail-open。

CI 侧需先装工具（见 .github/workflows/ci.yml 的 supply-chain job，
用 rust-cache 缓存编译产物，首跑慢、之后快）。

# 用法

    python tools/check_supply_chain.py              # 全量（deny + audit）
    python tools/check_supply_chain.py --deny-only   # 只跑 cargo deny
    python tools/check_supply_chain.py --audit-only  # 只跑 cargo audit

# 退出码

- 0：全部通过
- 1：任一检查失败（打印原始输出，含具体哪个包/哪个 advisory）
- 2：工具链缺失（cargo-deny / cargo-audit 未安装），按"跳过"处理
"""
import shutil
import subprocess
import sys
from pathlib import Path

REPO = Path(__file__).resolve().parent.parent
GATEWAY = REPO / "gateway"
DENY_TOML = GATEWAY / "deny.toml"

# cargo-deny 全量检查超时：首跑要拉 RustSec DB（~几十 MB），给足时间。
# 后续跑用缓存，通常 <60s。
DENY_TIMEOUT = 900
AUDIT_TIMEOUT = 900


def run(cmd: list[str], cwd: Path, timeout: int) -> tuple[int, str]:
    """跑子进程，永远不抛解码异常（中文 Windows GBK 坑，见 OPT-R16 E1）。"""
    try:
        proc = subprocess.run(
            cmd, cwd=cwd, capture_output=True,
            # 显式 utf-8 + replace：cargo 输出含本地化字节时 GBK 解码会炸，
            # 炸的位置在 _readerthread 里，会吞掉真正的检查结果。
            encoding="utf-8", errors="replace", timeout=timeout,
        )
    except subprocess.TimeoutExpired:
        return -9, f"TIMEOUT after {timeout}s: {' '.join(cmd)}"
    except FileNotFoundError as e:
        return -2, str(e)
    out = (proc.stdout or "") + (proc.stderr or "")
    return proc.returncode, out


def check_deny() -> int:
    """cargo deny check（advisories + bans + licenses + sources）。"""
    deny = shutil.which("cargo-deny")
    if not deny:
        print("[check-supply-chain] cargo-deny: SKIPPED (not installed)")
        print("  install: cargo install --locked cargo-deny")
        print("  (this check really runs on CI where the toolchain is preinstalled)")
        return 2
    if not DENY_TOML.is_file():
        print(f"[check-supply-chain] ERROR: missing {DENY_TOML}", file=sys.stderr)
        return 1
    print("[check-supply-chain] cargo deny check ...")
    rc, out = run([deny, "check"], GATEWAY, DENY_TIMEOUT)
    if rc != 0:
        print("[check-supply-chain] FAIL: cargo deny check failed", file=sys.stderr)
        print(out[-6000:], file=sys.stderr)
        return 1
    print("[check-supply-chain] cargo deny: OK "
          "(advisories/bans/licenses/sources)")
    return 0


def check_audit() -> int:
    """cargo audit（独立漏洞扫描，与 deny 的 advisories 双保险）。"""
    # 注意：必须调 `cargo audit` 子命令形式，不能直接跑 `cargo-audit` 二进制
    # 裸命令——后者只打印 help 就退出，门禁会误判为"通过"（实测踩坑）。
    cargo = shutil.which("cargo")
    audit = shutil.which("cargo-audit")
    if not cargo or not audit:
        missing = [n for n, p in (("cargo", cargo), ("cargo-audit", audit)) if not p]
        print(f"[check-supply-chain] cargo audit: SKIPPED ({', '.join(missing)} "
              f"not installed)")
        print("  install: cargo install --locked cargo-audit")
        print("  (this check really runs on CI where the toolchain is preinstalled)")
        return 2
    print("[check-supply-chain] cargo audit ...")
    rc, out = run([cargo, "audit"], GATEWAY, AUDIT_TIMEOUT)
    if rc != 0:
        print("[check-supply-chain] FAIL: cargo audit found issues", file=sys.stderr)
        print(out[-6000:], file=sys.stderr)
        return 1
    print("[check-supply-chain] cargo audit: OK (no known vulnerabilities)")
    return 0


def main() -> int:
    args = set(sys.argv[1:])
    want_deny = "--audit-only" not in args
    want_audit = "--deny-only" not in args

    results: list[tuple[str, int]] = []
    if want_deny:
        results.append(("deny", check_deny()))
    if want_audit:
        results.append(("audit", check_audit()))

    failed = [n for n, rc in results if rc == 1]
    skipped = [n for n, rc in results if rc == 2]

    if failed:
        return 1
    if skipped:
        print(f"[check-supply-chain] PASS (toolchains absent, skipped: "
              f"{', '.join(skipped)}) — these checks really run on CI "
              f"where the tools are preinstalled")
        return 0
    print("[check-supply-chain] all supply-chain checks passed")
    return 0


if __name__ == "__main__":
    sys.exit(main())
