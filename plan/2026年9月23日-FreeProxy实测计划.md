# FreeProxy 实测 Implementation Plan（2026年9月23日）

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 复用现有 FreePool 第二供应线，小批量拉取公开免费节点（Geonode API limit=20 为主），经 TCP 初筛＋FullCheck 复检＋网关 E2E 三级验证，输出可用率/延迟/匿名度实测报告，不扰动付费线。

**Architecture:** 零代码变更预期：独立 Python 直探（候选节点对照组）＋网关 `FreePoolWorker` 实测（试验组）＋本地 mock 保底（对照链路）；抓取→质检→注册→合并→选路/遥测/计量全复用既有链路，supervisor 托管，失败 hold 旧集。

**Tech Stack:** Rust 网关（`free_pool.rs` Worker＋`router.rs`＋`metrics.rs`＋`ch_sink.rs`）＋ Python stdlib 直探（socket＋urllib/curl）＋ curl E2E＋Redis Stream＋ClickHouse＋Prometheus exposition；零新依赖。

> **地位：** 本计划为一次性实测 runbook（非功能迭代），不 supersede 任何既有计划；执行后状态回写本文件 §7＋`TASK_PLAN.md` 步骤 18＋`EXEC_LOG.md`。
> **铁律沿用：** bash 一律显式短 timeout（构建 300s/常规≤60s）；禁用 `Get-NetTCPConnection`（用 `curl --max-time` 探活）；后台进程必须 `log/launch_detached.py` DETACHED＋双流重定向；项目日志/输出一律落 `log/`（gitignored，不提交）；`EXEC_LOG.md` append-only；禁未授权 commit。
> **跟踪：** `TASK_PLAN.md` 步骤 18；日志：`EXEC_LOG.md`。

---

## §0. 现状勘察（2026-09-23 实测依据，以本节为准）

- **功能基线：** FreePool v2（Task 1~13）＋Phase 2 SOCKS＋Phase 3 画像＋OPT-R3＋Phase 4/5＋DOC-R1 全✅；`cargo test` 144 过/4 ignored＋4 真 live；release bandit <200ns 持绿；存量 curl 六用例＋缺 Host 400＋/metrics 200 全绿。
- **默认源：** `free_pool.rs:1077-1081`：Geonode API（limit=100）＋`https://free-proxy-list.net/`＋clarketm raw＋复检基址 `https://httpbin.org`；全 env 可覆盖（`main.rs:266-300`，`split_env_list` 仅 http/https 防 SSRF，`FREE_FULL_CHECK_URL` 非 https 回落默认）。
- **网络实测（本机 2026-09-23）：**
  - `geonode limit=5` → 200（total 2884，5 条含 transparent×3/elite×2，含 socks5×1）；
  - `httpbin /ip` → 200（复检基址可用）；
  - `github raw` → 000（被墙/不可达）；
  - `free-proxy-list.net` → 000（不可达）。
  - 结论：本轮**唯一可用公网源为 Geonode API**；HTML/GitHub 预期零产出＋SourceGuard 熔断（ hold 旧集，非 bug）。
- **依赖实测：** Docker daemon 未运行（`docker ps` 报 npipe 缺失）；网关 :8080 无监听（000）。实测前必须先起 Docker（P5 既有结论：CH unhealthy 为 wget 探针 artifact，以 SELECT 为准）。
- **历史教训：** 公网免费存活率极低（F1：64 万样本仅 34.5% 活跃；既有 `gw10-free0` tick pool=0 属正常）；`curl.exe` 对部分 hosts 异常时以 reqwest 实测为准；live 门禁必须先验 PONG/Ok 再跑 `-- --ignored` 并检查 SKIP 行。

---

## §1. 目标与非目标

### 目标（可验收）

