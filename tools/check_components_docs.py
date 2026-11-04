#!/usr/bin/env python3
r"""模块 ↔ `docs/COMPONENTS.md` 双向结构门（OPT-R18 P1-4）。

## 为什么需要这道门（实测到的四类失真，全部真实存在）

`docs/COMPONENTS.md` 是读者理解「这个网关由哪几块组成」的唯一入口。它的
失效方式和 `check_admin_routes_documented.py` 完全一样：**只改一边，CI 全绿**。
落门时实测到四类失真同时存在：

1. **模块漏登记**：文档声称「模块表（19 个）」，而 `gateway/src/` 实际有
   **24** 个 `.rs`。漏掉的 5 个（`admin.rs` / `janitor.rs` / `net_connector.rs` /
   `node_registry.rs` / `redis_shard.rs`）里，`admin.rs` 是 1788 行的 Admin
   REST 面——读者按文档建心智模型会**完全不知道它的存在**。
2. **中英文自相矛盾**：同一份文件里 `vendor_arbitrage.rs` 中文行写 208 行、
   英文行写 218 行。两行描述同一个文件，必有一行是假的。
3. **关键 API 是编的**：`socks_handshake.rs` 那行写「关键 API：`socks_handshake`」，
   而该字符串在文件里**一次都没出现**（真实 API 是 `establish` / `greet_only`）。
   这类失真最坏：读者按它去搜代码，搜不到，会怀疑整份文档。
4. **单测总数停在 156**：而权威基线是 338。COMPONENTS.md 此前**不在**
   `check_doc_consistency.py` 的 ALLOWLIST 里，所以它自由漂移了十几轮。

## 门做什么（四类断言，全部结构化，不做全文 grep）

| # | 断言 | 拦住的失真 |
| - | ---- | ---------- |
| A | `gateway/src/*.rs` 集合 == 中文表集合 == 英文表集合 | 漏登记 / 幽灵模块 |
| B | 标题里的「N 个」== 实际文件数 == 表格行数 | 标题与表格不同步 |
| C | 每行「单测」== 该文件 `#[test]`/`#[tokio::test]` 数；且**总和 == 基线通过数 + 忽略数** | 数字漂移 / 中英不一致 |
| D | 「关键 API」里每个 token 必须在该 `.rs` 里以整词出现 | 编造的 API 名 |
| E | `DATAFLOW.md` / `COMPONENTS.md` / `ARCHITECTURE.md` 里反引号包裹的 `.rs` 必须存在 | 模块改名后遗留的死引用 |

**为什么不做全文 grep**：OPT-R17 实测过——说明性注释里恰好含目标 token 时，
门会对自己注入的回归报绿。这里一律先**解析成表格结构**再比对单元格。

## 为什么「行数」这一列被删掉了

它无法被低成本地保持准确：任何一行代码改动都会让它过期。若为此设精确门禁，
每个 PR 都会假红；若不设，它就长期是假的。本仓对这类列已有明确结论
（见 `check_free_pool_contract.py` 的「不设会周期性假红的数量门」）——
**保留一个无法维护的数字，就是保留一句对外失实**。故删除，改由「单测」
提供体量信号（它被 C 断言守住，且总和与权威基线自洽）。

## 用法

```bash
python3 tools/check_components_docs.py                # 校验
python3 tools/check_components_docs.py --refresh      # 从源码刷新「单测」列与标题计数
python3 tools/check_components_docs.py --self-test    # 变异自测（三处注入必须全红）
```

## 退出码

- 0：四类断言全部成立
- 1：任一断言不成立（逐条打印文件/模块/双方数值）
"""

from __future__ import annotations

import argparse
import re
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
sys.path.insert(0, str(ROOT / "tools"))

# 复用权威基线的解析（不在这里抄第二份正则）：基线的「通过数 / 忽略数」
# 是本门断言 C 的右端。
from check_doc_consistency import parse_baseline  # noqa: E402

SRC = ROOT / "gateway" / "src"
DOC = ROOT / "docs" / "COMPONENTS.md"

