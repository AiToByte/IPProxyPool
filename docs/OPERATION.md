# OPERATION — GW-R1 企业网关运维手册

> 跟踪表：`plan/2026年9月19日-GW-R1实施计划.md`；方法底座：banyan-skills（**独立仓库，不随本仓分发**，见文末「方法论底座说明」）。
> 本轮 Windows 记功能、Linux 记性能；eBPF/io_uring/MASQUE 只做 spike（§5）。

## 1. 一键起依赖

```powershell
docker compose up -d            # redis 6379 / clickhouse 8123 / prometheus 9090 / grafana 3000
docker exec ipproxy-redis redis-cli ping                                  # PONG
curl.exe -s "http://127.0.0.1:8123/" --data-binary "SHOW TABLES FROM proxy" --user "proxy:123456"
# proxy_telemetry_log
```

注意：宿主 9000 端口若被占用，compose 已把 CH 原生端口映射为
`127.0.0.1:9010:9000`（网关只用 HTTP 8123，不受影响）。

## 2. 启动网关与 Mock

```powershell
cargo build
D:\DevSoft\Conda\Miniconda3\python.exe log/launch_detached.py D:\DevSoft\Conda\Miniconda3\python.exe log/mockA.out log/mockA.err log/mock_upstream.py 8888 mock-a-us
D:\DevSoft\Conda\Miniconda3\python.exe log/launch_detached.py D:\DevSoft\Conda\Miniconda3\python.exe log/mockB.out log/mockB.err log/mock_upstream.py 8889 mock-b-jp
D:\DevSoft\Conda\Miniconda3\python.exe log/launch_detached.py D:\DevSoft\Conda\Miniconda3\python.exe log/mockC.out log/mockC.err log/mock_upstream.py 8890 mock-c-gb
D:\DevSoft\Conda\Miniconda3\python.exe log/launch_detached.py .\gateway\target\debug\pingora-proxy-gateway.exe log/gw.out log/gw.err
curl.exe -s -H "X-Api-Key: default_key" http://127.0.0.1:8916/   # 200（D3 缺省开门须带 Key）
curl.exe -s http://127.0.0.1:9091/metrics                # Prometheus exposition
```

后台进程必须经 `log/launch_detached.py`（DETACHED，不继承控制台），
日志一律落 `log/`，禁放 C 盘；`Get-NetTCPConnection` 禁用，
探活用 `curl --max-time`。

OPT-2 环境门：D3 起默认开启，无 `X-Api-Key` 头直接 403，
有头（即使错 Key）走租户鉴权（403/429）。本机开发带默认 Key
`default_key`（SDK/脚本已默认带，curl 手工加 `-H`）。

> ### ⚠️ `REQUIRE_API_KEY=0` 的真实语义 ≠「没有鉴权」（OPT-R12 实测澄清）
>
> 关掉鉴权门**不等于**放开匿名访问。实测（release 二进制，2026-09-29）：
> `REQUIRE_API_KEY=0` 时，**完全不带 `X-Api-Key` 的请求返回 200 并获得完整
> 代理服务**。机制是网关把无头请求**静默补成默认租户**
> （`unwrap_or_else(|| DEFAULT_API_KEY.to_string())`），而默认租户是
> **qps 10000 / 并发 10000** 的满额配额。
>
> 也就是说：`REQUIRE_API_KEY=0` 的语义是「**全网共享一个满额身份**」，
> 而不是「没有鉴权」。凡能连到该端口的人，都拿到同一个满额租户。
>
> **因此它只适用于本机开发或隔离网络，绝不可用于生产。**

