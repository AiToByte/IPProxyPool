# 用户使用文档 / User Guide

> 双语：中文在前，English after.
> 本指南所有命令均可复制执行（D3 后默认带 Key：`-H "X-Api-Key: default_key"`，生产换真 Key）。`#` 后为期望输出。

## 中文

### 1. 安装（10 分钟）

前置：Windows 10+/Server（开发）或任意 Linux（生产）、Docker Desktop、Miniconda Python（仅跑脚本）。

```powershell
git clone <repo> ; cd IPProxyPool
cp .env.example .env            # 按需改密码（生产必改；.env 永不进仓）
docker compose up -d            # Redis/CH/Prom/Grafana 四件套
docker exec ipproxy-redis redis-cli -a 123456 ping        # 期待 PONG
curl.exe -s "http://127.0.0.1:8123/ping" --user "proxy:123456"  # 期待 Ok.
cargo build --manifest-path gateway/Cargo.toml            # 约 3 分钟（release 约 4 分钟）
```

### 2. 五步 quickstart

```powershell
powershell -ExecutionPolicy Bypass -File tools/ipp.ps1 status   # 只读巡检（九行：容器＋mocks＋网关＋指标）
powershell -ExecutionPolicy Bypass -File tools/ipp.ps1 start -Mocks  # 全量拉起（幂等：已在跑即跳过）
curl.exe --max-time 5 -s -o NUL -w "gw:%{http_code}`n" -H "X-Api-Key: default_key" http://127.0.0.1:8916/   # 期待 200
curl.exe -s http://127.0.0.1:9091/metrics | Select-String "proxy_requests_total"   # 期待 4 行（2xx/4xx/5xx/other）
powershell -ExecutionPolicy Bypass -File tools/ipp.ps1 stop     # 精确停止（只杀网关＋mocks，不碰其他 Python）
```

### 3. 四种接入形态

**方式一：程序直调（推荐，零中转）**——请求打网关地址，上游放 Host 头：
```powershell
curl.exe http://127.0.0.1:8916/ -H "Host: httpbin.org" -H "X-Api-Key: default_key"   # 经默认池，期待 mock 体
curl.exe http://127.0.0.1:8916/ip -H "Host: httpbin.org" -H "X-Api-Key: default_key" -H "X-Proxy-Tier: free" -H "X-Proxy-Proto: socks5"  # 免费 socks5（池空则 503，正常）
# 粘滞：同 session 两次命中同节点
curl.exe http://127.0.0.1:8916/ -H "Host: httpbin.org" -H "X-Api-Key: default_key" -H "X-Proxy-Session: job-42" -H "X-Proxy-Country: US"
```
状态码速查：200 直通／400 缺 Host／403 无头或坏 Key 或 free＋敏感头／402 欠费／429 超限／503 无可用节点（免费池空亦 503）。

**方式二：SDK（stdlib/Python 零依赖；Node/Go/.NET/Java 见 tools/）**：
```python
from tools.ipp_sdk import IPPClient
c = IPPClient("http://127.0.0.1:8916", session="job-42")  # 缺省带 default_key；生产传 api_key="真Key"
status, body = c.get("http://httpbin.org/ip")   # 503 自动延迟重试 1 次
python tools/ipp_sdk.py --self-test   # 期待 self-test OK: plain=200 sticky=b'mock-a-us' badkey=403 nokey=403
```

**方式三：前置适配器（浏览器/系统代理/标准 HTTP_PROXY）**：
```powershell
D:\DevSoft\Conda\Miniconda3\python.exe log/launch_detached.py D:\DevSoft\Conda\Miniconda3\python.exe log/ipp-forward.out log/ipp-forward.err tools/ipp_forward.py 18080 127.0.0.1 8916
curl.exe -x http://127.0.0.1:18080 -H "X-Api-Key: default_key" http://127.0.0.1:8888/   # 期待 200 mock 体（直连网关同法是 400）
```
注意：适配器只做形态翻译不代填身份，调用方须自带 `X-Api-Key` 头；CONNECT（HTTPS 隧道）诚实 501，请用程序级集成；浏览器 PAC 见 `tools/ipp.pac`（HTTP 走 18080，HTTPS/内网直连）。

**方式四：一键启停与观测**：见 §2 `ipp.ps1` 三命令；观测 Grafana `:3000`（7 面板）＋`:9091/metrics`（30+ 序列）＋`SELECT count() FROM proxy.proxy_telemetry_log` 随流量涨。

### 4. 免费线开法

```powershell
$env:FREE_ENABLED="1"   # Powershell 当次有效；常驻写 .env 或系统环境（变量名同上）
# 重启网关后看水位（常态 0 属源站生态；Elite 偶发）：
curl.exe -s http://127.0.0.1:9091/metrics | Select-String "free_pool_nodes_total|source_elite|by_proto"
# 生产建议再加：$env:FREE_REQUIRE_ELITE="1"（只要 Elite，透明节点不进池）
```

### 5. 日常巡检与故障速查

- 巡检：`ipp.ps1 status` 九行全 200/PONG/Up；Grafana 成功率面板；`supervisor_restarts_total` 无持续增长。
- 网关 503 全域：池被隔离摘空或 mocks 挂了（查 CB 日志＋`quarantine:{domain}:{ip}`）。
- 403 全拦截：无头或 Key 未注册（带 `default_key` 即放行；生产换真 Key 后重测）。
- CH 查不到数：看 `[ChSink]` 日志（insert 失败会 hold 住 ack 等恢复）。
- Windows 网关数分钟退出一次：已知（panic/有序退出），`log/ipp-watchdog.out` 看重拉记录；生产跑 Linux。
- 自检 200/403 错位：先查 shell 代理 env（Clash 按 Host 头劫持本地请求）：`no_proxy` 加 `localhost,127.0.0.1` 或清 env 后重跑。
- 回退（应急）：`$env:REQUIRE_API_KEY="0"`＋`$env:GATEWAY_ADDR="0.0.0.0:8916"` 后重启网关。

### 6. FAQ

- **Q: 浏览器能直接用网关吗？** A: 不能（absolute-URI 400），经适配器 `:18080` 用 HTTP，或配 PAC。
- **Q: 免费池长期为 0 正常吗？** A: 正常（公网存活率极低），付费线不受扰。
- **Q: P99 多少正常？** A: dev 基线约 65ms（Windows debug 参考）；Linux 另验。
- **Q: 备份怎么做？** A: `tools/backup.ps1`（RDB＋FREEZE＋双卷 tar 落 `backup/<stamp>/` 四件套）；恢复见 OPERATION。

## English

### 1. Install (10 minutes)

Prereqs: Windows 10+/Server (dev) or any Linux (prod), Docker Desktop, Miniconda Python (scripts only).

```powershell
git clone <repo> ; cd IPProxyPool
cp .env.example .env            # change passwords (must for prod; .env never committed)
docker compose up -d            # Redis/CH/Prom/Grafana
docker exec ipproxy-redis redis-cli -a 123456 ping        # expect PONG
curl.exe -s "http://127.0.0.1:8123/ping" --user "proxy:123456"  # expect Ok.
cargo build --manifest-path gateway/Cargo.toml            # ~3 min (release ~4 min)
```

### 2. Five-step quickstart

```powershell
powershell -ExecutionPolicy Bypass -File tools/ipp.ps1 status   # read-only (nine lines: containers＋mocks＋gateway＋metrics)
powershell -ExecutionPolicy Bypass -File tools/ipp.ps1 start -Mocks  # full bring-up (idempotent)
curl.exe --max-time 5 -s -o NUL -w "gw:%{http_code}`n" -H "X-Api-Key: default_key" http://127.0.0.1:8916/   # expect 200
curl.exe -s http://127.0.0.1:9091/metrics | Select-String "proxy_requests_total"   # expect 4 rows
powershell -ExecutionPolicy Bypass -File tools/ipp.ps1 stop     # precise stop (gateway＋mocks only)
```

### 3. Four access modes

**Mode 1: direct app calls (recommended, zero hops)** — hit the gateway address, put upstream in Host:
```powershell
curl.exe http://127.0.0.1:8916/ -H "Host: httpbin.org" -H "X-Api-Key: default_key"   # default pool, expect mock body
curl.exe http://127.0.0.1:8916/ip -H "Host: httpbin.org" -H "X-Api-Key: default_key" -H "X-Proxy-Tier: free" -H "X-Proxy-Proto: socks5"  # free socks5 (503 when pool empty is normal)
# sticky: same session twice hits same node
curl.exe http://127.0.0.1:8916/ -H "Host: httpbin.org" -H "X-Api-Key: default_key" -H "X-Proxy-Session: job-42" -H "X-Proxy-Country: US"
```
Status cheat-sheet: 200 pass／400 missing Host／403 headerless-or-bad-key or free＋credentials／402 broke／429 limited／503 no nodes (empty free pool also 503).

**Mode 2: SDKs (stdlib Python, zero deps; Node/Go/.NET/Java in tools/)**:
```python
from tools.ipp_sdk import IPPClient
c = IPPClient("http://127.0.0.1:8916", session="job-42")  # carries default_key; pass api_key="real" in prod
status, body = c.get("http://httpbin.org/ip")   # one delayed retry on 503
python tools/ipp_sdk.py --self-test   # expect self-test OK: plain=200 sticky=b'mock-a-us' badkey=403 nokey=403
```

**Mode 3: forward adaptor (browsers/system proxy/standard HTTP_PROXY)**:
```powershell
D:\DevSoft\Conda\Miniconda3\python.exe log/launch_detached.py D:\DevSoft\Conda\Miniconda3\python.exe log/ipp-forward.out log/ipp-forward.err tools/ipp_forward.py 18080 127.0.0.1 8916
curl.exe -x http://127.0.0.1:18080 -H "X-Api-Key: default_key" http://127.0.0.1:8888/   # expect 200 mock body (direct-to-gateway same call is 400)
```
Note: the adaptor translates shape only, never injects identity — send `X-Api-Key` yourself; CONNECT (HTTPS tunneling) gets honest 501, use app-level integration; browser PAC at `tools/ipp.pac` (HTTP via 18080, HTTPS/intranet direct).

**Mode 4: one-click ops**: the three `ipp.ps1` commands (§2); observe Grafana `:3000` (7 panels)＋`:9091/metrics` (30+ series)＋`SELECT count() FROM proxy.proxy_telemetry_log` growing.

### 4. Enabling the free line

```powershell
$env:FREE_ENABLED="1"   # current shell; persist via .env or system env for permanence
# watch the level after gateway restart (0 is normal ecology; Elite sporadic):
curl.exe -s http://127.0.0.1:9091/metrics | Select-String "free_pool_nodes_total|source_elite|by_proto"
# production advice: also $env:FREE_REQUIRE_ELITE="1" (Elite only)
```

### 5. Daily checks & troubleshooting

- Checks: `ipp.ps1 status` nine lines all 200/PONG/Up; Grafana success panel; `supervisor_restarts_total` not climbing.
- 503 everywhere: pool quarantined empty or mocks down (CB logs＋`quarantine:{domain}:{ip}`).
- All 403: headerless or unregistered (send `default_key` to pass; retest after real key in prod).
- CH shows nothing: `[ChSink]` logs (failed inserts hold ack till recovery).
- Windows gateway exits every minutes: known (panic/orderly exit), see `log/ipp-watchdog.out` relaunch log; prod on Linux.
- Self-test 200/403 mismatch: check shell proxy env first (Clash routes local requests by Host): add `localhost,127.0.0.1` to `no_proxy` or clear env and rerun.
- Rollback (emergency): `$env:REQUIRE_API_KEY="0"`＋`$env:GATEWAY_ADDR="0.0.0.0:8916"`, restart gateway.

### 6. FAQ

- **Q: Can browsers use the gateway directly?** A: No (absolute-URI 400); via adaptor `:18080` for HTTP, or PAC.
- **Q: Free pool at 0 long-term normal?** A: Normal (tiny public survival); paid line unaffected.
- **Q: Normal P99?** A: ~65ms dev baseline (Windows debug reference); Linux separately.
- **Q: Backups?** A: `tools/backup.ps1` (RDB＋FREEZE＋dual-volume tar into `backup/<stamp>/`); restore in OPERATION.

### Related docs / 相关文档

- 架构全景见 [`SYSTEM-ARCHITECTURE.md`](SYSTEM-ARCHITECTURE.md)；功能细节见 [`FEATURES.md`](FEATURES.md)；数据怎么流见 [`DATAFLOW.md`](DATAFLOW.md)；开源与依赖见 [`OPEN-SOURCE.md`](OPEN-SOURCE.md)。
