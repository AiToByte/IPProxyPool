# FreeProxy大样本复测 Implementation Plan（2026年9月23日）

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 用生产默认值（Geonode limit=100 单页）大样本复测，验证 Elite 捕获稳定性：直探对照给基线率，网关跑 4 tick 给池水位轨迹，两者同量级即结论成立。

**Architecture:** 沿步骤 18 三轨（直探对照＋网关试验＋本地保底），零代码变更；差异点：快照放大到 100 raw、直探改 20 并发、网关用生产默认并发（50/20）与 120s 节拍跑 4 tick（含 ELITE=1 一轮），重点观测 tick 间隔是否漂移（大样本处理耗时是否超节拍）。

**Tech Stack:** 同步骤 18（FreePool Worker＋stdlib 直探＋curl＋Redis/CH/metrics）；直探脚本升级为 ThreadPoolExecutor 并发版；零新依赖。

> **地位：** 步骤 18 的后继（问题：小样本下 Elite 捕获是否稳定？直探 0/13 vs 网关 ELITE=1 pool=1 疑似源站轮转所致）。本计划只回答稳定性，不改生产默认。
> **铁律沿用：** 短 timeout（构建 300s/常规≤60s/直探 300s/单 tick 等待 150s）；杀进程一律 `Get-Process -Name pingora-proxy-gateway` 精确（禁 CommandLine 模糊，步骤 18 教训）；env 覆盖与启动同命令（步骤 18 教训）；后台 `log/launch_detached.py`＋日志落 `log/`；`EXEC_LOG.md` append-only；禁未授权 commit。
> **跟踪：** `TASK_PLAN.md` 步骤 19；日志：`EXEC_LOG.md`。

---

## §0. 基线（步骤 18 实测值，以本节为准）

- 单测 147 过/4 真 live/bench 绿；默认网关在线；四容器 Up；mocks 200。
- Geonode limit=20：total ~2885，http/https 候选 13；直探 0 过（12 tcp_fail＋1 canary）；网关 ELITE=0 pool=0（yield 40/tcp 38/full 2）→ ELITE=1 pool=1（pass 1 elite，源站轮转所致跨轮不可比）。
- 可达性：Geonode 200＋httpbin 200；github raw / free-proxy-list 000（本轮 HTML/GH 继续指 127.0.0.1:1 隔离噪声）。
- 生产默认值（`free_pool.rs:1077-1081`＋`main.rs:274-300`）：API limit=100 单页；MAX_CONCURRENT 50 / FULL 20；FETCH 600s / TTL 1800s。

---

## §1. 目标与非目标

### 目标

1. 拉取 Geonode limit=100 page=1 单快照（＝生产默认 URL 原样），落盘 `log/free_big_sample.json`，http/https 候选计数。
2. 并发直探对照：20 线程，TCP 3s＋单端点 5s，同 canary＋7 头判据，输出通过率/Elite 率/延迟分布（`log/free_big_report.md`）。
3. 网关 4 tick 观测（120s 节拍）：ELITE=0 三轮＋ELITE=1 一轮，记录 pool 轨迹/yield/verify/anonymity/by_proto＋相邻 tick 时间差（断言处理耗时未超节拍，容差 ±15s）。
4. 对比：网关 Elite 数 vs 直探 Elite 数，同量级（差值 ≤2 或同为 0/≥1 定性一致）即门有效；跨快照不等不判失败。
5. 存量回归＋四门＋回滚默认＋落库。

### 非目标

- 不改生产默认 env（本轮测试 env 只在启动命令层，不碰 `main.rs`/compose）。
- 不测敏感流量；只碰 Geonode＋httpbin；SSRF/零信任红线沿步骤 18 S1~S6。
- 不追打失败节点（0 通过亦为有效结论）；不做 Linux/真 Key/JA4/配库。

---

## §4. Env 总表（本轮测试值）