> ### `API_KEY`：生产必须覆盖默认 Key（OPT-R12 B1）
>
> 启动时读 `API_KEY` env：
>
> | 配置 | 生效的默认租户 Key | `X-Api-Key: default_key` |
> | ---- | ------------------ | ------------------------ |
> | 未设 `API_KEY` | `default_key`（公开弱值，仅供开发） | 有效 |
> | `API_KEY=<强随机值>` | 该值 | **立即 403 失效** |
> | `API_KEY` 为空／等于 `default_key` | **不注册任何默认租户**（全部 403） | 403 |
>
> - **一旦设置 `API_KEY`，`default_key` 立刻失效**（不再注册），旧客户端
>   会收到 403。升级前请确认所有调用方（含 5 个 SDK，SDK 默认带
>   `default_key`）都已改为传新 Key。
> - 误配（空串／纯空白／显式等于 `default_key`）**fail-closed**：不注册默认
>   租户、全部请求 403 —— 宁可拒绝服务，也不接受一个"等于没配"的 Key。
> - 生产取强随机值，例如
>   `API_KEY=[System.Guid]::NewGuid().ToString('N') + [System.Guid]::NewGuid().ToString('N')`
>   （Linux：`head -c 48 /dev/urandom | base64`）。注意 Key 经 `X-Api-Key` 头
>   传输，请确保不落进 shell 历史与日志。
>
> ### 危险组合的启动告警（OPT-R12 A1）
>
> 「弱默认 Key ＋ 满配额 ＋ **非回环**监听」三者同时成立时，网关启动会打
> 一条 `WARN`，并逐条给出整改动作。三者缺一不告警，因此本机开发与生产
> 正确配置都不会被这条打扰。

## 3. 三家真 Key 灰度（GW-R2，用户给 Key 后执行）

1. Key 入库：走 secret 管理（禁进仓库），staging 先配 1 家 1 国；
2. `main.rs` 初始池把对应 `mock-x` 换成真实 `ip:port + username/password`，
   provider 名改为真实名（`oxylabs` / `brightdata` / `netnut`），country 照实；
3. 观察：`query_provider_sla` 5min 窗 + Grafana 403 比面板；
   套利阈值不变（<80 降权 0 / >95 恢复 100），降权即 `weight→0`（选路过滤摘除，
   池内保留可恢复），恢复 `weight→100` 即时生效，无需重启
   （R2-1 起真权重语义，`matches` 过滤 `weight==0`；GW-1 的 retain/remove 已退役）；
4. 计费对账：`tenant.total_bytes × tier单价` 与供应商账单按周对（另起 GW-R2）；
5. 全量：逐家逐国放开，每步至少观察 1 个 60s 套利周期。

## 4. 日常巡检

| 信号 | 位置 | 阈值/动作 |
|---|---|---|
| 成功率 | Grafana panel 1 | <99% 查 403 比面板定位 provider |
| P99 附加时延 | panel 2 | 持续>100ms（dev 基线 65ms）查预热/CB 日志 |
| 403 比 | panel 3 | 突增=被目标站反爬，确认 quarantine key 生效 |
| 带宽/余额 | panel 4 / tenant balance | 余额不足先 `set_active(key,false)` 停服再充值 |
| 熔断 key | `quarantine:{domain}:{ip}` | TTL 到自愈；内存 TTL 独立，DEL 键不清内存 |
| Stream 堆积 | `XLEN stream:proxy:telemetry` | 持续增长=CB 消费组 lag，查 `XINFO GROUPS`；R2-5 起 XADD 带 MAXLEN ~10 万（消费组全挂时老数据先丢，XLEN 封顶）；sink 启动清幽灵消费者，毒丸/重复即时 ack 不进仓 |
| 后台并发 | 网关日志 `[Prober]/[Prewarmer]/[Sweep]/[Arbitrage]` | R2-7 起三 60s ticker 启动错峰（`staggered start` 行对齐验证）；prober 20 并发 + Client 闲置 10min 淘汰；prewarmer 建链/握手探测 100 并发封顶（http 建链＋socks greeting，P2-6 起），`tickets`==探测节点数（票据环已删） |
| 配置覆盖 | 启动环境变量 | R2-8 起 `REDIS_URL/CLICKHOUSE_URL(_USER/_PASSWORD/_DB)/GATEWAY_ADDR/METRICS_ADDR/*_INTERVAL_SECS` 全 env 化（缺省沿用 code 常量，compose 有示例）；后台 CB/sink/arbitrage/free_pool 由 supervisor 托管（panic/退出即 backoff 重启，`supervisor_restarts_total{worker}` 计数）；数据面日志 5xx 全量、其余 1/1000（`gateway_logs_sampled_total` 可观测）；`/metrics` 读超时 5s + 并发 64 封顶 |
| 免费线水位 | `free_pool_nodes_total` / 日志`[FreePool] tick` | 默认关闭（`FREE_ENABLED=1` 开）；水位突降=源站熔断（`free_pool_source_suspended{source}=1`）或质检门限过严（`free_pool_verify_total` 看 fail 分布）；country 缺省 ZZ，只服务无归属要求的流量；`FREE_REQUIRE_ELITE=1` 时仅 Elite 进池 |
| 免费线健康 | `free_pool_source_yield_total` / `free_pool_anonymity_total` | yield 骤降=源站挂；transparent 占比突增=源站质量恶化，考虑开 REQUIRE_ELITE；单节点转发延迟看 registry 日志（debug） |
| SOCKS 桥 | `free_pool_nodes_by_proto{proto}` / 日志`[SocksBridge]` | P2 起仅显式 `X-Proxy-Proto: socks5/socks4` 请求走桥；默认流量永不命中 socks（router 默认隔离＋peer 守卫＋粘滞 proto 复核三保险）；body 超 `SOCKS_MAX_BODY_BYTES`（默认 10MB）按失败计＋warn＋换节点重试 |