#: 统计测试函数。必须容忍 `#[tokio::test]` 与带参数的形态
#: （`#[tokio::test(flavor = "multi_thread")]`）——只认裸 `#[test]` 会漏计，
#: 而漏计会让「总和 == 基线」这条断言报出难以理解的偏差。
TEST_ATTR_RE = re.compile(r"^\s*#\[(?:tokio::)?test\b[^\]]*\]", re.M)

#: 中文表 / 英文表的表头签名。用列名识别而不是靠出现顺序——文档里还有
#: 「外部依赖」「基础设施」两张表，按顺序取会取错。
ZH_SIGNATURE = ("模块", "单测", "职责", "关键 API")
EN_SIGNATURE = ("Module", "Tests", "Responsibility", "Key APIs")

#: 子模块表的表头签名（OPT-R18 P2-1 引入）。
#:
#: 父模块拆分后，文档里会多出一张「子模块」表。若不校验它，这张表会自由
#: 漂移（子模块改名/删除后，文档仍宣称它存在——正是本门要拦的那类失真）。
ZH_SUB_SIGNATURE = ("子模块", "职责", "关键 API")
EN_SUB_SIGNATURE = ("Submodule", "Responsibility", "Key APIs")

#: 标题里的模块计数：`### 1. 模块表（19 个）` / `### 1. Modules (19)`。
ZH_COUNT_RE = re.compile(r"模块表[（(]\s*(\d+)\s*个\s*[）)]")
EN_COUNT_RE = re.compile(r"###\s*\d+\.\s*Modules\s*[（(]\s*(\d+)\s*[）)]")

#: 断言 E 的扫描范围：这三份文档的职责就是描述「系统由哪些模块构成」。
#:
#: **刻意不扫全部 docs/ 与 plan/**：计划文档里合法地会出现「计划新增
#: `foo.rs`」这类尚未存在的文件名，扫进来会造成周期性假红（假红门禁会被
#: 学会忽略，比没有门更糟）。结构文档里出现不存在的 `.rs` 才是真缺陷。
STRUCTURAL_DOCS = ("DATAFLOW.md", "COMPONENTS.md", "ARCHITECTURE.md")

#: 结构文档里反引号包裹的 `.rs` 文件名。
#:
#: 允许含 `/`（`router/health.rs` 这类子模块路径）——父模块拆分后，
#: 结构文档会引用子模块文件，必须一并校验其存在性，否则改名后会留下
#: 静默的死引用（读者按它去搜，搜不到）。
RS_REF_RE = re.compile(r"`([A-Za-z0-9_]+(?:/[A-Za-z0-9_]+)*\.rs)`")


def all_rs_relative_paths() -> set[str]:
    """`gateway/src/` 下所有 `.rs` 的相对路径（含子模块），如 `router/health.rs`。"""
    return {
        p.relative_to(SRC).as_posix()
        for p in SRC.rglob("*.rs")
        if p.is_file()
    }


def normalize_rs_ref(ref: str) -> str:
    """把文档里的 `.rs` 引用归一到「相对 `gateway/src/`」的形式。

    文档里的写法有两种：
    - `gateway.rs`（相对 `src/`）
    - `gateway/src/gateway.rs`（相对仓库根）

    两者指同一文件。若不归一，仓库根写法会被判成"死引用"——这是**假红**，
    而假红门禁会被学会忽略，比没有门更糟。
    """
    ref = ref.strip().replace("\\", "/")
    for prefix in ("gateway/src/", "src/"):
        if ref.startswith(prefix):
            return ref[len(prefix):]
    return ref




# =============================================================================
# 源码侧
# =============================================================================


