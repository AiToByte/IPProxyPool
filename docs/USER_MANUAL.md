# User Manual 用户使用手册

> 双语：中文在前，English after. Bilingual: Chinese first, English second.
> 命令与 `docs/OPERATION.md` 同源；OPERATION 为运维真相源，本手册为入门路径。
> Commands share the source with `docs/OPERATION.md`, which remains the ops source of truth.

## 中文

### 1. 安装前置

- Windows（验功能）或 Linux（验性能）；Rust 1.93＋`cargo`；Docker Desktop；Python 3（仅辅助脚本）。
- 端口：8080（网关）/9091（指标）/6379/8123/9090/3000（依赖）。CH 原生端口已避让为 `127.0.0.1:9010:9000`。

### 2. 启动（5 步）

```powershell
docker compose up -d
docker exec ipproxy-redis redis-cli ping
# 期待 PONG
curl.exe -s "http://127.0.0.1:8123/ping" --user "proxy:123456"
# 期待 Ok.
cargo build --manifest-path gateway/Cargo.toml
D:\DevSoft\Conda\Miniconda3\python.exe log/launch_detached.py .\gateway\target\debug\pingora-proxy-gateway.exe log/gw.out log/gw.err
curl.exe --max-time 5 -s -o NUL -w "gw:%{http_code}\n" http://127.0.0.1:8080/
# 期待 200
```

说明：后台进程必须经 `log/launch_detached.py`（DETACHED，不继承控制台）；日志一律落 `log/`；禁用 `Get-NetTCPConnection`（探活用 `curl --max-time`）。

### 3. 验证代理

```powershell
# 普通（无状态，LinUCB/加权）
curl.exe --max-time 5 -s -o NUL -w "%{http_code}\n" http://127.0.0.1:8080/
# 粘滞（同 session 两次命中同节点）
curl.exe --max-time 5 -s -o NUL -w "%{http_code}\n" -H "X-Session-Id: demo-1" -H "X-Tenant-Country: US" http://127.0.0.1:8080/
# 坏 Key（期待 403）、缺 Host（期待 400）
curl.exe --max-time 5 -s -o NUL -w "%{http_code}\n" -H "X-Api-Key: bad" http://127.0.0.1:8080/
curl.exe --max-time 5 -s -H "Host:" http://127.0.0.1:8080/ -o NUL -w "%{http_code}\n"
# 指标
curl.exe --max-time 5 -s http://127.0.0.1:9091/metrics | Select-String "free_pool_nodes_total"
```

### 4. 日常巡检（Grafana :3000 5 面板＋指标）

| 看什么 | 哪里 | 异常动作 |
|--------|------|----------|
| 成功率/P99/403 比/带宽 | Grafana 前 4 面板 | 查 `quarantine:{domain}:{ip}` 与 CB 日志 |
| 免费水位/verify/geo | 第 5 面板 | 水位突降查 `free_pool_source_suspended` 与源站 |
| Stream 堆积 | `XLEN stream:proxy:telemetry` | 持续增长查消费组 lag |
| 落库 | `SELECT count() FROM proxy.proxy_telemetry_log` | 不动看 `[ChSink]` 日志 |

### 5. 故障速查

- 全域 503：池被隔离摘空或 mocks 挂了；
- 403 全拦截：Key 未注册（默认 `default_key` 已注册）；
- 402：租户欠费，充值后恢复；
- 免费线零信任：禁认证/cookie/支付/银行流量；生产建议 `FREE_ENABLED=1＋FREE_REQUIRE_ELITE=1`；
- Windows 传 JSON 给 redis-cli 丢引号：用 `log/redis_inject.py`。

### 6. FAQ

- **Q: 免费池长期为 0 正常吗？** A: 正常。公网免费存活率极低（实测约每 100~200 raw 偶发 1 Elite），pool 常态 0，付费线不受扰。
- **Q: 网关进程自行退出？** A: Windows 已知 Pingora 停机路径有序退出（`All runtimes exited`），重启即恢复；生产跑 Linux。
- **Q: CH 容器 unhealthy？** A: 已知 wget 探针 artifact，以 `SELECT 1` 为准。
- **Q: P99 多少算正常？** A: dev 基线约 65ms（Windows debug，非生产承诺）；Linux 验收另起。

