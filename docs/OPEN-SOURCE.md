# 相关开源文档 / Open Source

> 双语：中文在前，English after.
> 本文件记录“用了谁、什么协议、为什么选、合规注意”。许可证版本以 crates.io／各项目官网为准，下表为 2026-09-26 快照；升级依赖后请同步本表。

## 中文

### 1. Rust 依赖（`gateway/Cargo.toml`）

| Crate | 版本 | 许可证 | 用途＋选型一句话 |
|-------|------|--------|-----------------|
| pingora／pingora-core／pingora-proxy／pingora-load-balancing | 0.6 | Apache-2.0 | Cloudflare 高性能代理框架（0.4 在 rustc 1.93 下依赖冲突，故用 0.6） |
| tokio | 1 | MIT | 异步运行时（full 特性；同步 main＋block_on 装配，见 S1） |
| reqwest | 0.12 | MIT/Apache-2.0 | 出站 HTTP（rustls-tls＋json＋socks＋stream；VPN-IMMUNE 显式 `no_proxy()`） |
| redis | 0.26 | MIT | Stream/PubSub/会话/隔离（tokio-comp＋streams 特性） |
| clickhouse | 0.13 | MIT | 数仓落库（chrono 特性供 DateTime64；lz4 默认） |
| serde／serde_json | 1.0 | MIT/Apache-2.0 | 序列化（遥测 payload／快照） |
| nalgebra | 0.33 | Apache-2.0 | LinUCB d=4 向量运算 |
| governor | 0.6 | MIT | 租户 QPS/并发节流 |
| maxminddb | 0.32 | ISC | GeoLite2 读取（库文件运营方配给） |
| dashmap／arc-swap／parking_lot | 6.0／1.7／0.12 | MIT | 无锁并发结构（选路快照／会话表／统计） |
| http／bytes／rand／base64／sha2／chrono等 | 见 Cargo.lock | MIT/Apache-2.0 为主 | 基础库（exact 版本锁 Cargo.lock，构建可复现） |

### 2. 基础设施镜像（`docker-compose.yml`）

| 镜像 | 版本 | 许可证 | 说明 |
|------|------|--------|------|
| redis | 7-alpine | BSD-3-Clause | Stream＋PubSub＋会话；requirepass＋回环绑定 |
| clickhouse/clickhouse-server | 24-alpine | Apache-2.0 | 遥测数仓；90 天 TTL |
| prom/prometheus | v2.53.0 | Apache-2.0 | 15s 抓取＋5 告警规则 |
| grafana/grafana | 11.1.0 | **AGPL-3.0** | 7 面板；**未修改、compose 拉起即用**——AGPL 传染不触及本仓代码（独立进程＋HTTP API 交互），商用分发镜像时保留声明即可 |
| oliver006/redis_exporter（profile） | v1.66.0 | MIT | Stream 积压可观测（未跑时规则静默） |
| prom/alertmanager（profile） | v0.27.0 | Apache-2.0 | 告警投递骨架（receiver 仍 empty） |

### 3. 数据与库许可

- **MaxMind GeoLite2**（可选库文件，不在仓内）：CC BY-SA 4.0——使用需署名（OPERATION 有署名行），库更新脚本 `deploy/geoip_update.py` 需自备 license key（禁进仓）。
- **免费代理源数据**（Geonode/openproxylist/monosans/relayglass 等公网列表）：仅做连通性验证＋匿名度分级，不做归因承诺；遵守各源 robots/用量礼貌（分页上限、ETag 304、失败熔断）。

### 4. 本仓开源 posture

- **许可证**：MIT，Copyright (c) 2026 aitobyte（见 `LICENSE`）。
- **贡献**：见 `CONTRIBUTING.md`；大事记 `CHANGELOG.md`（Keep-a-Changelog）。
- **密钥红线**：真 Key/license 永不进仓库（`.env` 已忽略；CI 用占位；见 OPERATION §3）。提交前查 `body.txt` 类残留不入仓。
- **上游致谢**：Cloudflare Pingora、tokio、reqwest、Redis、ClickHouse、Prometheus、Grafana、MaxMind（GeoLite2 需署名）。

