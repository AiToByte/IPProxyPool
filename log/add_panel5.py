"""P3-5 dashboard 第 5 面板：保留原文件字节格式，手术式追加（不重排）。"""
import io
import json

P = "deploy/grafana/dashboard.json"
s = io.open(P, encoding="utf-8").read()

panel_lines = [
    "    },",
    "    {",
    '      "id": 5,',
    '      "title": "Free tier water/verify/geo",',
    '      "type": "stat",',
    '      "datasource": { "type": "prometheus", "uid": "prometheus" },',
    '      "targets": [',
    "        {",
    '          "expr": "sum(free_pool_nodes_by_proto)",',
    '          "refId": "A"',
    "        },",
    "        {",
    '          "expr": "sum by (result) (free_pool_verify_total)",',
    '          "refId": "B"',
    "        },",
    "        {",
    '          "expr": "geoip_mismatch_total",',
    '          "refId": "C"',
    "        }",
    "      ],",
    '      "gridPos": { "h": 6, "w": 12, "x": 0, "y": 12 }',
    "    }",
]
old_tail = '    }\n  ]\n}\n'
assert s.endswith(old_tail), "tail mismatch"
body = s[: -len(old_tail)].rstrip("\n") + "\n"
body += "\n".join(panel_lines) + "\n  ]\n}\n"
io.open(P, "w", encoding="utf-8", newline="").write(body)

d = json.load(io.open(P, encoding="utf-8"))
print("panels:", len(d["panels"]), "ids:", [x["id"] for x in d["panels"]])