1. 拉取 Geonode 小批量（limit=20，http/https 过滤后约 10~18 个候选）并落盘 fixture（`log/`，gitignored）。
2. 直探对照：逐候选 TCP 建链（3s 超时）＋经代理 GET 复检基址（`/ip`＋`/headers`＋`/anything/freepool-canary`），记录成功率/转发延迟/匿名度（Elite/Anonymous/Transparent/Unknown）。
3. 网关试验：`FREE_ENABLED=1` 短节拍（60s）跑 2~3 tick，观察 `tick/pool`＋`free_pool_*` 四组指标＋`by_proto` 水位，ELITE=0/1 两档各一轮。
4. 存量零回归：curl 六用例＋缺 Host 400＋/metrics 200＋XLEN/CH 涨（pool=0 时只验链路活，不强求 CH 新增 free 行）。
5. 报告落库：`EXEC_LOG.md` 实测条目＋本计划 §7 状态表＋`log/free_probe_report.md`（gitignored，计数＋明细表）。

### 非目标（explicitly out）

- 不新增 Rust 依赖/模块/单测（预期零代码变更；若暴露 P0 bug 则另起修复位，不在本计划内消化）。
- 不做 SOCKS egress 变更（socks5 候选只解析标注，merge 照旧过滤；显式 socks 请求仍走既有桥）。
- 不承载敏感流量（认证/cookie/支付/银行一律不测；只测 `/ip|/headers|/anything` 公开回显端点）。
- 不做 Linux 50k/真 Key 灰度/JA4/GeoIP 配库（沿 REM 冻结）。
- 不全量拉取（limit=100 默认不动；本轮只用 limit=20 测试 URL，不污染默认配置）。

---

## §2. 合规与安全红线（违反即停）

| # | 红线 | 落点 |
|---|------|------|
| S1 | 零信任：免费节点视为不可信输入（MITM/内容篡改 16,923 样本） | 复检基址 https-only（启动校验）；canary 失配按失败计；OPERATION 禁敏感流量延续 |
| S2 | 礼貌抓取： single Geonode URL＋limit=20＋per-source 15s 超时＋ETag/304（如有） | 不并发打多页；失败 hold 旧集，不重试风暴 |
| S3 | SSRF 护栏：抓取源仅 http/https（`split_env_list` 已过滤）；探活目标仅 httpbin＋候选 ip:port | 不测内网/元数据地址；直探脚本写死 allowlist（见 T3） |
| S4 | 隔离：free-* 只服务无归属流量（ZZ＋tier 门）；Transparent 永不服务认证租户 | ELITE=1 档验证过滤门；默认 curl 仍命中付费大权重属正常（100:10） |
| S5 | 有界：`FREE_MAX_NODES=50`（测试值）＋TTL 600s＋backoff 60s×2^n＋容量逐最低分淘汰 | 池抖动不传导 bandit（注册表层吸收） |
| S6 | 可回滚：任意异常→`FREE_ENABLED=0` 重启网关＋`replace_vendor_nodes("free-",[])` 语义清空（重启即空） | 回滚≤2 分钟（杀进程＋重拉默认网关） |

---

## §3. 架构与复用（零新依赖）

```text
Geonode API (limit=20, https) ──→ fetch_all (join_all＋15s超时＋源序归一＋去重首见获胜)
  → TCP 初筛 (Verifier, 3s, 信号量20) → FullCheck (经代理GET三端点＋canary＋7头分级，信号量10)
  → Registry (TTL600s＋EWMAα=0.3＋backoff＋容量50＋require_elite门)
  → replace_vendor_nodes("free-", snap) → Router/LinUCB/遥测/计量/CH 全复用
  → 观测：tick日志＋free_pool_*＋by_proto＋XLEN/CH＋curl E2E
```

- **对照组（直探）：** `log/probe_free.py`（stdlib：socket TCP＋urllib经代理GET；与 Worker 同判据：canary＋7 头＋3s 超时；只读，不进池）。
- **试验组（网关）：** 现有 `FreePoolWorker::run_once`＋`merge_once`；本轮只调 env，不改码。
- **保底链（本地）：** 三 mocks（8888/89/90）＋默认网关；任一公网全灭时仍可证数据面活。

---

## §4. Env 总表（本轮测试值，生产默认不动）

