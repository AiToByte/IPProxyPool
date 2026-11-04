#!/usr/bin/env python3
"""
CI 断言：**可观测性与依赖声明不得失实**（OPT-R17）。

本门锁两类在 OPT-R17 复核中被实证的失实。二者形态不同，但**根因同源**：
「文档/配置里写了一件代码里不存在的事」，而这类东西**不会让任何测试变红**。

────────────────────────────────────────────────────────────────────────
失实 A：`Cargo.toml` 声明了依赖 feature，但 `src/` 零引用（"声明即失实"）
────────────────────────────────────────────────────────────────────────

实测：`gateway/Cargo.toml` 声明

    tower-http = { version = "0.5", features = ["cors", "trace", "timeout"] }

而 `admin.rs` 的函数注释写着：

    /// 构建 axum 路由（含 CORS + 超时 + 认证中间件）。

**两者都不实**：`src/` 全仓对 `tower-http` 零引用（Cargo.lock 里有两条
tower-http 记录纯属其它依赖的传递依赖），实际只有认证中间件。

危害不是"少了个功能"，而是**三重**：

1. 读者以为跨域已放开 → 实则 Admin UI 只能靠 Vite proxy/同域反代；
2. 以为有请求超时 → 实则慢请求可长期占用连接；
3. 以为有访问审计 → 实则 Admin 的写操作（改权重/隔离/余额）**无任何轨迹**。

这正是 `docs/CAPABILITY-AUDIT.md` §0 自己列的取证纪律第 1 条
（「数测试/函数前，先确认正则覆盖了目标语法」）的反面教材。

本门锁：`Cargo.toml` 里显式列出的每个 feature，必须能在 `src/` 找到
对应的类型引用。因 Rust 的 feature 与类型名不是机械对应关系，用一张
**显式映射表**而非通用规则——通用规则要么漏（映射不全）要么误报。

────────────────────────────────────────────────────────────────────────
失实 B：告警投递链路未文档化（"以为接了、实际没接"）
────────────────────────────────────────────────────────────────────────

实测：`deploy/prometheus/rules.yml` 有 15 条告警规则，但
`docs/OPERATION.md`（27KB 运维手册）**全文不出现 "alertmanager"**。
运维读手册 → 不知道告警不会外发 → 以为有告警 → 故障时才发现收不到。

而真实状态比"没接"更微妙：`prometheus.yml` 的 `alerting:` 块**早已接好**，
Alertmanager 容器也在（OPT-R17 起）默认启动，但
`route.receiver: "empty"` 是刻意留空 ⇒ **链路通到 Alertmanager，
但不外发**。这个"通到一半"的状态最容易误判，故必须写下来。

本门锁：`OPERATION.md` 必须写明三段链路各自的真实状态。

────────────────────────────────────────────────────────────────────────
失实 C：CH initdb 目录里放了"声称手动、实际自动"的迁移脚本（2026-10-28）
────────────────────────────────────────────────────────────────────────

实测：`deploy/clickhouse/002_reorder.sql` 的文件头写着

    **执行方式：手动、一次性。** 本文件不会被 `docker-entrypoint-initdb.d`
    自动执行（它只跑 `001_schema.sql`）。

**这是错的。** CH 官方 `/entrypoint.sh` 的实现是：

    for f in /docker-entrypoint-initdb.d/*; do
        case "$f" in *.sql) clickhouse-client --multiquery < "$f" ;; esac
    done

即顶层**所有** `.sql` 按文件名顺序全部自动执行（子目录不递归）。
而 `docker-compose.yml` 把**整个** `deploy/clickhouse/` 挂到该目录。

危害比"多跑一次"严重得多——`002` 是破坏性的（`CREATE _v2` → `INSERT SELECT`
全表复制 → `RENAME` 换名），且 `_v2` 的列清单是**写死的快照**：
此后 `001_schema.sql` 新增的列（如 `trace_id`）会在 `RENAME` 后**被覆盖丢失**，
得到一个缺列的仓库表，而代码侧照常写入 ⇒ 插入失败或字段静默丢弃。
`003_trace_id.sql` 虽在 `002` 之后执行，但顺序依赖本身就不该存在。

本门锁：`deploy/clickhouse/` 顶层**只允许** `001_schema.sql`（幂等的权威
schema）。一次性迁移脚本必须放 `manual/`。用**白名单**而非黑名单——
黑名单只能拦住已知的那一个，将来新增迁移脚本照样会被自动执行。

────────────────────────────────────────────────────────────────────────
失实 D：迁移脚本与权威 schema 对同一列给出不同类型
────────────────────────────────────────────────────────────────────────

全新部署走 `001_schema.sql`，存量部署走 `manual/*.sql` 的就地 `ALTER`。
两条路径若对同一列给出不同类型，会得到**两种不同的 schema**，而代码只有一份。
两者都能建表成功、都不报错，只在「新环境正常、老环境异常」时暴露——
而那时排查方向通常被误导到代码而不是 schema。

本门锁：`MIGRATION_TYPE_CONSISTENCY` 登记的列，两处类型必须逐字一致。

# 用法

    python tools/check_observability_truth.py

# 退出码

- 0：四项契约均在位
- 1：失实（打印具体项）

# 双向验证记录（OPT-R17）

- 向 Cargo.toml 加回 `features = ["cors"]` → 门红（cors 在 src/ 无引用）
- 从 OPERATION.md 删掉「告警投递」小节 → 门红
- 还原后 → 门绿

# 双向验证记录（失实 C / D，2026-10-28）

- 把 `manual/003_trace_id.sql` 拷回 `deploy/clickhouse/` 顶层 → 门红（指出
  「顶层只允许 001_schema.sql」并给出正确位置）
- 把 `003` 的 `ADD COLUMN ... String` 改成 `LowCardinality(String)` → 门红
  （指出两处类型不一致）
- 各自还原 → 门绿
"""