#### 网关在 Windows 上的生命周期限制（OPT-R14 B，P0）

> **Windows 上网关会在约 300 秒后自进退出（已修，请了解历史）。**

根因**不在本仓**，而在依赖：`pingora-core 0.6.0` 的
`Server::run()` 里有一行 `#[cfg(windows)] let shutdown_type = ShutdownType::Graceful;`。
Windows 上 `main_loop` 从不被 await（那段是 `#[cfg(unix)]`），
**没有信号等待**，`shutdown_type` 被硬编码为 Graceful ⇒
网关先 `sleep(grace_period)` 再 `process::exit(0)`。

- **现象：无 panic、无错误码，只有一句 `All runtimes exited, exiting now`**。
  本仓实测退出存活 **305~308s**（跨 FreePool 开关均复现）。
  正因如此难以定位，很容易被误以为“随机崩溃”。
- **修复：** `GATEWAY_GRACE_SECS` 改为**平台感知默认值**——
  **Windows 默认 86400s（1 天）**（仅为绕过框架限制），
  **Unix 仍为 300s**（那里是真正的优雅停机等待窗口，**行为零变化**）。
  Windows 上启动会打 `WARN` 写明该限制。
- **覆盖方式：** 需短命进程（如测试）显式设
  `GATEWAY_GRACE_SECS=1` 即可立退；Unix 上该变量仍是优雅停机窗口。
- **长期正确的修法：升级 Pingora** 到修复 Windows `main_loop`
  路径的版本。本仓未做（需联网解析新版本并重验整条数据面）。
- 回归防护：`python tools/check_windows_lifetime.py`（已进 CI）。
  把默认值“清理”回 300 在 review 中几乎看不出问题，但 Windows 上网关
  会重新开始自杀——这道门就是为防它。

#### 免费线供给侧可重复验证（OPT-R14 A）

```bash
# 探一个已在跑的网关
python tools/probe_free_pool.py --metrics http://127.0.0.1:9221/metrics
# 或顺带拉起网关、等一个抓取周期后自动判定
python tools/probe_free_pool.py --launch --wait 420
```

判定分四类，**刻意不含"节点数阈值"**：

| 判定 | 含义 | 该怎么办 |
| ---- | ---- | -------- |
| `VERIFIED` | 有节点通过验证并入池 | 供给侧链路完好 |
| `DEGRADED_ZERO` | 抓到候选但 0 个入池 | **不是缺陷**，见下方 churn 说明 |
| `FETCH_FAIL` | 没有任何源产出候选 | 抓取链路坏了，值得查（本机无公网 / 源全挂） |
| `INCONCLUSIVE` | 指标不可达 / 进程中途退出 | **不下代码结论**，见下方说明 |

