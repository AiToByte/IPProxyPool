# 亲手操作指南 / Hands-On Guide

> 双语：中文在前，English after.
> 本指南所有命令均可复制执行。口径：D3 门默认开（带 `-H "X-Api-Key: default_key"`，生产换真 Key）；网关 `:8916`／指标 `:9091`／适配器 `:18080`／mocks `:8888-8890`。`#` 后为期望结果。

## 中文

### 0. 先知道两件事

- **Key 门默认开**：凡经网关的请求必须带 `X-Api-Key`（开发 Key `default_key`）；mocks 与 metrics 无门。
- **引号陷阱（必读，血泪）**：绕代理 flag 在两种 shell 写法相反——**cmd 用 `--noproxy *`（不加引号！cmd 里单引号是普通字符，`--noproxy '*'` 会被当成带引号的三字符 pattern 而失效）**；**PowerShell 用 `--noproxy '*'`（不加引号 `*` 会被通配展开）**。下文命令块凡涉及本机检查，cmd 用户请把 `--noproxy '*'` 去掉单引号；另需小写 `no_proxy=localhost,127.0.0.1`（libcurl 优先认小写，`setx` 只对新窗口生效）。
- **网关会周期性退出**：Windows 下数分钟 panic/有序退出一次，属已知现象；`gw:000` 时重跑 §1 第三步即回（看护 schtasks 需管理员注册）。

### 1. 启动与巡检

```powershell
cd D:\_MyProject\SuperSoft\IPProxyPool
powershell -ExecutionPolicy Bypass -File tools/ipp.ps1 status
# 期待九行：4 容器 Up＋PONG/Ok＋mockA/B/C:200＋gw:200＋metrics:200
# 若 gw:000：powershell -ExecutionPolicy Bypass -File tools/ipp.ps1 start -Mocks
# 若适配器不通：D:\DevSoft\Conda\Miniconda3\python.exe log/launch_detached.py D:\DevSoft\Conda\Miniconda3\python.exe log/ipp-forward.out log/ipp-forward.err tools/ipp_forward.py 18080 127.0.0.1 8916
```

### 2. 基础五断言（网关语义）

```powershell
curl.exe --max-time 5 -s -o NUL -w "plain:%{http_code} " -H "X-Api-Key: default_key" http://127.0.0.1:8916/
# 期待 plain:200
curl.exe --max-time 5 -s -o NUL -w "nokey:%{http_code} " http://127.0.0.1:8916/
# 期待 nokey:403（D3 门）
curl.exe --max-time 5 -s -o NUL -w "badkey:%{http_code} " -H "X-Api-Key: bad" http://127.0.0.1:8916/
# 期待 badkey:403
curl.exe --max-time 5 -s -H "Host:" http://127.0.0.1:8916/ -o NUL -w "nohost:%{http_code} "
# 期待 nohost:400（Host 校验先于鉴权）
curl.exe --max-time 5 -s -o NUL -w "metrics:%{http_code}`n" http://127.0.0.1:9091/metrics
# 期待 metrics:200
```

### 3. 四种接入（逐个试）

**① 程序直调**（看包体证据——流量确实经网关）：
```powershell
curl.exe --max-time 5 -s -H "X-Api-Key: default_key" -H "X-Proxy-Session: demo-1" -H "X-Proxy-Country: US" http://127.0.0.1:8916/
# 期待 mock-a-us（粘滞＋国家约束命中美节点；同值再跑必中同一节点）
```

**② SDK**：
```powershell
D:\DevSoft\Conda\Miniconda3\python.exe tools/ipp_sdk.py --self-test
# 期待 self-test OK: plain=200 sticky=b'mock-a-us' badkey=403 nokey=403
```

**③ 适配器**（标准代理形态；注意直连网关同法是 400）：
```powershell
curl.exe --max-time 10 -s -x http://127.0.0.1:18080 -H "X-Api-Key: default_key" http://127.0.0.1:8888/
# 期待 mock-b-jp（适配器须透传你自带的 X-Api-Key，它自己不代填）
```

**④ 浏览器（可选）**：`tools/ipp.pac` 在浏览器代理自动配置里填 `file:///D:/_MyProject/SuperSoft/IPProxyPool/tools/ipp.pac`，HTTP 走 18080，HTTPS/内网直连；然后访问 `http://127.0.0.1:8888/` 应看到 mock 体（需在请求头管理插件里加 `X-Api-Key: default_key`，否则 403）。

### 4. 安全演示（D1，免费档拒收凭据）