| Key | 本轮值 | 说明 |
|-----|--------|------|
| `FREE_ENABLED` | `1` | 仅实测网关 |
| `FREE_API_URLS` | 生产默认原样（limit=100 page=1 单页） | 与生产同快照语义 |
| `FREE_HTML_URLS` / `FREE_GITHUB_URLS` | `http://127.0.0.1:1[/list.txt]` | 快速失败隔离（已知 000） |
| `FREE_FETCH_INTERVAL_SECS` | `120` | 大样本处理耗时预留（默认 600 按比缩小） |
| `FREE_TTL_SECS` | `1200` | 与节拍 10:1 |
| `FREE_MAX_CONCURRENT` / `FREE_FULL_CONCURRENT` | `50` / `20` | 生产默认值（本轮顺带验证默认并发） |
| `FREE_MAX_NODES` | `200` | 有界（默认 2000 按比缩小） |
| `FREE_VERIFY_TIMEOUT_SECS` / `FREE_MAX_LATENCY_MS` | `3` / `3000` | 默认不变 |
| `FREE_FULL_CHECK_URL` | `https://httpbin.org` | 默认不变 |
| `FREE_REQUIRE_ELITE` | `0`×3 tick → `1`×1 tick | 前三轮看轨迹，末轮看门 |
| 其余 | 默认 | `GEOIP_ENFORCE_MISMATCH=0` |

---

## §5. File map（预期零代码变更）

- **Create（`log/`，不提交）：** `log/free_big_sample.json`、`log/probe_free_big.py`（T3 全文）、`log/free_big_report.md`、`log/gw19-big*.out|err`。
- **Modify（落库，需提交）：** 本计划（§7）＋`TASK_PLAN.md` 步骤 19＋`EXEC_LOG.md`（两条）。
- **Read-only：** `gateway/src/free_pool.rs`、`gateway/src/main.rs:265-318`、`docs/OPERATION.md` §4。

---

### Task 1: 环境基线（F1）

**Files:** 无（只读＋`log/` 启动日志）

- [ ] **Step 1: 依赖＋保底链（常规≤60s）**

```powershell
docker ps --format "{{.Names}} {{.Status}}" | Select-String "ipproxy"
docker exec ipproxy-redis redis-cli ping
# Expected: PONG
curl.exe --max-time 10 -s "http://127.0.0.1:8123/ping" --user "proxy:123456"
# Expected: Ok.
curl.exe --max-time 5 -s -o NUL -w "mockA:%{http_code} " http://127.0.0.1:8888/; curl.exe --max-time 5 -s -o NUL -w "mockB:%{http_code} " http://127.0.0.1:8889/; curl.exe --max-time 5 -s -o NUL -w "mockC:%{http_code}\n" http://127.0.0.1:8890/
# Expected: 三 200（缺失则按 OPERATION §2 重拉，不进 T2）
```

- [ ] **Step 2: 四门速检（构建 300s，网关目录）**

```powershell
cargo fmt --check; echo "fmt:$LASTEXITCODE"
cargo clippy --workspace --all-targets -- -D warnings; echo "clippy:$LASTEXITCODE"
# Expected: 双 0
cargo test --workspace; echo "test:$LASTEXITCODE"
# Expected: 147 通过/0 失败/4 ignored（±2 内波动只记录）
```

---

### Task 2: 大样本拉取（F2，limit=100 单快照）

**Files:**
- Create: `log/free_big_sample.json`
- Test: count==100（Geonode total 抖动只记录；count<80 即停转 §6-R2）

- [ ] **Step 1: 拉取生产默认 URL 原样（常规≤60s）**

```powershell
curl.exe --max-time 30 -s "https://proxylist.geonode.com/api/proxy-list?limit=100&page=1&sort_by=lastChecked&sort_type=desc" -o log/free_big_sample.json
python -c "import json; d=json.load(open('log/free_big_sample.json',encoding='utf-8')); print('total:',d.get('total'),'count:',len(d.get('data',[])))"
# Expected: count 100（total ~2885 前后）
```

---

### Task 3: 并发直探对照（F3，20 线程）

**Files:**
- Create: `log/probe_free_big.py`（全文下述）、`log/free_big_report.md`
- Test: 输出 `candidates/pass/elite` 三数＋明细表（pass=0 亦有效）