> **公网免费代理存在天然 churn，池为 0 属常态而非缺陷。** 2026-09-30 实测两轮：
> Geonode 抓取 11275 个候选（intake 截到 `2000×2=4000`）、`tcp_fail` 2649/2962，
> 而**通过验证入池的分别是 30 个和 22 个**真实代理。逐轮波动明显。
> 正因如此，**本仓刻意不做"healthy ≥ N"的数量门**——那种门会周期性假红，
> 而假红门禁会被运维学会忽略，比没有门更糟。数量验证用上面的报告式探针。

> **`INCONCLUSIVE` 是一等公民，不是失败。** 本机实测网关在 detached 环境下约
> 5~6 分钟后自行退出（日志尾 `All runtimes exited`，无 panic），而免费代理首个
> 验证通过节点需约 5 分钟出现，两者窗口几乎重合。此时脚本若硬报 FAIL，等于用
> 环境问题污染代码结论。**该现象另有一个更严重的疑似 P0 根因**（异步上下文内
> drop tokio runtime，见 `log/gw-demo-end.err` 的历史 panic），已单独立项。

> **⚠️ 尚未验证：bandit 在真实 free 池规模（约 2000 节点）下的重分布。**
> 探针只覆盖**供给侧**（抓取→验证→入池）。重分布需要把每个请求归因到具体出口
> 节点，依赖 ClickHouse 遥测；本机 Docker/ClickHouse 不可用时遥测降级、无法归因。
> 已验证的结论仅限于 mock 三节点场景（1500 次请求 ⇒ 三节点均命中）。
> 另需注意量纲差异：free 臂的遗忘节拍是 1000 次（付费线 10000），churn 更高 ⇒
> 臂寿命更短 ⇒ 探索窗口比 mock 场景更宽——但这是**推断，非实测**。

| 免费套利 | `free_pool_action()` 函数分档（P3，非指标，无 exposition） | 池级成功率<50 摘除（TTL/复检自愈）/50~80 半权/≥80 hold；free 永不自动抬权（恢复走复检/health）；付费 80/95 冻结不动；Grafana 第 5 面板看水位＋verify 分布＋mismatch |
| GeoIP 画像 | `geoip_lookups_total{result}` / `geoip_mismatch_total` | 无库 Disabled 只观察不执法（首 tick 记一次 disabled）；mismatch 突增＝源站地理造假或库陈旧，先查库版本再定；执法留 Phase 4 |

租户管理：`register_tenant(id,key,qps,max_c,burst)` 注册（R2-3 起 burst 必传，常规取 `qps/10`）；`set_active` 启停；
计费 DC $0.2 / Res $3 / Mobile $15 每 GB；Free $0（计量字节但不计费，FreePool 节点 tier，Task 1 冻结）。R2-3 起余额≤0 鉴权直接 402（欠费），与 403（坏 Key/停用）区分；当次流量可扣成负数，下次请求拦截。
OPT-3 计费口径（已冻结）：只计最后一次 attempt 的出站字节，失败 attempt
的字节在重试前清零，不进账单；`logging` 侧不做补偿。

## 5. Spike 结论（本轮不落地，详见 `docs/SPIKE_R2.md`）

- **eBPF Sockmap**：收益在内网小包高频场景最大；本网关瓶颈在出站 TLS
  与供应商 RTT，Sockmap 跳过用户态拷贝的收益 <5%，且需 CAP_SYS_ADMIN +
  内核 ≥5.8，运维成本 > 收益 → 不落地，GW-R2 复评。
- **io_uring**：tokio 已用其做文件 IO；网络面 Pingora 自有 epoll 优化，
  切 io_uring 需换 runtime，得不偿失 → 不落地。
