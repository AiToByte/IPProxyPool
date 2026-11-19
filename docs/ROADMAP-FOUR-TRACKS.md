# 四轨优化路线图

**创建日期：2026-11-11**  
**状态：路线草案，供复查和逐阶段审批；不是生产代码实施授权。**

## 1. 目标与原则

目标是把当前智能出口网关演进成可验证、可回滚、可扩展的企业级多供应商出口平台。路线按以下顺序推进：

```text
当前工作树 P0 / CONNECT 收口
        ↓
T1 生产可信化
        ↓
T2 企业控制面
        ↓
T3 协议与接入扩展
        ↓
T4 智能路由 2.0
```

部分设计工作可以并行，但上线启用按依赖门推进。任何后续功能不得掩盖前置阶段未闭环的问题。

共同规则：

- 当前质量数字只取 [`QUALITY_BASELINE.md`](QUALITY_BASELINE.md)；覆盖率、性能和回放分别取各自基线。
- 测试需要证明外部行为，不用静态 grep 或内部计数器代替行为测试。
- 真实 Redis、ClickHouse、Linux、标准客户端和多实例验证分别标记；mock/stub 不能代替 live 证据。
- 安全边界、凭据脱敏、回滚方式和失败降级属于交付验收条件，不是后补文档。
- 保留历史计划与执行日志的原始记录；新状态写入新计划或追加到 `EXEC_LOG.md`。
- 未经阶段批准，不修改下一阶段生产实现。

## 2. 阶段 0：当前工作树冻结与事实校验

### 目标

冻结本轮已有的未提交 P0、CONNECT、回放工具、CI、测试和文档改动，形成可复核的输入基线。

### 交付

- 记录 HEAD、工作树状态、Rust/Python 工具链、平台和依赖版本。
- 逐项区分：已实现、已测试、真实依赖已验证、仍待验证。
- 复核 CONNECT 计量生命周期和当前产品文档差异。
- 保留 `repomix-output.xml`、既有文档和源文件中的用户改动。

### 退出门

- 未提交对象均已盘点；未验证能力没有写成正式已交付能力。
- 关键行为测试可复现；Rust workspace、Python 和相关文档门结果留档。
- P0 与 CONNECT 各自的剩余项、回滚开关和依赖明确。
- Linux 隔离构建与 live 故障恢复已有特定 revision/topology 证据；容量测试仍 `NOT RUN/BLOCKED`，Linux 长稳、生产 SLO、HA 仍未验证。2026-11-18 有本地 HTTP/CONNECT drain admission 与有界 flush 单测，非 Linux SIGTERM lifecycle 验证。
- 当前工作树和能力声明冻结仍需在每个新实现切片前重新执行；路线阶段状态以最新执行记录为准，不因本次 live 验证自动授权下一轨实现。

## 3. T1：生产可信化（第一优先级）

### 目标

证明单实例的现有 HTTP/SOCKS 数据面在配置错误、依赖故障、恢复、取消和关停时行为可预测且可运维。

### 工作流

1. **配置与启动**：typed 配置解析；非法监听、密钥、Redis/ClickHouse、shard 和连接组合 fail-fast；敏感错误不回显凭据。
2. **健康语义**：区分进程存活、配置有效、listener 已就绪、外部依赖降级和数据面可接流量；`livez` / `readyz` 已存在并经隔离 Linux 验证。2026-11-18 本地实现增加 drain/stopped 时 HTTP 与 CONNECT admission gate，但尚未对齐“其他 unready 状态是否拒绝流量”的决策，也未闭合 drain 标志与 Pingora shutdown watch 的 race 或证明有界 in-flight completion；不能把 probe 状态当 drain 完成证明。
3. **恢复与停止**：真实 Redis/ClickHouse 中断、恢复、积压、pending reclaim、worker 退出、强杀重启、优雅 drain、遥测 flush。
4. **容量基线**：Linux release、真实上游、telemetry on/off、连接池复用、长 CONNECT 隧道、p50/p95/p99/p999 和资源水位。
5. **发布准入**：SLO/告警接收方、runbook、错误预算、停止条件、二进制/配置回滚和演练证据。