```powershell
curl.exe --max-time 5 -s -o NUL -w "free+auth:%{http_code} " -H "X-Api-Key: default_key" -H "X-Proxy-Tier: free" -H "Authorization: Bearer x" http://127.0.0.1:8916/
# 期待 403（凭据不得经陌生免费出口）
curl.exe --max-time 5 -s -o NUL -w "free+cookie:%{http_code} " -H "X-Api-Key: default_key" -H "X-Proxy-Tier: free" -H "Cookie: s=1" http://127.0.0.1:8916/
# 期待 403
curl.exe --max-time 5 -s -o NUL -w "free-clean:%{http_code} " -H "X-Api-Key: default_key" -H "X-Proxy-Tier: free" http://127.0.0.1:8916/
# 期待 503（默认网关免费池空，语义正确非故障）
curl.exe --max-time 5 -s -o NUL -w "res+auth:%{http_code}`n" -H "X-Api-Key: default_key" -H "X-Proxy-Tier: res" -H "Authorization: Bearer x" http://127.0.0.1:8916/
# 期待 200（付费档零误伤）
```

### 5. 观测（三重证据对账）

```powershell
curl.exe --max-time 5 -s http://127.0.0.1:9091/metrics | Select-String "proxy_requests_total|free_pool_nodes_total"
# 期待 2xx/4xx 等计数随你上面的流量涨
docker exec ipproxy-redis redis-cli -a 123456 XLEN stream:proxy:telemetry
# 期待数字（流内保留）；消费组 lag 应为 0（泵紧跟）
docker exec ipproxy-clickhouse clickhouse-client --user proxy --password 123456 --query "SELECT count() FROM proxy.proxy_telemetry_log"
# 发几条流量等 1 分钟再查，行数应涨（落库证据）
# Grafana http://127.0.0.1:3000 （admin/admin，进 IPProxyPool 面板，7 面板有数）
```

### 6. 免费线（可选，耗时：需等 60s 节拍＋源站轮转，pool 常态 0 属正常）

```powershell
$env:FREE_ENABLED="1"
powershell -ExecutionPolicy Bypass -File tools/ipp.ps1 stop   # 先停默认网关（同端口）
# 新开一个 Powershell（继承上面的 env），同样命令 start -Mocks
# 等 2~3 分钟后查：curl ... :9091/metrics | Select-String "free_pool_nodes_total|source_elite|by_proto"
# 有 pool>0 即用 -H "X-Proxy-Tier: free" 打一次；玩完把 $env:FREE_ENABLED 删掉重拉默认网关
```

### 7. 停止

```powershell
powershell -ExecutionPolicy Bypass -File tools/ipp.ps1 stop   # 只杀网关＋mocks，不碰其他 Python
```

### 8. 故障速查（大概率会碰到）

- `gw:000`：网关 panic 周期到了，重跑 §1 第三步（看护 schtasks 注册需管理员，尚未注册）。
- `200/403 错位`：先查 `$env:http_proxy`（Clash 会按 Host 头劫持本地请求）——`curl -v` 首行会明示；清 env 或加 `no_proxy=localhost,127.0.0.1`。
- `clickhouse unhealthy`：wget 探针 artifact，以 `SELECT 1` 为准，无视。
- 适配器 404：它指向了旧 8080 或网关挂了——先确认网关 200，再看 `log/ipp-forward.err` 尾行指向的网关地址是否为 8916。

### 相关文档 / Related docs

- 系统架构见 [`SYSTEM-ARCHITECTURE.md`](SYSTEM-ARCHITECTURE.md)；功能见 [`FEATURES.md`](FEATURES.md)；数据流见 [`DATAFLOW.md`](DATAFLOW.md)；教程版见 [`USER-GUIDE.md`](USER-GUIDE.md)。

## English

### 0. Two things first

- **Key gate on by default**: every gateway request needs `-H "X-Api-Key: default_key"` (dev key; real key in prod). Mocks and metrics have no gate.
- **The gateway exits periodically**: panic/orderly exit every minutes on Windows (known); `gw:000` → rerun §1 step 3 (watchdog schtasks needs admin registration).

### 1. Start & inspect

```powershell
cd D:\_MyProject\SuperSoft\IPProxyPool
powershell -ExecutionPolicy Bypass -File tools/ipp.ps1 status
# expect nine lines: 4 containers Up + PONG/Ok + mockA/B/C:200 + gw:200 + metrics:200
# if gw:000: powershell -ExecutionPolicy Bypass -File tools/ipp.ps1 start -Mocks
# if adaptor down, relaunch it:
# D:\DevSoft\Conda\Miniconda3\python.exe log/launch_detached.py D:\DevSoft\Conda\Miniconda3\python.exe log/ipp-forward.out log/ipp-forward.err tools/ipp_forward.py 18080 127.0.0.1 8916
```

### 2. Five base asserts (gateway semantics)

```powershell
curl.exe --max-time 5 -s -o NUL -w "plain:%{http_code} " -H "X-Api-Key: default_key" http://127.0.0.1:8916/
# expect plain:200
curl.exe --max-time 5 -s -o NUL -w "nokey:%{http_code} " http://127.0.0.1:8916/
# expect nokey:403 (D3 gate)
curl.exe --max-time 5 -s -o NUL -w "badkey:%{http_code} " -H "X-Api-Key: bad" http://127.0.0.1:8916/
# expect badkey:403
curl.exe --max-time 5 -s -H "Host:" http://127.0.0.1:8916/ -o NUL -w "nohost:%{http_code} "
# expect nohost:400 (Host check precedes auth)
curl.exe --max-time 5 -s -o NUL -w "metrics:%{http_code}`n" http://127.0.0.1:9091/metrics
# expect metrics:200
```

### 3. Four access modes (try each)

**① Direct app calls** (body evidence — traffic really goes through the gateway):
```powershell
curl.exe --max-time 5 -s -H "X-Api-Key: default_key" -H "X-Proxy-Session: demo-1" -H "X-Proxy-Country: US" http://127.0.0.1:8916/
# expect mock-a-us (sticky + country pins US node; same values always pin same node)
```

**② SDK**:
```powershell
D:\DevSoft\Conda\Miniconda3\python.exe tools/ipp_sdk.py --self-test
# expect self-test OK: plain=200 sticky=b'mock-a-us' badkey=403 nokey=403
```

**③ Adaptor** (standard proxy shape; note same call direct to gateway is 400):
```powershell
curl.exe --max-time 10 -s -x http://127.0.0.1:18080 -H "X-Api-Key: default_key" http://127.0.0.1:8888/
# expect mock-b-jp (adaptor passes through YOUR X-Api-Key, never injects its own)
```

**④ Browser (optional)**: set PAC to `file:///D:/_MyProject/SuperSoft/IPProxyPool/tools/ipp.pac`, HTTP via 18080, HTTPS/intranet direct; visit `http://127.0.0.1:8888/` for the mock body (add `X-Api-Key: default_key` via a header plugin, else 403).

