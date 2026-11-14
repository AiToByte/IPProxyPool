# 供应链基线 / Supply-Chain Baseline

> 首次评估：2026-10-06（OPT-R16 C）。工具：cargo-deny 0.20.2 + cargo-audit 0.22.2。
>
> 门禁：`python tools/check_supply_chain.py`（deny 全量 + audit 独立扫描），
> 已接入 CI（`supply-chain` job）。策略见 `gateway/deny.toml`（每项有 rationale）。
>
> 本文件记录**首次评估的全部发现与 triage 结论**。数字会变，
> 但"每个发现都有处置结论"这条纪律不变。
>
> **2026-10-17 更新**：pingora 升级已执行并重 triage（§9），当前门禁**绿**。
> 下表为最新状态；首次评估历史按原文保留。

---

## 1. 结论（先说）

| 类别 | 数量 | 处置 |
| --- | --- | --- |
| 漏洞（vulnerability） | **4**（全部显式豁免） | 均为 **zstd 生态**（RUSTSEC-2026-0391/0398/0403/0410），**不可达 + 上游无补丁**，逐条论证见 §10。此前的 8 个已由 pingora 0.9.0 + rustls 0.23.46 清零（§9） |
| 无维护（unmaintained） | 4（首次 7） | 全部传递依赖、上游无修复版；deny 策略 `unmaintained = "workspace"`（直依即红，见 §4/§9） |
| Unsound（audit 补充） | 0（首次 3） | atty/lru 已随 pingora 升级退出依赖树 |
| Yanked | 1（yoke-derive 0.8.3） | **已修**：`cargo update -p` 到 0.8.4 |
| 重复版本 | 17 个包（首次 20） | warn 可见，暂不收紧（见 §5） |
| 许可证 | 373 package 全宽松（首次 364） | allowlist 已锁定，新增即红（见 §6） |
| 依赖来源 | 全部 crates.io | sources 门已锁定 |

**当前门禁状态：绿**（2026-10-17，deny 全量 + audit exit 0，含 rustls 等公告
DB 刷新后复扫）。首次评估时门禁为红——那是门禁在正常工作：它在核心框架里
发现了真实漏洞（2 个打在代理热路径上），处置是升级而非 ignore（§3 判定，
§9 执行）。后续任何变红都同理：**不要**为了变绿而加 ignore。

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

1. 编译通过 + 全量门禁绿（421 单测、clippy、22 门）。
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

## 5. 重复版本（首次 20 个包 → 2026-10-17 复测 17 个，warn）

×3：getrandom、hashbrown、syn、windows-sys；×2：cpufeatures、dashmap、
heck、indexmap、r-efi、rand、rand_core、socket2、synstructure、thiserror、
thiserror-impl、tower-http、zstd、zstd-safe——均为传递依赖 pin 住不同 major。
`bans.multiple-versions = warn`，**不设 skip**——warn 日志留痕但不阻断。

收紧条件（满足任一即改 deny）：直接依赖全部收敛到单一 major；
或某个重复包出现漏洞（届时必须升级，顺带收敛）。

---

## 6. 许可证（allowlist 已锁定）

首次评估 364 个 crate；2026-10-17 pingora 0.9 升级后复测 373 个 package：
MIT / Apache-2.0 / BSD / ISC / Zlib / Unlicense / CC0-1.0 / MIT-0 / BSL-1.0 /
Unicode-3.0 / CDLA-Permissive-2.0，**无 GPL/AGPL/LGPL**。`gateway/deny.toml`
的 allowlist 覆盖全部原子（升过程中 deny licenses 门始终绿），新增未知许可证即红。

---

## 7. 过程记录（本轮踩坑，勿重犯）

