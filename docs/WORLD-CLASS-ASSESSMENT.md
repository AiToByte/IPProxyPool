# IPProxyPool 世界级同类对比与当前能力评估

**评估基准日期：2026-11-11；证据增补：2026-11-18**

**评估范围：当前工作区源码、仓库内基线/执行记录，以及选定的官方产品/标准资料。**  
**性质：工程评估，不是安全认证、生产 SLA、独立基准测试或商业尽调。**

> **2026-11-18 证据增补**：Linux fmt/Clippy/workspace tests/release build 已通过；隔离 Linux 环境完成 Gateway probes、loopback 数据面、Redis memory/required-mode readiness、ClickHouse 恢复与 graceful SIGTERM。细节和证据边界见 [`EXEC_LOG.md`](../EXEC_LOG.md) 顶部收口快照。容量测试仍 `NOT RUN/BLOCKED`，不据此推断容量、SLO 或生产可用性。以下评估主体保留 11-11 基线判断；本增补仅更新相关事实，不重写其余结论。

## 1. 执行摘要

IPProxyPool 已形成具备差异化的智能出口网关：HTTP/SOCKS 出站、LinUCB 域名上下文选路、租户流量治理、节点热更新、主动健康、隔离自愈、免费第二供应线、Prometheus/Redis Stream/ClickHouse 遥测与离线回放均已在仓库中实现相应模块和测试。

当前工作树还包含 HTTP/1.1 CONNECT 独立 listener 和隧道实现。该能力是可选的明文 listener，配置 `CONNECT_ADDR` 后启用，并限制回环绑定；其实现与测试仍应作为当前候选工作看待，不能据此宣称已具备 TLS 入站、浏览器 PAC 完整接入或 H2/H3/MASQUE。

整体判断：

- **相较普通自建代理池**：在域名上下文选路、自动健康反馈、多供应商/免费池治理和自动化工程门禁方面有明显特色。
- **相较 Envoy / HAProxy / Squid**：不是通用基础设施替代品；协议广度、控制面、集群配置、身份集成、成熟运维和实战历史明显不足。
- **相较 Bright Data / Oxylabs 等代理网络**：软件方向不同。IPProxyPool 没有自己的大规模、授权且有 SLA 的住宅/ISP/移动 IP 资源，不能按资源规模或商业服务等级等量比较。
- **当前最大差距**：生产可信证据与运行生命周期、版本化企业控制面、标准协议入口、认证/审计/RBAC、真实供给质量和集群账务语义。

推荐路线固定为：**生产可信化 → 企业控制面 → 协议与接入扩展 → 智能路由 2.0**。每条轨道需独立批准、实施和验收，详细见 [`ROADMAP-FOUR-TRACKS.md`](ROADMAP-FOUR-TRACKS.md) 与 [下一轮生产可信化计划](../plan/2026年11月11日-生产可信化下一轮实施计划.md)。

## 2. 证据纪律与状态标签

- **[R] 仓内实现/静态核验**：源码、配置或测试结构存在。它不自动证明真实服务环境、生产负载或商业 SLA。
- **[T] 自动化测试验证**：在本报告日期执行过的命令与结果。只证明对应测试环境和测试覆盖到的行为。
- **[L] 真实依赖/二进制/活流量证据**：必须有 `EXEC_LOG.md` 或基线中的时间、环境和结果记录。mock/stub 不是 live 证据。
- **[S] 官方标准/产品资料**：只用于描述标准或厂商公开能力；厂商自述数字不是独立审计结果。
- **[G] 缺失/未验证**：仓内无足够证据，或者测试/资料明确表明仍是缺口。

能力状态使用“已验证 / 部分 / 未验证 / 缺失”。所有变化中的质量数字唯一来源为 [`QUALITY_BASELINE.md`](QUALITY_BASELINE.md)，覆盖率口径见 [`COVERAGE_BASELINE.md`](COVERAGE_BASELINE.md)，性能范围见 [`PERF_BASELINE.md`](PERF_BASELINE.md)，路由回放范围见 [`ROUTING_BASELINE.md`](ROUTING_BASELINE.md)。本报告不复制历史执行日志的旧数字作为当前状态。

## 3. 当前已实现能力

### 3.1 数据面和出口选择