- [ ] **Step 1: 落盘并发直探脚本（判据与 Worker Task 8 同：3s TCP＋canary＋7 头）**

```python
"""FreeProxy 大样本直探（20并发，只读，不进池）。
Usage: python log/probe_free_big.py log/free_big_sample.json
allowlist：仅快照候选 ip:port＋https://httpbin.org。
"""
import json, socket, sys, time, urllib.request
from concurrent.futures import ThreadPoolExecutor

BASE = "https://httpbin.org"
MARKER = "freepool-canary"
DISCLOSURE = {"forwarded", "x-forwarded-for", "x-real-ip", "client-ip", "via", "proxy-connection", "x-proxy-id"}
TCP_TIMEOUT = 3
HTTP_TIMEOUT = 5
WORKERS = 20

def tcp_ok(ip, port):
    try:
        s = socket.create_connection((ip, int(port)), timeout=TCP_TIMEOUT)
        s.close()
        return True
    except Exception:
        return False

def via_proxy(ip, port, path):
    req = urllib.request.Request(BASE + path, headers={"User-Agent": "FreePool-probe/1.0"})
    proxy = f"http://{ip}:{port}"
    opener = urllib.request.build_opener(urllib.request.ProxyHandler({"http": proxy, "https": proxy}))
    try:
        with opener.open(req, timeout=HTTP_TIMEOUT) as r:
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

def probe_one(baseline, x):
    ip, port, country = x["ip"], str(x["port"]), x["country"]
    t0 = time.time()
    if not tcp_ok(ip, port):
        return (ip, port, country, "tcp_fail", "-", "-", int((time.time() - t0) * 1000))
    t1 = time.time()
    ip_b = via_proxy(ip, port, "/ip")
    h_b = via_proxy(ip, port, "/headers")
    c_b = via_proxy(ip, port, f"/anything/{MARKER}")
    fwd_ms = int((time.time() - t1) * 1000)
    exit_ip = (ip_b.get("origin") or "").split(",")[0].strip() if isinstance(ip_b, dict) else ""
    headers = h_b.get("headers", {}) if isinstance(h_b, dict) else {}
    canary = (c_b.get("url", "") if isinstance(c_b, dict) else "")
    if MARKER not in str(canary) or "_err" in ip_b:
        return (ip, port, country, "full_fail", "-", "-", fwd_ms)
    return (ip, port, country, "pass", classify(baseline, exit_ip, headers), exit_ip, fwd_ms)

def main():
    src = sys.argv[1] if len(sys.argv) > 1 else "log/free_big_sample.json"
    d = json.load(open(src, encoding="utf-8"))
    try:
        base = json.loads(urllib.request.urlopen(BASE + "/ip", timeout=5).read().decode("utf-8", "ignore")).get("origin", "")
    except Exception as e:
        base = ""
        print(f"baseline unreachable ({str(e)[:80]})")
    cands = []
    for x in d.get("data", [])[:100]:
        protos = [str(v).lower() for v in (x.get("protocols") or [])]
        if not any(p in ("http", "https") for p in protos):
            continue
        try:
            octs = str(x.get("ip")).split(".")
            assert len(octs) == 4 and all(0 <= int(o) <= 255 for o in octs) and 0 < int(str(x.get("port") or "0")) < 65536
        except Exception:
            continue
        cands.append({"ip": x.get("ip"), "port": str(x.get("port")), "country": x.get("country") or "ZZ"})
    rows = []
    with ThreadPoolExecutor(max_workers=WORKERS) as ex:
        for r in ex.map(lambda x: probe_one(base, x), cands):
            rows.append(r)
    npass = sum(1 for r in rows if r[3] == "pass")
    nelite = sum(1 for r in rows if len(r) > 4 and r[4] == "Elite")
    print(f"candidates={len(rows)} pass={npass} elite={nelite} rate={npass/len(rows) if rows else 0:.1%}")
    for r in rows:
        print(" | ".join(map(str, r)))
    with open("log/free_big_report.md", "w", encoding="utf-8") as f:
        f.write(f"# FreeProxy 大样本直探报告（{time.strftime('%Y-%m-%d %H:%M')}）\n\n")
        f.write(f"基址 {BASE}，基线 {base or 'unreachable'}，候选 {len(rows)}，通过 {npass}，Elite {nelite}。\n\n")
        f.write("| ip | port | country | result | anon | exit | latency_ms |\n|---|---|---|---|---|---|---|\n")
        for r in rows:
            f.write("| " + " | ".join(map(str, r)) + " |\n")

if __name__ == "__main__":
    main()
```