def source_modules() -> list[str]:
    """`gateway/src/` 下的模块名，按名排序。

    **只统计顶层模块**：子目录（`router/`、`free_pool/`）不是独立"模块"，而是
    某个顶层模块的内部拆分。读者按文档建心智模型时，心智模型的单位是"职责"，
    而 `free_pool/` 的 6 个文件同属"免费供给线"这一职责——拆成 6 行会让读者
    以为系统由 30 个部件构成。故子模块的测试数**并入其父模块**（见
    `count_tests`），模块表仍是 24 行。

    Rust 有**两种**目录模块写法，本函数都要认：
      * `foo.rs` + `foo/`  —— 父文件在顶层（本仓的 `router` 是这种）；
      * `foo/mod.rs`       —— 父文件在目录里（本仓的 `free_pool` 是这种，
        因为拆分后父模块只剩类型定义与再导出，独立成 `.rs` 反而多一层跳转）。

    早期实现只 glob `*.rs`，于是 `free_pool` 被拆成目录模块后**整个模块从表里
    消失**、它的 60+ 个单测也不再计入合计——合计立刻与权威基线对不上。
    这条断言（C）正是靠"合计 == 基线"抓到的，不是靠人眼。
    """
    names: list[str] = [p.name for p in SRC.glob("*.rs")]
    for d in sorted(SRC.iterdir()):
        if not d.is_dir():
            continue
        mod_rs = d / "mod.rs"
        if mod_rs.is_file():
            names.append(f"{d.name}/mod.rs")
    return sorted(names)


def module_stem(module: str) -> str:
    """模块名 → 其目录名：`router.rs` → `router`，`free_pool/mod.rs` → `free_pool`。

    两种目录模块写法的父模块文件名不同（`router.rs` / `mod.rs`），但**目录名
    恒等于模块名**。所有"拼子模块路径"的地方都必须走这个函数——
    早期直接用 `Path(module).stem`，对 `free_pool/mod.rs` 会得到 `mod`，
    于是报出 `gateway/src/mod/verify.rs 存在但表里没有` 这种**假红**。
    """
    if module.endswith("/mod.rs"):
        return Path(module).parent.name
    return Path(module).stem


def module_dir(module: str) -> Path | None:
    """该模块的子模块目录（`router.rs` → `router/`），不存在则 `None`。

    Rust 的模块系统允许 `foo.rs` 与 `foo/` 并存：`foo.rs` 是父模块，
    `foo/*.rs` 是它的子模块。本仓的 `router` 正是这种结构（OPT-R18 P2-1
    把 3564 行的 `router.rs` 拆成 5 个职责子模块 + 2 个测试文件）。
    """
    if module.endswith("/mod.rs"):
        d = SRC / Path(module).parent.name
        return d if d.is_dir() else None
    d = SRC / Path(module).stem
    return d if d.is_dir() else None


def module_files(module: str) -> list[Path]:
    """该模块的**全部**源文件：父文件 + 子模块目录下所有 `.rs`。"""
    files: list[Path] = []
    if module.endswith("/mod.rs"):
        # 目录模块：`free_pool/mod.rs` 是父文件，其余 `.rs` 是子模块。
        d = SRC / Path(module).parent.name
        return sorted(d.glob("*.rs")) if d.is_dir() else []
    f = SRC / module
    if f.is_file():
        files.append(f)
    d = module_dir(module)
    if d is not None:
        files.extend(sorted(d.glob("*.rs")))
    return files


def count_tests(module: str) -> int:
    """该模块里的测试函数个数（含其子模块）。

    口径 = `#[test]` / `#[tokio::test]` 属性个数。实测该校验成立：
    24 个模块求和 == 通过数 + 忽略数，与 `cargo test` 的报数精确相等
    （见断言 C）。若将来引入宏生成测试，这条等式会先红——那正是我们希望
    它先红的地方。

    **子模块的测试并入父模块**：读者看到的是"选路模块有 62 个单测"，
    而不是"选路模块 0 个单测、另外 7 个不知名的文件各有一些"。
    """
    return sum(
        len(TEST_ATTR_RE.findall(f.read_text(encoding="utf-8")))
        for f in module_files(module)
    )


def module_source(module: str) -> str:
    """该模块（含子模块）的源码全文，用于校验「关键 API」真实存在。"""
    return "\n".join(f.read_text(encoding="utf-8") for f in module_files(module))



# =============================================================================
# 文档侧
# =============================================================================


def split_row(line: str) -> list[str] | None:
    s = line.strip()
    if not s.startswith("|") or not s.endswith("|"):
        return None
    return [c.strip() for c in s[1:-1].split("|")]


