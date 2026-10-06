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

### Added

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

### Security

- **已知未修复**（门禁按设计报红，变绿唯一正道是升级，见
  `docs/SUPPLY_CHAIN.md` §3）：
  - RUSTSEC-2026-0033 / 0034（pingora-core 请求走私，代理热路径可达）
  - RUSTSEC-2026-0035（pingora-cache 缓存投毒，代码零引用）
  - RUSTSEC-2024-0437（protobuf 递归崩溃，经 prometheus 间接引入）
- 已修：yoke-derive 0.8.3 → 0.8.4（去掉 yanked 版本）。

---

## [0.1.0] - 2026-09-19 ~ 2026-10-06

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


<!-- 版本链接注记：本仓尚无 tag、无 GitHub Release（实测 git tag 为空），故不写 compare 链接，写了就是死链。第一次切 release（v0.2.0，随 pingora 升级）时再补上。 -->