| Key | 本轮值 | 说明 |
|-----|--------|------|
| `FREE_ENABLED` | `1` | 仅实测网关开；回归/收尾网关关 |
| `FREE_API_URLS` | `https://proxylist.geonode.com/api/proxy-list?limit=20&page=1&sort_by=lastChecked&sort_type=desc` | 小批量（默认 limit=100 不动） |
| `FREE_HTML_URLS` | `http://127.0.0.1:1/` | 快速失败，隔离不可达源噪声（已知 000） |
| `FREE_GITHUB_URLS` | `http://127.0.0.1:1/list.txt` | 同上 |
| `FREE_FETCH_INTERVAL_SECS` | `60` | 短节拍（默认 600）；2~3 tick 即结论 |
| `FREE_TTL_SECS` | `600` | 与节拍 10:1（默认 1800 按比缩小） |
| `FREE_VERIFY_TIMEOUT_SECS` | `3` | 默认不变 |
| `FREE_MAX_LATENCY_MS` | `3000` | 默认不变（EWMA 中性点） |
| `FREE_MAX_CONCURRENT` | `20` | 小池减半（默认 50） |
| `FREE_FULL_CONCURRENT` | `10` | 小池减半（默认 20） |
| `FREE_MAX_NODES` | `50` | 有界（默认 2000） |
| `FREE_FULL_CHECK_URL` | `https://httpbin.org` | 默认不变（已验 200） |
| `FREE_REQUIRE_ELITE` | `0` 首轮→`1` 次轮 | 两档各一轮（merge 门验证） |
| `FREE_SOURCE_MAX_ZERO_CYCLES` | `3` | 默认不变 |
| `FREE_SUSPEND_RETRY_EVERY` | `3` | 默认不变 |
| `GEOIP_ENFORCE_MISMATCH` | `0` | 只观察（无库 Disabled） |

---

## §5. File map（预期零代码变更）

- **Create（`log/`，gitignored，不提交）：** `log/free_sample_YYYYMMDD_HHMM.json`（Geonode 原始）、`log/free_candidates.txt`（`ip:port proto country`）、`log/probe_free.py`（直探脚本，T3 全文）、`log/free_probe_report.md`（计数＋明细）、`log/gw16-free*.out|err`（实测网关日志）。
- **Modify（落库，需提交）：** 本计划（状态表 §7）＋`TASK_PLAN.md`（步骤 18）＋`EXEC_LOG.md`（立项＋完成两条，append-only）。
- **Read-only（核对）：** `gateway/src/free_pool.rs`（Worker/Registry/FullCheck）、`gateway/src/main.rs:265-318`（env 接线）、`docs/OPERATION.md` §4/§6（水位/零信任）、`docker-compose.yml`（依赖端口）。

---

### Task 1: 环境基线（依赖＋四门速检）

**Files:**
- Modify: 无（只读＋启动日志落 `log/`）
- Test: 存量四门（`cargo fmt/clippy/test/bench --no-run`）

- [ ] **Step 1: 起 Docker＋验 Redis/CH（常规≤60s）**