1. **cargo-deny 0.20 改了 schema**（PR #611）：`[advisories]` 删除的键是
    vulnerability/unsound/notice/severity-threshold（写了就直接拒绝运行，
    unexpected-value / deprecated 错误，行号精确）；`[licenses]` 没有
    unlicensed/deny/copyleft 键。不要凭旧文档写配置，用
    `cargo deny init` 生成模板对照。本轮连踩两次才对。
    **（2026-10-17 更正）**：经官方文档核实，yanked 与 unmaintained 键在 0.20
    并未删除，只是取值变更：`yanked = deny|warn(默认)|allow`、
    `unmaintained = all(默认)|workspace|transitive|none`（取值为"作用域"而非
    lint 级别）。当时把 lint 级别值写进去报 unexpected-value，误判为键不存在。
2. **新模型对漏洞类更严**：vulnerability 无条件 error（无 warn 级别）；
    licenses 纯 allowlist（不在列即失败）。传递依赖的 unmaintained 可用
    `unmaintained = "workspace"` 一并覆盖（直依仍 fail、传递仅 warn），
    比 4 条逐一 ignore 更可维护（2026-10-17 采纳，见 §9）。
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

---

## 9. pingora 升级重 triage（2026-10-17，§3 预判的升级已执行）

**触发**：门禁本轮 audit DB 刷新（`~/.cargo/advisory-db` 拉到 1334 条），
漏洞从首次评估的 4 个变 **8 个**——新增 4 个 rustls 公告
（RUSTSEC-2026-0346~0349，2026-10-14 发布，评估时还不存在）。按 §3 判定走
升级路线（**不加 ignore**）。

**执行**（均为 lock/manifest 层面，无 src 代码改动）：

| 变更 | 说明 |
| --- | --- |
| pingora 0.6 → **0.9.0** | 4 个 pingora crate 同步（umbrella/core/proxy/load-balancing） |
| rustls 0.23.45 → **0.23.46** | `cargo update -p rustls` patch 升级，解 4 个 rustls 公告 |

选 0.9 而非 0.8 的决定性理由：**0.8.1 仍依赖 `prometheus ^0.13`**，protobuf
2.28（RUSTSEC-2024-0437）还在依赖树里；0.9.0 起 pingora-core 把 Prometheus
拆为独立可选 crate，protobuf 整体退出依赖树。顺带 `daemonize`→`daemonix`
Linux 分支（消 1 个 unmaintained）。

**API 兼容性实证**：我方 API 面本就极小（§3.2 列的 7 个导入点），header
写操作全走显式 API（`insert_header`/`remove_header`），不触 0.9 移除的
`DerefMut`；`prometheus` 集成、`PeerOptions::curve`、`async_write_vec`
均未使用。`cargo check` 一次通过，**src 零改动**。

**结果**（复跑门禁全绿）：

- 漏洞：**8 → 0**（deny advisories ok + audit exit 0 交叉验证）
- unmaintained 警告：10 → **4**（atty/daemonize/proc-macro-error/yaml-rust
  随升级退出树；atty unsound + lru×2 unsound 同步消失）
- 重复版本：20 → **17** 个包；许可证 allowlist 覆盖 373 个 package（门未动即绿）
- `cargo fmt`/`clippy -D warnings`/`test --locked`（**338 passed / 8 ignored**）/
  `bench --no-run` 全绿

**deny.toml 策略配套变更**（非放行，是策略落地）：

1. `[advisories] unmaintained = "workspace"`——剩 4 个 unmaintained 全为
   **传递依赖**且 "No safe upgrade is available"（derivative←pingora-core、
   paste←simba←nalgebra、rustls-pemfile←pingora-rustls、unsafe-libyaml←
   serde_yaml←pingora-core，路径见 `cargo tree -i` 实证），本仓 manifest 无
   升级路径；策略保证**直依 unmaintained 一律 error**（今后相关风险不会被此
   配置放过），传递仅 warn 可见。未逐条 ignore（符合 §4 原则）。
2. `ignore` 保持空 + 新增 `unused-ignored-advisory = "deny"`——健忘防护。
3. 修正 §7-1 的键位记录（见该处 2026-10-17 更正）。

**§3.3 验收标准对照（当场执行）**：

