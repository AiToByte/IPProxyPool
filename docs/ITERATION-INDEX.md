# 后续优化迭代索引

**建立日期：2026-11-11**  
**用途：为复查、状态追踪与阶段审批提供唯一入口。**

## 已沉淀文档

| 文档 | 职责 | 当前状态 |
|---|---|---|
| [`WORLD-CLASS-ASSESSMENT.md`](WORLD-CLASS-ASSESSMENT.md) | 当前能力、世界级同类对比、差距、证据和主张边界 | 当前工作树评估；工作区能力不等同于已发布能力 |
| [`ROADMAP-FOUR-TRACKS.md`](ROADMAP-FOUR-TRACKS.md) | 生产可信化、企业控制面、协议接入、智能路由 2.0 的依赖与退出门 | 路线草案；逐阶段批准 |
| [`2026年11月11日-生产可信化下一轮实施计划.md`](../plan/2026年11月11日-生产可信化下一轮实施计划.md) | T1 下一轮逐项 RED/GREEN/live/review/rollback 计划 | DRAFT / 待事实核对与用户批准 |
| [`CAPABILITY-AUDIT.md`](CAPABILITY-AUDIT.md) | 历史能力复核与既有取证记录 | 含撰写时历史快照；以质量/性能专用基线为准 |

## 顺序与审批

```text
当前未提交 P0 / CONNECT 冻结、取证与收口
→ T1 生产可信化
→ T2 企业控制面
→ T3 协议与接入扩展
→ T4 智能路由 2.0
```

设计或事实复核可以并行；生产实现和启用必须通过各自依赖门。每一轮必须先把当前最优计划写入 `plan/` 并标明状态，完成事实核对和批准后再写 RED 测试；RED 复现后才实现，GREEN 后再审查和验证。任何一项未验证时不得借后续阶段的计划宣称已完成。

## 每轮状态记录模板

| 字段 | 记录内容 |
|---|---|
| 计划 ID / 状态 | DRAFT → FACT-CHECKED → APPROVED → RED-RECORDED → IMPLEMENTING → GREEN → REVIEWED → VERIFIED → CLOSED |
| 基线 | HEAD、工作树文件清单、工具链、OS、依赖版本 |
| 影响面 | 生产代码、测试、配置、文档和非目标 |
| 证据 | RED/GREEN 命令和退出码；真实服务/模拟服务区分；复现条件 |
| 审查 | Rust/通用 code review、安全 review、文档一致性 |
| 回滚 | feature/config disable、旧二进制/配置、触发条件、演练结果 |
| 收尾 | 适用门禁、未验证项、文档同步、执行记录链接 |

## 权威基线

- 测试数字：[`QUALITY_BASELINE.md`](QUALITY_BASELINE.md)
- 覆盖率：[`COVERAGE_BASELINE.md`](COVERAGE_BASELINE.md)
- 容量：[`PERF_BASELINE.md`](PERF_BASELINE.md)
- 选路：[`ROUTING_BASELINE.md`](ROUTING_BASELINE.md)
- 供应链：[`SUPPLY_CHAIN.md`](SUPPLY_CHAIN.md)
- 运行证据：根目录 `EXEC_LOG.md`，只追加，不改写历史。

## 当前阻塞声明

T1 计划明确为 `DRAFT`。计划批准前不开始生产可信化新实现。本轮文档写入没有批准当前未提交的 P0 或 CONNECT 工作，也没有把这些工作树改动标记为发布级能力。