def parse_tables(text: str) -> list[tuple[list[str], list[list[str]]]]:
    """解析全文所有 markdown 表格，返回 `[(表头, 数据行), ...]`。

    先解析再挑选，而不是"按出现顺序取第 N 张表"——后者在文档增删一节表格时
    会静默取错对象，给出完全无关的报错。
    """
    lines = text.split("\n")
    tables: list[tuple[list[str], list[list[str]]]] = []
    i = 0
    while i < len(lines):
        header = split_row(lines[i])
        if header is None:
            i += 1
            continue
        sep = split_row(lines[i + 1]) if i + 1 < len(lines) else None
        if sep is None or not all(re.fullmatch(r":?-{3,}:?", c) for c in sep if c):
            i += 1
            continue
        rows: list[list[str]] = []
        j = i + 2
        while j < len(lines):
            cells = split_row(lines[j])
            if cells is None or len(cells) != len(header):
                break
            rows.append(cells)
            j += 1
        tables.append((header, rows))
        i = j
    return tables


def pick_tables(
    tables: list[tuple[list[str], list[list[str]]]], signature: tuple[str, ...]
) -> list[tuple[list[str], list[list[str]]]]:
    """**全部**匹配该表头签名的表（不是第一张）。

    为什么必须返回全部：一个模块拆成子模块后，文档里就会出现**多张**同签名的
    「子模块」表——`router` 一张、`free_pool` 一张。早期只取第一张，于是第二张
    整表被忽略：门禁报「`free_pool/verify.rs` 存在但子模块表里没有它」，
    而表里明明写着——**门禁自己产生了假红**，比没有门更糟（会被学会忽略）。
    """
    return [(h, r) for h, r in tables if tuple(h) == signature]


def module_of(cell: str) -> str:
    r"""反引号包裹的模块名 → 裸文件名：``\`main.rs\``` / `main.rs` → `main.rs`。"""
    return cell.strip().strip("`").strip()


def _deep_copy_doc(doc: dict) -> dict:
    """深拷贝解析结果，供 `--self-test` 注入失真用。

    必须拷贝**所有**键（含 `sub_zh` / `sub_en` / `counts`）——早先只拷
    `zh`/`en`，结果注入时把子模块表整个丢掉，自测自己把门打挂了（报
    `KeyError: 'sub_zh'`）。注入工具自身出错，等于自测没跑。
    """
    out: dict = {}
    for k, v in doc.items():
        if isinstance(v, dict):
            out[k] = {
                n: (dict(r) if isinstance(r, dict) else r) for n, r in v.items()
            }
        else:
            out[k] = v
    return out


def parse_doc(text: str) -> dict:
    """把文档解析成 `{zh: {module: row}, en: {module: row}}` + 标题计数 + 子模块表。"""
    tables = parse_tables(text)
    out: dict = {"zh": {}, "en": {}, "counts": {}, "sub_zh": {}, "sub_en": {}}
    for key, sig in (("zh", ZH_SIGNATURE), ("en", EN_SIGNATURE)):
        # 主模块表**只有一张**（模块表是全仓唯一清单），故取第一张即可；
        # 子模块表可能有多张（每个被拆分的父模块一张），见下面的 pick_tables。
        found = pick_tables(tables, sig)
        if not found:
            continue
        header, rows = found[0]
        for cells in rows:
            record = dict(zip(header, cells))
            out[key][module_of(cells[0])] = record

    for key, sig in (("sub_zh", ZH_SUB_SIGNATURE), ("sub_en", EN_SUB_SIGNATURE)):
        for header, rows in pick_tables(tables, sig):
            for cells in rows:
                record = dict(zip(header, cells))
                out[key][module_of(cells[0])] = record

    m = ZH_COUNT_RE.search(text)
    out["counts"]["zh"] = int(m.group(1)) if m else None
    m = EN_COUNT_RE.search(text)
    out["counts"]["en"] = int(m.group(1)) if m else None
    return out


# =============================================================================
# 断言
# =============================================================================


def api_tokens(cell: str) -> list[str]:
    """「关键 API」单元格 → token 列表。

    单元格形如 `` `a/b/c` ``：按 `/` 拆；`ProxyNode::new` 这类取最后一段
    （`new`），因为读者要搜的是可搜到的标识符。
    """
    raw = cell.strip().strip("`")
    if not raw:
        return []
    return [t.split("::")[-1].strip() for t in raw.split("/") if t.strip()]