- **MASQUE/QUIC 隧道**：供应商普遍只收 TCP 代理，QUIC 出口无对端；
  自建 Mesh 可用 WireGuard（模板见 `docs/EGRESS_MESH.md`）→ 隧道用 WG，
  MASQUE 延后。
- **BGP Anycast**：需 ASN/地址段与运营商会话，本轮只给方案文档，
  生产割接另起 GW-R2。

## 6. 故障速查

- 网关 503 全域：池被 quarantine 摘空（查 CB 日志 + Redis key）或 mocks 挂了；
 - 免费线零信任：免费节点**禁止**承载含认证/cookie/支付/银行流量（D1 起网关层强制：`tier=free`＋`Authorization`/`Cookie` 直接 403，租户侧规约同步：敏感租户绑定 tier≠free；`FREE_REQUIRE_ELITE=1` 为敏感实践）；Transparent 节点在 REQUIRE_ELITE=1 时被 merge 门强制过滤，为 0 时仅服务无归属流量（OPERATION 警告）；
 - VPN 免疫（H1/H2 加固后）：网关全链路直连（共享 Client 禁系统代理；桥/复检/探针显式代理本就免疫），结果与操作员本机 VPN 状态无关；`curl.exe` 从不走系统代理，可作真直连基线；匿名度分级是“出口 vs 直连基线”比较；
- 复检基址必须 https（启动校验，非法回落默认）；抓取源仅 http/https（file/dict/gopher 一律过滤，防 SSRF）；
- SOCKS 桥零信任延续：socks 节点同样禁敏感流量（与免费线同规）；握手/CONNECT 只连验证与请求目标，不做扫描；relay 只在 E2E 脚本出现，不进生产；
- GeoLite2 配库（P3）：MaxMind 账号取 license→下 GeoLite2-City.mmdb→挂载进容器/宿主→`GEOIP_MMDB_PATH` 指向→重启网关（热加载不做）；无库默认 Disabled，免费线行为不变（`geoip_lookups_total{result="disabled"}` 可见）；
  - **使用 GeoLite2 即须随部署附带下列署名**（见本节末「第三方数据署名」块）。
- GeoLite2 自动更新（P4，每周三 03:00 UTC）：`MAXMIND_LICENSE_KEY` 环境传入（不要落盘）后跑
  `python deploy/geoip_update.py --out-dir ./data`（干跑验证：无 key 时 exit 2＋usage）；
  Linux cron 例：`0 3 * * 3 cd /opt/IPProxyPool && MAXMIND_LICENSE_KEY=$KEY python3 deploy/geoip_update.py`；
  Windows schtasks 例：`schtasks /create /tn GeoIPUpdate /tr "python D:\IPProxyPool\deploy\geoip_update.py" /sc weekly /d WED /st 03:00`（key 经计划任务环境变量传入）；
  更新后重启网关（热加载不做）；生效标志：启动行 `[GeoIP] live DB loaded`＋`geoip_lookups_total{result="hit"}` 上涨；
  执法开关 `GEOIP_ENFORCE_MISMATCH=1` 仅在库 Live 且观察一段时间无误报后开（默认 0 只观察），
  开后 mismatch 按复检失败计（backoff＋`geo_fail`，TTL 内自愈）；
- 缺 Host 400：R2-2 起畸形请求（无 Host 头）直接 400，不占租户配额；
- 后台工人停转：CB/sink/arbitrage 由 supervisor 托管，`supervisor_restarts_total{worker}` 涨即正在自愈（指数 backoff 1s 起 60s 封顶）；
- 403 全拦截：X-API-Key 未注册或无头（D3 缺省开门；默认 `default_key` 已在 main 注册，带头即放行）；
- /metrics 无数据：确认走 :8916 有流量（intercept 的 403 也计数）；
- CH 查不到数：流式泵已上线（`ch_sink_group` 常驻，batch 5000/1s），查
  `SELECT count() FROM proxy.proxy_telemetry_log` 应随流量涨；不动则看
  网关日志 `[ChSink]`（insert 失败会 hold 住 ack 等 CH 恢复）；