1. ✅ 编译 + 当时实测 `338 passed / 8 ignored` + clippy + fmt（见上；这是 2026-10-17 当时的实测值）。
2. ✅ Windows 存活实测：dev 实例（debug build）live E2E 全程存活 **397s**
    （2026-10-17 11:25:28Z 起至 11:32:05Z taskkill），期间 admin API 批次
    正常响应；P0 kill 演练（C1 口径）已于 2026-10-17 下午复跑通过——
    冷启动 self-heal + 两次强杀均重拉回 200 + 收摊零残留
    （见 EXEC_LOG 步骤 48 演练条目）；OPT-R14 原本项（>330s 长时存活）
    由步骤 39 t=348s/447s 实证覆盖。
3. ✅ 端到端压测复跑（`tools/bench_gateway.py --skip-build`，2026-10-17
    共 3 轮均 exit 0）：PEAK 依次 **12,739 / 19,819 / 19,786 QPS**
    （conc=4、100% 成功；p99 实测第 1 轮 1.5ms / 第 3 轮 1.0ms，
    第 2 轮未存档），
    与验收锚点 `docs/PERF_BASELINE.md` 首测
    **17,215 QPS @ conc=4** 同量级，无吞吐回退（burst 峰值单轮方差 ±30%
    量级，第 1 轮 12,739 属小样本 burst 方差；持续负载被默认租户
    10,000 QPS 配额门整形为 ~10k，429 属预期门控行为；地板
    156,287 / 163,727 / 166,373 QPS 远高于 e2e，链路非瓶颈）。
4. ✅ deny + audit 全绿（本节）。


---

## 10. 2026-11-01：zstd 生态 4 条公告的处置（**豁免**，不是修复）

### 10.1 为什么本轮才发现

`check_supply_chain.py` 此前每轮都因**拉不到 RustSec DB** 而跳过（离线），
所以这 4 条公告一直没有被检查到——门禁"绿"是因为没跑，不是因为没问题。
本轮 `cargo deny` 用**缓存**库跑通后才暴露。**这本身是一条教训**：
"跳过"与"通过"在报告里必须能区分（已改进门禁，见 §10.5）。

### 10.2 清单

| Advisory | 受影响符号 | 本仓版本 | 补丁版本 | 标题 |
| --- | --- | --- | --- | --- |
| RUSTSEC-2026-0391 | `zstd::stream::raw::{Encoder,Decoder}::with_context` | zstd 0.13.3 / 0.14.0 | zstd ≥0.14.3 | 复用上下文时废弃流的数据泄漏到下一个流 |
| RUSTSEC-2026-0398 | `zstd_safe::Seekable::decompress` | zstd-safe 7.x | zstd-safe ≥8.2.0 | 越界偏移时把 `Vec` 长度设到容量之外 |
| RUSTSEC-2026-0403 | `zstd_safe::DCtx::decompress_stream` | zstd-safe 7.x | zstd-safe ≥8.2.0 | 旧式帧流式解压失败时暴露未初始化内存 |
| RUSTSEC-2026-0410 | zstd-sys 的 wasm32 malloc/free shim | zstd-sys 2.x | zstd-sys ≥2.1.2 | 分配失败时破坏内存 |

依赖链（`cargo tree -i` 实证，两个 zstd 版本都在树里）：
- `zstd 0.13.3` ← `pingora-header-serde 0.9.0`（要求 `zstd ^0.13.1` + `zstd-safe ^7.1.0`）
- `zstd 0.14.0` ← `pingora-core 0.9.0`

### 10.3 为什么是「不可修」而不是「没升」

- 公告的 `patched` 只有 `zstd >= 0.14.3` / `zstd-safe >= 8.2.0` / `zstd-sys >= 2.1.2`；
  **0.13.x 与 zstd-safe 7.x 线上不存在修复版**，而 pingora-header-serde 锁死在这两条线。
- `cargo search pingora` 实测：**`pingora` 最新就是 0.9.0**（即当前版本）。
  ⇒ 没有任何含修复的 pingora 可升。

### 10.4 为什么是「不可达」（源码级调用图，2026-11-01 实测）

`grep` 于 `~/.cargo/registry/src/*/pingora-*/src/` 全树，三个受影响符号命中数**均为 0**：

