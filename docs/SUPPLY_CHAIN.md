# 供应链基线 / Supply-Chain Baseline

> 首次评估：2026-10-06（OPT-R16 C）。工具：cargo-deny 0.20.2 + cargo-audit 0.22.2。
>
> 门禁：`python tools/check_supply_chain.py`（deny 全量 + audit 独立扫描），
> 已接入 CI（`supply-chain` job）。策略见 `gateway/deny.toml`（每项有 rationale）。
>
> 本文件记录**首次评估的全部发现与 triage 结论**。数字会变，
> 但"每个发现都有处置结论"这条纪律不变。

---

## 1. 结论（先说）

| 类别 | 数量 | 处置 |
| --- | --- | --- |
| 漏洞（vulnerability） | **4** | 需升级 pingora 0.6 → 0.9（R17 级工作，见 §3） |
| 无维护（unmaintained） | 7 | 全部为传递依赖，pingora 升级后重 triage（见 §4） |
| Unsound（audit 补充） | 3（atty、lru×2） | 同上，随升级重 triage |
| Yanked | 1（yoke-derive 0.8.3） | **已修**：`cargo update -p` 到 0.8.4 |
| 重复版本 | 20 个包 | warn 可见，暂不收紧（见 §5） |
| 许可证 | 364 crate 全宽松 | allowlist 已锁定，新增即红（见 §6） |
| 依赖来源 | 全部 crates.io | sources 门已锁定 |

**当前门禁状态：红。** 这是门禁在正常工作——它在我们的核心框架里发现了
真实漏洞（其中 2 个打在代理热路径上）。**不要**为了变绿而加 ignore；
变绿的唯一正道是升级（§3）。

---

## 2. 漏洞明细（双工具交叉验证一致）

| Advisory | 包 | 本仓版本 | 修复版本 | 标题 |
| --- | --- | --- | --- | --- |
| RUSTSEC-2026-0035 | pingora-cache | 0.6.0 | ≥0.8.0 | Cache poisoning via insecure-by-default cache key |
| RUSTSEC-2026-0033 | pingora-core | 0.6.0 | ≥0.8.0 | HTTP Request Smuggling via Premature Upgrade |
| RUSTSEC-2026-0034 | pingora-core | 0.6.0 | ≥0.8.0 | HTTP Request Smuggling via HTTP/1.0 and Transfer-Encoding Misparsing |
| RUSTSEC-2024-0437 | protobuf | 2.28.0 | ≥3.7.2 | Crash due to uncontrolled recursion |

依赖链（`cargo tree -i` 实证）：
- protobuf@2.28.0 ← prometheus@0.13.4 ← pingora-core@0.6.0
- 三个 pingora advisory 直接打在 0.6.0 本体

**结论：四个漏洞一次框架升级全解**（pingora 0.6 → 0.8+ 会连带换掉
prometheus/protobuf 链）。但 0.x 的 minor 升级即 breaking change，
见 §3 评估。

---

## 3. pingora 升级评估（R17 级工作，不在本轮范围）

### 3.1 可达性分析（决定能否 ignore）

| 漏洞 | 是否可达 | 结论 |
| --- | --- | --- |
| pingora-core 请求走私 ×2 | **100% 可达**——我们就是代理，HTTP 解析/转发是热路径 | **不可 ignore，必须升级** |
| pingora-cache 缓存投毒 | 代码零引用 `pingora_cache`，Cargo 无 cache 特性 | 理论不可达，但版本旗标仍红；随升级解决，不单独 ignore |
| protobuf 递归崩溃 | 代码零直接引用（命中全是 "protocol" 子串误报）；经由 prometheus 间接引入 | 同上，随升级解决 |

**判定：升级不可绕过。** 两个走私漏洞打在代理核心攻击面上，
任何 ignore 都是不诚实的。

### 3.2 升级成本面

- 可选目标：0.8.0（2026-03，最小修复版）/ 0.8.1 / **0.9.0**（2026-09，最新）。
  倾向 0.9.0（除非它自身有 advisory，升级时复查）。
- 我方 API 面小（7 个导入点）：RequestHeader/ResponseHeader、Error/Result、
  Opt/Server、HttpPeer、ProxyHttp/Session（仅 new_ctx/request_filter/
  upstream_peer/upstream_request_filter/fail_to_connect/logging）。
- **最大风险**：`main.rs:220` 的 Windows workaround 直引
  `pingora-core 0.6.0` 的 `Server::run()` 内部实现，而 0.8/0.9 恰恰重做了
  优雅关闭（changelog 实证）。workaround 可能失效或变得多余，
  必须重做 300s+ 存活实测（OPT-R14 的方法）。
