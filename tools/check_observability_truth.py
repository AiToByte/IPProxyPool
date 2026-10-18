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

# 用法

    python tools/check_observability_truth.py

# 退出码

- 0：两项契约均在位
- 1：失实（打印具体项）

# 双向验证记录（OPT-R17）

- 向 Cargo.toml 加回 `features = ["cors"]` → 门红（cors 在 src/ 无引用）
- 从 OPERATION.md 删掉「告警投递」小节 → 门红
- 还原后 → 门绿
"""

import re
import sys
from pathlib import Path

REPO = Path(__file__).resolve().parent.parent
CARGO_TOML = REPO / "gateway" / "Cargo.toml"
SRC_DIR = REPO / "gateway" / "src"
OPERATION = REPO / "docs" / "OPERATION.md"

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
        "\n".join(f.read_text(encoding="utf-8") for f in sorted(SRC_DIR.glob("*.rs")))
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

    # ---------- 汇报 ----------
    if failures:
        print("[check-observability-truth] FAIL:", file=sys.stderr)
        for f in failures:
            print(f"  - {f}", file=sys.stderr)
        return 1

    print(
        f"[check-observability-truth] OK: {checked} 个显式 feature 均真被引用；"
        f"告警投递三要素在 docs/OPERATION.md 齐备"
    )
    return 0


if __name__ == "__main__":
    sys.exit(main())