import re
import sys
from pathlib import Path

REPO = Path(__file__).resolve().parent.parent
CARGO_TOML = REPO / "gateway" / "Cargo.toml"
SRC_DIR = REPO / "gateway" / "src"
OPERATION = REPO / "docs" / "OPERATION.md"
CH_INITDB = REPO / "deploy" / "clickhouse"

# `docker-entrypoint-initdb.d` 顶层**只允许**存在的 SQL 文件。
#
# 为什么要有这条：CH 官方 `/entrypoint.sh` 的实现是
#     for f in /docker-entrypoint-initdb.d/*; do
#         case "$f" in *.sql) clickhouse-client --multiquery < "$f" ;; esac
#     done
# 即**顶层所有 `.sql` 按文件名顺序全部自动执行**（子目录不递归）。
#
# 实测事故（2026-10-28 发现）：`002_reorder.sql` 曾放在这个目录顶层，其文件头
# 声称「不会被 initdb 自动执行（它只跑 001_schema.sql）」——**那是错的**。
# 而它是**破坏性**脚本（`CREATE _v2` → `INSERT SELECT` 全表复制 → `RENAME`），
# 自动执行的后果是：
#   ① 全新部署多做一次全表复制，留下一张 `_old` 垃圾表；
#   ② 更严重：`_v2` 的列清单是**写死的快照**，会把此后 `001_schema.sql` 新增的
#      列（如 `trace_id`）在 `RENAME` 后**覆盖丢失**，得到一个缺列的仓库表，
#      而代码侧照常写入 ⇒ 插入失败或字段静默丢弃。
# 这与 OPT-R17 的原始病灶同类：**声明（"手动"）与实际（"自动"）不一致**。
#
# 断言方式：白名单而非黑名单——将来新增一次性迁移脚本时，人**必须**把它放进
# `manual/`，否则本门判红。黑名单只能拦住「已知的那一个」。
ALLOWED_INITDB_SQL = {"001_schema.sql"}