### 端口 443 的 HTTPS 代理：TLS 证书怎么配（OPT-R15）

网关对端口 **443** 的 HTTP 节点走 **TLS**（`is_tls_for_node`），其他端口走明文。

**证书校验默认是严格的**（`PeerOptions::verify_cert` 缺省 `true`）——代理证书必须由
**受信任 CA 签发**，且 SAN 必须覆盖 SNI。SNI 取的是**目标域名**（请求的 `Host`），
不是代理地址，所以一个只签了 `localhost` 的证书会在访问任何真实目标时失败，
报错形如：

```
tls connect error cause: invalid peer certificate: UnknownIssuer, SNI: <你的目标域名>
```

**让网关信任自建 CA（零代码改动，框架正规通道）**：`pingora-rustls` 的
`load_platform_certs_incl_env_into_store` 会处理 **`SSL_CERT_FILE`** / `SSL_CERT_DIR`，
rustls 的 root store 在连接器构建时由它填充。启动前设置即可：

```powershell
$env:SSL_CERT_FILE = "C:\path\to\ca.pem"
```

**自签测试证书的两个坑（都会让 openssl 误报"没问题"、而 rustls 拒绝）**：

- **必须带 SKI + AKI**：缺 `AuthorityKeyIdentifier` 时 `openssl s_client` 仍显示
  `Verification: OK`，但 rustls / Python 严格模式报
  `Missing Authority Key Identifier`。别依赖校验器宽松。
- **叶证书 SAN 必须覆盖目标域名**，否则报 hostname 不匹配而非 UnknownIssuer。

**不要用"关闭证书校验"绕过**：`PeerOptions::verify_cert = false` 在 rustls 后端
**无效**（它只参与 hostname 匹配，证书链校验是硬编码的；作者在同一处留了
`allowing to disable verification` 的 TODO），而且即便有效也是不该采用的做法。
正解就是给代理配受信任证书。

**可复现验证**：`tools/verify_tls_egress.py` 生成 CA+叶证书并校验本地 443 代理链路；
端到端需再用 `TEST_POOL_NODES` 把 `127.0.0.1:443` 注入池（见下条）。

### 测试节点注入通道：`TEST_POOL_NODES`（仅测试用）

节点池由 `RouterEngine` 的 `ArcSwap` 持有，**没有运行时注入节点的公开 API**，
端到端验证需要把特定节点放进候选池时用这个变量。格式逗号分隔，每项
`ip:port:tier:country:provider:weight`，例：

```powershell
$env:TEST_POOL_NODES = "127.0.0.1:443:residential:HX:https-proxy:100"
```

**安全边界（刻意收紧）**：**默认空 ⇒ 零影响**（不设即行为不变）；**仅接受回环地址**，
公网条目直接拒绝并 `warn`（杜绝"用测试通道把任意地址塞进池"）；解析失败跳过并
`warn`、**不 panic**（测试通道不该有能力打挂网关）。

配合 `X-Proxy-Country: <country>` 可把选路唯一锁定到该节点，避免 bandit 优选其他臂
——这在验证特定节点行为时是必需的（否则会误以为"没命中"，实为被优选臂抢走）。

### 遥测落地端静默丢数据：怎么发现、怎么修（OPT-R15）

**问题（实测复现，非推演）：** `REDIS_URL` 少写密码 ⇒ 遥测**静默丢光**，而网关照常服务、
指标上也看不出异常。当时只有翻日志才发现，且报错原文 `Protocol error: unauthenticated
multibulk length` **看着像 ClickHouse 错误，其实是 Redis 协议错误**——第一直觉必然查错方向。

**现在可编程察觉**（`/metrics`，`backend` = `redis` | `clickhouse`）：

| 指标 | 含义 |
|---|---|
| `telemetry_sink_up{backend}` | 最近一次写入尝试成功=1／失败=0 |
| `telemetry_sink_failures_total{backend}` | 写入失败次数（累计故障史，恢复后**不**清零） |
| `telemetry_sink_last_ok_unixtime_seconds{backend}` | 最近一次**成功**写入的 Unix 秒；`0` = 试过但从未成功 |
| `telemetry_dropped_total` | 因落地失败而丢弃的事件数（原有指标） |