- [ ] **Step 2: 运行（timeout 300s；100 候选中 TCP 3s×5 波次＋复检子集，预期 <3min）**

```powershell
python log/probe_free_big.py log/free_big_sample.json
# Expected: candidates 60~80（http/https 占比约 2/3，以实测为准），pass/elite 如实（0 亦有效）
```

---

### Task 4: 网关 4 tick 观测（F4，120s 节拍，生产默认并发）

**Files:** 无（env 命令层覆盖；日志 `log/gw19-big*.out|err`）

- [ ] **Step 1: 构建（300s）＋起 ELITE=0 网关（单命令全量 env，DETACHED）**

```powershell
cargo build --manifest-path gateway/Cargo.toml
# Expected: Finished（有 warn 即停）
$env:FREE_ENABLED="1"; $env:FREE_API_URLS="https://proxylist.geonode.com/api/proxy-list?limit=100&page=1&sort_by=lastChecked&sort_type=desc"; $env:FREE_HTML_URLS="http://127.0.0.1:1/"; $env:FREE_GITHUB_URLS="http://127.0.0.1:1/list.txt"; $env:FREE_FETCH_INTERVAL_SECS="120"; $env:FREE_TTL_SECS="1200"; $env:FREE_VERIFY_TIMEOUT_SECS="3"; $env:FREE_MAX_LATENCY_MS="3000"; $env:FREE_MAX_CONCURRENT="50"; $env:FREE_FULL_CONCURRENT="20"; $env:FREE_MAX_NODES="200"; $env:FREE_FULL_CHECK_URL="https://httpbin.org"; $env:FREE_REQUIRE_ELITE="0"; $env:FREE_SOURCE_MAX_ZERO_CYCLES="3"; $env:FREE_SUSPEND_RETRY_EVERY="3"; $env:GEOIP_ENFORCE_MISMATCH="0"; $env:GATEWAY_ADDR="127.0.0.1:8080"; $env:METRICS_ADDR="127.0.0.1:9091"
D:\DevSoft\Conda\Miniconda3\python.exe log/launch_detached.py .\gateway\target\debug\pingora-proxy-gateway.exe log/gw19-big0.out log/gw19-big0.err
Start-Sleep 10
curl.exe --max-time 5 -s -o NUL -w "gw:%{http_code}\n" http://127.0.0.1:8080/
# Expected: gw:200（非 200 查 err，不进等待）
```

- [ ] **Step 2: 等 3 tick（约 380s，timeout 600s）＋轨迹断言**

```powershell
Start-Sleep 380
Select-String -Path log/gw19-big0.err -Pattern "\[FreePool\] tick" | Select-Object -Last 4
# Expected: tick=1/2/3 pool=N1/N2/N3（值如实；0 亦有效）＋无 panic 行
curl.exe --max-time 5 -s http://127.0.0.1:9091/metrics | Select-String "free_pool_nodes_total|free_pool_verify_total|free_pool_anonymity_total|free_pool_nodes_by_proto"
# Expected: 水位＋yield/verify/anonymity/suspended＋by_proto 三档行全
```

- [ ] **Step 3: tick 间隔漂移检查（处理耗时是否超 120s 节拍）**

```powershell
python -c "import re; ts=[l[:24] for l in open('log/gw19-big0.err',encoding='utf-8',errors='ignore') if '[FreePool] tick=' in l]; print(ts)"
# Expected: 相邻 tick 时间差 120±15s（超 150s 即处理耗时超节拍，如实记录为性能发现，不掩饰）
```