- pingora-header-serde 实际只用 `zstd::dict` + `zstd_safe::{CCtx,DCtx}::create`
  + `decompress_to_buffer`（**不是** `decompress_stream`）；
- 公告原文明确 `zstd::stream::read::Decoder` / `decode_all` / bulk API **不受 0403 影响**；
- 0410 仅 wasm32，本仓只构建 x86_64-windows / x86_64-linux ⇒ **平台不可达**；
- 本仓自身代码对 zstd **零引用**（`grep -rn zstd src/` 无命中），不存在第二条路径。

### 10.5 ⚠️ 一次**失败的方法论**（留档，勿重犯）

本想把"受影响符号不在二进制里"作为**硬证据**：

1. `llvm-nm` 查 PE 符号 → `with_context`/`Seekable`/`decompress_stream` 全为 0，
   看着很完美；**但对照组（确定被调用的 `decompress_to_buffer`）也是 0**
   ⇒ 该方法对 Windows PE 无效，前面的"证据"一文不值。
2. 改用 PDB 字符串搜索后对照组成立（8），但 `with_context`(8) / `Seekable`(1)
   的命中来自**通用名污染**——`with_context` 实际来自 `anyhow`（完全无关的 crate）。

⇒ 最终证据是**源码级调用图**，不是符号级。只有 `decompress_stream` 的符号级
读数（0）是干净的、且与源码结论一致。**教训**：任何"未命中即安全"的检查，
必须先跑一个**必然命中**的对照；没有对照的 0 命中等于没有检查。

### 10.6 豁免的退出条件（不会变成僵尸）

`deny.toml` 里 `unused-ignored-advisory = "deny"`：一旦 pingora 升级到含修复的
版本（届时这些 advisory 不再命中），门禁**自动报错**，强制删掉对应 ignore。
实测：注入一条不命中的 `RUSTSEC-2020-0001` ⇒ `error[advisory-not-detected]` 判红。

### 10.7 门禁改进：把「不确定」变成「确定」（三次迭代，前两次都错了）

**第一版（错）**：裸跑 `cargo audit`，非 0 即红。
⇒ 离线机器上这道门**永远红**（audit 要 git fetch 公告库）。假红会被学会忽略，
等于把安全门降级成噪声。

**第二版（错）**：字符串匹配网络错误特征串，是则 SKIP。
⇒ **不稳定**：同一台机器、同一份代码，实测**时绿时红**（网络重试行为不同，
有时报错文案不含特征串）。靠解析 stderr 判断"失败的性质"本身就是脆弱的。

**第三版（现行）**：把不确定性从根上消掉——**全程离线 + 两个工具同源 + 豁免单一真源**：

| 措施 | 解决的问题 |
| --- | --- |
| `cargo audit --no-fetch --stale` | 彻底不联网 ⇒ 结果确定，不再有网络抖动 |
| `-d ~/.cargo/advisory-dbs/advisory-db-*` | **两个工具的默认库不是同一个**：cargo-deny 用 `~/.cargo/advisory-dbs/advisory-db-<hash>`，cargo-audit 默认 `~/.cargo/advisory-db`。本机实测后者是**旧快照**（连 RUSTSEC-2026-0391 都没有）⇒ 不指定的话 audit 会在旧库上得出"0 漏洞"的**假绿**，比不跑更危险 |
| `--ignore` 列表从 `deny.toml` 读 | audit 不读 `deny.toml`。两套豁免清单必然漂移（"deny 说豁免了、audit 说没豁免"，一个绿一个红，谁对说不清）⇒ 把 deny.toml 当唯一真源 |

离线后失败原因只剩"真的发现漏洞"，故非 0 即判红，**不需要再猜字符串**；
库不存在（首次运行/未同步）则显式 SKIP 并说明"本次没有做 audit 侧校验，
advisories 仍由 cargo deny 覆盖"。

**验证**：连续 3 次 exit 0（稳定）；删掉 0410 的 ignore ⇒ 判红并指名
`RUSTSEC-2026-0410`（判别力）；`unused-ignored-advisory = "deny"` 保证豁免不会变僵尸。