| 能力 | 状态 | 仓内边界与证据 |
|---|---|---|
| HTTP 正向代理 | 已验证 | Pingora `ProxyHttp` 请求路径，含鉴权、目标解析、转发、计量、遥测；见 `gateway/src/gateway/`。 |
| HTTP 代理出站 | 已验证 | 普通代理上游连接路径和节点选择。 |
| SOCKS4/4a/5 出站 | 已验证 | 独立 SOCKS 握手与桥接模块，覆盖认证、目标编码、拒绝响应和超时。 |
| LinUCB 上下文选路 | 已验证 | `gateway/src/bandit.rs`、路由集成与 release 微基准。学习信号按目标域名上下文与节点历史表现影响决策。 |
| 加权、粘滞、约束、排除 | 已验证 | `gateway/src/router/`：tier/country/proto 过滤、session 绑定、失败节点排除和零权重处理。 |
| 付费供应商 SLA 套利调权 | 部分 | 有 SLA 策略和权重调整；策略与真实供应商 SLA/合同验证仍需生产输入。 |
| HTTP/1.1 CONNECT 入站 | 部分 | 当前工作树的独立 listener、authority 校验、HTTP/SOCKS 上游建链、预读、双向 relay、半关闭、idle/max lifetime、计量与取消测试。明文只能绑回环；不含 TLS 入站、H2/H3、UDP/MASQUE。 |

### 3.2 健康、供给与数据闭环

| 能力 | 状态 | 仓内边界与证据 |
|---|---|---|
| 主动健康探测 | 已验证（仓内） | 探针结果回灌路由、连续失败摘除、成功恢复和安全阀；免费池按比例抽样，节点 churn/quorum 仍有边界。 |
| 被动隔离/熔断 | 已验证（仓内） | Redis Stream 消费、内存快速隔离、Redis 状态和 PubSub 扇出；依赖真实 Redis 运行条件的结论须看 live 记录。 |
| 免费第二供应线 | 部分 | 多源抓取、两级验证、匿名度分级、GeoIP、熔断、backoff、配额和 Router 合并均有实现；公开免费源不提供稳定性或授权保证。 |
| 遥测与数仓 | 部分 | MPSC → Redis Stream → ClickHouse sink、trace ID 与指标；故障路径有测试，真实环境恢复与长期积压能力仍需持续演练。 |
| GeoIP | 部分 | MaxMind MMDB lookup、三态 verdict、默认 fail-open；缺数据库时 Disabled。 |
| 离线选路回放 | 部分 | Python 回放、ClickHouse 查询契约、Rust/Python parity；当前合成结果只适合作为回归护栏，不是生产因果收益。 |

### 3.3 租户、管理和工程质量

| 能力 | 状态 | 仓内边界与证据 |
|---|---|---|
| API Key 租户鉴权 | 已验证（单实例语义） | QPS、burst、并发槽、余额和按字节 tier 计费；静态 Key，不是完整 IAM。 |
| 租户 Redis 写穿 | 部分 | 异步 dirty flush、版本化确认和取消安全；不是金融账本，不保证多实例原子配额。 |
| Admin API/UI | 部分 | 节点查询、增删、调权、隔离、节点文件持久化和 UI；租户生命周期、RBAC、全量配置发布和集群协调尚缺。 |
| Prometheus/Grafana | 已验证（仓内） | 具备 exposition、告警与仪表盘；指标语义门禁存在。尚未证明目标生产 SLO。 |
| 自动化质量门 | 已验证（测试范围内） | 当前 [`QUALITY_BASELINE.md`](QUALITY_BASELINE.md) 记录 445 passed、10 ignored、22 Python 静态门；Windows 覆盖率见专用基线，测试数量不等于生产成熟度。 |
| 真实容量 | 部分 | [`PERF_BASELINE.md`](PERF_BASELINE.md) 为单进程、loopback 上游、遥测关闭环境，持续约 10k 成功 QPS 受到租户限流器限制；裸产能、遥测开销、真实远端上游及 Linux 长稳未覆盖。 |

## 4. 外部参照与可比边界

### 4.1 Envoy

Envoy 是通用、可扩展的 L4/L7 data plane，不是单一代理池产品。官方文档展示其 xDS 动态资源族（LDS/RDS/CDS/EDS/SDS/RTDS/ECDS/ADS）、活动健康检查、outlier detection、circuit breaker、stats/access logs/tracing 和丰富 filter 模型。[S1][S2][S3]

**IPProxyPool 可取长处**：版本化控制面、动态 cluster/endpoint、配置 ACK/NACK、主动/被动健康分离、健康事件、熔断溢出观测、标准遥测。  
**IPProxyPool 的不同点**：以出口节点供应、目标域名学习、租户计费和免费池治理为中心；Envoy 的通用扩展生态不是这里的替代方案。

### 4.2 Squid 与 HAProxy

Squid 的公开文档支持对 ACL 与认证边界的具体比较：ACL 可匹配目标、端口、方法、协议、TLS 属性，并可使用认证辅助器。[S6][S7] Squid 也长期服务于 HTTP proxy 和缓存/访问控制场景。[S6]

HAProxy 官方文档明确支持 TCP 转发的 PROXY protocol v1/v2，用于保留客户端地址；收发两端都必须显式启用。[S8] 这提供了成熟代理链路的一项可核实能力，但不代表本报告已全面对比 HAProxy 的所有运行时、健康检查和 CONNECT 能力。