### 验收门

- 所有配置边界有 RED/GREEN 行为测试。
- Redis/ClickHouse live 演练分别记录依赖版本、故障注入方式、恢复时间、数据积压和结论。
- ready 只在对应流量入口实际可用时为真。
- Linux 与 Windows 相关行为分别验证，不能互相替代。
- 取消/停止后没有遗留租户槽、worker respawn、丢失的可确认数据或重复计费。
- 性能结果给出硬件、版本、拓扑、限流配置、遥测状态和分位值；不把开发机结果写成容量承诺。
- 独立 Rust code review 和 security review 通过。

### 非目标

本阶段不重写路由策略、不实现 RBAC/集群控制面、不扩展 H2/H3/MASQUE、不采购代理 IP 资源。

## 4. T2：企业控制面

### 前置

T1 生产可信化通过；依赖状态、配置来源和 readiness 语义稳定。

### 工作流

1. 明确配置意图、运行健康、自动决策、账务四类状态的所有权。
2. 建立版本化 `ConfigSnapshot`、checksum、创建者、审批状态、发布时间和回滚版本。
3. 将节点、租户、路由策略、健康策略和 secret reference 纳入 schema；支持 schema 版本升级和兼容策略。
4. 多实例配置发布：发布版本、实例 ACK/NACK、过期保护、失联实例隔离、配置漂移检测。
5. 身份与权限：OIDC/SSO、RBAC、service account、API key rotation、操作审计、最小权限。
6. 租户配额一致性：明确 reservation/settlement/账本模型，提供幂等事件；在一致性设计和 live 证明前不宣称金融级余额。
7. 建立 Admin API/UI 与 schema 双向一致性、灰度发布、审批和自动/手动回滚。

### 验收门

- 配置可追溯、可比对、可发布、可回滚。
- 多实例能报告同一配置版本或给出明确 NACK/降级状态。
- Secret 不进入配置导出、日志、URL 或审计明文。
- RBAC 的允许/拒绝路径有集成行为测试。
- 租户并发配额有跨实例竞争和恢复测试。

### 非目标

不自行发明完整通用 xDS 克隆；优先定义本项目最小版本化控制协议，之后再决定是否适配 Envoy xDS/Kubernetes Gateway API。

## 5. T3：协议与接入扩展

### 前置

T1 的安全、ready、生命周期、观测和运行回滚门关闭；T2 提供稳定配置与 secret 模型。

### 独立批次

- **T3-A HTTP/1.1 CONNECT**：当前候选 listener 先完成真实标准客户端、真实 HTTP proxy 和 SOCKS 上游验证；验证 framing、认证、预读、半关闭、超时、取消、计量、no-retry-after-200、优雅停机。
- **T3-B TLS 入站**：证书加载 fail-fast、SNI、热更新、TLS policy、mTLS 可选、证书轮换和告警；不默认 MITM。
- **T3-C HTTP/2 CONNECT / Extended CONNECT**：按 RFC 8441/相关 HTTP 语义另立设计；普通 CONNECT 与 Extended CONNECT 不混为同一种 authority 语义。
- **T3-D HTTP/3 CONNECT**：QUIC、连接/stream 生命周期、拥塞控制、代理链兼容和运营指标独立验收。
- **T3-E PAC 服务**：模板、目标分类、签名/版本、缓存、发布回滚及客户端兼容单独验收。
- **T3-F CONNECT-UDP / CONNECT-IP**：只有明确客户需求、供应链和 Linux/QUIC 验证能力后再立专项。

### 验收门

每个批次分别提供协议矩阵、真实标准客户端、失败语义、安全审查、metrics/logs、SLO 和 rollback；任何一个 batch 未过不得宣称全部协议支持。

### 非目标

默认不实现目标 HTTPS 解密/MITM。H2、H3、CONNECT-UDP、CONNECT-IP/MASQUE 不因为 HTTP/1.1 CONNECT 已完成而自动进入开发。

