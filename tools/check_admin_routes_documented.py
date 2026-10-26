#!/usr/bin/env python3
r"""双向校验 Admin HTTP 路由与 docs/ADMIN_API.md 的端点清单。

为什么需要这道门
----------------
代码加了路由但文档没写 -> 客户端按文档调用不到；文档写了但代码没注册 ->
调用必 404。两种漂移都只能靠人眼，CI 不会红。

设计约束
--------
只比对 `### METHOD \`/api/v1/...\`` 标题与 `build_router_with` 里的
`.route(path, method(handler))`，**不做全文模糊匹配**：否则说明文字或 curl
示例里的路径就能让门假绿（OPT-R17 的教训：门被自己的注释满足）。

用法
----
  python3 tools/check_admin_routes_documented.py
  python3 tools/check_admin_routes_documented.py --admin X --doc Y  # 变异自测
"""

from __future__ import annotations

import argparse
import re
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
DEFAULT_ADMIN = ROOT / "gateway" / "src" / "admin.rs"
DEFAULT_DOC = ROOT / "docs" / "ADMIN_API.md"
API_PREFIX = "/api/v1"


class GateError(Exception):
    pass


def strip_line_comments(text: str) -> str:
    """丢掉整行 `//` 注释，避免注释里的 `.route(...)` 被当成真实注册。"""
    return "\n".join(
        line for line in text.splitlines() if not line.lstrip().startswith("//")
    )


def route_block(src: str) -> str:
    """截取 build_router_with 的注册区（到 Router::new() 为止）。"""
    start = src.find("fn build_router_with(")
    if start < 0:
        raise GateError("找不到 build_router_with（路由注册入口被改名？）")
    end = src.find("Router::new()", start)
    if end < 0:
        raise GateError("build_router_with 内找不到 Router::new() 终点")
    return strip_line_comments(src[start:end])


def matching_paren(text: str, open_idx: int) -> int:
    """返回与 text[open_idx] 的 '(' 配对的 ')' 下标（跳过字符串字面量）。"""
    depth = 0
    in_str = False
    escaped = False
    for i in range(open_idx, len(text)):
        ch = text[i]
        if in_str:
            if escaped:
                escaped = False
            elif ch == "\\":
                escaped = True
            elif ch == '"':
                in_str = False
            continue
        if ch == '"':
            in_str = True
        elif ch == "(":
            depth += 1
        elif ch == ")":
            depth -= 1
            if depth == 0:
                return i
    raise GateError("括号不配对（解析器需要更新）")


def normalize(path: str) -> str:
    """axum 的 `:addr` 与文档的 `{addr}` 归一为同一形态。"""
    return re.sub(r":([A-Za-z_][A-Za-z0-9_]*)", r"{\1}", path)


def code_routes(src: str) -> set[tuple[str, str]]:
    block = route_block(src)
    routes: set[tuple[str, str]] = set()
    count = 0
    for match in re.finditer(r"\.route\s*\(", block):
        count += 1
        open_idx = block.find("(", match.start())
        close_idx = matching_paren(block, open_idx)
        args = block[open_idx + 1 : close_idx]

        path_match = re.match(r'\s*"([^"]+)"\s*,', args)
        if not path_match:
            raise GateError(f"无法解析 .route 参数: {args[:80]!r}")
        path = normalize(path_match.group(1))

        # 覆盖 get(a).post(b) 这类链式写法；handler 名不会以 `get(` 开头。
        methods = re.findall(r"\b(get|post|patch|delete)\s*\(", args[path_match.end() :])
        if not methods:
            raise GateError(f"路由 {path} 没有解析到 HTTP method")
        for method in methods:
            routes.add((method.upper(), API_PREFIX + path))

    if count == 0:
        raise GateError("没有解析到任何 .route(...)，拒绝假绿")
    return routes


def doc_routes(md: str) -> tuple[set[tuple[str, str]], list[tuple[str, str]]]:
    routes: set[tuple[str, str]] = set()
    duplicates: list[tuple[str, str]] = []
    in_fence = False
    for line in md.splitlines():
        if line.strip().startswith("```"):
            in_fence = not in_fence
            continue
        if in_fence:
            continue
        match = re.match(r"^###\s+(GET|POST|PATCH|DELETE)\s+`([^`]+)`", line)
        if not match:
            continue
        method, path = match.group(1), normalize(match.group(2))
        if not path.startswith(API_PREFIX + "/"):
            raise GateError(f"文档端点不在 {API_PREFIX} 下: {line.strip()}")
        key = (method, path)
        if key in routes:
            duplicates.append(key)
        routes.add(key)
    return routes, duplicates


def fmt(items: set[tuple[str, str]]) -> str:
    return "\n".join(f"  {method} {path}" for method, path in sorted(items))


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--admin", type=Path, default=DEFAULT_ADMIN)
    parser.add_argument("--doc", type=Path, default=DEFAULT_DOC)
    args = parser.parse_args()

    try:
        code = code_routes(args.admin.read_text(encoding="utf-8"))
        docs, duplicates = doc_routes(args.doc.read_text(encoding="utf-8"))
    except (OSError, GateError) as exc:
        print(f"[check-admin-routes] FAIL: {exc}", file=sys.stderr)
        return 1

    missing = code - docs
    extra = docs - code
    failed = False

    if duplicates:
        failed = True
        print(
            "[check-admin-routes] FAIL: 文档重复端点: "
            + ", ".join(f"{method} {path}" for method, path in duplicates),
            file=sys.stderr,
        )
    if missing:
        failed = True
        print(
            "[check-admin-routes] FAIL: 代码已注册但文档未写（客户端按文档会 404）:",
            file=sys.stderr,
        )
        print(fmt(missing), file=sys.stderr)
    if extra:
        failed = True
        print(
            "[check-admin-routes] FAIL: 文档已写但代码未注册（调用必失败）:",
            file=sys.stderr,
        )
        print(fmt(extra), file=sys.stderr)
    if failed:
        return 1

    print(f"[check-admin-routes] OK: {len(code)} 个 method+path 双向一致")
    return 0


if __name__ == "__main__":
    sys.exit(main())