IPProxyPool 的智能域名感知选路是差异化；Squid/HAProxy 则拥有更长的协议和生产运维历史。

### 4.3 商业代理服务网络

厂商公开页面会给出 IP 池规模、国家覆盖、session/轮换、响应时间和 SLA 等产品主张。例如 Bright Data 页面声明其住宅池规模、地理定位、sticky/rotating sessions 和服务指标。[S8]

这些是**厂商自述**，不是本报告独立验证结果。它们与 IPProxyPool 的软件路由能力并非同一维度。IPProxyPool 当前依赖公开免费源，无自有商业住宅/ISP/移动资源和供应合同，故资源规模与 SLA 是结构性差距，而非靠选路算法就能补齐。

### 4.4 标准协议方向

- HTTP/1.1 CONNECT 使用 authority-form，隧道在成功响应后成为盲转发；HTTP framing 和隧道边界应按 RFC 9110/9112 验收。[S9][S10]
- RFC 8441 的 HTTP/2 Extended CONNECT 要求协商 `SETTINGS_ENABLE_CONNECT_PROTOCOL`，其 protocol pseudo-header 与普通 CONNECT 语义不同。[S11]
- RFC 9298 CONNECT-UDP 通过 HTTP Datagrams/Capsules 代理 UDP，并引入目标限制、源地址校验、拥塞和 MTU 等要求。[S12]
- RFC 9484 CONNECT-IP 隧道承载任意 IP packet，需要地址、路由、MTU、IPv6、ICMP 和防伪语义。[S13]

本项目只在可选 HTTP/1.1 CONNECT listener 上实现 TCP 隧道；H2/H3、CONNECT-UDP、CONNECT-IP/MASQUE 是后续独立协议项目，不应合并为“支持 CONNECT”。

## 5. 对比矩阵

| 维度 | IPProxyPool 当前 | Envoy | Squid / HAProxy | 商业代理网络 | 结论 |
|---|---|---|---|---|---|
| 域名上下文学习选路 | LinUCB 已实现；线上因果收益未证 | 通用 LB/策略强，非本项目式代理池学习重点 | 传统负载/规则强，未以本仓定义比较其所有版本能力 | 厂商具资源策略与商业调度 | IPProxyPool 有特色，需真实回放/灰度证明价值。 |
| 供给质量与 IP 资源 | 公开免费源 + 用户配置付费节点 | 用户自有 upstream | 用户自有资源/peer | 自有/授权网络、地区覆盖和商业运维 | 最大结构性差距。 |
| 标准协议 | HTTP/SOCKS 出站；工作树有 loopback-only HTTP/1.1 CONNECT | 协议和 filter 面广，H2/H3 生态成熟 | 长期成熟 HTTP/TCP proxy | 产品提供面广但由套餐决定 | IPProxyPool 的入口协议与代理兼容仍窄。 |
| 控制面 | 单 Admin API/UI、节点热更新 | 完整 xDS/config 管理生态 | 成熟配置/运维接口，产品定位不同 | SaaS portal/API | 缺少版本化多实例配置、RBAC 和回滚。 |
| 健康与熔断 | 主动探针 + 被动隔离，多层 Redis | 主动 health/outlier/circuit breaker 成熟 | 长期生产健康检查能力 | 供应商 SLA/平台健康治理 | 本地逻辑已完整，分维度 quorum、规模验证和事件审计待提升。 |
| 身份与计费 | 静态 Key、单实例租户额度/余额 | 强认证扩展与委托身份能力 | ACL/认证成熟 | 用户/团队/套餐/账单 | 明显需要 OIDC/RBAC、审计账本和跨实例配额。 |
| 可观测性 | Prometheus + ClickHouse + trace_id | stats/logs/tracing 生态全面 | 日志/统计成熟 | 面向客户的状态与用量平台 | 仓内基础扎实，但缺 SLO、OTel、生产对账和运维流程。 |
| 生产证据 | 测试/基线丰富，live 与 Linux HA 不足 | 多年大规模生态和运维资料 | 多年部署积累 | 商业 SLA/资源运营 | 不能以测试数量替代规模、故障和恢复证据。 |

## 6. 主要差距与风险

### P0：生产可信度

- 配置校验与运行状态缺少统一 typed 模型；错误模式可能降级而非 fail-fast。
- 当前已补齐 `livez` / `readyz` 和若干隔离 Linux live 依赖恢复证据（2026-11-18）；这不证明 admission gate 在所有数据面路径生效，也不证明生产容量、HA 或 SLO。
- Redis/ClickHouse 故障恢复、积压、重启、pending reclaim 和长期运行仍需按场景持续演练；当前已有的恢复证据仅覆盖 `EXEC_LOG.md` 明确记载的注入与对账路径。
- 当前性能数据未覆盖 telemetry enabled、多实例、Linux、真实远端节点与长隧道并发。
- 尚缺生产 SLO、告警接收方、runbook、回滚时间目标和故障演习证据。