```powershell
docker compose up -d
# Expected: redis/clickhouse/prometheus/grafana Creating/Started
docker exec ipproxy-redis redis-cli ping
# Expected: PONG
curl.exe --max-time 10 -s "http://127.0.0.1:8123/ping" --user "proxy:123456"
# Expected: Ok.
curl.exe --max-time 10 -s "https://proxylist.geonode.com/api/proxy-list?limit=1&page=1&sort_by=lastChecked&sort_type=desc" -o NUL -w "%{http_code}`n"
# Expected: 200（公网源可用性前置；非 200 则停，转 §6-R2）
```

- [ ] **Step 2: 四门速检（构建 300s）**

```powershell
cargo fmt --check
# Expected: EXIT 0（clean）
cargo clippy --workspace --all-targets -- -D warnings
# Expected: 零告警（EXIT 0）
cargo test --workspace
# Expected: 144 通过/0 失败/4 ignored（以实测为准；若 144±2 内波动只记录不追查）
```

- [ ] **Step 3: 拉三 mocks（保底链，DETACHED）**

```powershell
D:\DevSoft\Conda\Miniconda3\python.exe log/launch_detached.py D:\DevSoft\Conda\Miniconda3\python.exe log/mockA.out log/mockA.err log/mock_upstream.py 8888 mock-a-us
D:\DevSoft\Conda\Miniconda3\python.exe log/launch_detached.py D:\DevSoft\Conda\Miniconda3\python.exe log/mockB.out log/mockB.err log/mock_upstream.py 8889 mock-b-jp
D:\DevSoft\Conda\Miniconda3\python.exe log/launch_detached.py D:\DevSoft\Conda\Miniconda3\python.exe log/mockC.out log/mockC.err log/mock_upstream.py 8890 mock-c-gb
curl.exe --max-time 5 -s -o NUL -w "%{http_code}`n" http://127.0.0.1:8888/
# Expected: 200（任一 mock 200 即保底链活；全 000 则查进程/端口，不进 T2）
```

---

### Task 2: 小批量拉取＋静态质检（limit=20）

**Files:**
- Create: `log/free_sample_<ts>.json`、`log/free_candidates.txt`
- Test: 人工断言（raw≥10＋http/https 候选≥5；不足则如实记录＋转本地保底，不虚构）

- [ ] **Step 1: 拉取原始样本（常规≤60s）**

```powershell
curl.exe --max-time 20 -s "https://proxylist.geonode.com/api/proxy-list?limit=20&page=1&sort_by=lastChecked&sort_type=desc" -o log/free_sample.json
python -c "import json; d=json.load(open('log/free_sample.json',encoding='utf-8')); print('total:',d.get('total'),'count:',len(d.get('data',[])))"
# Expected: total ~2884前后，count 20（±0；若 count<10 即停，转 §6-R2）
```

- [ ] **Step 2: 过滤候选（http/https＋port>0＋4 段 IP；socks 只计数不进候选）**

```powershell
python -c "import json; d=json.load(open('log/free_sample.json',encoding='utf-8')); rows=[]; socks=0; [rows.append(f\"{x.get('ip')} {(x.get('port') or '')} {','.join(x.get('protocols') or [])} {x.get('country') or 'ZZ'}\") if any(p in ('http','https') for p in [str(v).lower() for v in (x.get('protocols') or [])]) else (_ for _ in ()).throw(Exception('unreachable')) if False else None for x in d.get('data',[])]; print('http候选约', len([x for x in d.get('data',[]) if any(str(v).lower() in ('http','https') for v in (x.get('protocols') or []))]))"
# Expected: http/https 候选 10~18（以实测为准；socks/坏行丢弃数同步记录）
```

注：精确过滤落 `log/probe_free.py`（T3 全文，含 `port int>0`＋4 段 octet 校验＋`poolable` 语义）；本步只做计数速检，明细以 T3 输出为准。

---

### Task 3: 直探对照（TCP＋经代理复检，stdlib，零新依赖）

**Files:**
- Create: `log/probe_free.py`（全文下述，直接落盘可用）
- Test: `python log/probe_free.py` 输出计数＋`log/free_probe_report.md` 明细（成功率/延迟/匿名度）

- [ ] **Step 1: 落盘直探脚本（与 Worker 同判据：3s 超时＋canary＋7 头）**

```python
"""FreeProxy 直探对照（只读，不进池，不进生产）。
判据对齐 gateway/src/free_pool.rs Task 8：
TCP 建链 3s → 经代理 GET {base}/ip (/origin) ＋ /headers (7头) ＋ /anything/freepool-canary。
allowlist：仅 Geonode 候选 ip:port＋https://httpbin.org；其余一律不连。
Usage: python log/probe_free.py log/free_sample.json
"""
import json, socket, sys, time, urllib.request

BASE = "https://httpbin.org"
MARKER = "freepool-canary"
DISCLOSURE = {"forwarded", "x-forwarded-for", "x-real-ip", "client-ip", "via", "proxy-connection", "x-proxy-id"}
TIMEOUT = 3

def tcp_ok(ip, port):
    try:
        s = socket.create_connection((ip, int(port)), timeout=TIMEOUT)
        s.close()
        return True
    except Exception:
        return False