# `manual/` 下的迁移脚本若重复声明了 `001_schema.sql` 里已有的列，两处的**类型**
# 必须逐字一致。
#
# 为什么需要这条：全新部署走 `001`（权威 schema），存量部署走 `manual/*.sql`
# （就地 ALTER）。两条路径若对同一列给出不同类型，会得到**两种不同的 schema**——
# 而代码只有一份。例如 `001` 写 `trace_id String`、迁移写
# `trace_id LowCardinality(String)`：两者都能建表成功，但 LowCardinality 的
# 字典编码与 `String` 的写入路径不同，某些情况下会静默截断/降级。
# 这类分歧不会让任何测试变红，只会在"新环境正常、老环境异常"时暴露，
# 而那时排查方向通常被误导到代码而不是 schema。
MIGRATION_TYPE_CONSISTENCY = [
    ("manual/003_trace_id.sql", "trace_id"),
]


# Cargo.toml feature -> src/ 中必须出现的标识符。
#
# 为何是显式表而非通用规则：Rust 的 feature 名与它启用的类型名没有机械
# 对应（`timeout` → `TimeoutLayer`，`cors` → `CorsLayer`，但拼写规则各 crate
# 自定）。通用启发式（把 feature 名转驼峰去 grep）在本例恰好能过，但换个
# crate 就会静默漏检——静默漏检的门比没有门更危险。
#
# 只登记**本仓显式列出**的 feature。传递依赖的 feature 由上游 crate 自己
# 声明，不在本门管辖范围（那属于 cargo-deny / audit 的职责）。
FEATURE_TO_SYMBOL = {
    "tower-http/cors": "CorsLayer",
    "tower-http/trace": "TraceLayer",
    "tower-http/timeout": "TimeoutLayer",
}

# 告警投递三要素：OPERATION.md 必须让运维能回答这三个问题。
ALERT_DELIVERY_REQUIRED = [
    ("链路现状", "告警投递"),           # 有专门小节
    ("不外发", "不外发"),               # 明说不会外发（最关键的反误判信息）
    ("route.receiver", "route.receiver"),  # 指向真正的配置文件与字段
]


def parse_listed_features(toml_text: str) -> list[tuple[str, list[str]]]:
    """抽出 `crate = { ..., features = [...] }` 形式的显式 feature 列表。

    只认显式 `features = [...]`（本仓的书写惯例），不解析 `default-features`。
    返回 [(crate_name, [feature, ...]), ...]。
    """
    out: list[tuple[str, list[str]]] = []
    # 匹配：`name = { ... features = [ "a", "b" ] ... }`（跨行），或
    #      `name = { version = "x", features = ["a"] }`。
    for m in re.finditer(
        r"^([A-Za-z0-9_-]+)\s*=\s*\{(.*?)\}", toml_text, re.M | re.S
    ):
        crate, body = m.group(1), m.group(2)
        fm = re.search(r"features\s*=\s*\[([^\]]*)\]", body, re.S)
        if not fm:
            continue
        feats = re.findall(r'"([^"]+)"', fm.group(1))
        if feats:
            out.append((crate, feats))
    return out


def strip_comments(src: str) -> str:
    """剔除 Rust 注释，只留真正的代码 token。

    # 为何必须剔除（实测踩中）

    门写成"在 src/ 全文里 grep `CorsLayer`"时，**自己写的解释性注释**里出现了
    `CorsLayer::permissive()` 这个词（说明为什么不加 CORS），于是即使删掉了
    feature 声明、代码里一行 CORS 都没有，门照样报绿。

    这比"门失效"更糟：它让门**看起来**在工作。双��验证（注入回退必须变红）
    是唯一能发现这类问题的手段——本门因此把双���验证写进了 docstring。

    注释形态：行注释 `//`、块注释 `/* */`（含嵌套，本仓无嵌套但一并处理）。
    粗暴的 `split("//")` 会误伤 URL 字符串（`http://`），故用逐行扫描 + 状态机。
    """
    out: list[str] = []
    in_block = False
    for line in src.splitlines():
        buf: list[str] = []
        i = 0
        n = len(line)
        in_str = False  # 朴素字符串态（够用：本仓无含 // 的字符串字面量）
        while i < n:
            ch = line[i]
            if in_block:
                if line.startswith("*/", i):
                    in_block = False
                    i += 2
                    continue
                i += 1
                continue
            if in_str:
                if ch == "\\":
                    buf.append(line[i : i + 2])
                    i += 2
                    continue
                if ch == '"':
                    in_str = False
                buf.append(ch)
                i += 1
                continue
            if line.startswith("/*", i):
                in_block = True
                i += 2
                continue
            if line.startswith("//", i):
                break  # 丢弃该行剩余部分
            if ch == '"':
                in_str = True
            buf.append(ch)
            i += 1
        out.append("".join(buf))
    return "\n".join(out)


