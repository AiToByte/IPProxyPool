# OPERATION — GW-R1 企业网关运维手册

> 跟踪表：`plan/2026年9月19日-GW-R1实施计划.md`；方法底座：banyan-skills P1-P5。
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
curl.exe -s http://127.0.0.1:8080/                       # 200
curl.exe -s http://127.0.0.1:9091/metrics                # Prometheus exposition
```

后台进程必须经 `log/launch_detached.py`（DETACHED，不继承控制台），
日志一律落 `log/`，禁放 C 盘；`Get-NetTCPConnection` 禁用，
探活用 `curl --max-time`。

OPT-2 环境门：`REQUIRE_API_KEY=1` 启动网关后，无 `X-API-Key` 头直接 403，
有头（即使错 Key）走租户鉴权（403/429）；默认关闭时无头走 `default_key`
宽限额，存量行为不变。

## 3. 三家真 Key 灰度（GW-R2，用户给 Key 后执行）

1. Key 入库：走 secret 管理（禁进仓库），staging 先配 1 家 1 国；
2. `main.rs` 初始池把对应 `mock-x` 换成真实 `ip:port + username/password`，
   provider 名改为真实名（`oxylabs` / `brightdata` / `netnut`），country 照实；
3. 观察：`query_provider_sla` 5min 窗 + Grafana 403 比面板；
   套利阈值不变（<80 降权 0 / >95 恢复 100），降权只是 `retain` 摘除，
   恢复需重启（GW-1 `adjust_vendor_weight` retain/remove 垫片，GW-R2 改真权重）；
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
| 后台并发 | 网关日志 `[Prober]/[Prewarmer]/[Sweep]/[Arbitrage]` | R2-7 起三 60s ticker 启动错峰（`staggered start` 行对齐验证）；prober 20 并发 + Client 闲置 10min 淘汰；prewarmer 真 TCP 探测 100 并发封顶，`tickets`==探测节点数（票据环已删） |
| 配置覆盖 | 启动环境变量 | R2-8 起 `REDIS_URL/CLICKHOUSE_URL(_USER/_PASSWORD/_DB)/GATEWAY_ADDR/METRICS_ADDR/*_INTERVAL_SECS` 全 env 化（缺省沿用 code 常量，compose 有示例）；后台 CB/sink/arbitrage 由 supervisor 托管（panic/退出即 backoff 重启，`supervisor_restarts_total{worker}` 计数）；数据面日志 5xx 全量、其余 1/1000（`gateway_logs_sampled_total` 可观测）；`/metrics` 读超时 5s + 并发 64 封顶 |

租户管理：`register_tenant(id,key,qps,max_c,burst)` 注册（R2-3 起 burst 必传，常规取 `qps/10`）；`set_active` 启停；
计费 DC $0.2 / Res $3 / Mobile $15 每 GB。R2-3 起余额≤0 鉴权直接 402（欠费），与 403（坏 Key/停用）区分；当次流量可扣成负数，下次请求拦截。
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
- 缺 Host 400：R2-2 起畸形请求（无 Host 头）直接 400，不占租户配额；
- 后台工人停转：CB/sink/arbitrage 由 supervisor 托管，`supervisor_restarts_total{worker}` 涨即正在自愈（指数 backoff 1s 起 60s 封顶）；
- 403 全拦截：X-API-Key 未注册（默认 `default_key` 已在 main 注册）；
- /metrics 无数据：确认走 :8080 有流量（intercept 的 403 也计数）；
- CH 查不到数：流式泵已上线（`ch_sink_group` 常驻，batch 5000/1s），查
  `SELECT count() FROM proxy.proxy_telemetry_log` 应随流量涨；不动则看
  网关日志 `[ChSink]`（insert 失败会 hold 住 ack 等 CH 恢复）；
- Windows 传 JSON 给 redis-cli 会丢引号：用 `log/redis_inject.py`（raw RESP）。
