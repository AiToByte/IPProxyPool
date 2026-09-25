# Usage 使用指南

> 双语：中文在前，English after. Bilingual: Chinese first, English second.
> 核心铁律：网关是反向式 egress 路由——请求打到网关地址，真实上游放 `Host` 头。
> Core rule: the gateway is reverse-style egress routing — send requests to the gateway address, put the real upstream in the `Host` header.
> D3 起网关默认开 Key 门：下例均带 `-H "X-Api-Key: default_key"`（开发缺省 Key，生产换真 Key）；无头即 403。

## 中文

### 方式一：程序直调（推荐，零中转）

```powershell
curl.exe http://127.0.0.1:8916/ -H "Host: httpbin.org" -H "X-Api-Key: default_key"       # 经默认池
curl.exe http://127.0.0.1:8916/ip -H "Host: httpbin.org" -H "X-Api-Key: default_key" -H "X-Proxy-Tier: free" -H "X-Proxy-Proto: socks5"
```

选择头（与网关 `parse_routing_spec` 同名，误名会被静默忽略）：`X-Api-Key`（鉴权）／`X-Proxy-Session`（粘滞，同值命中同节点）／`X-Proxy-Country`（国家约束）／`X-Proxy-Tier`（free 等）／`X-Proxy-Proto`（socks5/socks4，仅显式请求走桥）。

### 方式二：Python SDK（`tools/ipp_sdk.py`，stdlib 零依赖）

```python
from ipp_sdk import IPPClient
c = IPPClient("http://127.0.0.1:8916", session="job-42", tier="free", proto="socks5")
status, body = c.get("http://httpbin.org/ip")   # 503 自动延迟重试 1 次
```

自检：`python tools/ipp_sdk.py --self-test`（普通 200＋粘滞 200＋坏 Key 403）。

### 方式三：前置适配器（`tools/ipp_forward.py`，接浏览器/系统代理/标准 `HTTP_PROXY`）

```powershell
D:\DevSoft\Conda\Miniconda3\python.exe log/launch_detached.py D:\DevSoft\Conda\Miniconda3\python.exe log/ipp-forward.out log/ipp-forward.err tools/ipp_forward.py 18080 127.0.0.1 8916
# 适配器只做形态翻译不代填身份：D3 起调用方须自带 X-Api-Key 头（适配器透传），否则经网关 403。
curl.exe -x http://127.0.0.1:18080 -H "X-Api-Key: default_key" http://127.0.0.1:8888/    # 200（直连网关同法是 400）
```

Windows 系统代理（按需手动执行，脚本不自动改系统）：设置 → 网络和 Internet → 代理 → 手动设置 → 地址 `127.0.0.1` 端口 `18080`（仅 HTTP；HTTPS 走 CONNECT 会被 501 拒绝，见限制表）。PowerShell 写法（管理员，按需）：

```powershell
Set-ItemProperty "HKCU:\Software\Microsoft\Windows\CurrentVersion\Internet Settings" ProxyEnable 1
Set-ItemProperty "HKCU:\Software\Microsoft\Windows\CurrentVersion\Internet Settings" ProxyServer "http=127.0.0.1:18080"
# 还原：ProxyEnable 0
```

### 方式四：一键启停与观测（`tools/ipp.ps1`）

```powershell
powershell -ExecutionPolicy Bypass -File tools/ipp.ps1 status        # 只读巡检
powershell -ExecutionPolicy Bypass -File tools/ipp.ps1 start -Mocks  # 全量拉起（幂等）
powershell -ExecutionPolicy Bypass -File tools/ipp.ps1 stop          # 精确停止
```

观测：Grafana `:3000` 5 面板；`:9091/metrics` 16 组指标；`SELECT count() FROM proxy.proxy_telemetry_log` 随流量涨。

### 多语言片段（展示，语义同 SDK）

Node (`fetch`)：`fetch("http://127.0.0.1:8916/ip", {headers:{Host:"httpbin.org","X-Api-Key":"default_key","X-Proxy-Tier":"free"}})`。
.NET：`HttpClient` 发 `GET http://127.0.0.1:8916/ip` 并加 `Host`/`X-*` 请求头（必带 `X-Api-Key`；注意 `HttpClient` 默认保护 Host 头，需用 `request.Headers.Host`）。
Go：`http.NewRequest("GET","http://127.0.0.1:8916/ip",nil)` 后 `req.Host="httpbin.org"`＋`req.Header.Set("X-Api-Key","default_key")`＋`req.Header.Set("X-Proxy-Tier","free")`。
Java：`HttpRequest.newBuilder(URI.create("http://127.0.0.1:8916/ip")).header("Host","httpbin.org").header("X-Api-Key","default_key")`。

