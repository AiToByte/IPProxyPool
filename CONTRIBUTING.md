# Contributing 贡献指南

> 本文件双语：中文在前，English after. Bilingual: Chinese first, English second.

## 中文

### 1. 先落库再执行（Plan-first）

任何多步骤工作先写实施方案并落库，再执行：

1. 在 `plan/` 新建 dated 方案（例：`plan/2026年9月23日-XXX实施计划.md`），含目标/架构/任务 checkbox/验收门；
2. `TASK_PLAN.md` 登记步骤，`EXEC_LOG.md` append-only 记立项；
3. 按方案逐任务执行，完成后回写状态（方案§状态表＋EXEC_LOG 完成条＋TASK_PLAN✅）。

### 2. TDD

- 先写失败单测（红），再最小实现（绿），再重构；
- 每个修复/优化必须有回归单测锁定。

### 3. 四门禁（Four gates，缺一不可）

在 `gateway/` 目录执行：

```powershell
cargo fmt --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
cargo bench --workspace --no-run
```

真 live 测试（需 Redis＋ClickHouse 在线，先验 `PONG`/`Ok`，检查无 SKIP 行）：

```powershell
cargo test --workspace -- --ignored --nocapture
```

性能断言：`cargo test --release bandit`（8 臂选路 <200ns）。

### 4. 提交规范

- 信息格式：`docs|fix|feat: <中文摘要>`（例：`fix: FreePool信号量许可持有`）；
- 一次提交只含一个步骤的变更；`log/*.out|err` 与本地数据不提交；
- 真供应商 Key、license key 永不进仓库（走环境变量/secret 管理）。

### 5. 禁区

- 不跨 `await` 持有 `parking_lot` 锁/DashMap 守卫；
- 不跳门禁标✅；`EXEC_LOG.md` 只追加不改写历史；
- 终端命令一律显式短 timeout（构建 300s/常规≤60s）。

## English

### 1. Plan-first

For any multi-step work, write the implementation plan and file it before executing:

1. Create a dated plan under `plan/` (e.g. `plan/2026年9月23日-XXX实施计划.md`) with goal/architecture/checkbox tasks/acceptance gates;
2. Register the step in `TASK_PLAN.md` and append a kickoff entry to `EXEC_LOG.md` (append-only);
3. Execute task by task, then write back status (plan status table + EXEC_LOG completion entry + `TASK_PLAN.md` ✅).

### 2. TDD

- Failing test first (red), minimal implementation (green), then refactor;
- Every fix/optimization must be locked by a regression test.

### 3. Four gates (all required)

Run inside `gateway/`:

```powershell
cargo fmt --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
cargo bench --workspace --no-run
```

Live tests (require Redis + ClickHouse online; verify `PONG`/`Ok` first and check for no SKIP lines):

```powershell
cargo test --workspace -- --ignored --nocapture
```

Performance assertion: `cargo test --release bandit` (8-arm routing <200ns).

### 4. Commit conventions

- Message: `docs|fix|feat: <summary>`;
- One step per commit; never commit `log/*.out|err` or local data;
- Real vendor Keys and license keys must never enter the repo (use env vars/secret management).

### 5. Forbidden

- Never hold `parking_lot` locks/DashMap guards across `await`;
- Never skip gates; `EXEC_LOG.md` is append-only;
- Every shell call must set an explicit short timeout (build 300s/regular ≤60s).