def column_type(sql: str, column: str) -> str | None:
    """从 SQL 文本里取 `column` 的**类型声明**（到行尾或逗号为止）。

    只做本仓需要的最小解析：找形如 `column Type...` 的行，取 `column` 之后的
    部分、去掉行尾逗号与注释。用于比对「权威 schema」与「迁移脚本」对同一列
    是否给出同一类型。
    """
    for raw in sql.splitlines():
        line = raw.split("--", 1)[0].strip()
        if not line:
            continue
        # 迁移脚本里可能带 `ADD COLUMN IF NOT EXISTS` / `AFTER x` 前缀。
        for marker in ("ADD COLUMN IF NOT EXISTS ", "ADD COLUMN "):
            if line.upper().startswith(marker):
                line = line[len(marker) :].strip()
                break
        parts = line.split(None, 1)
        if len(parts) != 2:
            continue
        name, rest = parts[0].strip("`,"), parts[1]
        if name != column:
            continue
        # 截到行内第一个逗号（列定义结束）；`AFTER xxx` 是迁移语法，不是类型。
        rest = rest.split(",")[0]
        if " AFTER " in f" {rest.upper()} ":
            rest = re.split(r"\s+AFTER\s+", rest, flags=re.IGNORECASE)[0]
        return " ".join(rest.split())
    return None


