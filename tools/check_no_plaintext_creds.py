#!/usr/bin/env python3
"""CI 断言：仓库不得出现明文口令形态（OPT-R8 A5）。

# 背景

凭据经**进程 argv** 泄漏是本项目 P1 阻断项：任何同机用户都能读到
（Windows: `Get-CimInstance Win32_Process` / `wmic process`；Linux: `/proc/<pid>/cmdline`）。
典型形态：

- `redis-cli -a <password> ...` → 改用 `REDISCLI_AUTH` 环境变量
- `curl --user user:password`     → 改用请求头（`X-ClickHouse-Key`）
- `clickhouse-client --password <pw>` → 改用 `CLICKHOUSE_PASSWORD` 环境变量

OPT-R8 A1~A4 已把现存 9 处改为环境变量/请求头。本断言防止**回归**：
有人再写回 `-a $env:REDIS_PASSWORD` 或硬编码 `--user "proxy:123456"` 时立即红。

# 判定口径（刻意保守，避免误伤）

只拦**确定的**明文形态，不做启发式猜测：

1. `redis-cli` 紧跟 `-a`（无论右侧是字面量还是变量——变量形态同样泄漏，
   因为 PowerShell/Bash 展开后就是明文）。
2. `--user` / `--password` 紧跟一个非 `--` 开头的值。
3. `X-ClickHouse-Key: <非空>` 这类**请求头**形态允许（值来自 env 变量，
   不在 argv）；但 `--user u:p` 形态拦截。

允许的例外（显式白名单，逐条带理由）：

- 文档（`*.md`）中的示例命令：教学材料必须能照抄，但**不得**含真实口令
  （仓库统一用开发缺省 `123456`，`.env.example` 已声明生产必须替换）。
- `docker-compose.yml` 的 `${REDIS_PASSWORD:-123456}` 形态：这是 compose 的
  变量插值，**不会**把口令写进 argv（除 redis-server 自身，见 OPT-R8 决策 1）。
- `deploy/geoip_update.py` 的 `MAXMIND_LICENSE_KEY`：已有强制 env + 日志脱敏。

# 用法

```bash
python tools/check_no_plaintext_creds.py
```

# 退出码

- 0：未发现明文口令形态
- 1：发现（逐条打印文件:行号与修复建议）
"""

from __future__ import annotations

import re
import sys
from pathlib import Path

REPO = Path(__file__).resolve().parent.parent

# 扫描范围：运维脚本 + compose + CI workflow（Rust 源码里凭据只来自 env，不扫）。
SCAN_GLOBS = ["tools/*.ps1", "tools/*.py", "docker-compose.yml", ".github/workflows/*.yml"]

# 本文件自身承载「违规形态」的字面样例（文档字符串里要说明长什么样），
# 扫自己必然自我告警。故排除——这不是放行其它文件，只是别让检查器自己红。
SELF = Path(__file__).resolve()

# 明文口令形态。捕获组 1=前缀上下文，仅用于报错信息。
#
# 三条形态的**排除规则**（避免把「其实已用 env」的写法误判为明文）：
#   * `redis-cli` 的 `-a`：`-a` 右侧只能是字面量或变量——两者都会展开成明文，
#     故一律拦。**例外**：`--no-auth-warning` 之类无关参数不构成放行。
#   * `clickhouse-client --password`：若写成 `"$${CLICKHOUSE_PASSWORD}"`（compose
#     形态，经容器 shell 展开成环境变量引用而非字面量）或 `"$env:CLICKHOUSE_PASSWORD"`
#     经 `docker exec -e` 传入，则密码**不在宿主 argv**，应放行。判据：`-e NAME=`
#     出现在同一命令里，或值以 `${` / `"$${` 包裹变量引用。
#   * `curl --user u:p`：同理，`-H "X-ClickHouse-Key: ..."` 形态才是正解。
PATTERNS: list[tuple[re.Pattern[str], str]] = [
    (re.compile(r"\bredis-cli\b[^\n]*\s-a\s+\S+"), "redis-cli -a <password>：改用 REDISCLI_AUTH 环境变量"),
    (re.compile(r"\bclickhouse-client\b[^\n]*\s--password\s+\S+"),
     "clickhouse-client --password <pw>：改用 CLICKHOUSE_PASSWORD 环境变量"),
    (re.compile(r"\bcurl(?:\.exe)?\b[^\n]*\s--user\s+\S+:\S+"),
     "curl --user user:pass：改用请求头（X-ClickHouse-User / X-ClickHouse-Key）"),
    (re.compile(r"\bdocker\s+exec\b[^\n]*\s--user\s+\S+:\S+"),
     "docker exec ... --user u:p：改用 -e 传环境变量"),
]