- 次风险：0.9 把 Prometheus 拆成独立 crate（我方 metrics 是手写，
  未用其集成，应无影响，升级时验证）；连接池重构（HttpPeer 行为可能变）。

### 3.3 升级验收标准（届时执行）

1. 编译通过 + 全量门禁绿（284 单测、clippy、13 门）。
2. Windows 300s+ 存活实测（OPT-R14 方法复用）。
3. 端到端压测复跑（`tools/bench_gateway.py`），QPS 不低于本次基线。
4. `cargo deny check` + `cargo audit` 全绿（含 unmaintained 重 triage）。

---

## 4. 无维护包 triage（7 个，全部传递依赖）

| 包 | 版本 | 引入方 | 处置 |
| --- | --- | --- | --- |
| atty | 0.2.14 | （传递，经 clap/env_logger 链） | 随升级重 triage |
| daemonize | 0.5.0 | （传递） | 同上 |
| derivative | 2.2.0 | （传递） | 同上 |
| paste | 1.0.15 | （传递） | 同上 |
| proc-macro-error | 1.0.4 | （传递） | 同上 |
| rustls-pemfile | 2.2.0 | （传递，经 reqwest/hyper-rustls 链） | 同上；注意后继是 rustls-pki-types，升级时确认是否已切换 |
| yaml-rust | 0.4.5 | （传递） | 同上 |

原则：传递依赖的 unmaintained **不逐个 ignore**（ignore 是显式风险接受，
7 个全加等于宣布"我们接受 7 个无人维护的包"，而不去修）。
正确顺序是先升级直接依赖（pingora），新依赖树大概率甩掉其中几个，
剩下的再逐个处理（升级直接依赖 / 换替代品 / 写理由 ignore）。

补充（audit 独立发现，deny 未单列）：atty 另有 unsound（unaligned read，
RUSTSEC-2021-0145）、lru@0.14.0 有 2 个 unsound（经 pingora-cache/pool 引入）。
同样随升级重 triage。

---

## 5. 重复版本（20 个包，warn）

syn（3 个）、hashbrown（4 个）、getrandom（3 个）、windows-sys（3 个）等，
均为传递依赖 pin 住不同 major。`bans.multiple-versions = warn`，
**不设 skip**——warn 日志留痕但不阻断。

收紧条件（满足任一即改 deny）：直接依赖全部收敛到单一 major；
或某个重复包出现漏洞（届时必须升级，顺带收敛）。

---

## 6. 许可证（allowlist 已锁定）

2026-10 实测 364 个 crate：MIT / Apache-2.0 / BSD / ISC / Zlib /
Unlicense / CC0-1.0 / MIT-0 / BSL-1.0 / Unicode-3.0 / CDLA-Permissive-2.0，
**无 GPL/AGPL/LGPL**。`gateway/deny.toml` 的 allowlist 覆盖全部原子，
新增未知许可证即红。

---

## 7. 过程记录（本轮踩坑，勿重犯）

1. **cargo-deny 0.20 改了 schema**（PR #611）：`[advisories]` 没有
   vulnerability/yanked/unmaintained/unsound 键，`[licenses]` 没有
   unlicensed/deny/copyleft 键——写了就直接拒绝运行（unexpected-value /
   deprecated 错误，行号精确）。不要凭旧文档写配置，用
   `cargo deny init` 生成模板对照。本轮连踩两次才对。
2. **新模型更严**：advisories 靠 ignore 控制（命中即失败，无 warn 级别）；
   licenses 纯 allowlist（不在列即失败）。"先 warn"的策略在此版本下
   不存在——看到就修，修不了才 ignore+理由。
3. **`cargo-audit` 必须调子命令形式**（`cargo audit`），裸跑 `cargo-audit`
   只打印 help 就退出，门禁会误判为通过（已修，见 check_supply_chain.py 注释）。
4. **rust-toolchain.toml pin 导致 hang**：pin 到未安装的 1.93.0 会触发
   rustup 全量下载，本机网络下直接 hang 住所有 cargo 调用。已撤回，
   改用文档记录验证版本。教训：pin 工具链前先确认目标环境已预装。
5. **子进程编码**：`text=True` 在中文 Windows 按 GBK 解码，cargo 输出含
   本地化字节即炸（`UnicodeDecodeError` 在 `_readerthread` 里吞掉真实结果）。
   一律 `encoding="utf-8", errors="replace"`（本轮在 check_jvm_sdk.py 修过
   同一类，这次直接写对）。

---

## 8. 相关文件

- `gateway/deny.toml` — 四类门禁策略（每项有 rationale）
- `tools/check_supply_chain.py` — 门禁脚本（deny + audit 双跑）
- `.github/workflows/ci.yml` — `supply-chain` job
- [`QUALITY_BASELINE.md`](QUALITY_BASELINE.md) — 测试数/门数权威源