def check(
    text: str,
    *,
    extra_modules: frozenset[str] = frozenset(),
    doc_override: dict | None = None,
    api_override: dict[str, str] | None = None,
) -> list[str]:
    """返回问题列表；空列表即全绿。

    三个 override 参数只服务于 `--self-test`：注入失真，验证本门有鉴别力。
    """
    problems: list[str] = []
    baseline = parse_baseline()

    src_set = set(source_modules()) | set(extra_modules)
    doc = doc_override if doc_override is not None else parse_doc(text)

    if not doc["zh"] or not doc["en"]:
        missing = [k for k in ("zh", "en") if not doc[k]]
        return [
            f"解析不到 {'中文' if 'zh' in missing else ''}"
            f"{'/' if len(missing) == 2 else ''}"
            f"{'英文' if 'en' in missing else ''}模块表。"
            f"表头必须是 {list(ZH_SIGNATURE)} / {list(EN_SIGNATURE)}，"
            "且表头下紧跟 `| --- |` 分隔行。"
        ]

    # ---- A. 三集合相等（双向）----
    for key, label in (("zh", "中文表"), ("en", "英文表")):
        documented = set(doc.get(key) or {})
        for m in sorted(src_set - documented):
            problems.append(f"[A] `gateway/src/{m}` 存在，但{label}里没有它（读者不知道它存在）")
        for m in sorted(documented - src_set):
            problems.append(f"[A] {label}写了 `{m}`，但 `gateway/src/{m}` 不存在（幽灵模块）")

    # ---- B. 标题计数 ----
    for key, label in (("zh", "中文标题「模块表（N 个）」"), ("en", "英文标题「Modules (N)」")):
        n = doc["counts"].get(key)
        if n is None:
            problems.append(f"[B] 找不到{label}，无法核对模块数")
        elif n != len(src_set):
            problems.append(f"[B] {label}写着 {n}，实际有 {len(src_set)} 个 `.rs` 文件")

    # ---- C. 单测数：逐模块 + 总和 ----
    total_doc = 0
    for m in sorted(src_set):
        if m not in doc["zh"] or m not in doc["en"]:
            continue  # A 已经报过了，不重复刷屏
        actual = count_tests(m)
        zh_val = doc["zh"][m].get("单测", "")
        en_val = doc["en"][m].get("Tests", "")
        total_doc += actual
        if zh_val != en_val:
            problems.append(f"[C] `{m}` 单测数中英不一致：中文 {zh_val!r} / 英文 {en_val!r}")
        for val, label in ((zh_val, "中文"), (en_val, "英文")):
            if not val.isdigit():
                problems.append(f"[C] `{m}` 的{label}单测数不是整数：{val!r}")
            elif int(val) != actual:
                problems.append(
                    f"[C] `{m}` 单测数：{label}文档 {val} / 源码实有 {actual}"
                )

    want_total = baseline["tests"] + baseline["ignored"]
    if total_doc and total_doc != want_total:
        problems.append(
            f"[C] 按模块统计的单测之和 {total_doc} ≠ 权威基线（通过 {baseline['tests']}"
            f" + 忽略 {baseline['ignored']} = {want_total}）。"
            "差额说明有测试落在未登记的模块里，或计数口径变了。"
        )

    # ---- D. 关键 API 必须是真实标识符 ----
    for m in sorted(src_set):
        if m not in doc["zh"]:
            continue
        cell = (api_override or {}).get(m) or doc["zh"][m].get("关键 API", "")
        tokens = api_tokens(cell)
        if not tokens:
            problems.append(f"[D] `{m}` 的「关键 API」为空——这一列对读者无信息量")
            continue
        body = module_source(m) if (SRC / m).is_file() else ""
        for tok in tokens:
            if not re.search(rf"\b{re.escape(tok)}\b", body):
                problems.append(
                    f"[D] `{m}` 声称关键 API 含 `{tok}`，但该标识符在文件里不存在"
                    "（读者会搜不到，进而怀疑整份文档）"
                )

    # ---- E. 结构文档引用的 .rs 必须存在（防模块改名/删除后留下死引用）----
    # 校验范围 = `gateway/src/` 下**全部** `.rs` 的相对路径（含子模块），
    # 而不只是顶层模块——否则 `router/health.rs` 这类引用永远不被校验。
    rs_paths = all_rs_relative_paths()
    for doc_name in STRUCTURAL_DOCS:
        path = ROOT / "docs" / doc_name
        if not path.is_file():
            continue
        for lineno, line in enumerate(path.read_text(encoding="utf-8").split("\n"), 1):
            for ref in RS_REF_RE.findall(line):
                if normalize_rs_ref(ref) not in rs_paths:
                    problems.append(
                        f"[E] `docs/{doc_name}:{lineno}` 引用了 `{ref}`，"
                        "但 `gateway/src/` 下没有该文件（改名/删除后遗留的死引用）"
                    )

    # ---- F. 子模块表必须与磁盘上的子模块集合一致，且关键 API 真实存在 ----
    # 父模块拆分后（OPT-R18 P2-1），文档多出一张「子模块」表。若不校验它，
    # 子模块改名/删除后文档会继续宣称它存在——正是本门要拦的那类失真。
    for key, label in (("sub_zh", "中文子模块表"), ("sub_en", "英文子模块表")):
        documented = set(doc[key])
        # 排除 `mod.rs`：它是**父模块自身**，不是子模块。
        #
        # 这条排除是 `free_pool` 改成目录模块后才暴露的：`router` 用的是
        # `router.rs` + `router/`，目录里没有 `mod.rs`，故早期实现直接
        # glob 整个目录也没出问题；而 `free_pool` 用的是 `free_pool/mod.rs`，
        # 于是 `mod.rs` 被误当成子模块，报「`free_pool/mod.rs` 存在但子模块表
        # 里没有它」——**门禁自己产生了假红**。假红比没有门更糟（会被学会忽略）。
        actual = {
            f"{module_stem(m)}/{p.name}"
            for m in source_modules()
            for p in (module_dir(m).glob("*.rs") if module_dir(m) else [])
            if p.name != "mod.rs"
        }
        if not documented and not actual:
            continue  # 没有子模块的模块不需要这张表
        if not documented:
            problems.append(
                f"[F] 磁盘上有子模块 {sorted(actual)}，但{label}里没有这张表"
                "（读者不知道 `router` 已被拆分）"
            )
            continue
        for m in sorted(actual - documented):
            problems.append(
                f"[F] `gateway/src/{m}` 存在，但{label}里没有它（读者不知道它存在）"
            )
        for m in sorted(documented - actual):
            problems.append(
                f"[F] {label}写了 `{m}`，但磁盘上没有该文件（幽灵子模块）"
            )
        # 关键 API 必须在该子模块文件里真实出现。
        for m in sorted(documented & actual):
            cell = doc[key][m].get("关键 API", "").strip()
            if not cell or cell == "—":
                continue
            body = (SRC / m).read_text(encoding="utf-8")
            for tok in api_tokens(cell):
                if not re.search(rf"\b{re.escape(tok)}\b", body):
                    problems.append(
                        f"[F] `{m}` 声称关键 API 含 `{tok}`，但该标识符在文件里不存在"
                        "（读者会搜不到，进而怀疑整份文档）"
                    )

    return problems


