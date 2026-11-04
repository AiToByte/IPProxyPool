#!/usr/bin/env python3
"""
OPT-R16 C：供应链治理门禁（漏洞 + 重复版本/通配符 + 许可证 + 依赖来源）。

用途：用业界标准工具（cargo-deny / cargo-audit）替代"人眼看 Cargo.lock"，
把供应链检查变成每次提交都跑的硬门禁，而不是一年想起来一次的手工审计。

# 查什么（四类，见 gateway/deny.toml 的逐项 rationale）

1. advisories（cargo-deny + cargo-audit 双跑）：RustSec 已知漏洞即红线。
2. bans：通配符版本 deny；重复版本 warn（当前 17 个包多版本，全为传递
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
- 2：**未真正执行**——工具链缺失（cargo-deny / cargo-audit 未安装）
     或公告库不可达（离线）。按"跳过"处理，但**必须打印原因**：
     "跳过"与"通过"在结论上不能混为一谈（2026-11-01 改进，见
     docs/SUPPLY_CHAIN.md §10.7）
"""
import re
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


def deny_ignore_ids() -> list[str]:
    """从 `deny.toml` 的 `[advisories] ignore` 里取 advisory ID 列表。

    **单一真源**：`cargo deny` 与 `cargo audit` 是两套工具、各有一份豁免机制
    （deny 读 `deny.toml` 的 `ignore`，audit 只认命令行 `--ignore`）。若各自
    维护一份，必然漂移——漂移的后果是"deny 说豁免了、audit 说没豁免"，两个门
    一个绿一个红，谁对说不清。故本门把 deny.toml 当唯一来源，读出来喂给 audit。
    """
    if not DENY_TOML.is_file():
        return []
    text = DENY_TOML.read_text(encoding="utf-8")
    # 只扫 `ignore = [ ... ]` 这一段，避免误抓注释里提到的历史 ID。
    m = re.search(r"^ignore\s*=\s*\[(.*?)^\]", text, re.S | re.M)
    if not m:
        return []
    return re.findall(r'id\s*=\s*"([^"]+)"', m.group(1))


def audit_db_dir() -> Path | None:
    """cargo-deny 用的那份 RustSec 库目录（`~/.cargo/advisory-dbs/advisory-db-*`）。

    **为什么要显式指定给 cargo audit**：两个工具的默认库**不是同一个**——
    cargo-deny 用 `~/.cargo/advisory-dbs/advisory-db-<hash>`（自己 clone 的），
    cargo-audit 默认 `~/.cargo/advisory-db`。本机实测：前者已更新到含 zstd 公告，
    后者是**旧快照**（连 RUSTSEC-2026-0391 都没有）。
    若不显式指定，audit 会在旧库上得出"0 漏洞"的**假绿**——比不跑更危险。
    """
    base = Path.home() / ".cargo" / "advisory-dbs"
    if not base.is_dir():
        return None
    dirs = sorted(d for d in base.glob("advisory-db-*") if d.is_dir())
    return dirs[0] if dirs else None


def check_audit() -> int:
    """cargo audit：**离线、指定库、复用 deny 的豁免清单**的二次校验。

    设计取舍（2026-11-01 重做，此前实现有三个缺陷）：

    1. **不再联网**。原先裸跑 `cargo audit` 要 git fetch 公告库，离线必失败。
       早期把失败一律判红 ⇒ 离线机器上这道门**永远红**；后来我改成"字符串匹配
       网络错误"⇒ **不稳定**（同一台机器上时绿时红，实测复现）。
       现在固定 `--no-fetch --stale -d <deny 的库>`：全程无网络，结果确定。
    2. **显式指定库**，与 cargo-deny 同源（见 `audit_db_dir` 的理由）——
       否则 audit 会在旧库上给出假绿。
    3. **豁免清单从 deny.toml 读**（见 `deny_ignore_ids`），不另维护一份。

    剩下的失败原因只有"真的发现漏洞"，故非 0 即判红，不需要再猜字符串。
    库不存在（首次运行/未同步）则显式 SKIP——那是环境问题，不是安全问题。
    """
    # 必须调 `cargo audit` 子命令形式，不能跑 `cargo-audit` 裸命令——
    # 后者只打印 help 就退出，门禁会误判为"通过"（实测踩坑）。
    cargo = shutil.which("cargo")
    audit = shutil.which("cargo-audit")
    if not cargo or not audit:
        missing = [n for n, p in (("cargo", cargo), ("cargo-audit", audit)) if not p]
        print(f"[check-supply-chain] cargo audit: SKIPPED ({', '.join(missing)} "
              f"not installed)")
        print("  install: cargo install --locked cargo-audit")
        print("  (this check really runs on CI where the toolchain is preinstalled)")
        return 2

    db = audit_db_dir()
    if db is None:
        print("[check-supply-chain] cargo audit: SKIPPED (no RustSec DB at "
              "~/.cargo/advisory-dbs — run `cargo deny check` once to populate it)")
        print("  注意：本次**没有**做 audit 侧校验；advisories 仍由 cargo deny 覆盖。")
        return 2

    ignores = deny_ignore_ids()
    cmd = [cargo, "audit", "--no-fetch", "--stale", "-d", str(db)]
    for i in ignores:
        cmd += ["--ignore", i]
    print(f"[check-supply-chain] cargo audit (offline, db={db.name}, "
          f"ignore={len(ignores)} from deny.toml) ...")
    rc, out = run(cmd, GATEWAY, AUDIT_TIMEOUT)
    if rc != 0:
        print("[check-supply-chain] FAIL: cargo audit found issues", file=sys.stderr)
        print(out[-6000:], file=sys.stderr)
        return 1
    print("[check-supply-chain] cargo audit: OK (no un-ignored vulnerabilities; "
          "allowed warnings are reported by the tool itself)")
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
        print(f"[check-supply-chain] PASS with SKIP: {', '.join(skipped)} 未真正执行"
              f"（工具缺失或公告库不可达）。CI 有网络与预装工具链，那里会跑全量；"
              f"本地离线时**不要把这条 PASS 当作「已确认无新漏洞」**。")
        return 0
    print("[check-supply-chain] all supply-chain checks passed")
    return 0


if __name__ == "__main__":
    sys.exit(main())
