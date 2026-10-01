# 质量基线（唯一权威来源 / Single Source of Truth）

> 本文件是**测试数字的唯一权威来源**。`README.md`、`docs/FEATURES.md` 等
> 面向读者的文档中的测试数字，必须与本文件一致；一致性由
> `tools/check_doc_consistency.py` 在 CI 中断言。

## 为何要设"唯一权威来源"

历史上这些数字散落在 10+ 个文件里，每次迭代都要手改，**漂移是必然的**：

- 某轮加了测试，`README.md` 忘了改 → 对外声称的质量数字变成假的；
- `CHANGELOG.md` / `TASK_PLAN.md` / `plan/*.md` 里的数字是**历史记录**，
  本来就该保留当时的值，但没人分得清"哪个数字该更新、哪个是史实"。

本文件把两件事分开了：**当前值**写在这里（会变），**历史值**留在历史文件里
（不该变，且 CI 不会去动它们）。

## 当前基线

| 指标 | 值 | 复现命令 |
| ---- | -- | -------- |
| 单元测试通过数 | **274** | `cd gateway && cargo test --locked --workspace` |
| 单元测试失败数 | **0** | 同上 |
| 忽略测试数 | **7** | 同上 |
| 其中：真 live 测试 | **4** | 需外部依赖（CH/Redis/mock 上游），不进快速门 |
| 其中：串行分配契约测试 | 2 | 需 `--test-threads=1 --include-ignored --exact` |
| release bandit 臂数 | 8 臂均 <200ns | `cargo test --locked --release bandit` |
| Python 静态门 | 12 个 | `python tools/check_*.py` |

### 7 个忽略测试的构成

**4 个真 live 测试**（依赖外部服务，故默认忽略）：

- `analytics::tests::live_insert_and_sla_roundtrip`（需 ClickHouse）
- `ch_sink::tests::live_pump_once_lands_rows`（需 ClickHouse）
- `circuit_breaker::tests::live_quarantine_persists_and_broadcasts`（需 Redis）
- `telemetry::tests::live_flush_writes_stream`（需 ClickHouse）

**3 个串行分配契约测试**（需进程级分配计数器无并行污染）：

- `router::tests::opt_r9_c1_isolated_lookup_does_not_allocate`（隔离查表零分配）
- `gateway::tests::opt_r10_b1_arm_for_hit_path_does_not_allocate`（LinUCB 臂表命中路径零分配）
- `router::tests::opt_r11_b1_pick_weighted_streaming_hit_path_does_not_allocate`（流式加权选路零分配）

后两个由 CI 的 `allocation-contract tests` 步骤**串行**执行，命令形如：

```bash
cargo test --locked --workspace router::tests::opt_r11_b1_pick_weighted_streaming_hit_path_does_not_allocate \
  -- --test-threads=1 --include-ignored --exact
```

三个参数缺一不可，且步骤内会 `grep -q "1 passed"` 兜底——实测踩过：只给
过滤器不给 `--include-ignored`（或用短名配 `--exact`）时，libtest 会静默
`0 passed`，等于这道门没跑。

## 更新流程

改动测试后：

1. 跑全量：`cd gateway && cargo test --locked --workspace`，读 `test result:` 行。
2. 若 `ignored` 数变了，同步更新上面「6 个忽略测试的构成」清单。
3. 更新本文件表格的三个数字。
4. 跑 `python tools/check_doc_consistency.py`，它会把所有活文档的数字与本文件对齐。
5. 在 `EXEC_LOG.md` 记录实测输出。

**不要**手工去改 `CHANGELOG.md` / `TASK_PLAN.md` / `plan/*.md` / `EXEC_LOG.md`
里的历史数字——那些是史实，改了就等于伪造记录。