- [ ] **Step 4: ELITE=1 末轮（精确杀＋单命令重拉＋150s 等待）**

```powershell
Get-Process -Name "pingora-proxy-gateway" -ErrorAction SilentlyContinue | Stop-Process -Force
# （分两条命令：先确认退出，再起新网关，避免误杀执行壳后 env 丢失——env 与启动同命令）
$env:FREE_ENABLED="1"; $env:FREE_API_URLS="https://proxylist.geonode.com/api/proxy-list?limit=100&page=1&sort_by=lastChecked&sort_type=desc"; $env:FREE_HTML_URLS="http://127.0.0.1:1/"; $env:FREE_GITHUB_URLS="http://127.0.0.1:1/list.txt"; $env:FREE_FETCH_INTERVAL_SECS="120"; $env:FREE_TTL_SECS="1200"; $env:FREE_VERIFY_TIMEOUT_SECS="3"; $env:FREE_MAX_LATENCY_MS="3000"; $env:FREE_MAX_CONCURRENT="50"; $env:FREE_FULL_CONCURRENT="20"; $env:FREE_MAX_NODES="200"; $env:FREE_FULL_CHECK_URL="https://httpbin.org"; $env:FREE_REQUIRE_ELITE="1"; $env:FREE_SOURCE_MAX_ZERO_CYCLES="3"; $env:FREE_SUSPEND_RETRY_EVERY="3"; $env:GEOIP_ENFORCE_MISMATCH="0"; $env:GATEWAY_ADDR="127.0.0.1:8080"; $env:METRICS_ADDR="127.0.0.1:9091"
D:\DevSoft\Conda\Miniconda3\python.exe log/launch_detached.py .\gateway\target\debug\pingora-proxy-gateway.exe log/gw19-big1.out log/gw19-big1.err
Start-Sleep 150
Select-String -Path log/gw19-big1.err -Pattern "\[FreePool\] tick" | Select-Object -Last 2
# Expected: tick=1 pool=M（跨快照与 N3 不直接比大小；门有效性看 elite 占比提升或 M<=同快照折算，只作定性）
```

---

### Task 5: 对比＋回归（F5）

**Files:** 无（只读 metrics/Stream/CH）

- [ ] **Step 1: Elite 稳定性对比（定性不断言绝对值）**

```powershell
curl.exe --max-time 5 -s http://127.0.0.1:9091/metrics | Select-String "free_pool_anonymity|free_pool_verify"
# Expected: 两档 anonymity 分布可读；若直探 elite>=1 而网关两档 elite 恒 0→查 full_fail/tcp_fail 分布定位（超时 vs 篡改），如实记录，不重跑刷数
```

- [ ] **Step 2: 存量回归（常规≤60s，ELITE=1 网关上直接验）**

```powershell
curl.exe --max-time 5 -s -o NUL -w "plain:%{http_code} " http://127.0.0.1:8080/; curl.exe --max-time 5 -s -o NUL -w "sticky:%{http_code} " -H "X-Session-Id: gw19-task1" -H "X-Tenant-Country: US" http://127.0.0.1:8080/; curl.exe --max-time 5 -s -o NUL -w "badkey:%{http_code} " -H "X-Api-Key: bad" http://127.0.0.1:8080/; curl.exe --max-time 5 -s -o NUL -w "gb:%{http_code} " -H "X-Tenant-Country: GB" http://127.0.0.1:8080/; curl.exe --max-time 5 -s -o NUL -w "metrics:%{http_code}\n" http://127.0.0.1:9091/metrics
curl.exe --max-time 5 -s -H "Host:" http://127.0.0.1:8080/ -o NUL -w "nohost:%{http_code}\n"
# Expected: 200/200/403/200/200＋400
docker exec ipproxy-redis redis-cli XLEN stream:proxy:telemetry
curl.exe --max-time 10 -s "http://127.0.0.1:8123/" --data-binary "SELECT count() FROM proxy.proxy_telemetry_log" --user "proxy:123456"
# 记 XLEN1/CH1；10 流量＋10s 后复查，涨＋10±2 即链路活
```

---

