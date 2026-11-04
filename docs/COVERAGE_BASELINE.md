# Coverage Baseline 覆盖率基线

> 双语：中文在前，English after. Bilingual: Chinese first, English second.
> 机器可读副本：`docs/coverage_baseline.json`。门禁（`tools/check_coverage_baseline.py`）
> 校验两者**逐字一致**——不一致时**至少有一边在骗人**，而读者只会看 markdown 那边。
> Machine-readable twin: `docs/coverage_baseline.json`; the gate asserts the
> two agree character-for-character.

## 1. 测量方法

| 项 | 值 |
|----|----|
| 工具 | `cargo-llvm-cov 0.9.1` + `llvm-tools-preview` |
| 编译器 | `rustc 1.99.1 (58cf8f9c3 2026-10-13)` / MSVC `x86_64-pc-windows-msvc` |
| 命令 | `cargo llvm-cov --locked --summary-only --json` |
| 判定指标 | **行覆盖率**（regions/functions 受泛型单态化与内联决策影响，噪声大得多） |
| 总体行覆盖率 | **74.15%**（10800 行中 8008 行被覆盖） |
| 总体 region 覆盖率 | 75.29%（17259 中 12995 被覆盖） |
| 总体函数覆盖率 | 76.93%（1231 中 947 被覆盖） |
| 回退容差 | **1.0 个百分点**（见 §3） |

## 2. 逐模块覆盖率（按行覆盖率升序：最需要关注的在前）

| 模块 | 行覆盖率 | region 覆盖率 | 函数覆盖率 |
|------|---------|-------------|-----------|
| `gateway/hooks.rs` | 0.00% | 0.00% | 0.00% |
| `gateway/socks.rs` | 5.85% | 3.54% | 11.11% |
| `admin/handlers_ops.rs` | 10.14% | 9.23% | 14.29% |
| `free_pool/worker.rs` | 13.60% | 14.21% | 9.09% |
| `main.rs` | 32.60% | 32.18% | 58.51% |
| `ch_sink.rs` | 49.20% | 53.88% | 50.00% |
| `admin/handlers_tenant.rs` | 49.33% | 52.31% | 70.00% |
| `telemetry.rs` | 53.70% | 51.37% | 59.09% |
| `circuit_breaker.rs` | 55.53% | 55.77% | 70.59% |
| `analytics.rs` | 58.26% | 59.09% | 50.00% |
| `metrics/record.rs` | 62.72% | 65.97% | 75.36% |
| `gateway/policy.rs` | 75.00% | 82.76% | 71.43% |
| `vendor_arbitrage.rs` | 81.82% | 81.73% | 70.97% |
| `admin/dto.rs` | 82.05% | 82.50% | 75.00% |
| `admin/handlers_node.rs` | 82.22% | 87.57% | 86.96% |
| `router/node_mgmt.rs` | 82.35% | 81.70% | 70.00% |
| `geo.rs` | 82.89% | 79.75% | 83.33% |
| `bandit.rs` | 84.58% | 88.98% | 93.02% |
| `admin/mw.rs` | 87.50% | 85.37% | 69.23% |
| `node_registry.rs` | 87.92% | 90.09% | 86.59% |
| `gateway/mod.rs` | 88.31% | 88.29% | 87.50% |
| `socks_bridge.rs` | 88.94% | 89.43% | 86.54% |
| `prober.rs` | 89.81% | 91.22% | 92.31% |
| `free_pool/sources.rs` | 90.00% | 91.34% | 88.37% |
| `tenant.rs` | 91.78% | 91.93% | 90.53% |
| `free_pool/mod.rs` | 92.39% | 88.19% | 76.92% |
| `gateway/bandit.rs` | 93.20% | 94.33% | 86.36% |
| `socks_handshake.rs` | 93.82% | 94.30% | 78.57% |
| `redis_shard.rs` | 94.12% | 96.39% | 100.00% |
| `router/health.rs` | 94.12% | 94.50% | 90.91% |
| `pool.rs` | 94.28% | 95.18% | 88.57% |
| `free_pool/verify.rs` | 94.39% | 91.81% | 100.00% |
| `router/quarantine.rs` | 95.51% | 95.07% | 93.75% |
| `free_pool/registry.rs` | 95.96% | 94.64% | 95.24% |
| `router.rs` | 96.30% | 92.37% | 100.00% |
| `metrics/mod.rs` | 96.49% | 96.24% | 92.86% |
| `metrics/render.rs` | 97.66% | 98.33% | 100.00% |
| `gateway/upstream.rs` | 97.73% | 98.55% | 100.00% |
| `router/select.rs` | 97.97% | 96.37% | 96.43% |
| `admin/mod.rs` | 100.00% | 100.00% | 100.00% |
| `fingerprint.rs` | 100.00% | 100.00% | 100.00% |
| `janitor.rs` | 100.00% | 100.00% | 100.00% |
| `model.rs` | 100.00% | 100.00% | 100.00% |
| `net_connector.rs` | 100.00% | 100.00% | 100.00% |
| `router/buckets.rs` | 100.00% | 100.00% | 100.00% |