### 限制表（实测结论）

| 事项 | 行为 |
|------|------|
| absolute-URI 直连网关 | 400（须经适配器） |
| CONNECT（HTTPS 隧道） | 适配器 501；请用程序级集成 |
| chunked 请求体／>10MB | 适配器 501／413 |
| 免费池水位 | 常态 0（公网存活率极低）；`tier=free` 池空时正确 503 |
| `tier=free`＋`Authorization`/`Cookie` | 403（D1：匿名共享出口拒收凭据，防泄露给陌生出口；网关自有 `X-API-Key` 不受影响） |
| 鉴权（D3 缺省开门） | 无 `X-Api-Key` 头即 403；开发带 `default_key`，生产换真 Key（SDK 缺省已带） |
| 局域网敞口 | 缺省 `127.0.0.1:8916` 仅回环（D3 已收紧）；需局域网/容器可达时显式 `GATEWAY_ADDR=0.0.0.0:8916`，多机走 WireGuard 后绑 WG 地址 |
| 公网目标经免费节点 | 目标必须公网可达（免费节点回连你内网必失败） |
| Windows 网关进程 | 约 5 分钟有序退出一次（已知），重起即恢复；生产跑 Linux |
| 本机 VPN（Clash 等） | 网关全链路免疫（直连，不跟随系统代理）；`curl.exe` 可作真直连基线；若某出口恰为 VPN 段，先用直连/显式代理/网关三路对照再下结论 |

### FAQ

- **Q: 浏览器能直接用网关吗？** A: 不能直连（absolute-URI 400），经适配器 `:18080` 可用 HTTP；HTTPS 受限见上表。
- **Q: 程序零改码接入？** A: 设 `HTTP_PROXY=http://127.0.0.1:18080`（仅 HTTP 生效；库若发 CONNECT 则 501）。
- **Q: 如何固定出口？** A: `X-Proxy-Session` 粘滞（同值同节点）；`X-Proxy-Tier` 限定档；免费出口本就轮转，勿假设固定。
- **Q: 自检/验证结果与预期不符（尤其 200/403 错位）？** A: 先查 shell 代理 env（`$env:http_proxy`）：若经 Clash 且 `no_proxy` 未豁免本机，实测其按 `Host` 头路由代理请求——你的请求可能根本没到网关（如直达 mock 拿到 `mock-a-us` 200，或经 VPN 拿到 VPN 出口）。修法：`no_proxy` 加 `localhost,127.0.0.1` 或清代理 env 后重跑；`curl -v` 首行会明示是否用了代理。

## English

### Mode 1: Direct app calls (recommended, zero hops)

```powershell
curl.exe http://127.0.0.1:8916/ -H "Host: httpbin.org" -H "X-Api-Key: default_key"
curl.exe http://127.0.0.1:8916/ip -H "Host: httpbin.org" -H "X-Api-Key: default_key" -H "X-Proxy-Tier: free" -H "X-Proxy-Proto: socks5"
```

Selector headers (exact names as gateway `parse_routing_spec`; wrong names are silently ignored): `X-Api-Key` (auth) / `X-Proxy-Session` (sticky: same value, same node) / `X-Proxy-Country` / `X-Proxy-Tier` / `X-Proxy-Proto` (socks5/socks4, bridge only on explicit requests).

### Mode 2: Python SDK (`tools/ipp_sdk.py`, stdlib only)

```python
from ipp_sdk import IPPClient
c = IPPClient("http://127.0.0.1:8916", session="job-42", tier="free", proto="socks5")
status, body = c.get("http://httpbin.org/ip")   # one delayed retry on 503
```

Self-test: `python tools/ipp_sdk.py --self-test` (plain 200 + sticky 200 + bad-key 403 + no-key 403).

### Mode 3: Forward adaptor (`tools/ipp_forward.py`, for browsers/system proxy/standard `HTTP_PROXY`)

```powershell
D:\DevSoft\Conda\Miniconda3\python.exe log/launch_detached.py D:\DevSoft\Conda\Miniconda3\python.exe log/ipp-forward.out log/ipp-forward.err tools/ipp_forward.py 18080 127.0.0.1 8916
# The adaptor only translates shape, never injects identity: clients must send X-Api-Key themselves (passed through), else 403 past the gateway.
curl.exe -x http://127.0.0.1:18080 -H "X-Api-Key: default_key" http://127.0.0.1:8888/    # 200 (same call direct to gateway is 400)
```