# 放行规则：该行已把凭据作为**环境变量**传入容器（密码不在宿主 argv），
# 或 `--password` 的值是 shell/compose 的**变量引用**形态。
#
# 两种变量引用都要认：
#   * compose 形态 `${VAR}` / GitHub Actions service 形态 `$$VAR`（容器内展开）；
#   * PowerShell 形态 `$env:VAR`（宿主展开为值，但配合 `docker exec -e VAR=` 使用，
#     此时 `-e` 已把值注入容器环境，命令行里只有变量名）。
ENV_INJECT = re.compile(
    r"(docker\s+exec\b[^\n]*\s-e\s+\w+=)"   # docker exec -e VAR=value
    r"|(\$\$+\{?\w*(?:PASSWORD|AUTH|KEY))"     # compose/GHA 的 ${VAR} / $$VAR
)
# PowerShell 形态 `$env:VAR` 只在**配合 `docker exec -e VAR=`** 时才安全
# （值注入容器环境，命令行里只有变量名）。孤立的 `redis-cli -a $env:PASSWORD`
# 仍会把口令展开进 argv——故这里要求同一行必须出现 `-e VAR=` 才放行。
PS_ENV = re.compile(r"\$env:\w*(?:PASSWORD|AUTH|KEY)")
DOCKER_EXEC_ENV = re.compile(r"\bdocker\s+exec\b[^\n]*\s-e\s+\w+=")


def _is_env_safe(line: str) -> bool:
    """该行的凭据是否已走环境变量（密码不进 argv）。"""
    if ENV_INJECT.search(line):
        return True
    # `$env:VAR` 需与 `docker exec -e VAR=` 同行才算安全注入。
    if PS_ENV.search(line) and DOCKER_EXEC_ENV.search(line):
        return True
    return False



def iter_files() -> list[Path]:
    out: list[Path] = []
    for g in SCAN_GLOBS:
        out.extend(sorted(REPO.glob(g)))
    return [p for p in out if p.is_file()]


def main() -> int:
    files = [p for p in iter_files() if p.resolve() != SELF]
    if not files:
        print("[check-creds] no files matched scan globs — nothing to check")
        return 0

    findings: list[tuple[Path, int, str]] = []
    for path in files:
        try:
            text = path.read_text(encoding="utf-8", errors="replace")
        except OSError as exc:
            print(f"[check-creds] ERROR: cannot read {path}: {exc}", file=sys.stderr)
            return 2
        for lineno, line in enumerate(text.splitlines(), 1):
            # 注释行不算（本断言只拦「会被执行」的命令行）。
            stripped = line.strip()
            if stripped.startswith("#") or stripped.startswith("<!--"):
                continue
            # 该行已把凭据以环境变量/变量引用形式传入 → 密码不在宿主 argv，放行。
            if _is_env_safe(line):
                continue
            for rx, hint in PATTERNS:
                if rx.search(line):
                    findings.append((path, lineno, hint))

    checked = len(files)
    if not findings:
        print(f"[check-creds] OK: no plaintext-credential argv patterns in {checked} file(s)")
        return 0

    print(f"[check-creds] FAIL: {len(findings)} plaintext-credential pattern(s) in {checked} file(s)",
          file=sys.stderr)
    print(
        "[check-creds] Passwords in process argv are readable by any local user via\n"
        "[check-creds] `Get-CimInstance Win32_Process` (Windows) or `/proc/<pid>/cmdline` (Linux).\n"
        "[check-creds] Use environment variables (REDISCLI_AUTH / CLICKHOUSE_PASSWORD) or\n"
        "[check-creds] request headers (X-ClickHouse-Key) instead.\n",
        file=sys.stderr,
    )
    for path, lineno, hint in findings:
        rel = path.relative_to(REPO).as_posix()
        print(f"  {rel}:{lineno}  {hint}", file=sys.stderr)
    return 1


if __name__ == "__main__":
    sys.exit(main())