## English

### 1. Rust dependencies (`gateway/Cargo.toml`)

| Crate | Version | License | Use ＋ one-line rationale |
|-------|---------|---------|---------------------------|
| pingora／pingora-core／pingora-proxy／pingora-load-balancing | 0.6 | Apache-2.0 | Cloudflare high-performance proxy framework (0.4 conflicts under rustc 1.93, hence 0.6) |
| tokio | 1 | MIT | async runtime (full; sync main＋block_on assembly, see S1) |
| reqwest | 0.12 | MIT/Apache-2.0 | egress HTTP (rustls-tls＋json＋socks＋stream; explicit `no_proxy()` for VPN-IMMUNE) |
| redis | 0.26 | MIT | Stream/PubSub/session/quarantine |
| clickhouse | 0.13 | MIT | warehouse sink (chrono for DateTime64; lz4 default) |
| serde／serde_json | 1.0 | MIT/Apache-2.0 | serialization (telemetry payloads/snapshots) |
| nalgebra | 0.33 | Apache-2.0 | LinUCB d=4 vectors |
| governor | 0.6 | MIT | tenant QPS/concurrency throttling |
| maxminddb | 0.32 | ISC | GeoLite2 reads (DB file operator-supplied) |
| dashmap／arc-swap／parking_lot | 6.0／1.7／0.12 | MIT | lock-free concurrency |
| http／bytes／rand／base64／sha2／chrono etc. | see Cargo.lock | mostly MIT/Apache-2.0 | basics (exact versions pinned, reproducible builds) |

### 2. Infra images (`docker-compose.yml`)

| Image | Version | License | Notes |
|-------|---------|---------|-------|
| redis | 7-alpine | BSD-3-Clause | Stream＋PubSub＋sessions; requirepass＋loopback |
| clickhouse/clickhouse-server | 24-alpine | Apache-2.0 | warehouse; 90-day TTL |
| prom/prometheus | v2.53.0 | Apache-2.0 | 15s scrape＋5 alert rules |
| grafana/grafana | 11.1.0 | **AGPL-3.0** | 7 panels; **unmodified, used as-is via compose** — AGPL does not reach repo code (separate process＋HTTP API); keep notices when distributing |
| oliver006/redis_exporter (profile) | v1.66.0 | MIT | stream-lag observability (rules silent until it runs) |
| prom/alertmanager (profile) | v0.27.0 | Apache-2.0 | delivery skeleton (receiver still empty) |

### 3. Data & DB licensing

- **MaxMind GeoLite2** (optional DB file, not in repo): CC BY-SA 4.0 — attribution required when used (attribution line in OPERATION); updater needs your own license key (never committed).
- **Free proxy source data** (public lists): connectivity＋anonymity verification only, no attribution claims; respect robots/usage politeness (page caps, ETag 304, failure fuses).

### 4. This repo's posture

- **License**: MIT, Copyright (c) 2026 aitobyte (`LICENSE`).
- **Contributing**: `CONTRIBUTING.md`; changelog `CHANGELOG.md` (Keep-a-Changelog).
- **Secrets red line**: real keys/licenses never enter the repo (`.env` ignored; placeholders in CI; see OPERATION §3). Keep stray files like `body.txt` out.
- **Upstream thanks**: Cloudflare Pingora, tokio, reqwest, Redis, ClickHouse, Prometheus, Grafana, MaxMind (GeoLite2 needs attribution).

### Related docs / 相关文档

- 架构全景见 [`SYSTEM-ARCHITECTURE.md`](SYSTEM-ARCHITECTURE.md)；功能细节见 [`FEATURES.md`](FEATURES.md)；数据怎么流见 [`DATAFLOW.md`](DATAFLOW.md)；上手操作见 [`USER-GUIDE.md`](USER-GUIDE.md)。