def main() -> int:
    failures: list[str] = []

    for p in (CARGO_TOML, OPERATION):
        if not p.is_file():
            print(f"[check-observability-truth] ERROR: 缺文件 {p}", file=sys.stderr)
            return 1

    # ---------- 失实 A：依赖 feature 必须真被引用 ----------
    toml_text = CARGO_TOML.read_text(encoding="utf-8")
    # 只在**去注释后的代码**里找符号——否则本门会被自己的说明注释满足。
    src_text = strip_comments(
        # **必须 rglob**：目录模块（`router/`、`free_pool/`、`gateway/`、`metrics/`、
        # `admin/`）里的符号用顶层 glob 是看不见的。本门曾用 `SRC_DIR.glob("*.rs")`，
        # 于是 admin 拆成目录后立刻误报「`tower-http/timeout` 声明了但 src 里找不到
        # `TimeoutLayer`」——而它明明就在 `admin/mod.rs` 的 `build_router_with` 里。
        # 这类**假红**比漏检更糟（会被学会忽略），而同样的 glob 缺陷如果反向作用
        # （符号只在子模块里用到、门以为没用到），就会变成**假绿**——那才是真正的
        # 静默失效。故一律递归。
        "\n".join(f.read_text(encoding="utf-8") for f in sorted(SRC_DIR.rglob("*.rs")))
    )

    checked = 0
    for crate, feats in parse_listed_features(toml_text):
        for feat in feats:
            key = f"{crate}/{feat}"
            if key not in FEATURE_TO_SYMBOL:
                # 不在映射表内 ⇒ 无法判定其类型名。刻意**不判失败**也
                # **不打印**：本仓 40+ 个显式 feature 里绝大多数是这类
                # （`tokio/full`、`serde/derive`…），逐个 note 会把真正
                # 的失败信号淹没在 40 行噪声里——这正是 OPT-R12 记录过的
                # 教训（`check_auth_docs.py` 曾把正确的澄清句当失实句判红，
                # 靠"否定感知"修正）。安静跳过。
                continue
            symbol = FEATURE_TO_SYMBOL[key]
            checked += 1
            if symbol not in src_text:
                failures.append(
                    f"依赖失实：Cargo.toml 声明 {crate} 的 feature "
                    f'"{feat}"，但 src/ 全仓找不到 `{symbol}`。'
                    f"要么真正使用它，要么把它从 features 里删掉——"
                    f"声明了却不用，会让读者以为能力存在。"
                )

    # ---------- 失实 B：告警投递必须被文档化 ----------
    op = OPERATION.read_text(encoding="utf-8")
    for label, needle in ALERT_DELIVERY_REQUIRED:
        if needle not in op:
            failures.append(
                f"可观测性失实：docs/OPERATION.md 缺少「{label}」"
                f"（检索串 {needle!r} 未命中）。"
                f"运维读手册必须能知道告警**不会外发**——"
                f"否则会以为有告警、实际故障时收不到。"
            )

    # ---------- 失实 C：initdb 目录里不得有破坏性脚本 ----------
    if CH_INITDB.is_dir():
        for f in sorted(CH_INITDB.glob("*.sql")):
            if f.name not in ALLOWED_INITDB_SQL:
                failures.append(
                    f"部署失实：`deploy/clickhouse/{f.name}` 位于 "
                    f"`docker-entrypoint-initdb.d` 挂载的**顶层**，会被 CH 官方 "
                    f"entrypoint 在**每次全新部署时自动执行**。"
                    f"顶层只允许 {sorted(ALLOWED_INITDB_SQL)}（幂等的权威 schema）。"
                    f"一次性迁移脚本请放进 `deploy/clickhouse/manual/`"
                    f"（子目录不递归执行），并在其文件头写明执行方式。"
                )

    # ---------- 失实 D：迁移脚本与权威 schema 的列类型必须一致 ----------
    schema_file = CH_INITDB / "001_schema.sql"
    if schema_file.is_file():
        schema_sql = schema_file.read_text(encoding="utf-8")
        for rel, column in MIGRATION_TYPE_CONSISTENCY:
            mig = REPO / "deploy" / "clickhouse" / rel
            if not mig.is_file():
                continue
            want = column_type(schema_sql, column)
            got = column_type(mig.read_text(encoding="utf-8"), column)
            if want is None:
                failures.append(
                    f"schema 失实：`001_schema.sql` 里找不到列 `{column}`，"
                    f"而 `{rel}` 声称迁移它。两者必须同时存在或同时不存在。"
                )
            elif got is None:
                failures.append(
                    f"schema 失实：`{rel}` 里找不到列 `{column}` 的 ADD COLUMN，"
                    f"但 `001_schema.sql` 已声明它。迁移脚本漏了这一步，"
                    f"存量部署会缺列。"
                )
            elif want != got:
                failures.append(
                    f"schema 失实：列 `{column}` 类型不一致——"
                    f"`001_schema.sql` 是 `{want}`，`{rel}` 是 `{got}`。"
                    f"全新部署与存量升级会得到**两种不同的 schema**，"
                    f"而代码只有一份。"
                )

    # ---------- 汇报 ----------
    if failures:
        print("[check-observability-truth] FAIL:", file=sys.stderr)
        for f in failures:
            print(f"  - {f}", file=sys.stderr)
        return 1

    print(
        f"[check-observability-truth] OK: {checked} 个显式 feature 均真被引用；"
        f"告警投递三要素在 docs/OPERATION.md 齐备；"
        f"CH initdb 顶层只有 {sorted(ALLOWED_INITDB_SQL)}；"
        f"{len(MIGRATION_TYPE_CONSISTENCY)} 个迁移列类型与权威 schema 一致"
    )
    return 0


if __name__ == "__main__":
    sys.exit(main())