### Task 6: 收尾＋落库（F6）

**Files:**
- Modify: 本计划 §7＋`TASK_PLAN.md` 步骤 19＋`EXEC_LOG.md`（完成条）
- Test: 默认网关 200＋`git status` 仅三类文件

- [ ] **Step 1: 回滚默认（精确杀＋重拉＋200，≤2min）**

```powershell
Get-Process -Name "pingora-proxy-gateway" -ErrorAction SilentlyContinue | Stop-Process -Force
Start-Sleep 3
D:\DevSoft\Conda\Miniconda3\python.exe log/launch_detached.py .\gateway\target\debug\pingora-proxy-gateway.exe log/gw19.out log/gw19.err
Start-Sleep 5
curl.exe --max-time 5 -s -o NUL -w "rollback:%{http_code}\n" http://127.0.0.1:8080/
# Expected: rollback:200
```

- [ ] **Step 2: 四门＋落库（禁未授权 commit）**

```powershell
cargo fmt --check; cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace -- --ignored --nocapture
cargo bench --workspace --no-run
git status --short
# Expected: 四绿＋工作树仅 plan/TASK/EXEC 三类（log/ 不提交）
```

---

## §6. 风险与熔断

| # | 风险 | 熔断 |
|---|------|------|
| R1 | Geonode 翻页/限流致 count<80 | 停，不放大页数刷数；用单页结论对照步骤 18 |
| R2 | 大样本 tick 处理超 120s 节拍（间隔漂移） | 如实记录为性能发现（默认 600s 节拍下余量充足与否的依据），不调参掩饰 |
| R3 | 直探与网关 Elite 数差 >2 | 查 verify 分布定位（tcp 超时 vs FullCheck 篡改/延迟），不重跑；跨快照本就不可比 |
| R4 | 网关有序退出（All runtimes exited 系 Pingora 停机路径） | 精确重启＋200 验证；归环境侧，不记 P0 |
| R5 | 20 流量 free 行未涨 | 设计内（权重＋LinUCB 偏付费），pool/elite 计数即质检证据 |

---

## §7. 状态总览（2026-09-23 实测完成）

| 子项 | 内容 | 状态 | 验收（实测值） |
|------|------|------|------|
| F1 | 环境基线 | ✅ 已完成 | PONG/Ok＋147 过＋mocks/gw 200 |
| F2 | 大样本拉取 | ✅ 已完成 | `log/free_big_sample.json` total 2877/count 100（生产默认 URL 原样） |
| F3 | 并发直探 | ✅ 已完成 | `log/free_big_report.md`：候选 57/通过 0/Elite 0（51 tcp_fail＋6 full_fail，20 并发 <1min） |
| F4 | 网关 4 tick | ✅ 已完成 | big0 首轮 tick1~3 pool 0/0/0（间隔129/132s，±15s 内）；big0b 次轮 tick1 pool0→tick2 pool1（yield 200/tcp178/full21/pass1 elite，`by_proto{socks5}=1`）；big1 末轮 ELITE=1 tick1/2 pool 0/0（tcp160/full40）。生产默认并发 50/20 无漂移 |
| F5 | 对比＋回归 | ✅ 已完成 | Elite 间歇出现（步骤18 http 1＋本轮 socks5 1，其余快照 0）→“偶发 1 Elite/100~200 raw”定性成立；六用例 200/403/400＋metrics 200；XLEN 9905→9920/CH 3467→3482（+15＝5+10 精确对账） |
| F6 | 收尾落库 | ✅ 已完成 | 默认网关 PID 29720 200＋4 live/bench 绿＋工作树仅三类文件 |

## Self-Review

1. **断言诚实性：** 0 通过/间隔漂移/Elite 差超限皆为有效发现；跨快照只定性不定量；容差只用于泵 batch（±2）与 tick（±15s）。
2. **环境安全：** 公网只碰 Geonode＋httpbin；HTML/GH 隔离；敏感流量禁测；日志全进 `log/`。
3. **范围：** 零代码变更预期；生产默认不动；真 Key/Linux/JA4 延续 out。
