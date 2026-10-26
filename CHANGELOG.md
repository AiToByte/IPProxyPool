# Changelog

> 版本政策：本仓遵循 [语义化版本 2.0.0](https://semver.org/lang/zh-CN/)。
> `0.x` 阶段 minor 即 breaking（按 semver 规定），1.0 之前不承诺 API 稳定。
>
> - **何时 bump**：破坏性变更 → minor+1（如 0.1.0 → 0.2.0）；新功能（兼容）→
>   minor+1 同样处理（0.x 无 patch 位可用）；纯修复 → patch+1。
>   0.x 阶段不区分 minor/patch 的兼容语义，统一按"是否破坏"决定。
> - **谁来 bump**：改 `gateway/Cargo.toml` 的 `version` **同时**在本文件加
>   `## [x.y.z]` 小节（由 `tools/check_changelog.py` 在 CI 强制，缺小节即红）。
> - **tag 规则**：bump 提交后打 `vX.Y.Z` tag（如 `v0.2.0`）并推送。
>   本仓此前从未切过 release、无 tag（实测 `git tag` 为空），故历史版本
>   全部记在 `## [0.1.0]` 下，不补 tag、不伪造 release 记录。
> - **下一次 bump**：pingora 0.6 → 0.9 框架升级落地时 → `0.2.0`
>   （见 `docs/SUPPLY_CHAIN.md` §3）。

格式遵循 [Keep a Changelog](https://keepachangelog.com/zh-CN/1.1.0/)。
`Unreleased` 节记录已合并但未发布的内容；发布时将其改名为版本号。

---

## [Unreleased]

### Security

- 升级 `rustls-webpki 0.103.15→0.103.16`（RUSTSEC-2026-0382：证书校验可能接受
  不受信任的中间证书）与 `webpki-roots 1.0.9→1.0.10`（RUSTSEC-2026-0381：
  内置根证书信任了已撤销的 emSign 中间证书）。仅改 `Cargo.lock`，无 API 变更；
  本机 `cargo audit --no-fetch` 由 2 vulnerabilities 归零，326 单测/clippy 仍绿。

### Fixed

- **Admin API 的「声明即失实」**（OPT-R17 复核实证）：`gateway/Cargo.toml` 声明
  `tower-http` 的 `cors`/`trace`/`timeout` 三个 feature，而 `src/` 全仓
  **零引用**；`admin.rs::router` 的文档注释更声称「含 CORS + 超时」——
  **两者都不存在**。三重后果：读者以为跨域已放开（实则 UI 只能靠 Vite
  proxy/同域反代）、以为有请求超时（实则慢请求可长期占满连接）、以为有
  访问审计（实则改权重/隔离/余额这类写操作**无任何轨迹**）。
  处置：
  - **真正补上 `TimeoutLayer`**（`ADMIN_TIMEOUT_SECS = 30`，套在认证中间件
    外层，使未认证的挂起请求同样受约束），新增 3 个测试锁层序与错误体契约；
  - **新增 `audit_logging` 中间件**（用 `log` 而非 `TraceLayer`：后者发的是
    `tracing` 事件，本项目装的是 `env_logger`，无 subscriber ⇒ 会得到
    「加了审计却一条不打印」的静默失效）。记录方法/路径/状态码/耗时，
    4xx/5xx 走 `warn`。**只记是否带 Key，绝不记值**（日志会进 `log/` 并被
    备份，写入密钥等于新建泄漏路径）；
  - **移除 `cors` 与 `trace` feature**，让依赖清单与真实代码一致。
    不补 CORS 是安全选择：`X-Admin-Key` 头认证 + permissive CORS 会让
    任意站点的 JS 都能发起带凭据的跨域请求。
  - 副作用：Cargo.lock 中 `tower-http` 的 `tracing` 传递依赖随之移除
    （实证 `tracing` 本就是死重量，非本仓其它代码所需）。
- **告警投递链路的注释与实际相反**（OPT-R17）：`docker-compose.yml` 曾注释着
  「取消注释＋重启 prom 即投递」并挂载 `alertmanager.yml` 到 prometheus
  容器——**该挂载无效且多余**（Prometheus 不读它；收件人配置由 Alertmanager
  自己读 `/etc/alertmanager/alertmanager.yml`）。误导性注释与挂载均已移除。
  澄清：`prometheus.yml` 的 `alerting:` 块**早已接好**，从不需要改动。
- **Alertmanager 默认不启动**：此前 `profiles: ["observability"]` 使
  `docker compose up -d` 不起它，而 `prometheus.yml` 的 `alerting:` 块
  无条件指向 `alertmanager:9093` ⇒ 默认部署下告警链路是断的。已移出
  profile，与 Redis/Prom 同级默认启动。结果是**链路默认可达、投递默认关闭**
  （receiver 仍是刻意的 `empty`）：运维可在 Alertmanager UI 分组/silence/ack，
  而非只能去 Prometheus UI 干看。

### Added

- 第 15 道静态门 `tools/check_admin_routes_documented.py`：双向校验
  `gateway/src/admin.rs` 的 `.route(...)` 与 `docs/ADMIN_API.md` 的
  `### METHOD` 标题，防「代码加了路由文档没写（客户端 404）」与「文档写了
  代码没注册（调用必失败）」。只做结构化比对，**不做全文 grep**，避免说明
  文字/curl 示例让门假绿。变异自测：注入未文档化路由→红；文档写 ghost
  端点→红；路由只出现在注释→绿。
- **控制面节点热更新（步骤 52）**：`POST /api/v1/nodes` /
  `DELETE /api/v1/nodes/{addr}` 运行时增删托管节点，`PATCH .../weight`
  对托管节点同步落盘；`NODES_FILE`（缺省 `data/nodes.json`）用
  临时文件+fsync+rename 原子替换，文件存在即以文件为准（损坏拒绝启动），
  `NODES_FILE=""` 关闭持久化。`RouterEngine` 新增池写串行锁，修复
  FreePool merge 与 Admin 调权/增删并发时的 lost update；同时修复
  `scale_vendor_weights` 漏重建 buckets（factor=0 摘除对桶快路径无效）。
  Admin UI 节点页新增表单与删除；compose 暴露 `:9092` 并挂
  `gateway-data` 卷。Admin API 错误映射补 409/500；新增 20 个单测
  （306→326）与真实二进制 E2E（新增→重启仍在→删除→重启仍不在）。
- `docs/OPERATION.md` 新增「§4.1 告警投递（Alertmanager）」小节：
  写明三段链路各自的**真实状态**（Prometheus→Alertmanager ✅ 已接线、
  Alertmanager 服务 ✅ 默认起、Alertmanager→人 ❌ `receiver: "empty"`
  故不外发），并附 4 条常见误解澄清。此前 27KB 运维手册**全文不出现
  "alertmanager"**，运维读手册无法得知告警不会送达任何人。
- 第 13 道静态门 `tools/check_observability_truth.py`：锁两类「文档/配置
  声称存在、代码里不存在」的失实（依赖 feature 未被引用；告警投递未文档化）。
  **双向验证**：注入 `features = ["cors"]` → 判红指出 `CorsLayer` 缺失；
  删掉 OPERATION.md 告警小节 → 判红；还原 → 绿。
  过程中踩中一个值得记录的陷阱：门最初按「全文 grep 符号」实现，结果被
  **自己的说明注释**满足（注释里为解释「为什么不加 CORS」写了
  `CorsLayer::permissive()` 字样）而**假绿** —— 这比「门失效」更糟，它让门
  看起来在工作。修正为**剥离注释后**再检索。
- 第 14 道静态门 `tools/check_no_bom.py`：要求**配置类文件不得带 UTF-8 BOM**
  （与 `check_ps1_bom.py` 恰好反向，故独立成文件而非加分支）。实证缺陷：
  `gateway/Cargo.toml` 带 BOM ⇒ `Swatinem/rust-cache` 解析 manifest 失败、
  回退到**整文件缓存**，静默丢掉按 `Cargo.lock` 哈希的细粒度 key
  （OPT-R9 A4 引入 rust-cache 的初衷）。Cargo 本身容忍 BOM，所以**构建与
  测试全绿、只是缓存悄悄退化** —— 这类「不失败只变慢」的问题最难发现。
  **非本轮引入**：`70792ee`（本轮动手前的 HEAD）即已带 BOM。双向验证：
  加回 BOM → 判红；剥离 → 绿。

## [0.2.0] - 2026-10-17

### Added

- Admin Web UI（`web/admin` SPA，对齐 `docs/ADMIN_UI.md`）：登录 + 仪表盘/节点池/
  租户与计费/免费池/断路器/设置 六页（admin 契约同批收口，见 [0.1.0] Fixed）。
- 供应链治理门禁：`gateway/deny.toml`（advisories/bans/licenses/sources）、
  `tools/check_supply_chain.py`（cargo-deny + cargo audit 双跑）、
  CI `supply-chain` job、`docs/SUPPLY_CHAIN.md`（首次评估与 triage）。
- 端到端吞吐压测工具：`tools/bench/main.go`（Go 客户端+上游）、
  `tools/bench_gateway.py`（一键编排）、`docs/PERF_BASELINE.md`
  （峰值 17,215 QPS @ 100%，持续 10k 整形）。
- 文档一致性门覆盖 `docs/CAPABILITY-AUDIT.md`、`docs/SUPPLY_CHAIN.md`
 （ALLOWLIST + EXPECTED_KEYS 双表同步自检）。
- CI shell 兼容门：`tools/check_ci_shell_compat.py`
 （`shell: powershell` 的 run 块必须纯 ASCII，防 em dash 截断事故）。

### Fixed

- CI 全部 7 个失败点（R16-A 起 GitHub Actions 首次全绿，之前从未绿过）：
  .NET SDK 门（csproj 用 C# 注释致 MSB4025）、release bandit 绝对纳秒断言、
  PSScriptAnalyzer 位置参数、PS 5.1 parse 非 ASCII 截断、Docker 缺 cmake、
  零分配契约 glibc 平台差异。
- `free_pool.rs` 客户端缓存淘汰：`Instant` 基准过近时
  `checked_sub` 返 None 导致全清（改用相对时长判据）。
- OPT-R11 A1 红测的 profile 依赖（release 下稳定失败，改 profile 无关断言）。

### Changed

- 依赖升级：pingora 0.6 → 0.9.0（umbrella/core/proxy/load-balancing 四 crate
  同步，src 零代码改动）+ rustls 0.23.45 → 0.23.46（patch）。升级决策台账见
  `docs/SUPPLY_CHAIN.md` §9；端到端压测复跑 3 轮 PEAK
  12,739 / 19,819 / 19,786 QPS（conc=4，100%），与首测锚点
  17,215 QPS（`docs/PERF_BASELINE.md`）同量级，无吞吐回退；持续负载被
  默认租户 10,000 QPS 配额门整形为 ~10k（429，预期门控行为）。

### Security

- RustSec 漏洞公告 8 → **0**（deny advisories ok + audit exit 0 交叉验证）：
  RUSTSEC-2026-0033/0034（pingora-core HTTP 请求走私，critical）、
  RUSTSEC-2026-0035（pingora-cache 缓存投毒，high）、
  RUSTSEC-2026-0346~0349（rustls TLS 秘密暴露/降级/资源放大，medium/low）、
  RUSTSEC-2024-0437（protobuf 递归崩溃，经 prometheus 0.13 传递链）。
  处置为**升级而非 ignore**（pingora 0.6 → 0.9.0 + rustls 0.23.46，
  见 Changed；台账与验收见 `docs/SUPPLY_CHAIN.md` §9）。
- 残余 4 个传递依赖 unmaintained（No safe upgrade is available）：以
  `deny.toml unmaintained = "workspace"` 挡直依（直依 unmaintained 一律
  error、升级即红，传递仅 warn 可见）；不逐条 ignore，`ignore` 保持空。
- 已修：yoke-derive 0.8.3 → 0.8.4（去掉 yanked 版本）。

---

## [0.1.0] - 2026-09-19 ~ 2026-10-17

> 本节汇总仓库创建以来的全部工作（此前无 release、无 tag、无 changelog，
> 故按提交历史如实归纳，不拆分虚构的中间版本）。

### Added

- GW-R1 网关五模块 + LinUCB 自学习选路 + 会话粘滞 + 多租户限流。
- FreePool v2 免费代理供给（多源抓取、分级验证、匿名度分级、GeoIP）。
- Phase 2 SOCKS 出站（SOCKS4/5 握手、443 TLS）。
- Phase 3 画像与学习（R13 LinUCB 永久锁死修复、分档探索配额）。
- Phase 4 合规开关与 GeoIP 运维（D3 Key 门默认开、监听收环）。
- Phase 5 韧性演练（遥测落地端健康三指标 + 3 告警、断电自愈）。
- Windows 300 秒自杀修复（OPT-R14，pingora-core 0.6.0 兼容绕过）。
- 5 语言 SDK（Python/Go/Java/Node/.NET）+ 错误契约统一。
- 12→13 个 Python 静态门（全部进 CI，多数做过双向验证）。
- 31 个指标族 + 15 条 Prometheus 告警 + Grafana provisioning。
- 能力复核报告（`docs/CAPABILITY-AUDIT.md`，含首版 6 处错误更正）。

### Fixed

- 免费档零信任（`tier=free` + 凭据头 → 403）。
- 隔离三层（内存 → Redis SETEX → PubSub 扇出）。
- MaxMind 署名合规、文档漂移门、API Key 卫生。
- Admin API 契约收口（GW-5，对齐 docs/ADMIN_API.md §4）：`/nodes`、`/tenants`
  回归文档承诺的**裸数组**；PATCH balance 真实落库（原为只改本地副本的假实现），
  DELETE quarantine 真实解除（原为空操作）；全部非 2xx 响应统一为 `{error, code}`
  JSON（401/404/400，含未知路径 fallback）；非有限余额（NaN/±Inf）拒绝为 400。

> 本窗口（2026-10-17）内新增的 pingora 0.6 → 0.9.0 升级、RustSec 漏洞
> 8 → 0 清零与压测复跑记录，按本文件"Unreleased 节记录已合并未发布内容"
> 的政策放在 [Unreleased]（下次 bump 0.2.0 时并入版本节）。

<!-- 版本链接注记：本仓尚无 tag、无 GitHub Release（实测 git tag 为空），故不写 compare 链接，写了就是死链。第一次切 release（v0.2.0，随 pingora 升级）时再补上。 -->
