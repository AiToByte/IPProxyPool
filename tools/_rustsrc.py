"""门禁脚本共用：读取 Rust 模块源码（兼容单文件与目录模块两种布局）。

# 为什么需要这个助手

本仓的模块拆分（OPT-R18 P2-1）把多个巨型单文件改成了**目录模块**：
`router/`、`free_pool/`、`gateway/`、`metrics/`。而门禁脚本早期一律写死
`gateway/src/xxx.rs` 这样的单文件路径，于是**每拆一个模块就有一批门禁报"缺文件"**：

- `check_components_docs.py` 只 glob 顶层 `*.rs` ⇒ 目录模块整个从模块表消失；
- `check_free_pool_contract.py` 写死 `free_pool.rs` ⇒ 拆完即红；
- `check_promql_metrics.py` 写死 `metrics.rs` ⇒ 拆完即红。

三次都是同一个根因：**"模块"在 Rust 里有两种合法物理形态，而脚本只认一种**。
故把"读模块源码"收敛到这里，新增门禁一律走本助手，不要再自己拼路径。

# 用法

    from _rustsrc import module_source
    text = module_source(REPO / "gateway" / "src", "metrics")

`module_source` 会按以下顺序找（与 rustc 的模块解析一致）：
  1. `<src>/<module>/mod.rs`（目录模块，父文件在目录里）
  2. `<src>/<module>.rs`（单文件）
两种布局都存在时优先目录形态——那是拆分后的权威形态。
"""

from __future__ import annotations

from pathlib import Path


def module_files(src_dir: Path, module: str) -> list[Path]:
    """该模块的**全部**源文件（父文件 + 子模块），按路径排序。

    单文件布局返回 `[foo.rs]`；目录布局返回 `foo/` 下所有 `.rs`
    （含 `mod.rs`——它就是父文件）。
    """
    d = src_dir / module
    if (d / "mod.rs").is_file():
        return sorted(d.glob("*.rs"))
    f = src_dir / f"{module}.rs"
    if f.is_file():
        return [f]
    return []


def module_source(src_dir: Path, module: str) -> str:
    """该模块全部源码拼接（找不到则返回空串，调用方自行判空报错）。

    **拼接而非只读父文件**：调用方多是 grep 指标名/符号名，子模块里也可能有；
    只读 `mod.rs` 会漏掉拆到子模块的内容——那正是拆分后最典型的误判来源。
    """
    return "\n".join(p.read_text(encoding="utf-8") for p in module_files(src_dir, module))
