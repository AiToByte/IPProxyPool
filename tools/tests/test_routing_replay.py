"""选路回放的 ClickHouse HTTP 契约与合成数字回归测试。"""

from __future__ import annotations

import contextlib
import io
import json
import os
import re
import subprocess
import sys
import threading
import unittest
from collections.abc import Iterator
from datetime import datetime
from http.server import BaseHTTPRequestHandler, HTTPServer
from pathlib import Path
from queue import Queue
from unittest.mock import patch

ROOT = Path(__file__).resolve().parents[2]
sys.path.insert(0, str(ROOT / "tools"))
import routing_replay as replay  # noqa: E402


class Schema:
    """从权威 DDL 提取契约，避免把生产代码里的错误表名抄成测试常量。"""

    def __init__(self) -> None:
        ddl = (ROOT / "deploy/clickhouse/001_schema.sql").read_text(encoding="utf-8")
        ddl = re.sub(r"--[^\n]*", "", ddl)
        table = re.search(
            r"CREATE TABLE IF NOT EXISTS\s+(\w+)\.(\w+)\s*\((.*?)\)\s*ENGINE",
            ddl,
            re.S,
        )
        if table is None:
            raise AssertionError("权威 DDL 中未找到遥测建表语句")
        self.database, self.table = table.group(1, 2)
        self.columns = dict(
            re.findall(r"^\s*(\w+)\s+([^\n]+)", table.group(3), re.M)
        )
        self.columns.pop("INDEX", None)
        self.time_column = next(
            name for name, kind in self.columns.items() if kind.startswith("DateTime64(3,")
        )


@contextlib.contextmanager
def clickhouse_stub(
    empty_body: str | None = None,
) -> Iterator[tuple[str, Queue[tuple[str, str, dict[str, str], str]]]]:
    """收取真实 HTTP 请求；测试数据和凭据均为虚构，关闭访问日志防止泄漏。"""
    requests: Queue[tuple[str, str, dict[str, str], str]] = Queue()
    schema = Schema()

    class Handler(BaseHTTPRequestHandler):
        def log_message(self, format: str, *args: object) -> None:
            pass

        def do_POST(self) -> None:
            sql = self.rfile.read(int(self.headers["Content-Length"])).decode("utf-8")
            requests.put((self.command, self.path, dict(self.headers.items()), sql))
            # 仅解释时间投影：秒级转换必须真的丢失小数，不能靠固定小数响应掩盖错误。
            milliseconds = re.search(
                rf"toUnixTimestamp64Milli\(\s*{schema.time_column}\s*\)"
                r"\s*/\s*1000(?:\.0)?\s+AS\s+ts\b",
                sql,
                re.I,
            )
            rows = []
            for index, fraction in enumerate(("123", "987")):
                ts = datetime.fromisoformat(f"2026-10-27T12:34:56.{fraction}+00:00").timestamp()
                rows.append(
                    {
                        "ts": ts if milliseconds else int(ts),
                        "trace_id": f"stub-trace-{index}",
                        "target_domain": "示例.example",
                        "out_ip": "192.0.2.1",
                        "provider": "stub-provider",
                        "tier": "datacenter",
                        "status_code": 200 if index == 0 else 502,
                        "latency_ms": 123 + index,
                    }
                )
            body = empty_body if empty_body is not None else (
                "\n".join(json.dumps(row, ensure_ascii=False) for row in rows) + "\n"
            )
            payload = body.encode("utf-8")
            self.send_response(200)
            self.send_header("Content-Type", "application/x-ndjson; charset=utf-8")
            self.send_header("Content-Length", str(len(payload)))
            self.end_headers()
            self.wfile.write(payload)

    server = HTTPServer(("127.0.0.1", 0), Handler)
    thread = threading.Thread(target=server.serve_forever, kwargs={"poll_interval": 0.01}, daemon=True)
    thread.start()
    try:
        yield f"http://127.0.0.1:{server.server_port}/", requests
    finally:
        server.shutdown()
        server.server_close()
        thread.join(timeout=2)