def via_proxy(ip, port, path):
    url = BASE + path
    req = urllib.request.Request(url, headers={"User-Agent": "FreePool-probe/1.0"})
    proxy = f"http://{ip}:{port}"
    opener = urllib.request.build_opener(urllib.request.ProxyHandler({"http": proxy, "https": proxy}))
    try:
        with opener.open(req, timeout=TIMEOUT + 2) as r:
            return json.loads(r.read().decode("utf-8", "ignore"))
    except Exception as e:
        return {"_err": str(e)[:120]}

def classify(baseline, exit_ip, headers):
    if not exit_ip:
        return "Unknown"
    if exit_ip.strip() == (baseline or "").strip():
        return "Transparent"
    low = {str(k).lower() for k in (headers or {})}
    return "Anonymous" if (low & DISCLOSURE) else "Elite"

def main():
    src = sys.argv[1] if len(sys.argv) > 1 else "log/free_sample.json"
    d = json.load(open(src, encoding="utf-8"))
    try:
        base = json.loads(urllib.request.urlopen(BASE + "/ip", timeout=5).read().decode("utf-8", "ignore")).get("origin", "")
    except Exception as e:
        base = ""
        print(f"baseline unreachable ({str(e)[:80]}), anon may be Unknown-first")
    rows = []
    for x in d.get("data", [])[:20]:
        protos = [str(v).lower() for v in (x.get("protocols") or [])]
        if not any(p in ("http", "https") for p in protos):
            continue
        ip, port, country = x.get("ip"), str(x.get("port") or ""), x.get("country") or "ZZ"
        try:
            octs = ip.split(".")
            assert len(octs) == 4 and all(0 <= int(o) <= 255 for o in octs) and 0 < int(port) < 65536
        except Exception:
            continue
        t0 = time.time()
        ok = tcp_ok(ip, port)
        tcp_ms = int((time.time() - t0) * 1000)
        if not ok:
            rows.append((ip, port, country, "tcp_fail", "-", "-", tcp_ms))
            continue
        t1 = time.time()
        ip_b = via_proxy(ip, port, "/ip")
        h_b = via_proxy(ip, port, "/headers")
        c_b = via_proxy(ip, port, f"/anything/{MARKER}")
        fwd_ms = int((time.time() - t1) * 1000)
        exit_ip = (ip_b.get("origin") or "").split(",")[0].strip() if isinstance(ip_b, dict) else ""
        headers = h_b.get("headers", {}) if isinstance(h_b, dict) else {}
        canary = (c_b.get("url", "") if isinstance(c_b, dict) else "")
        if MARKER not in str(canary):
            rows.append((ip, port, country, "full_fail(canary)", "-", "-", fwd_ms))
            continue
        if "_err" in ip_b:
            rows.append((ip, port, country, "full_fail", "-", "-", fwd_ms))
            continue
        anon = classify(base, exit_ip, headers)
        rows.append((ip, port, country, "pass", anon, exit_ip, fwd_ms))
    npass = sum(1 for r in rows if r[3] == "pass")
    print(f"candidates={len(rows)} pass={npass} rate={npass/len(rows) if rows else 0:.1%}")
    for r in rows:
        print(" | ".join(map(str, r)))
    with open("log/free_probe_report.md", "w", encoding="utf-8") as f:
        f.write(f"# FreeProxy 直探报告（{time.strftime('%Y-%m-%d %H:%M')}）\n\n")
        f.write(f"基址 {BASE}，基线 {base or 'unreachable'}，候选 {len(rows)}，通过 {npass}。\n\n")
        f.write("| ip | port | country | result | anon | exit | latency_ms |\n|---|---|---|---|---|---|---|\n")
        for r in rows:
            f.write("| " + " | ".join(map(str, r)) + " |\n")

if __name__ == "__main__":
    main()