**三条告警**（`deploy/prometheus/rules.yml`，已过 `tools/check_promql_metrics.py` 校验）：

- `TelemetrySinkDown`（critical，2m）——`telemetry_sink_up == 0`。持续 2m 才报，过滤单次毛刺。
- `TelemetrySinkNeverSucceeded`（critical，10m）——`last_ok == 0`。比 Down 更严重：
  不是「曾经好过现在坏了」，而是「从上线起就没通过」。
- `TelemetrySinkStale`（warning，5m）——`up=1` 但 10 分钟无成功写入（假活/挂起）。
  表达式里的 `> 0` 用于排除「一次都没成功」，否则会与上一条重复误报。

**两点口径要记住：**

1. **序列懒出现**：未配置/未跑过的落地端**不渲染样本行**（而非渲染 `up=0`），
   所以「ClickHouse 压根没配」的部署不会被 Down 规则误伤。告警的默认状态是"沉默"，不是"一片红"。
2. **两级互相独立**：Redis 好、ClickHouse 坏时 `backend="redis"` 仍为 1，只有 CH 那条转 0。
   `TelemetryStreamBacklogHigh` 依赖 `redis_exporter`（compose 里 profile 门控，未接时静默不发），
   本组只依赖网关 `/metrics`、**默认可用**，是它的兜底。

**按 backend 排查：**

- `redis`：查 `REDIS_URL` 是否含密码。**Redis 密码是容器启动参数** `--requirepass`，
  **不在容器环境变量里** —— `docker inspect --format '{{range .Config.Env}}' <c>` 查不到，
  只能读 `.Config.Cmd`（`docker inspect <c> --format '{{.Config.Cmd}}'`）。这是最容易踩的诊断陷阱。
- `clickhouse`：查 `CLICKHOUSE_USER` / `CLICKHOUSE_PASSWORD` / `CLICKHOUSE_DB`；
  也可能是容器只是 Exited（`docker start ipproxy-clickhouse`）。
- 两级都 0：优先怀疑网络/凭据根本没配对，而不是网关问题。

**为什么序列化失败不算落地端不健康**：JSON 序列化失败是网关自己的 bug，标到
Redis/ClickHouse 上会把责任指错方向，也无法靠改依赖解决——故只有**真正与依赖交互**
成功/失败才更新 `telemetry_sink_up`。

- Windows 传 JSON 给 redis-cli 会丢引号：用 `log/redis_inject.py`（raw RESP）。
- OPT-R4 C5/C6 凭据：复制 `.env.example` 为 `.env` 后改密码；`tools/ipp.ps1` 自动加载（CI/显式 env 优先）；compose 用 `${VAR:-缺省}` 引用。依赖端口只绑回环（6379/8123/9090/3000），局域网直达已封。
- Live 测试（`cargo test -- --ignored`）需带密环境：`$env:REDIS_URL="redis://:xxx@127.0.0.1:6379/"`＋`$env:CLICKHOUSE_PASSWORD="xxx"`（与 .env 同值）；CI 用无密 service 走缺省。
- OPT-R4 C12 备份恢复：`tools/backup.ps1` 产出 `backup/<stamp>/`（redis-dump.rdb＋ch-telemetry-freeze＋clickhouse-data.tar＋grafana-data.tar）。备份口径：Redis 先 BGSAVE 再拷 `dump.rdb`→`redis-dump.rdb`；CH 先 `ALTER TABLE proxy.proxy_telemetry_log FREEZE`→拷 `shadow/<N>` 为 `ch-telemetry-freeze`→`UNFREEZE WITH NAME` 清理（File BACKUP 需服务端白名单，本镜像未配故不用），另加 `clickhouse-data.tar` 卷 tar 全量兜底；Grafana 卷 tar 为 `grafana-data.tar`。恢复：停服（`ipp.ps1 stop`＋`compose stop`）→ 双 tar 回放（`clickhouse-data.tar`/`grafana-data.tar` 解回各自具名卷；rdb→`ipproxy-redis:/data/dump.rdb`；`ch-telemetry-freeze` 按需拷回）→ 起服＋副本表验行数（建临时表对行数再换名，不碰生产表）＋`SELECT count()` 对账。演练建议：季度一次。
- D2 局域网敞口声明：`GATEWAY_ADDR` 缺省 `127.0.0.1:8916`（D3 已收紧；此前 `0.0.0.0:8916` 局域网可直达）。多机用 WireGuard 组网后绑 WG 地址（如 `GATEWAY_ADDR=10.8.0.1:8916`），密钥走 WG，不改网关代码；容器场景显式覆写 `0.0.0.0:8916`（见 compose）。D3 破坏性收紧（REQUIRE_API_KEY 默认 1＋监听收 127.0.0.1）已按拍板执行（步骤 27），回退置 `REQUIRE_API_KEY=0`＋`GATEWAY_ADDR=0.0.0.0:8916`。