## 3. 为什么是「只卡回退、不卡达标」

计划文档（`plan/2026年10月27日-OPT-R18高质量迭代方向评估.md` §3 P2-5）明确写道：
「先跑 `cargo-llvm-cov` 出基线**再**设地板；不要一上来卡阈值（会逼出"为覆盖而覆盖"的测试）」。

本仓照此执行，**方向性选择**如下：

- **向上没有门槛**——不要求覆盖率必须达到某个绝对数字。设绝对门槛会逼出
  「为了点亮某几行而写的测试」，这类测试的断言往往无鉴别力（步骤 51 的
  `assert!(ADMIN_TIMEOUT_SECS > 0)` 就是数量涨了、质量为零的样本）。
- **向下有门槛**——覆盖率比基线低超过 **1.0 个百分点**即判红。

容差取 1.0pp 的理由：

- **不是 0**：`cargo-llvm-cov` 的统计会随编译器版本、泛型单态化数量等
  **非本仓代码**的因素小幅浮动；0 容差会周期性假红，而假红门禁会被学会
  忽略——比没有门更糟。
- **不是 5**：5pp 的容差足以放过「新增 800 行生产代码、0 个测试」这种真实退化。
- **1.0pp** 落在「噪声」与「退化」之间。

## 4. 怎么读这张表

行覆盖率低**不等于**质量差，但值得逐个确认「低覆盖的那部分是不是**该测**的」。

> **本节刻意不复述具体百分比**：§2 的表格由 `--refresh` 机器回写，正文里的
> 数字会随每次测量漂移成「文档骗人」。要数字就看表，这里只讲**为什么**。
> （与 `docs/COMPONENTS.md` 删掉「行数」列是同一条结论：维护不了的数字不该写。）

- **`main.rs` 排在最前是预期的**——它是装配层（env 接线、supervisor、
  Pingora 启动），逻辑薄且高度依赖真实进程环境；给它凑单测会写出
  「断言构造函数返回了构造函数」的假测试。
- **`telemetry.rs` / `ch_sink.rs` / `analytics.rs` 低是因为依赖真实
  Redis/ClickHouse**——写这份基线时本机这两个服务没有运行，相关路径无法端到端
  覆盖。这是**已知的环境缺口，不是测试缺失**；对应的活流量验证在
  EXEC_LOG 步骤 40/43。补齐后应随 `--refresh` 自然上升（`analytics.rs` 在
  2026-10-28 补 `trace_id` 透传测试后就从约 53% 升到约 65%，是活例子）。
- **`node_registry.rs` / `tenant.rs` / `router/health.rs` 这些核心逻辑模块
  已在高位**，符合「测试集中在有分支逻辑的地方」的预期。
- **看的是趋势不是单点**：本门只拦「比基线低超过 1pp」，不评价绝对值。
  某个模块本来就低、且已确认那部分不该测，就让它低着——只要别更低。
- **`#[ignore]` 的 live 测试体会拉低覆盖率，这是结构性的、不是退化**。
  `cargo llvm-cov` 默认不带 `--include-ignored`，故那些函数体一行都不会被执行，
  但它们**是代码行**，会计入分母。实测：给 `analytics.rs` 新增
  `live_trace_id_roundtrip`（需真实 ClickHouse）后，该文件从 65.00% 掉到 57.02%
  ——门禁当时**正确判红**（它只看得见"覆盖率掉了"，看不见"掉的原因是新加的
  live 测试"）。处置是 `--refresh` 刷新基线并在本条记录原因，而不是删掉那条
  测试：live 测试的价值在于**真跑时能抓出串列/缺列**，那正是单测抓不到的。
  同理，本仓每加一条 live 测试，总体覆盖率会有小幅下移——看到下移先查
  `git diff` 里有没有新增 `#[ignore]`，再判断是不是真退化。