class ClickHouseReplayTests(unittest.TestCase):
    def assert_schema_query(self, sql: str, database: str, hours: int) -> None:
        schema = Schema()
        # 表名和列都取自 DDL；连同 WHERE/ORDER BY 检查，避免只修 SELECT。
        target = re.search(r"\bFROM\s+(\S+)", sql, re.I)
        self.assertIsNotNone(target)
        self.assertEqual(target.group(1).replace("`", ""), f"{database}.{schema.table}")
        projection = re.search(r"\bAS ts,\s*(.*?)\s+FROM\b", sql, re.I)
        self.assertIsNotNone(projection)
        columns = {name.strip() for name in projection.group(1).split(",")}
        self.assertEqual(
            columns,
            {"trace_id", "target_domain", "out_ip", "provider", "tier", "status_code", "latency_ms"},
        )
        self.assertTrue(columns <= schema.columns.keys())
        self.assertRegex(sql, rf"WHERE\s+{schema.time_column}\s*>=\s*now64\(3\)")
        self.assertRegex(sql, rf"INTERVAL\s+{hours}\s+HOUR\b")
        self.assertRegex(sql, rf"ORDER BY\s+{schema.time_column}\s+ASC\b")
        self.assertRegex(sql, r"FORMAT\s+JSONEachRow\s*$")
        self.assertNotRegex(sql, r"\btimestamp\b")

    def test_default_loader_posts_schema_query_and_credentials_in_headers(self) -> None:
        with patch.dict(os.environ, {}, clear=False):
            os.environ.pop("CLICKHOUSE_DB", None)
            with clickhouse_stub() as (url, requests):
                rows = replay.load_clickhouse(url, "stub-user", "stub-key", 24)
                method, path, headers, sql = requests.get(timeout=2)
        self.assertEqual((method, path), ("POST", "/"))
        headers = {name.lower(): value for name, value in headers.items()}
        self.assertEqual(headers["x-clickhouse-user"], "stub-user")
        self.assertEqual(headers["x-clickhouse-key"], "stub-key")
        self.assertNotIn("stub-key", sql + path)
        self.assertNotIn("stub-user", sql + path)
        self.assert_schema_query(sql, Schema().database, 24)
        self.assertEqual(len(rows), 2)
        self.assertEqual(rows[0].target_domain, "示例.example")
        self.assertEqual((rows[0].trace_id, rows[1].status_code), ("stub-trace-0", 502))
        self.assertEqual((rows[0].provider, rows[0].tier, rows[0].latency_ms),
                         ("stub-provider", "datacenter", 123.0))

    def test_millisecond_timestamp_projection_preserves_same_second_events(self) -> None:
        with patch.dict(os.environ, {}, clear=False):
            os.environ.pop("CLICKHOUSE_DB", None)
            with clickhouse_stub() as (url, requests):
                rows = replay.load_clickhouse(url, "stub-user", "stub-key", 1)
                sql = requests.get(timeout=2)[3]
        for row, fraction in zip(rows, ("123", "987")):
            expected = datetime.fromisoformat(f"2026-10-27T12:34:56.{fraction}+00:00").timestamp()
            self.assertAlmostEqual(row.ts, expected, places=6)
        self.assertAlmostEqual(rows[1].ts - rows[0].ts, 0.864, places=6)
        self.assert_schema_query(sql, Schema().database, 1)

    def test_loader_accepts_custom_database(self) -> None:
        with clickhouse_stub() as (url, requests):
            rows = replay.load_clickhouse(url, "stub-user", "stub-key", 7, database="replay_ci")
            sql = requests.get(timeout=2)[3]
        self.assertEqual(len(rows), 2)
        self.assert_schema_query(sql, "replay_ci", 7)

    def test_cli_database_default_environment_and_explicit_override(self) -> None:
        cases = ((None, [], Schema().database),
                 ("tenant_warehouse", [], "tenant_warehouse"),
                 ("tenant_warehouse", ["--ch-db", "replay_override"], "replay_override"))
        for env_database, extra_args, expected in cases:
            with self.subTest(database=expected), clickhouse_stub() as (url, requests):
                env = {**os.environ, "PYTHONDONTWRITEBYTECODE": "1", "PYTHONIOENCODING": "utf-8"}
                env.pop("CLICKHOUSE_DB", None)
                if env_database is not None:
                    env["CLICKHOUSE_DB"] = env_database
                result = subprocess.run(
                    [sys.executable, str(ROOT / "tools/routing_replay.py"), "--source", "ch",
                     "--ch-url", url, "--ch-user", "stub-user", "--ch-password", "stub-key",
                     "--hours", "3", "--format", "json", *extra_args],
                    env=env, capture_output=True, text=True, encoding="utf-8", timeout=15,
                )
                self.assertEqual(result.returncode, 0, result.stderr)
                self.assertEqual(len(json.loads(result.stdout)), 4)
                self.assert_schema_query(requests.get(timeout=2)[3], expected, 3)
                self.assertNotIn("stub-key", result.stdout + result.stderr)

    def test_invalid_database_is_rejected_before_any_http_request(self) -> None:
        invalid_names = ("", "x; DROP DATABASE proxy", "x.y", "x`", "x\n", "1warehouse")
        with clickhouse_stub() as (url, requests):
            for database in invalid_names:
                with self.subTest(database=database):
                    with self.assertRaises(SystemExit) as error:
                        replay.load_clickhouse(url, "stub-user", "stub-key", 24, database=database)
                    self.assertIn("数据库名", str(error.exception))
                    self.assertNotIn("stub-key", str(error.exception))
            self.assertTrue(requests.empty())

    def test_empty_json_each_row_reports_table_and_window_without_credentials(self) -> None:
        for body in ("", "\n  \n"):
            with self.subTest(body=body), patch.dict(os.environ, {}, clear=False):
                os.environ.pop("CLICKHOUSE_DB", None)
                with clickhouse_stub(body) as (url, requests):
                    with self.assertRaises(SystemExit) as error:
                        replay.load_clickhouse(url, "stub-user", "stub-key", 6)
                    self.assertEqual(requests.get(timeout=2)[0], "POST")
            message = str(error.exception)
            schema = Schema()
            for detail in ("0 行", f"{schema.database}.{schema.table}", "6", "时间窗"):
                self.assertIn(detail, message)
            for secret in ("stub-user", "stub-key", url):
                self.assertNotIn(secret, message)


