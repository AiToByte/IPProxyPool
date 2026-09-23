# IPProxyPool 企业级高可用智能 IP 代理池与网关

# IPProxyPool — Enterprise HA Smart IP Proxy Pool & Gateway

> 双语：中文在前，English after. Bilingual: Chinese first, English second.
> Enterprise high-availability smart IP proxy pool and gateway built on Cloudflare Pingora (Rust).

## 中文简介

IPProxyPool 是基于 Pingora 的企业级代理网关：智能选路（LinUCB＋加权＋粘滞＋隔离）、双轨自愈（遥测→熔断→隔离）、免费第二供应线（抓取→两级质检→合并）、SOCKS 出站、租户计费、全链路可观测（Prometheus＋Grafana＋ClickHouse 数仓）。

- 数据面：`:8080`（Pingora 五阶段：鉴权→选路→转发→计量→遥测），P99 dev 基线约 65ms（Windows debug 参考值，非生产承诺）。
- 控制面：prober/prewarmer/sweep/arbitrage/free_pool 后台 ticker＋supervisor 托管。
- 质量：156 单测＋4 真 live＋release bandit 8 臂 <200ns＋curl 全回归＋韧性演练（Redis/CH/Mock 断电自愈）。

文档：[`docs/ARCHITECTURE.md`](docs/ARCHITECTURE.md)（架构）· [`docs/TECHNICAL.md`](docs/TECHNICAL.md)（技术）· [`docs/COMPONENTS.md`](docs/COMPONENTS.md)（组件）· [`docs/USER_MANUAL.md`](docs/USER_MANUAL.md)（用户手册）· [`docs/OPERATION.md`](docs/OPERATION.md)（运维）· [`CONTRIBUTING.md`](CONTRIBUTING.md)（贡献）。

### 快速开始（5 步，详见用户手册）

```powershell
docker compose up -d                                        # Redis/CH/Prom/Grafana
docker exec ipproxy-redis redis-cli ping                    # PONG
cargo build --manifest-path gateway/Cargo.toml
D:\DevSoft\Conda\Miniconda3\python.exe log/launch_detached.py .\gateway\target\debug\pingora-proxy-gateway.exe log/gw.out log/gw.err
curl.exe --max-time 5 -s -o NUL -w "gw:%{http_code}\n" http://127.0.0.1:8080/   # 200
```

### 核心配置摘要（全表见技术说明 §6）

| 场景 | 环境变量 |
|------|----------|
| 开免费线 | `FREE_ENABLED=1`（默认关；生产建议再加 `FREE_REQUIRE_ELITE=1`） |
| 无头鉴权 | `REQUIRE_API_KEY=1`（无 `X-API-Key` 直接 403） |
| 改监听 | `GATEWAY_ADDR`／`METRICS_ADDR` |
| GeoIP 配库 | `GEOIP_MMDB_PATH`（无库 Disabled；执法 `GEOIP_ENFORCE_MISMATCH` 默认关） |

### 路线图

- 待输入：三家真 Key 灰度（staging 1%）、Linux 50k 性能验收；
- 延续 OUT：uTLS/eBPF/io_uring（见 `docs/SPIKE_R2.md`）；
- 开源：MIT（`LICENSE`，版权 aitobyte）。

## English

IPProxyPool is a Pingora-based enterprise proxy gateway: smart routing (LinUCB + weighted + sticky + quarantine), dual-track self-healing (telemetry → breaker → quarantine), a free second supply line (fetch → two-stage verify → merge), SOCKS egress, tenant billing, and full observability (Prometheus + Grafana + ClickHouse warehouse).

- Data plane: `:8080` (Pingora five phases: auth → route → forward → meter → telemetry); P99 dev baseline ~65ms (Windows debug reference, not a production commitment).
- Control plane: prober/prewarmer/sweep/arbitrage/free_pool background tickers, supervised.
- Quality: 156 unit tests + 4 live + release bandit 8-arm <200ns + full curl regression + resilience drills (Redis/CH/Mock outage self-healing).

Docs: [`docs/ARCHITECTURE.md`](docs/ARCHITECTURE.md) · [`docs/TECHNICAL.md`](docs/TECHNICAL.md) · [`docs/COMPONENTS.md`](docs/COMPONENTS.md) · [`docs/USER_MANUAL.md`](docs/USER_MANUAL.md) · [`docs/OPERATION.md`](docs/OPERATION.md) · [`CONTRIBUTING.md`](CONTRIBUTING.md).

### Quickstart (5 steps, see user manual)

```powershell
docker compose up -d                                        # Redis/CH/Prom/Grafana
docker exec ipproxy-redis redis-cli ping                    # PONG
cargo build --manifest-path gateway/Cargo.toml
D:\DevSoft\Conda\Miniconda3\python.exe log/launch_detached.py .\gateway\target\debug\pingora-proxy-gateway.exe log/gw.out log/gw.err
curl.exe --max-time 5 -s -o NUL -w "gw:%{http_code}\n" http://127.0.0.1:8080/   # 200
```

### Core config cheatsheet (full table: technical §6)

| Scenario | Env vars |
|----------|----------|
| Enable free line | `FREE_ENABLED=1` (off by default; production advice adds `FREE_REQUIRE_ELITE=1`) |
| Headless auth | `REQUIRE_API_KEY=1` (headerless → 403) |
| Listen addrs | `GATEWAY_ADDR` / `METRICS_ADDR` |
| GeoIP DB | `GEOIP_MMDB_PATH` (Disabled without DB; enforcement `GEOIP_ENFORCE_MISMATCH` off) |

### Roadmap

- Blocked on input: three-vendor real-key gray release (staging 1%), Linux 50k performance acceptance;
- Staying OUT: uTLS/eBPF/io_uring (see `docs/SPIKE_R2.md`);
- License: MIT (`LICENSE`, copyright aitobyte).