- **把内联 `mod tests` 拆成独立 `tests.rs` 会让覆盖率"下降"，这是口径变化**。
  `cargo llvm-cov` 的报告**不含 `tests.rs`**（`router/tests.rs` 从来就不在里面），
  但**内联在业务文件里的 `#[cfg(test)] mod tests` 会被计入**该文件——而测试是
  会执行的，于是那段代码近乎 100% 覆盖，**凭空抬高**了该文件与总体的数字。
  实测（2026-10-31 拆 `free_pool`）：总体 78.98% → 76.87%（−2.1pp），
  离开测量集 1364 行、其中 1336 行（97.9%）原本是"覆盖"的——正好是内联测试
  代码的形状。**这是数字变诚实，不是代码变差**：生产代码的覆盖情况一行没变。
  反过来也成立：把测试塞进业务文件能"提高"覆盖率，这正是"为覆盖而覆盖"的
  一种隐蔽形态，故本仓宁可让数字低一点也要把测试放独立文件。
  **同一现象已复现五次**（`router` → `free_pool` → `gateway` → `metrics` → `admin`）：
  `free_pool` 78.98%→76.87%（−1364 行 / 其中 97.9% 原覆盖）、
  `gateway` 76.87%→75.80%（−632 行 / 其中 96.5% 原覆盖）、
  `metrics` 75.80%→74.97%（−388 行 / 其中 **100.0%** 原覆盖）、
  `admin` 74.97%→73.87%（−470 行 / 其中 **100.2%** 原覆盖）。
  比例都落在 96%~100%——正是"测试执行 ⇒ 内联测试代码近乎全绿"的特征
  （`metrics`/`admin` 那两次是 ~100%，因为离开的**全部**是测试代码，没夹带业务行；
  `admin` 算出 100.2% 是整数取整 + 拆分后内联决策变化带来的 1 行噪声，不是新覆盖）。
  故看到覆盖率在**拆分模块**后下移 1~2pp，第一反应应是查 `git diff`
  里有没有 `mod tests` 从业务文件迁出，而不是去找"哪段代码没测"。

## 5. 门禁

`tools/check_coverage_baseline.py` 做三件事：

1. **基线 ↔ 文档一致**：本文件里的每个数字必须与 `docs/coverage_baseline.json` 逐字一致。
2. **基线 ↔ 源码一致**：基线里有、实测报告里没有的文件（或反之）判红——
   文件被删/改名/新增后必须刷新基线。
3. **回退检测**：实测行覆盖率低于基线超过 1.0pp 判红。

变异自测四处注入（覆盖率回退 / 基线文件缺失 / markdown-JSON 不一致 /
「不可比模式只跳过数值判定」）必须全部判红。

**刷新基线是一条命令**（`--refresh` 会一并回写本文件的表格与总体数字）：

```bash
cd gateway && cargo llvm-cov --locked --summary-only --json --output-path coverage.json
cd .. && python3 tools/check_coverage_baseline.py --refresh --json gateway/coverage.json
```

为什么让 `--refresh` 顺手改文档：刷新后文档必然失同步，而断言 1 会立刻判红。
若把「改文档」留给人工，只有两个结果——人漏了（门禁红，但红的原因是「文档
没跟上」而非「覆盖率退化」，噪声稀释门禁可信度），或人懒得逐行改 29 行而
随便填个数（**文档开始骗人**，正是本门要防的事）。机器生成的数字由机器回写，
人只维护 §3/§4/§5 的论证文字。

### 5.1 平台与工具链可比性（重要）

覆盖率**数字不是跨工具链、跨平台可比的**：单态化数量、内联决策、
`cfg(unix)/cfg(windows)` 分支的展开都会随 rustc 版本与目标平台变化，
实测这类差异足以超过 1pp 的容差。若不区分，CI 上换个编译器版本就**假红**，
而假红门禁会被学会忽略——比没有门更糟。

故门禁按 **工具链 + 目标三元组** 判定可比性：

- **可比**（同 rustc 版本、同目标三元组）⇒ 做全部三项断言，含数值回退。
- **不可比**（版本或平台不同）⇒ **只**做断言 1 与 2（文件集、文档一致），
  并**显式打印**跳过数值判定的原因。绝不静默放行。

本仓的实测结果：

| 场景 | 平台 | 工具链 | 数值回退判定 |
|------|------|--------|------------|
| 本地开发机 | `x86_64-pc-windows-msvc` | rustc 1.99.1 | **执行**（与基线完全可比） |
| CI `coverage` job | `x86_64-unknown-linux-gnu` | CI 的 stable | 跳过（显式声明） |

CI 的 `coverage` job 仍然**真跑** `cargo llvm-cov` 并校验：
文件集一致性（新增/删除模块未刷新基线会判红）+ 文档↔JSON 一致性 +
门禁自身的变异自测。数值回退的实际执行处在与基线同平台的开发机上。

### 5.2 English

The gate asserts three things: (1) every number in this document matches
`docs/coverage_baseline.json` character-for-character; (2) the set of files
in the baseline equals the set in a fresh measurement (no stale or missing
files); (3) the measured line coverage is not more than **1.0 percentage
point** below the baseline.

There is deliberately **no upward threshold**. A coverage floor breeds
tests written to light up lines rather than to prove behaviour; the
anti-regression floor catches the real failure mode (new untested code)
without creating that pressure.

Coverage numbers are **not comparable across toolchains or platforms**
(different monomorphization, inlining decisions and `cfg` branches), so the
gate compares numerically only when the rustc version and target triple both
match the baseline. Otherwise it still enforces file-set and document
consistency and **prints why** the numeric check was skipped — it never
silently passes.