class SyntheticReplayRegressionTests(unittest.TestCase):
    def test_default_cli_metrics_match_pre_fix_baseline(self) -> None:
        # 修复前采集的真实合成回放数字，防止 SQL 修复意外影响选路计算。
        output = io.StringIO()
        with contextlib.redirect_stdout(output):
            self.assertEqual(replay.main(["--format", "json"]), 0)
        results = json.loads(output.getvalue())
        expected = (
            ("round_robin", 178, 0.296667, 0.409557, 385, 222.333, 446.7),
            ("weighted", 185, 0.308333, 0.4191, 394, 171.733, 446.7),
            ("linucb", 313, 0.521667, 0.575317, 180, 114.752, 248.916),
            ("oracle", 462, 0.77, 0.754379, 0, 149.274, 248.916),
        )
        for result, baseline in zip(results, expected):
            actual = tuple(result[key] for key in (
                "policy", "success", "success_rate", "mean_reward", "avoidable", "p50_ms", "p99_ms"
            ))
            self.assertEqual(actual, baseline)
            self.assertEqual(result["requests"], 600)
        self.assertEqual(len(results), len(expected))

    def test_self_test_retains_existing_invariants(self) -> None:
        self.assertEqual(replay.self_test(verbose=False), 0)


if __name__ == "__main__":
    unittest.main()