# =============================================================================
# 刷新（从源码回写数字，避免手抄）
# =============================================================================


def refresh(text: str) -> str:
    """把「单测」列与标题计数按源码重写；其余字节不动。"""
    counts = {m: count_tests(m) for m in source_modules()}
    n = len(counts)

    def fix_row(line: str) -> str:
        cells = split_row(line)
        if cells is None or len(cells) != len(ZH_SIGNATURE):
            return line
        name = module_of(cells[0])
        if name not in counts:
            return line
        # 只替换第 2 格（单测），保留行内其余格式与空格。
        return re.sub(
            r"^(\|\s*`?" + re.escape(name) + r"`?\s*\|)\s*\d+\s*\|",
            lambda mo: f"{mo.group(1)} {counts[name]} |",
            line,
        )

    text = "\n".join(fix_row(line) for line in text.split("\n"))
    text = ZH_COUNT_RE.sub(lambda mo: mo.group(0).replace(mo.group(1), str(n)), text)
    text = EN_COUNT_RE.sub(lambda mo: mo.group(0).replace(mo.group(1), str(n)), text)
    return text


def main() -> int:
    ap = argparse.ArgumentParser()
    ap.add_argument("--doc", type=Path, default=DOC)
    ap.add_argument("--refresh", action="store_true", help="从源码刷新数字后写回")
    ap.add_argument(
        "--self-test",
        action="store_true",
        help="变异自测：三处注入（漏登记模块 / 改单测数 / 编造 API）必须全部判红",
    )
    args = ap.parse_args()

    if not args.doc.is_file():
        print(f"[check-components-docs] FAIL: 找不到 {args.doc}", file=sys.stderr)
        return 1

    text = args.doc.read_text(encoding="utf-8")

    if args.refresh:
        updated = refresh(text)
        if updated != text:
            args.doc.write_text(updated, encoding="utf-8")
            print(f"[check-components-docs] 已刷新 {args.doc.relative_to(ROOT)} 的单测列与标题计数")
        else:
            print("[check-components-docs] 刷新后无变化")
        text = updated

    problems = check(text)
    if problems:
        print(
            "[check-components-docs] FAIL: 模块表与 `gateway/src/` 不一致。",
            file=sys.stderr,
        )
        for p in problems:
            print(f"  - {p}", file=sys.stderr)
        print(
            "  修法：`python3 tools/check_components_docs.py --refresh` 刷新数字；"
            "新增/删除模块时手工补/删整行（职责与关键 API 是判断，不能自动生成）。",
            file=sys.stderr,
        )
        return 1

    print(
        f"[check-components-docs] OK: {len(source_modules())} 个模块三方一致，"
        f"单测合计 {sum(count_tests(m) for m in source_modules())} 与基线自洽。"
    )

    if not args.self_test:
        return 0

    # ---- 变异自测：三处注入，必须各自判红 ----
    probes: list[tuple[str, list[str]]] = []

    # ① 源码多出一个未登记模块（模拟"加了 .rs 忘了写文档"）
    probes.append(
        (
            "漏登记模块",
            check(text, extra_modules=frozenset({"ghost_module.rs"})),
        )
    )

    # ② 篡改某模块的单测数（模拟数字漂移）
    tampered = _deep_copy_doc(parse_doc(text))
    target = "router.rs"
    if target in tampered["zh"]:
        old = tampered["zh"][target].get("单测", "0")
        tampered["zh"][target]["单测"] = str(int(old) + 1 if old.isdigit() else 1)
    probes.append(("篡改单测数", check(text, doc_override=tampered)))

    # ③ 编造一个不存在的 API（模拟文档写了源码里没有的东西）
    probes.append(
        (
            "编造 API",
            check(text, api_override={"router.rs": "select_node_excluding/no_such_api_xyz"}),
        )
    )

    # ④ 子模块表漏登记（模拟"父模块拆了子模块但文档没写"）
    sub_tampered = _deep_copy_doc(parse_doc(text))
    sub_tampered["sub_zh"] = {
        n: r for n, r in sub_tampered["sub_zh"].items() if n != "router/health.rs"
    }
    probes.append(("漏登记子模块", check(text, doc_override=sub_tampered)))

    # ⑤ 子模块表编造幽灵文件（模拟"文档写了磁盘上没有的子模块"）
    ghost_tampered = _deep_copy_doc(parse_doc(text))
    ghost_tampered["sub_zh"]["router/ghost.rs"] = {
        "子模块": "router/ghost.rs",
        "职责": "编造的子模块",
        "关键 API": "—",
    }
    probes.append(("幽灵子模块", check(text, doc_override=ghost_tampered)))

    green = [name for name, problems in probes if not problems]
    if green:
        print(
            f"[check-components-docs] FAIL: 变异自测未判红——这些注入仍然通过：{green}，"
            "说明对应断言失效（门是摆设）",
            file=sys.stderr,
        )
        return 1
    print(
        "[check-components-docs] self-test OK: 漏登记模块 / 篡改单测数 / 编造 API / "
        "漏登记子模块 / 幽灵子模块 五处注入全部正确判红。"
    )
    return 0


if __name__ == "__main__":
    sys.exit(main())