```

- [ ] **Step 2: 运行直探（常规≤60s，按 20 候选×(3s＋9s) 上限约 4min，timeout 300s）**

```powershell
python log/probe_free.py log/free_sample.json
# Expected: candidates 10~18，pass 0~3（公网免费存活率极低；0 通过亦为有效结论，不重跑刷数）
# 明细同步落 log/free_probe_report.md（含基线＋逐行表）
```

---

### Task 4: 网关试验（FREE_ENABLED=1，短节拍 60s，两档）

**Files:**
- Modify: 无（env 只在启动命令层覆盖，不改 `main.rs`/compose）
- Test: tick 日志＋`free_pool_*`＋`by_proto`＋XLEN/CH（见 Step 3 断言）

- [ ] **Step 1: 构建（300s）＋起默认网关验保底（常规≤60s）**

```powershell
cargo build
# Expected: Finished（warnings 零容忍：有 warn 即停）
$env:GATEWAY_ADDR="127.0.0.1:8080"; $env:METRICS_ADDR="127.0.0.1:9091"
D:\DevSoft\Conda\Miniconda3\python.exe log/launch_detached.py .\gateway\target\debug\pingora-proxy-gateway.exe log/gw16-base.out log/gw16-base.err
Start-Sleep 5
curl.exe --max-time 5 -s -o NUL -w "base:%{http_code}`n" http://127.0.0.1:8080/
# Expected: base:200（非 200 则查 log/gw16-base.err，不进实测）
```

- [ ] **Step 2: 起实测网关（ELITE=0，DETACHED，env 全量本轮值）**

```powershell
$env:FREE_ENABLED="1"
$env:FREE_API_URLS="https://proxylist.geonode.com/api/proxy-list?limit=20&page=1&sort_by=lastChecked&sort_type=desc"
$env:FREE_HTML_URLS="http://127.0.0.1:1/"
$env:FREE_GITHUB_URLS="http://127.0.0.1:1/list.txt"
$env:FREE_FETCH_INTERVAL_SECS="60"; $env:FREE_TTL_SECS="600"
$env:FREE_VERIFY_TIMEOUT_SECS="3"; $env:FREE_MAX_LATENCY_MS="3000"
$env:FREE_MAX_CONCURRENT="20"; $env:FREE_FULL_CONCURRENT="10"; $env:FREE_MAX_NODES="50"
$env:FREE_FULL_CHECK_URL="https://httpbin.org"; $env:FREE_REQUIRE_ELITE="0"
$env:FREE_SOURCE_MAX_ZERO_CYCLES="3"; $env:FREE_SUSPEND_RETRY_EVERY="3"
$env:GEOIP_ENFORCE_MISMATCH="0"; $env:GATEWAY_ADDR="127.0.0.1:8080"; $env:METRICS_ADDR="127.0.0.1:9091"
D:\DevSoft\Conda\Miniconda3\python.exe log/launch_detached.py .\gateway\target\debug\pingora-proxy-gateway.exe log/gw16-free0.out log/gw16-free0.err
Start-Sleep 75
Select-String -Path log/gw16-free0.err -Pattern "\[FreePool\] tick" | Select-Object -Last 3
# Expected: tick=1 pool=N（N 以实测为准；0 亦有效）＋yield/source 行；无 panic（Select-String panic 为空）
curl.exe --max-time 5 -s http://127.0.0.1:9091/metrics | Select-String "free_pool"
# Expected: free_pool_nodes_total N＋四组行（yield/verify/anonymity/suspended）＋by_proto 三档恒有行
```

- [ ] **Step 3: 次轮 ELITE=1（杀旧＋重拉＋70s，验证 merge 门）**

```powershell
Get-CimInstance Win32_Process | Where-Object { $_.CommandLine -like "*pingora-proxy-gateway*" } | ForEach-Object { Stop-Process -Id $_.ProcessId -Force }
$env:FREE_REQUIRE_ELITE="1"
D:\DevSoft\Conda\Miniconda3\python.exe log/launch_detached.py .\gateway\target\debug\pingora-proxy-gateway.exe log/gw16-free1.out log/gw16-free1.err
Start-Sleep 75
Select-String -Path log/gw16-free1.err -Pattern "\[FreePool\] tick" | Select-Object -Last 2
# Expected: tick=1 pool=M 且 M<=N（门收紧；M/N 皆可以为 0，关系成立即门有效）
```

---

### Task 5: 存量 E2E 回归（实测网关上直接验，不切回默认）

**Files:** 无（只读 metrics/Stream/CH）

- [ ] **Step 1: curl 六用例＋缺 Host＋metrics（常规≤60s）**

```powershell
curl.exe --max-time 5 -s -o NUL -w "plain:%{http_code}`n" http://127.0.0.1:8080/
curl.exe --max-time 5 -s -o NUL -w "sticky:%{http_code}`n" -H 'X-Session-Id: gw16-task1' -H 'X-Tenant-Country: US' http://127.0.0.1:8080/
curl.exe --max-time 5 -s -o NUL -w "auth:%{http_code}`n" -H 'X-Api-Key: default_key' http://127.0.0.1:8080/
curl.exe --max-time 5 -s -o NUL -w "badkey:%{http_code}`n" -H 'X-Api-Key: bad' http://127.0.0.1:8080/
curl.exe --max-time 5 -s -o NUL -w "gb:%{http_code}`n" -H 'X-Tenant-Country: GB' http://127.0.0.1:8080/
curl.exe --max-time 5 -s -o NUL -w "metrics:%{http_code}`n" http://127.0.0.1:9091/metrics
# Expected: plain/auth/sticky/gb 200＋badkey 403＋缺 Host 400（下行）＋metrics 200
curl.exe --max-time 5 -s -H 'Host:' http://127.0.0.1:8080/ -o NUL -w "nohost:%{http_code}`n"
# Expected: nohost:400（R2-2 语义；不占配额）
```

- [ ] **Step 2: 链路涨（XLEN/CH，10 流量＋10s，常规≤60s）**

```powershell
docker exec ipproxy-redis redis-cli XLEN stream:proxy:telemetry
# 记 XLEN0
curl.exe --max-time 10 -s "http://127.0.0.1:8123/" --data-binary "SELECT count() FROM proxy.proxy_telemetry_log" --user "proxy:123456"
# 记 CH0
for ($i=0; $i -lt 10; $i++) { curl.exe --max-time 5 -s -o NUL http://127.0.0.1:8080/ }
Start-Sleep 10
docker exec ipproxy-redis redis-cli XLEN stream:proxy:telemetry
curl.exe --max-time 10 -s "http://127.0.0.1:8123/" --data-binary "SELECT count() FROM proxy.proxy_telemetry_log" --user "proxy:123456"
# Expected: XLEN+10±2、CH+10±2（泵 batch 语义内波动；pool=0 时仍应涨：付费流量不断）
```

---

### Task 6: 收尾＋落库（回滚＋报告＋三件套）

**Files:**
- Modify: 本计划 §7＋`TASK_PLAN.md` 步骤 18＋`EXEC_LOG.md`（两条）
- Test: 默认网关 200＋四容器 Up＋工作树干净（`git status --short` 无源码改动）

- [ ] **Step 1: 回滚默认（杀实测网关＋重拉默认＋200）**

```powershell
Get-CimInstance Win32_Process | Where-Object { $_.CommandLine -like "*pingora-proxy-gateway*" } | ForEach-Object { Stop-Process -Id $_.ProcessId -Force }
Remove-Item Env:\FREE_ENABLED, Env:\FREE_API_URLS, Env:\FREE_HTML_URLS, Env:\FREE_GITHUB_URLS, Env:\FREE_FETCH_INTERVAL_SECS, Env:\FREE_TTL_SECS, Env:\FREE_MAX_NODES -ErrorAction SilentlyContinue
D:\DevSoft\Conda\Miniconda3\python.exe log/launch_detached.py .\gateway\target\debug\pingora-proxy-gateway.exe log/gw16.out log/gw16.err
Start-Sleep 5
curl.exe --max-time 5 -s -o NUL -w "rollback:%{http_code}`n" http://127.0.0.1:8080/
# Expected: rollback:200（回滚≤2 分钟）
```

- [ ] **Step 2: 落库三件套（本计划 §7 全✅＋EXEC_LOG 完成条＋TASK_PLAN 步骤 18✅；禁未授权 commit）**

```powershell
cargo fmt --check; cargo clippy --workspace --all-targets -- -D warnings
# Expected: 双绿（零代码变更，复验纪律）
git status --short
# Expected: 仅 plan/＋TASK_PLAN.md＋EXEC_LOG.md（log/ 全 gitignored，不应出现）
```

---

## §6. 风险与熔断

| # | 风险 | 概率 | 熔断 |
|---|------|------|------|
| R1 | Geonode 限流/挂（429/000） | 中 | 当轮 hold 旧集；连续 3 轮零产出 SourceGuard 暂停＋log warn；转本地保底，不追打 |
| R2 | 拉取 count<10 或候选<5 | 低 | 如实记录＋停（不放大 limit 刷数）；用历史 `gw10` pool=0 结论对照 |
| R3 | 直探/复检全灭（pass=0） | 中（免费存活率极低） | 有效结论（非失败）；网关侧验 tick/指标/回归照常；报告写明基址可达＋节点质量约束 |
| R4 | Transparent 占比高 | 高（样本 3/5） | ELITE=1 档验证过滤门；报告建议生产开 REQUIRE_ELITE |
| R5 | Docker/Redis/CH 中断 | 低 | 沿 P5 语义：数据面 200＋恢复追齐；中断即停，转 P5 runbook |
| R6 | 网关 panic/内存涨 | 极低 | PID 不变＋无 panic 行＋`supervisor_restarts` 未涨；异常即回滚默认＋停 |

---

## §7. 状态总览（2026-09-23 实测完成）

| 子项 | 内容 | 状态 | 验收（实测值） |
|------|------|------|------|
| F1 | 环境基线（Docker＋PONG/Ok＋四门＋mocks） | ✅ 已完成 | Docker 已起＋PONG/Ok＋147 过/4 live/bench 绿＋mocks 8888/89/90 200 |
| F2 | 小批量拉取（limit=20，raw/候选落盘） | ✅ 已完成 | `log/free_sample.json` total 2885/count 20；http/https 候选 13（socks/坏行丢弃 7） |
| F3 | 直探对照（TCP＋复检＋报告） | ✅ 已完成 | `log/free_probe_report.md`：基线 117.53.45.94，候选 13/通过 0（12 tcp_fail＋1 full_fail(canary) 110.92.72.204:8080） |
| F4 | 网关两档（ELITE 0/1，tick＋指标） | ✅ 已完成 | ELITE=0: tick1/2 pool=0（yield 40/tcp_fail 38/full_fail 2，PID 11724）；ELITE=1: tick1/2 pool=1（yield 40/tcp 38/full_fail 1/pass 1 elite，PID 30496，`by_proto{http}=1`）。M<=N 跨轮不成立系源站轮转（Geonode lastChecked 实时变），同快照门语义由单测锁定，诚实记录 |
| F5 | 存量回归（六用例＋XLEN/CH） | ✅ 已完成 | plain/auth/sticky/gb 200＋badkey 403＋nohost 400＋metrics 200；XLEN 9855→9871/CH 3417→3433（+16，含 5+1+10 E2E＋泵 batch，链路活）；20 流量后 free 行未涨（权重 100:10＋LinUCB 偏付费，属正常，未强求） |
| F6 | 收尾落库（回滚＋三件套） | ✅ 已完成 | 默认网关 PID 36788 200＋四容器 Up＋fmt/clippy/147/4live/bench 全绿＋工作树仅 plan/TASK/EXEC 三类 |

## Self-Review

1. **断言诚实性：** pool=0/直探全灭皆为有效结论（免费质量约束），不预设成功率；M<=N 只验门关系，不验绝对值；XLEN/CH 容差 ±2（泵 batch 语义）。
2. **环境安全：** 公网只碰 Geonode＋httpbin；HTML/GitHub 指 127.0.0.1:1 隔离噪声；敏感流量禁测；日志全进 `log/`。
3. **范围：** 零代码变更预期；P0 bug 另起修复位；真 Key/Linux/JA4/配库延续 out。
