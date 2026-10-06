# Security Policy

> 本文件是漏洞披露流程的唯一权威说明。供应链漏洞的 triage 状态见
> [`docs/SUPPLY_CHAIN.md`](docs/SUPPLY_CHAIN.md)（cargo-deny + cargo audit
> 双工具每次提交都跑，发现即记录在案）。

---

## 支持的版本

| 版本 | 是否支持 |
| --- | --- |
| main 分支当前状态 | ✅ 支持 |
| 0.1.x | ✅ 支持（仓库唯一的版本线，见 `gateway/Cargo.toml`） |
| < 0.1.0 | ❌ 不存在（本仓从未切过 release、无 tag） |

0.x 阶段不承诺 API 稳定，但**安全修复会优先合入 main**，不受功能排期影响。

---

## 如何报告漏洞

**请勿直接开 public Issue**——Issue 是公开的，细节会在修复前暴露。

首选渠道（按顺序）：

1. **GitHub Private Vulnerability Reporting**：仓库页 → **Security** 标签 →
   **Report a vulnerability**（需登录 GitHub）。报告仅维护者可见，
   修复后可协商公开时间线。
2. 若上述入口不可用（如权限问题）：开一个**不含任何技术细节**的 Issue，
   标题注明 `[SECURITY] 请求私下联系`，维护者会开私下通道跟进。
   Issue 正文**只写一句话**："发现疑似安全问题，请求私下沟通渠道。"
   不要贴 PoC、不要贴调用栈、不要贴影响分析。

报告请包含（私下渠道中）：
- 影响的组件与版本（`gateway/Cargo.toml` 的 version + 相关 commit）
- 复现步骤（越小越好，最好是单条命令或最小配置）
- 你判断的影响范围（RCE / 越权 / 信息泄露 / DoS / 供应链）
- 是否已公开（若已公开请给链接，响应会加速）

---

## 响应承诺

| 阶段 | 时限 |
| --- | --- |
| 确认收到 | 3 个工作日内 |
| 初步评估（是否成立、严重度） | 7 个工作日内 |
| 修复或缓解方案 | 按严重度：Critical 14 天内，High 30 天内，Medium/Low 随常规迭代 |
| 公开披露 | 修复合并后协商，默认修复后 30 天内发 advisory |

以上是维护者的公开承诺。若超时未响应，报告人可自行选择公开披露——
这是你的权利，本文件明确授予。

---

## 范围

**在范围内**：
- `gateway/` 的代理数据面（鉴权绕过、越权访问租户、SSRF、请求走私、
  TLS 验证绕过、DoS）。
- 认证与多租户逻辑（`tenant.rs`、`gateway.rs` 鉴权映射）。
- 依赖供应链（参考 `docs/SUPPLY_CHAIN.md` 的已知清单报**新增**问题，
  重复报已知项会被直接引用已有 triage 关闭）。
- 5 语言 SDK 的凭据处理（`tools/ipp_sdk*`）。

**不在范围内**（请直接报上游）：
- pingora / tokio / rustls 等上游 crate 自身的漏洞（请报 RustSec 或上游仓库；
  但若你确认它经由本仓可达，欢迎同时私下告知，我们会加速升级）。
- 免费代理源本身的恶意行为（那是公网数据源的质量问题，不是本仓漏洞；
  请走正常 Issue）。
- 需要物理接触运行机器、社会工程、或纯理论无 PoC 的"可能有问题"。

---

## 已知问题（透明披露，不重复 triage）

以下问题已在 `docs/SUPPLY_CHAIN.md` §2~§4 留痕，**无需重复报告**，
重复报告会被直接引用已有结论关闭：

- RUSTSEC-2026-0033 / 0034（pingora-core 请求走私）：代理热路径可达，
  待 pingora 0.6 → 0.9 框架升级修复（R17）。
- RUSTSEC-2026-0035（pingora-cache 缓存投毒）：代码零引用，随升级解决。
- RUSTSEC-2024-0437（protobuf 递归崩溃）：经 prometheus 间接引入，随升级解决。
- 7 个无维护传递依赖：随升级重 triage。

这些 advisory 本来就是公开的（RustSec DB），隐瞒没有意义。
本文件的存在意义是给**未知**问题一条私下通道。

---

## 安全相关的仓库配置现状（诚实标注）

| 项 | 状态 |
| --- | --- |
| Private vulnerability reporting | 已尝试经 API 启用，未能直接确认；Security 标签页为准 |
| Dependabot alerts / security updates | 未启用（依赖更新目前靠人工 `cargo update` + 门禁复核） |
| Secret scanning / push protection | 未启用（仓库无机密：API Key 均为运行时环境变量，见 `tools/check_no_plaintext_creds.py` 门禁） |
| 供应链门禁 | ✅ `cargo deny` + `cargo audit` 每次提交都跑（CI `supply-chain` job） |

Dependabot 与 secret scanning 未启用是**已知缺口**，不是疏忽：
前者与现有的 cargo-deny advisories 门禁重复（RustSec DB 是同一数据源），
后者在本仓无机密可扫。引入前会先评估误报成本，暂不引入。