### P1：企业控制面与身份

- 缺少 OIDC/SSO、RBAC、组织和密钥轮换。
- 租户余额异步 Redis 持久化不是账本，多实例并发一致性未解决。
- 缺配置快照、版本、发布审批、灰度、ACK/NACK、回滚和跨实例漂移检测。
- Admin API 已能热更新节点，但租户/策略/供应商配置热更新不完整。

### P1：协议与出口安全

- 无 TLS 入站；H2/H3 CONNECT、PAC server distribution 未完成。
- CONNECT 仅为工作树实现；尚无本文评估日期的真实标准客户端与真实上游生产验收记录。
- 缺统一 SSRF/metadata/private-address 出口策略、DNS rebinding 防护和租户级目标 allow/deny policy。
- 上游 TLS proxy/证书信任模型尚未作为完整配置能力交付。

### P1：策略与供应质量

- LinUCB 合成样本回放不是因果实验；线上反事实和倾向概率记录不足。
- 公共免费代理来源质量和授权不可保证；不能承诺稳定池规模。
- 无供应商合同、真实 SLA 和跨地域资源质量基线。
- 活跃健康对免费池高 churn 的失败计数、tier/country quorum 仍有边界。

### P2：协议前沿

CONNECT-UDP/CONNECT-IP/MASQUE、QUIC-native dataplane、eBPF/io_uring 只有在客户需求、上游兼容、协议安全和运维门具备时再评估。它们不是当前生产可信化的替代品。

## 7. 当前生产主张边界

可以说：

- 仓库包含成熟的 HTTP/SOCKS egress、智能选路、节点治理、租户限额和遥测实现；
- 当前质量基线记录 445 pass / 10 ignored，具体值以 `QUALITY_BASELINE.md` 为唯一真源；
- **当前工作树中**存在 loopback-only HTTP/1.1 CONNECT 实现与协议测试；
- 性能基线是在单进程、回环上游、遥测关闭条件下取得，限流器影响持续吞吐；
- Linux 隔离构建与 live 依赖恢复已有按特定版本/拓扑记录的证据；仍未证明 Linux 容量、长稳、HA、目标生产 SLO 或真实远端上游表现。

不能说：

- 已完成企业级多活集群账务；
- 已具备商业代理网络规模或住宅 IP 资源护城河；
- 已支持 TLS/H2/H3/MASQUE/PAC 的全链路浏览器代理；
- LinUCB 合成回放数字是线上收益预测；
- 测试数量或静态门数量代表生产 SLA、安全认证或 80% 覆盖率；
- 未运行的 Redis/ClickHouse live 测试已经通过；
- Linux 隔离构建与 live 依赖恢复证明了 Linux 容量、长稳、HA、目标生产 SLO 或真实远端上游表现。

## 8. 官方参考资料

以下是官方标准/产品资料。厂商产品页数字均视为厂商自述，不作为独立核验结果。

- [S1 Cloudflare: How we built Pingora](https://blog.cloudflare.com/how-we-built-pingora-the-proxy-that-connects-cloudflare-to-the-internet/)
- [S2 Envoy architecture overview](https://www.envoyproxy.io/docs/envoy/latest/intro/arch_overview/arch_overview)
- [S3 Envoy xDS dynamic configuration](https://www.envoyproxy.io/docs/envoy/latest/intro/arch_overview/operations/dynamic_configuration)
- [S4 Envoy upstream health checking](https://www.envoyproxy.io/docs/envoy/latest/intro/arch_overview/upstream/health_checking)
- [S5 Envoy circuit breaking](https://www.envoyproxy.io/docs/envoy/latest/intro/arch_overview/upstream/circuit_breaking)
- [S6 Squid introduction](https://www.squid-cache.org/Intro/)
- [S7 Squid ACL configuration](https://www.squid-cache.org/Doc/config/acl/)
- [S8 HAProxy PROXY protocol](https://www.haproxy.com/documentation/haproxy-configuration-tutorials/proxying-essentials/client-ip-preservation/enable-proxy-protocol/)
- [S9 RFC 9110 HTTP Semantics](https://www.rfc-editor.org/rfc/rfc9110.html)
- [S10 RFC 9112 HTTP/1.1](https://www.rfc-editor.org/rfc/rfc9112.html)
- [S11 RFC 8441 Bootstrapping WebSockets with HTTP/2](https://www.rfc-editor.org/rfc/rfc8441.html)
- [S12 RFC 9298 Proxying UDP in HTTP](https://www.rfc-editor.org/rfc/rfc9298.html)
- [S13 RFC 9484 Proxying IP in HTTP](https://www.rfc-editor.org/rfc/rfc9484.html)