Windows system proxy (manual, scripts never touch it): Settings → Network & Internet → Proxy → Manual → `127.0.0.1:18080` (HTTP only; HTTPS CONNECT gets 501, see limits). PowerShell (admin, on demand):

```powershell
Set-ItemProperty "HKCU:\Software\Microsoft\Windows\CurrentVersion\Internet Settings" ProxyEnable 1
Set-ItemProperty "HKCU:\Software\Microsoft\Windows\CurrentVersion\Internet Settings" ProxyServer "http=127.0.0.1:18080"
# revert: ProxyEnable 0
```

### Mode 4: One-click ops & observability (`tools/ipp.ps1`)

```powershell
powershell -ExecutionPolicy Bypass -File tools/ipp.ps1 status
powershell -ExecutionPolicy Bypass -File tools/ipp.ps1 start -Mocks   # idempotent
powershell -ExecutionPolicy Bypass -File tools/ipp.ps1 stop
```

Observe: Grafana `:3000` (5 panels); `:9091/metrics` (16 groups); `SELECT count() FROM proxy.proxy_telemetry_log` grows with traffic.

### Snippets (illustrative, same semantics as SDK)

Node (`fetch`): `fetch("http://127.0.0.1:8916/ip", {headers:{Host:"httpbin.org","X-Api-Key":"default_key","X-Proxy-Tier":"free"}})`.
.NET: `HttpClient` `GET http://127.0.0.1:8916/ip` plus `Host`/`X-*` headers (`X-Api-Key` required; note: set `Host` via `request.Headers.Host`).
Go: `http.NewRequest(...)` then `req.Host="httpbin.org"` + `req.Header.Set("X-Api-Key","default_key")` + `req.Header.Set("X-Proxy-Tier","free")`.
Java: `HttpRequest.newBuilder(URI.create("http://127.0.0.1:8916/ip")).header("Host","httpbin.org").header("X-Api-Key","default_key")`.

### Limits (drill conclusions)

| Item | Behavior |
|------|----------|
| absolute-URI direct to gateway | 400 (use the adaptor) |
| CONNECT (HTTPS tunneling) | adaptor 501; use app-level integration |
| chunked body / >10MB | adaptor 501 / 413 |
| Free pool level | usually 0 (tiny public survival); `tier=free` correctly 503 when empty |
| `tier=free` + `Authorization`/`Cookie` | 403 (D1: anonymous shared egress refuses credentials, blocks leaks to stranger exits; the gateway's own `X-API-Key` unaffected) |
| Auth (D3 gate on by default) | headerless requests get 403; send dev `default_key`, production uses real keys (SDK sends one by default) |
| LAN exposure | default `127.0.0.1:8916` loopback-only (tightened by D3); override `GATEWAY_ADDR=0.0.0.0:8916` for LAN/containers, multi-host binds the WireGuard address after meshing |
| Public targets via free nodes | target must be publicly reachable (free nodes can't dial your intranet) |
| Windows gateway process | orderly exit ~every 5 min (known), restart recovers; production runs Linux |
| Local VPN (Clash etc.) | gateway fully immune (direct, ignores system proxy); `curl.exe` is a true-direct baseline; if an exit looks like a VPN range, triple-compare (direct / explicit-proxy / via-gateway) before concluding |

### FAQ

- **Q: Can browsers use the gateway directly?** A: No (absolute-URI 400); via adaptor `:18080` HTTP works; HTTPS limits above.
- **Q: Zero-code app onboarding?** A: Set `HTTP_PROXY=http://127.0.0.1:18080` (HTTP only; CONNECT from libs gets 501).
- **Q: Pin the exit?** A: `X-Proxy-Session` sticky (same value, same node); `X-Proxy-Tier` constrains tier; free exits rotate by nature, never assume fixed.
- **Q: Self-test/verification disagrees with expectations (esp. swapped 200/403)?** A: Check shell proxy env first (`$env:http_proxy`): via Clash without localhost exemption, measured behavior routes proxied requests by `Host` header — your request may never reach the gateway (e.g. hitting mocks directly for a `mock-a-us` 200, or exiting via VPN). Fix: add `localhost,127.0.0.1` to `no_proxy` or clear proxy env and re-run; `curl -v` states proxy use on its first lines.