---

## 第三方数据署名 / Third-Party Data Attribution

> **何时适用：** 一旦你在部署中启用了 GeoLite2（即设置了 `GEOIP_MMDB_PATH` 并加载了
> `.mmdb` 文件），**必须**把下面的署名随部署一并提供。GeoLite2 数据库**不在本仓库内**，
> 需自行从 MaxMind 获取，署名义务发生在**使用方**。
>
> **为何单列一节：** `docs/OPEN-SOURCE.md` 曾声明「OPERATION 有署名行」，但本文件
> 此前**只有 GeoLite2 的操作步骤、没有任何署名**（OPT-R10 实测发现并补齐）。
> GeoLite2 采用 **CC BY-SA 4.0**，该许可**强制要求署名**（Attribution 4.0
> International §3(a)）——这与其他技术债不同，它是**法律风险**而非工程问题。

### GeoLite2（使用即须附带）

```
This product includes GeoLite2 data created by MaxMind, available from
https://www.maxmind.com.

GeoLite2 Endpoints / GeoLite2 City databases are provided under the
Creative Commons Attribution-ShareAlike 4.0 International License (CC BY-SA 4.0):
https://creativecommons.org/licenses/by-sa/4.0/

Copyright © MaxMind, Inc.
```

署名四要素对照（CC BY-SA 4.0 §3(a) 要求）：

| 要素 | 本项目对应内容 |
|---|---|
| 提供者署名 | `Copyright © MaxMind, Inc.` |
| 许可名称与链接 | `CC BY-SA 4.0` ＋ `https://creativecommons.org/licenses/by-sa/4.0/` |
| 免责声明 | MaxMind 官方站点（数据「AS IS」，无担保） |
| 数据来源 | `https://www.maxmind.com` |

**建议的提供方式**（任选其一，或多种并行）：

- 部署环境可见处（如本机 `data/` 目录内放一份 `ATTRIBUTION.txt`）；
- 你的产品/服务对外的「开源许可 / 数据来源」页面；
- 对外分发的二进制/容器镜像的 labels 与文档。

**不要做的事：** 不要移除本节，也不要把 GeoLite2 数据库提交进本仓库
（`.gitignore` 的 `data/` 已覆盖；数据库需 MaxMind 账号与 license key 获取）。
自动更新脚本 `deploy/geoip_update.py` 只负责下载，**不负责也不应该**自动注入署名
到你的对外物料——那是部署方的义务。

---

## 方法论底座说明（不随本仓库分发）

`banyan-skills` 是本项目采用的「先落库再执行」方法底座，**位于独立仓库**
（`https://github.com/AiToByte/Banyan`），**不随本仓库分发**（被 `.gitignore`
排除，非 git submodule）。它只提供工作流规范文档，不含任何运行时代码——
**本仓库的构建、测试、部署均不依赖它**。若需查阅该方法论，请另行 clone；
本仓库的实现与文档不因它缺失而受影响。