## 6. T4：智能路由 2.0

### 前置

T1 的遥测数据完整性与故障恢复通过；T2 的租户/配置边界稳定；需要 T3 的流量和健康事件字段可关联。

### 工作流

1. 固化 decision/attempt 数据契约：`trace_id`、policy_version、完整候选集或候选摘要、选中节点、排除原因、provider、目标特征、延迟、错误、成本和结果。
2. 建立真实 ClickHouse 决策/尝试序列，保留合成样本仅作算法自测。
3. 通过随机探索、holdout、shadow decisions 和 propensity 记录减少支持偏差；评估 inverse propensity / doubly robust 等离线策略评估。
4. 对比 round robin、weighted、LinUCB、constrained/contextual Thompson Sampling 和 oracle 上界；固定样本、时间窗、seed、策略版本。
5. 定义多目标 reward：成功率、延迟、流量成本、供应商 SLA、地理要求、风险/合规和租户优先级。
6. 灰度：shadow → 小比例 canary → 扩大流量；定义自动停止指标和回滚策略。
7. 免费池用有效贡献率、出口去重、节点年龄、失效率和供应枯竭预警，而不以抓取数量做成功指标。

### 验收门

- 同批数据改前/改后回放和不确定性说明齐全。
- 真实线上试验有 holdout、观测窗口、回滚触发器和审批。
- 不以合成回放绝对值宣称生产收益。
- 决策策略可解释、可重放、可回滚。

### 非目标

在真实候选集重建、反事实偏差和回滚门通过前，不替换生产策略、不引入不可解释黑盒模型。

## 7. 依赖与并行

| 工作 | 允许开始 | 必须满足才能生产启用 |
|---|---|---|
| T1 配置/readiness 设计 | 当前基线已盘点 | T1 live/故障/恢复/回滚门全部关闭 |
| T2 控制面 schema 设计 | 可与 T1 后期设计并行 | T1 ready/依赖/停止语义通过 |
| T3 协议取证/测试向量 | 可与 T1/T2 设计并行 | 对应 listener 配置、安全、观测和 rollback 已验收 |
| T4 离线评估设计 | 可并行设计 | 真实数据可还原，T1 遥测可靠、T2 租户边界稳定 |
| H2/H3/MASQUE | 仅专项立项后 | CONNECT/TLS/生产生命周期各自先验收 |

## 8. 每项计划状态机

每个可实现工作项必须执行：

```text
DRAFT
→ FACT-CHECKED
→ APPROVED
→ RED-RECORDED
→ IMPLEMENTING
→ GREEN
→ REVIEWED
→ VERIFIED
→ CLOSED
```

- `DRAFT` / `FACT-CHECKED`：只做事实取证、设计和审查，不改生产代码。
- `APPROVED`：范围、影响文件、验收命令、外部依赖、负责人和回滚方式齐全，才能写测试。
- `RED-RECORDED`：测试实际执行且因目标行为缺陷失败，才允许实现。
- `GREEN`：相关测试通过后，执行 code review 和 security review。
- `VERIFIED`：完整适用的格式、lint、测试、结构、真实服务/平台门禁有结果记录。
- `CLOSED`：能力文档、基线和执行记录完成必要同步；历史记录不得改写。
- CRITICAL/HIGH 发现返回 `IMPLEMENTING`；未通过真依赖测试只能标“未验证”，不得以 mock 替代。

## 9. 发布级全局门

- 质量数字唯一来自 `QUALITY_BASELINE.md`。
- 每条能力声明链接源码、命令或 `EXEC_LOG.md` 证据。
- 配置意图、健康状态、自动策略和财务账务分离。
- 普通 HTTP、CONNECT、TLS、H2/H3、PAC、MASQUE 分别有独立支持矩阵。
- 每条可变策略有 disable switch、上一版本/配置和 rollback 证据。
- 真实 Redis/ClickHouse、Linux、多实例、供应商 SLA 和生产容量均标明确证据状态。
- 不提交、不推送，除非用户之后明确要求。