## English

### 1. Prerequisites

- Windows (function) or Linux (performance); Rust 1.93 + `cargo`; Docker Desktop; Python 3 (helper scripts only).
- Ports: 8080 (gateway) / 9091 (metrics) / 6379 / 8123 / 9090 / 3000 (deps). CH native port remapped to `127.0.0.1:9010:9000`.

### 2. Startup (5 steps)

```powershell
docker compose up -d
docker exec ipproxy-redis redis-cli ping
# expect PONG
curl.exe -s "http://127.0.0.1:8123/ping" --user "proxy:123456"
# expect Ok.
cargo build --manifest-path gateway/Cargo.toml
D:\DevSoft\Conda\Miniconda3\python.exe log/launch_detached.py .\gateway\target\debug\pingora-proxy-gateway.exe log/gw.out log/gw.err
curl.exe --max-time 5 -s -o NUL -w "gw:%{http_code}\n" http://127.0.0.1:8080/
# expect 200
```

Notes: background processes must go through `log/launch_detached.py` (DETACHED, no console inheritance); all logs under `log/`; `Get-NetTCPConnection` is banned (probe with `curl --max-time`).

### 3. Verify proxying

```powershell
# plain (stateless, LinUCB/weighted)
curl.exe --max-time 5 -s -o NUL -w "%{http_code}\n" http://127.0.0.1:8080/
# sticky (same session hits same node twice)
curl.exe --max-time 5 -s -o NUL -w "%{http_code}\n" -H "X-Session-Id: demo-1" -H "X-Tenant-Country: US" http://127.0.0.1:8080/
# bad key (expect 403), missing Host (expect 400)
curl.exe --max-time 5 -s -o NUL -w "%{http_code}\n" -H "X-Api-Key: bad" http://127.0.0.1:8080/
curl.exe --max-time 5 -s -H "Host:" http://127.0.0.1:8080/ -o NUL -w "%{http_code}\n"
# metrics
curl.exe --max-time 5 -s http://127.0.0.1:9091/metrics | Select-String "free_pool_nodes_total"
```

### 4. Daily inspection (Grafana :3000, 5 panels + metrics)

| What | Where | On anomaly |
|------|-------|------------|
| Success/P99/403 ratio/bandwidth | first 4 panels | check `quarantine:{domain}:{ip}` and CB logs |
| Free level/verify/geo | 5th panel | level drop → `free_pool_source_suspended` and sources |
| Stream backlog | `XLEN stream:proxy:telemetry` | keeps growing → consumer lag |
| Landing | `SELECT count() FROM proxy.proxy_telemetry_log` | frozen → `[ChSink]` logs |

### 5. Quick troubleshooting

- Global 503: pool quarantined empty or mocks down;
- All 403: key unregistered (`default_key` is pre-registered);
- 402: tenant out of balance, top up to recover;
- Free zero-trust: no auth/cookie/payment/banking traffic; production advice `FREE_ENABLED=1 + FREE_REQUIRE_ELITE=1`;
- Windows redis-cli drops JSON quotes: use `log/redis_inject.py`.

### 6. FAQ

- **Q: Free pool staying at 0, normal?** A: Normal. Public free survival is tiny (~1 Elite per 100–200 raw in drills); pool is usually 0; paid lines unaffected.
- **Q: Gateway exits by itself?** A: Known Windows Pingora shutdown-path exit (`All runtimes exited`); restart recovers; production runs Linux.
- **Q: CH container unhealthy?** A: Known wget-probe artifact; trust `SELECT 1`.
- **Q: What P99 is normal?** A: Dev baseline ~65ms (Windows debug, not a production commitment); Linux acceptance is separate work.