### 4. Security demo (D1, free tier refuses credentials)

```powershell
curl.exe --max-time 5 -s -o NUL -w "free+auth:%{http_code} " -H "X-Api-Key: default_key" -H "X-Proxy-Tier: free" -H "Authorization: Bearer x" http://127.0.0.1:8916/
# expect 403 (credentials must not exit via stranger free nodes)
curl.exe --max-time 5 -s -o NUL -w "free+cookie:%{http_code} " -H "X-Api-Key: default_key" -H "X-Proxy-Tier: free" -H "Cookie: s=1" http://127.0.0.1:8916/
# expect 403
curl.exe --max-time 5 -s -o NUL -w "free-clean:%{http_code} " -H "X-Api-Key: default_key" -H "X-Proxy-Tier: free" http://127.0.0.1:8916/
# expect 503 (default gateway free pool empty — correct semantics, not a fault)
curl.exe --max-time 5 -s -o NUL -w "res+auth:%{http_code}`n" -H "X-Api-Key: default_key" -H "X-Proxy-Tier: res" -H "Authorization: Bearer x" http://127.0.0.1:8916/
# expect 200 (paid tier unaffected)
```

### 5. Observe (triple-evidence reconciliation)

```powershell
curl.exe --max-time 5 -s http://127.0.0.1:9091/metrics | Select-String "proxy_requests_total|free_pool_nodes_total"
# expect 2xx/4xx counters grow with your traffic above
docker exec ipproxy-redis redis-cli -a 123456 XLEN stream:proxy:telemetry
# expect a number (stream retains); consumer lag should be 0 (pump keeps up)
docker exec ipproxy-clickhouse clickhouse-client --user proxy --password 123456 --query "SELECT count() FROM proxy.proxy_telemetry_log"
# send traffic, wait 1 min, re-query — rows must grow (warehouse evidence)
# Grafana http://127.0.0.1:3000 (admin/admin, IPProxyPool board, 7 panels with data)
```

### 6. Free line (optional, slow: 60s ticks＋source rotation; pool 0 is normal)

```powershell
$env:FREE_ENABLED="1"
powershell -ExecutionPolicy Bypass -File tools/ipp.ps1 stop   # stop default gateway first (same port)
# open a NEW Powershell (inherits env above), same start -Mocks command
# after 2–3 min check: curl ... :9091/metrics | Select-String "free_pool_nodes_total|source_elite|by_proto"
# with pool>0, fire once with -H "X-Proxy-Tier: free"; afterwards unset $env:FREE_ENABLED and relaunch default gateway
```

### 7. Stop

```powershell
powershell -ExecutionPolicy Bypass -File tools/ipp.ps1 stop   # kills gateway＋mocks only, leaves other Pythons alone
```

### 8. Troubleshooting (you will likely hit these)

- `gw:000`: gateway panic cycle — rerun §1 step 3 (watchdog schtasks needs admin, not registered yet).
- `200/403 mismatch`: check `$env:http_proxy` first (Clash hijacks local requests by Host) — `curl -v` first line tells; clear env or add `no_proxy=localhost,127.0.0.1`.
- `clickhouse unhealthy`: wget-probe artifact, trust `SELECT 1`, ignore.
- Adaptor 404: it points at old 8080 or gateway is down — confirm gateway 200 first, then check `log/ipp-forward.err` tail for the gateway address (must be 8916).
