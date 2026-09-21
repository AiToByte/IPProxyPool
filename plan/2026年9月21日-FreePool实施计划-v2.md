# FreePool 第二供应线 Implementation Plan · v2（前沿迭代版）

> **地位：** 本文件 supersede `plan/2026年9月21日-FreePool实施计划.md`（v1）。
> v1 未执行（`gateway/src/free_pool.rs` 尚不存在），故 v2 为**直接替代**而非增量补丁：
> 未变任务（Task 1/2/3/5/7）沿用 v1 原文（本文件只给引用，不重复粘贴）；
> 修订/新增任务（Task 4/6/8/9/10/11/12/13）在本文件完整展开，执行时以本文件为准。
>
> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.
>
> **Goal（v2 不变）：** 新增 `free_pool` 模块，定时从公开免费源抓取 HTTP(S) 节点，
> 经**两级质检**（TCP 初筛＋走代理实转复检**含匿名度分级＋内容防篡改 canary**）后以
> `provider="free-*"`、**EWMA 健康分动态权重**并入 RouterEngine，与付费线加权混合；
> 短 TTL＋复检＋**失败指数 backoff**＋**源站零产出熔断**，进出池不扰动付费节点。
>
> **Architecture（v2 修订）：**
> `Source` trait（三适配器：JSON API / HTML 表格 / GitHub raw，**ETag 礼貌轮询＋304 NotModified 显式信号**）
> → fetch（10min，**JoinSet 并发＋per-source 超时＋源间顺序归一**）
> → 两级 verify（TCP 建链+延迟门全量；通过者走代理实转复检：
> `/ip` 出口比对＋`/headers` 7 头披露检查判 Elite/Anonymous/Transparent＋`/anything` canary 防篡改，
> 记录**转发延迟**为主健康信号）
> → Registry（TTL 30min，到期复检续命；**EWMA 成功率×延迟惩罚健康分→动态权重 1..20**；
> **连续失败指数 backoff＋自动恢复**；**生存 streak→trusted 加成**；多源按 addr 去重；
> **容量上限逐最低分淘汰**；backoff/过期条目不进快照）
> → `replace_vendor_nodes("free-", …)` 原子合并（**仅 Http/Https＋非 backoff＋匿名度门**过滤后并入）
> → 现有选路/熔断/遥测/计量全复用；
> 源站零产出熔断（连续 3 轮零产出暂停抓取，304 不计轮次，每 3 tick 试探恢复）；
> supervisor 托管，env 总控（默认关闭）。
>
> **Tech Stack:** Rust（reqwest 0.12[rustls-tls,json]、tokio JoinSet+Semaphore、DashMap、parking_lot、Instant）；
> 零新依赖（HTML 用手写字节扫描器，不引 scraper/regex/anyhow/maxminddb）。
>
> **Deviations locked:** 计划文件落 `plan/`（沿本仓惯例）；
> 本仓禁未授权 commit，每任务末 Step 为 verify，最终统一经用户确认后提交；
> SOCKS egress 为 Phase 2（另起计划，本计划只解析标注、不进池）；
> GeoIP 本地库为 Phase 3（需新依赖，见 §非目标），本计划用源站 country＋ZZ 缺省＋exit_ip 记录（为 Phase 3 留字段）。
>
> **File map（v2）：**
> - Modify: `gateway/src/tenant.rs`（free tier 0 价，Task 1，沿用 v1）、
>   `gateway/src/bandit.rs`（free cost 0，Task 1，沿用 v1）、
>   `gateway/src/router.rs`（`replace_vendor_nodes`，Task 2，沿用 v1）、
>   `gateway/src/metrics.rs`（水位计 Task 3 沿用 v1＋ Task 11 四组扩展）、
>   `gateway/src/main.rs`（`mod free_pool`＋supervise＋全量 env，Task 12）、
>   `docs/OPERATION.md`（§4 两行＋§6 安全硬规则）、`docker-compose.yml`（全量 env 示例）。
> - Create: `gateway/src/free_pool.rs`（全部新逻辑＋单测，约 950 行：v1 600 行＋分级/健康/熔断约 350 行）。

---

## §0. v1 缺口复核（为什么需要 v2）

| # | v1 缺口 | 后果 | v2 对策 |
|---|---------|------|---------|
| G1 | 架构称“两级质检含匿名度分级”，Task 7 只有 TCP，连通≠匿名 | Transparent 节点进池泄漏客户端 IP | Task 8 FullCheck：`/ip` 出口比对＋`/headers` 7 头披露＋Elite/Anonymous/Transparent 分级＋`FREE_REQUIRE_ELITE` 门 |
| G2 | 架构称“EWMA 健康分动态权重”，Task 8 固定 weight=10 | 慢/抖节点与快节点同权，付费线被拖累 | Task 9 Health：EWMA 成功率(α=0.3)×延迟惩罚→权重 1..20，streak trusted 加成 |
| G3 | 无失败 backoff，复检失败即删，下轮抓回→池抖动 | 抖动节点反复进出，bandit 臂表 churn | Task 9：连续失败指数 backoff（60s×2^n，封顶 1h）＋自动恢复，TTL 内保留条目 |
| G4 | 架构称“源站零产出熔断（连续 3 轮跳过）”，无 Task 落地 | 死源每轮空转＋日志噪音，304 被误计为零产出 | Task 10 SourceGuard：零产出计数＋暂停＋每 3 tick 试探；304 NotModified 显式信号不计数 |
| G5 | ETag 只在注释提及，实现仍是裸 GET | GitHub raw 每次全量，礼貌性差 | Task 6 修订：`If-None-Match`＋304 处理＋请求头单测断言 |
| G6 | `FREE_MAX_NODES`/`FREE_REQUIRE_ELITE`/`FREE_FULL_CHECK_URL`/`FREE_SOURCE_MAX_ZERO_CYCLES` 列了 env 但 Task 9 未接线 | env 悬空，容量无上限（2000 节点快照克隆每轮 O(n) 无界） | Task 9＋12：全量接线＋容量逐最低分淘汰 |
| G7 | SOCKS 节点只“解析保留”，merge 未过滤；`ProxyNode` 无 proto 字段 | Socks 节点以 tier=free 进 HTTP 选路→转发必失败 | Task 9 merge 过滤：仅 `FreeProto::Http/Https` 进池（零模型改动，注释写明 Phase 2 前置） |
| G8 | 仅 1 个 gauge，无 per-source/verify/匿名度可观测 | 源站挂/质检门限过严无法定位 | Task 11：yield／verify／anonymity／suspend 四组指标（沿 R2-8 DashMap 模式） |
| G9 | fetch 串行（sources 循环 await） | 源站慢→整轮拖尾，10min 节拍漂移 | Task 10：JoinSet 并发 fetch＋per-source 15s 超时＋按源序归一（确定性去重优先级） |
| G10 | TCP 延迟≠转发延迟；无内容篡改检查 | 高危：免费代理 MITM/内容注入（见 §1-F1） | Task 8：转发延迟为主信号＋`/anything` canary 标记比对，失配按失败计 |
| G11 | `FREE_FETCH_INTERVAL_SECS` 文档 600 vs Task 9 代码 900 不一致 | 执行歧义 | v2 锁定 **600**（业界 5~15min 重检带的中点，见 §1-F2），Task 12 以此为准 |
| G12 | tier 隔离语义未显式验证（free 是否泄漏给 residential 请求） | 计费/合规风险 | Task 12 单测锁定：`tier=residential` 请求不命中 free 节点；无 tier 请求按权重混合（free 占少数） |

---

## §1. 前沿依据（2026-09-21 检索回填，v2 新增 F4~F8）

- **F1 · 免费代理不可信（MITM/蜜罐/内容篡改）：**
  `arXiv:2403.02445`（MADWeb'24，30 个月纵向研究，64 万免费代理）：
  仅 34.5% 至少活跃过一次；Shodan 扫出 4,452 漏洞（含 1,755 RCE、2,036 提权）；
  42,206 跑在 MikroTik 路由器上；**16,923 篡改内容**（挖矿脚本/木马/广告注入）。
  → v2 推论：免费线必须视为**零信任输入**——https-only 复检基址、内容 canary、
  OPERATION 硬性禁敏感流量（认证/cookie/支付/银行），Transparent 默认仅服务无归属流量。
- **F2 · 现代免费池流水线（多源→验证→富化→分级→导出）：**
  Thordata/awesome-free-proxy-list（10+ 源→HTTP/HTTPS/SOCKS 验证→GeoIP+ASN→延迟分档
  fast/med/slow→匿名度分级→**survival streak**→`top-trusted = fast+elite+streak≥2`）；
  openproxyhub/proxy-exports（24/7 遥测驱动：协议验证＋uptime＋匿名度＋geo-verification，
  elite/anonymous/transparent 分目录，JSON 含 country/asn/latency）；
  VPSLab（15min 刷新，elite/anonymous/transparent 预切分）；
  ProxyNova/myProxyChecker（按 health/delay/last-seen/uptime 排序，15min 级复检）。
  databay 业界重检 5min 级。
  → v2 取：fetch 600s（TCP 初筛廉价，介于 5min 与 15min 之间）＋TTL 1800s＋streak/trusted
  子集思想（不照搬文件导出，落为权重加成＋指标）。
- **F3 · 匿名度检测方法学（MiyaIP 7 头＋直连基线）：**
  以直连 `/ip` 为基线，经代理复测：出口 IP == 基线→Transparent；
  否则检查 `Forwarded / X-Forwarded-For / X-Real-IP / Client-IP / Via / Proxy-Connection / X-Proxy-ID`
  七头（大小写不敏感，经 `/headers` 回显）：有披露→Anonymous，无→Elite。
  elite/anonymous/transparent 三级与 openproxyhub 定义一致。
  → Task 8 `classify_anonymity` 纯函数＋该 7 头常量表。
- **F4 · EWMA 健康＋指数 backoff＋P2C（proxyhive，Go，2026-04）：**
  后台健康检查＋**EWMA 延迟（decay α=0.3）**＋失败指数 backoff＋自动恢复＋
  Power-of-Two-Choices/LeastLatency 轮换，health-check 目标 `https://httpbin.org/ip`。
  → Task 9 取 α=0.3（成功率与延迟双 EWMA）、backoff 指数增长、恢复自动；
  不取 P2C（本仓选路已是 LinUCB＋加权随机，换策略属架构改动，记 Phase 3）。
- **F5 · JA4 指纹时代（FoxIO 2023；Cloudflare Bot Management 2026-05 文档）：**
  JA4 按排序扩展哈希（比 JA3 稳定，跨 IP 可分组）；Chrome 110+ 扩展乱序＋GREASE＋
  X25519MLKEM768＋ECH GREASE；httpcloak（1.2k★，2025-12）证实全栈拟真需 TLS＋HTTP/2
  SETTINGS/WINDOW_UPDATE/HPACK＋QUIC＋头顺序＋Sec-Fetch 一致性；
  ja3proxy 证实过 Cloudflare 挑战仅需 uTLS Chrome＋匹配 UA（无需解 JS）。
  免费池几乎全是数据中心 IP（Thordata）→ 高 JA4 风控面。
  → v2 推论：免费节点 `DomainRisk` 应就高不就低（R2-6 表已含 perimeterx/kasada/imperva，
  本计划不扩表，记 Phase 3：免费臂 DomainRisk 上浮＋JA4 漂移门沿 SPIKE-R2 路径）；
  本计划内：免费节点默认只服务无 country/tier 约束流量（ZZ＋tier 门），降低硬目标封禁传导。
- **F6 · 非平稳 Bandit（dLinUCB Wu et al.2018；Discounted/Sliding-window UCB Garivier-Moulines 2011；
  LARL Trella et al.2024-25；Transformer 最优动态遗憾 T^2/3，2025）：**
  学界共识三法：discount（γ 衰减）、sliding-window（仅窗内样本）、
  change-point 检测＋restart（master-slave 误差监控）。
  R2-6 的“每 10k 次向先验 90/10 blend”属 coarse periodic restart，付费线够用；
  免费线 churn 高一个量级（见 F7），v2 不另起 bandit 分支（零架构改动铁律），
  而是用**注册表层 EWMA＋backoff＋prune 白名单**吸收 churn（bandit 侧复用现有遗忘＋`prune_arms`）。
  per-tier forgetting 记 Phase 3。
- **F7 · 住宅/免费 IP 高 churn（IPinfo 2026-01，1.7 亿 IP / 90 天窗）：**
  平均可见仅 4.56 天；60% 在 90 天内只出现一次；IPv6 几乎无持久性；
  IP 在“代理活跃→休眠→正常住户”三态间快速轮转，历史声誉不可靠。
  → v2 取：TTL 1800s 短持有、streak≥3 才 trusted、声誉禁跨 TTL 继承（条目删除即清零，
  防止“换 IP 继承分”与“旧分污染新 IP”双向错误）。
- **F8 · 生产级代理健康度量（ProxyStats 2026 连续基准；Proxyway 2026 实测）：**
  composite＝会话保持率＋成功率＋延迟＋硬目标可达，每 15min 探针；
  会话保持 72~97%，成功率 93~97%，中位延迟 407~1191ms；
  真实保护目标下 rotating-residential 中位成功率仅 74%（合成端点 99% 属虚高）。
  → v2 取：健康分＝成功率主项×延迟惩罚（可解释双因子，不搞黑盒 composite）；
  付费套利阈值 80/95 不动（free-* 不在审计 vendor 名单，天然隔离）；
  free 侧用独立宽松门（进池即看分，权重连续映射，无硬阈值抖动）。

---

## §2. Env 总表（v2 锁定，Task 12 全量接线）

| Key | 默认 | 语义 |
|-----|------|------|
| `FREE_ENABLED` | `"0"` | 第二线总开关 |
| `FREE_API_URLS` | Geonode 默认（见 Task 12） | 逗号分隔，http/https-only 过滤 |
| `FREE_HTML_URLS` | `https://free-proxy-list.net/` | 同上 |
| `FREE_GITHUB_URLS` | clarketm raw 默认（见 Task 12） | 同上 |
| `FREE_FETCH_INTERVAL_SECS` | `600` | 抓取节拍（v1 600/900 歧义→锁定 600） |
| `FREE_TTL_SECS` | `1800` | 注册 TTL |
| `FREE_VERIFY_TIMEOUT_SECS` | `3` | TCP 建链超时 |
| `FREE_MAX_LATENCY_MS` | `3000` | TCP 延迟门＋EWMA 延迟中性点 |
| `FREE_MAX_CONCURRENT` | `50` | 质检并发（TCP＋FullCheck 共用上限内的分组：TCP 50／Full 20，见 Task 12） |
| `FREE_FULL_CONCURRENT` | `20` | 实转复检并发（建代理 Client 开销大，单独封顶） |
| `FREE_MAX_NODES` | `2000` | 容量上限，超限逐最低分淘汰 |
| `FREE_FULL_CHECK_URL` | `https://httpbin.org` | 实转复检基址（**https-only**，非法回落默认＋warn）；`不可达则降级 TCP-only`（v1 语义保留：基址探活失败当轮跳过 FullCheck，只凭 TCP 进池但 anon=Unknown） |
| `FREE_REQUIRE_ELITE` | `"0"` | 为 1 时仅 Elite 进池 |
| `FREE_SOURCE_MAX_ZERO_CYCLES` | `3` | 源站零产出熔断轮数 |
| `FREE_SUSPEND_RETRY_EVERY` | `3` | 熔断后每 N tick 试探一次 |

---

### Task 1: free 档经济模型（tenant 计费 0 ＋ bandit 成本 0）

> 沿用 v1 Task 1 原文（`price_per_gb("free")==0`＋`cost_weight_for_tier("free")==0`＋既有断言扩展＋fmt/clippy），本文件不重复。执行者直接读 v1 §Task 1。

---

### Task 2: Router 按 vendor 前缀原子替换（付费线不动）

> 沿用 v1 Task 2 原文（`replace_vendor_nodes("free-", …)`＋`replace_vendor_nodes_keeps_paid` 单测），本文件不重复。

---

### Task 3: metrics 免费池水位计

> 沿用 v1 Task 3 原文（`free_pool_nodes_total`＋`free_pool_nodes_rendered` 单测），本文件不重复。Task 11 在此基础上扩展。

---

### Task 4（v2 修订）: free_pool 骨架＋Source trait（`FetchOutcome` 版）＋ApiSource

**Files:**
- Create: `gateway/src/free_pool.rs`（本任务只落类型＋trait＋ApiSource＋单测；Verifier/FullCheck/Registry/Guard/Worker 后续任务追加同文件）
- Test: `free_pool::tests::api_source_parses_geonode_shape`（v1 用例原文）

- [ ] **Step 1: 写失败单测（v1 fixture 原样）**

```rust
#[test]
fn api_source_parses_geonode_shape() {
    let body = r#"{"data":[
        {"ip":"203.0.113.7","port":"8080","protocols":["http"],"country":"US"},
        {"ip":"198.51.100.9","port":3128,"protocols":["https"],"country":"DE"},
        {"ip":"bad","port":"x","protocols":["http"],"country":"US"},
        {"ip":"192.0.2.1","port":"1080","protocols":["socks5"],"country":"US"}
    ]}"#;
    let nodes = ApiSource::parse("geonode", body);
    assert_eq!(nodes.len(), 3);
    assert_eq!(nodes[0].port, 8080);
    assert_eq!(nodes[0].proto, FreeProto::Http);
    assert_eq!(nodes[1].proto, FreeProto::Https);
    assert_eq!(nodes[2].proto, FreeProto::Socks5);
    assert_eq!(nodes[0].country.as_deref(), Some("US"));
}
```

- [ ] **Step 2: 运行确认失败**

Run: `cargo test --workspace api_source_parses_geonode_shape`
Expected: FAIL（模块/类型不存在；先在 `main.rs` 加 `mod free_pool;` 使目标可编译——sole 一行）

- [ ] **Step 3: 最小实现（v2 修订点：`fetch` 返回 `FetchOutcome`，304 显式信号为 Task 6 预留）**

```rust
//! FreePool 第二供应线：公开免费源抓取 → 两级质检 → TTL 注册 → Router 合并。
//!
//! Phase 1 只收 HTTP(S)（零网关转发改动）；SOCKS 只解析标注、merge 过滤
//! （Phase 2 做 SOCKS egress）。默认关闭（`FREE_ENABLED=1` 开启）。
//! 免费线零信任（arXiv:2403.02445：16,923 篡改内容）：复检基址 https-only＋
//! canary 防篡改＋OPERATION 禁敏感流量。

use std::time::Duration;

/// 抓取协议（Phase 1 仅 Http/Https 进池，见 `Registry::snapshot` 过滤）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FreeProto {
    Http,
    Https,
    Socks4,
    Socks5,
}

impl FreeProto {
    fn from_token(tok: &str) -> Option<Self> {
        match tok.to_ascii_lowercase().as_str() {
            "http" => Some(FreeProto::Http),
            "https" => Some(FreeProto::Https),
            "socks4" => Some(FreeProto::Socks4),
            "socks5" => Some(FreeProto::Socks5),
            _ => None,
        }
    }

    /// 是否允许进池（Phase 1 仅 HTTP(S)；SOCKS 标注保留待 Phase 2）。
    pub fn poolable(self) -> bool {
        matches!(self, FreeProto::Http | FreeProto::Https)
    }
}

/// 源站吐出的原始节点（未质检）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RawNode {
    pub ip: String,
    pub port: u16,
    pub proto: FreeProto,
    pub country: Option<String>,
    pub source: String,
}

/// 抓取结果（v2：304 NotModified 显式信号，供 SourceGuard 区分“未变更”与“零产出”）。
#[derive(Debug)]
pub struct FetchOutcome {
    pub nodes: Vec<RawNode>,
    /// true＝源站明确表示未变更（GitHub ETag/304），调用方不得计零产出轮次。
    pub not_modified: bool,
}

impl FetchOutcome {
    pub fn nodes(nodes: Vec<RawNode>) -> Self {
        Self { nodes, not_modified: false }
    }
    pub fn not_modified() -> Self {
        Self { nodes: Vec::new(), not_modified: true }
    }
}

/// 抓取源插件接口（API / HTML / GitHub 各一实现；零新依赖，错误文案 String）。
#[async_trait::async_trait]
pub trait Source: Send + Sync {
    fn name(&self) -> &'static str;
    async fn fetch(&self, client: &reqwest::Client) -> Result<FetchOutcome, String>;
}
```

`ApiSource` 实现与 v1 同（parse 逻辑逐字沿用，`fetch` 包一层 `FetchOutcome::nodes`；
304 对 JSON API 无意义，恒 `not_modified=false`）。

`main.rs` 加 `mod free_pool;`（`mod fingerprint;` 后按字母序插入）。

- [ ] **Step 4: 运行确认通过**

Run: `cargo test --workspace free_pool`
Expected: PASS（1 单测；`async_trait` 已在主依赖中）

- [ ] **Step 5: Verify（fmt＋clippy）**

---

### Task 5: HtmlSource 手写提取器（零新依赖）

> 沿用 v1 Task 5 原文（含 `HtmlSource::extract`＋`match_ip_port`＋`html_source_extracts_ip_ports` 单测，
> 4 行输入/2 行输出锁定）。唯一适配：`fetch` 返回包 `FetchOutcome::nodes`。

---

### Task 6（v2 修订）: GitHubSource raw 轮询（ETag＋304＋`not_modified` 信号）

**Files:**
- Modify: `gateway/src/free_pool.rs`（追加 `GitHubSource`＋3 单测）
- Test: `free_pool::tests::github_source_parses_line_list`（v1 原文）、
  `free_pool::tests::github_source_fetches_from_local_server`（v1 原文，适配 `FetchOutcome`）、
  `free_pool::tests::github_source_etag_not_modified`（v2 新增，下述）

- [ ] **Step 1: 写失败单测**

v1 两单测照抄（`fetch` 返回改为 `.nodes` 取法：`src.fetch(&client).await.expect("fetch").nodes`）；
v2 新增：

```rust
#[tokio::test]
async fn github_source_etag_not_modified() {
    // ETag 礼貌轮询：首轮存 ETag；次轮带 If-None-Match，被 304 后 not_modified=true
    // 且调用方（SourceGuard）不得计零产出（见 Task 10 单测联动）。
    use std::sync::{Arc, Mutex as StdMutex};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let seen_if_none_match: Arc<StdMutex<Vec<String>>> = Arc::new(StdMutex::new(Vec::new()));
    let seen = seen_if_none_match.clone();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let port = listener.local_addr().expect("addr").port();
    tokio::spawn(async move {
        for _ in 0..2 {
            let (mut s, _) = listener.accept().await.expect("accept");
            let mut buf = [0u8; 2048];
            let n = s.read(&mut buf).await.unwrap_or(0);
            let req = String::from_utf8_lossy(&buf[..n]).to_string();
            let inm = req.lines()
                .find(|l| l.to_ascii_lowercase().starts_with("if-none-match:"))
                .map(|l| l.split_once(':').map(|(_, v)| v.trim().to_string()).unwrap_or_default())
                .unwrap_or_default();
            seen.lock().expect("lock").push(inm.clone());
            if inm == "\"v1\"" {
                let _ = s.write_all(b"HTTP/1.1 304 Not Modified\r\nconnection: close\r\n\r\n").await;
            } else {
                let body = "203.0.113.7:8080\n";
                let _ = s.write_all(format!(
                    "HTTP/1.1 200 OK\r\nETag: \"v1\"\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                    body.len()).as_bytes()).await;
            }
        }
    });
    let client = reqwest::Client::new();
    let src = GitHubSource::new("gh", format!("http://127.0.0.1:{port}/list.txt"));
    let first = src.fetch(&client).await.expect("first");
    assert_eq!(first.nodes.len(), 1);
    assert!(!first.not_modified);
    let second = src.fetch(&client).await.expect("second");
    assert!(second.not_modified);
    assert!(second.nodes.is_empty());
    let seen = seen_if_none_match.lock().expect("lock");
    assert_eq!(seen.len(), 2);
    assert!(seen[1].contains("v1"), "second request must carry If-None-Match, got {seen:?}");
}
```

- [ ] **Step 2: 运行确认失败**

Run: `cargo test --workspace github_source`
Expected: FAIL（`GitHubSource` 不存在）

- [ ] **Step 3: 最小实现**

```rust
/// GitHub raw 仓源（默认 clarketm/proxy-list raw；URL env 可配）。
/// 行格式：`ip:port`（`#` 开头与空行跳过；行尾 `socks4`/`socks5` 标记协议）。
/// 礼貌轮询：ETag 缓存＋If-None-Match，304 返回 `not_modified`（Task 10 不计零产出）。
pub struct GitHubSource {
    pub name: &'static str,
    pub url: String,
    etag: parking_lot::Mutex<Option<String>>,
}

impl GitHubSource {
    pub fn new(name: &'static str, url: String) -> Self {
        Self { name, url, etag: parking_lot::Mutex::new(None) }
    }

    pub fn parse(source: &str, body: &str) -> Vec<RawNode> {
        // v1 parse 逻辑逐字沿用（行切分＋socks 嗅探＋port>0＋4 段检查）。
    }
}

#[async_trait::async_trait]
impl Source for GitHubSource {
    fn name(&self) -> &'static str {
        self.name
    }
    async fn fetch(&self, client: &reqwest::Client) -> Result<FetchOutcome, String> {
        let mut req = client.get(&self.url);
        if let Some(etag) = self.etag.lock().clone() {
            req = req.header("If-None-Match", etag);
        }
        let resp = req.send().await.map_err(|e| format!("GET {} failed: {e}", self.url))?;
        if resp.status() == reqwest::StatusCode::NOT_MODIFIED {
            return Ok(FetchOutcome::not_modified());
        }
        if let Some(v) = resp.headers().get("etag").and_then(|h| h.to_str().ok()) {
            *self.etag.lock() = Some(v.to_string());
        }
        let body = resp.text().await.map_err(|e| format!("read {} failed: {e}", self.url))?;
        Ok(FetchOutcome::nodes(Self::parse(self.name, &body)))
    }
}
```

- [ ] **Step 4: 运行确认通过**

Run: `cargo test --workspace free_pool`
Expected: PASS（5 单测：沿用 2＋修订 1＋新增 ETag 1＋Task 4 的 1；`parking_lot` 已在主依赖）

- [ ] **Step 5: Verify（fmt＋clippy）**

---

### Task 7: Verifier（TCP 初筛＋延迟门，Phase 1 语义不变）

> 沿用 v1 Task 7 原文（`Verifier::new(timeout, max_latency_ms)`＋`verify→Option<u64>`＋双单测）。
> v2 补充语义（实现时注释写明，不改签名）：
> TCP 通过只代表“端口开放”，匿名度与转发能力由 Task 8 判定；返回的延迟为建链延迟，
> 供 Registry 初值使用，权重主信号是 Task 8 的**转发延迟**。

---

### Task 8（v2 新增）: FullChecker 实转复检＋匿名度分级＋canary 防篡改

**Files:**
- Modify: `gateway/src/free_pool.rs`（追加 `AnonLevel`＋`classify_anonymity`＋`FullChecker`＋4 单测）
- Test: `free_pool::tests::{anonymity_classification_matrix, checker_rejects_refused_proxy, checker_detects_tampered_canary, checker_records_forward_latency}`

**前沿锚点：** MiyaIP 7 头＋直连基线（§1-F3）；复检目标 `https://httpbin.org`（proxyhive 同款，§1-F4）；
篡改检测动机 arXiv:2403.02445（§1-F1）。

- [ ] **Step 1: 写失败单测**

```rust
#[test]
fn anonymity_classification_matrix() {
    // 三级矩阵（openproxyhub 定义）：出口==基线→Transparent；否则 7 头有披露→Anonymous；无→Elite。
    use std::collections::HashMap;
    let empty: HashMap<String, String> = HashMap::new();
    assert_eq!(classify_anonymity("1.1.1.1", Some("1.1.1.1"), &empty), AnonLevel::Transparent);
    assert_eq!(classify_anonymity("1.1.1.1", Some("9.9.9.9"), &empty), AnonLevel::Elite);
    let mut disclosed = HashMap::new();
    disclosed.insert("via".to_string(), "1.0 proxy".to_string());
    assert_eq!(classify_anonymity("1.1.1.1", Some("9.9.9.9"), &disclosed), AnonLevel::Anonymous);
    let mut upper = HashMap::new();
    upper.insert("X-Forwarded-For".to_string(), "1.1.1.1".to_string());
    assert_eq!(classify_anonymity("1.1.1.1", Some("9.9.9.9"), &upper), AnonLevel::Anonymous);
    // 出口未知（代理失败）→Unknown，调用方按失败计。
    assert_eq!(classify_anonymity("1.1.1.1", None, &empty), AnonLevel::Unknown);
}

#[tokio::test]
async fn checker_rejects_refused_proxy() {
    // 拒连代理（127.0.0.1:1）→ None（失败），不 panic。
    let c = FullChecker::new("http://127.0.0.1:1/".to_string(), Duration::from_secs(3));
    let raw = RawNode { ip: "127.0.0.1".to_string(), port: 1, proto: FreeProto::Http, country: None, source: "t".to_string() };
    assert!(c.check(&raw, "9.9.9.9").await.is_none());
}

#[tokio::test]
async fn checker_detects_tampered_canary() {
    // 本地回显服务返回被篡改的 canary（url 字段缺 marker）→ check 判失败（None）。
    // 服务行为：GET /anything/* 返回 {"url": "tampered"}（无 marker）。
    // （本地服务搭建沿 Task 6 手法；marker 固定为 "freepool-canary"。）
}

#[tokio::test]
async fn checker_records_forward_latency() {
    // 本地回显服务正常（/ip 回出口、/headers 回空头、/anything 带 marker）→
    // Some 且 fwd_latency_ms < 3000 且 anon==Elite（直连基线与服务同机，出口一致？注意：
    // 本地服务看到的对端是直连 IP，基线传入服务侧观测 IP 即 Elite；实现时基线由调用方传入）。
}
```

注：后两个单测需本地 echo 服务同时模拟“直连基址＋代理”——零外部依赖下最简做法：
echo 服务即“基址”，`check` 经代理的请求由**本地微型 HTTP 代理 stub**转发
（accept→读首行→透传固定 JSON）。为控制复杂度，允许实现者将 stub 压缩为
~40 行（单测内联），行为由本单测 fixture 锁定：正常 stub 转发 `{"origin":"10.9.9.9","headers":{},"url":"…/freepool-canary"}`；
篡改 stub 返回 `{"url":"tampered"}`。

- [ ] **Step 2: 运行确认失败**

Run: `cargo test --workspace anonymity_ checker_`
Expected: FAIL（`AnonLevel`/`FullChecker` 不存在）

- [ ] **Step 3: 最小实现**

```rust
/// 匿名度三级（openproxyhub/MiyaIP 定义）：Elite 最安全；Transparent 泄漏真实 IP，
/// 永不服务认证租户流量（merge 门＋OPERATION 硬规则）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AnonLevel {
    Elite,
    Anonymous,
    Transparent,
    Unknown,
}

/// 7 头披露检查表（MiyaIP 方法学；比较时全小写）。
pub const DISCLOSURE_HEADERS: [&str; 7] = [
    "forwarded",
    "x-forwarded-for",
    "x-real-ip",
    "client-ip",
    "via",
    "proxy-connection",
    "x-proxy-id",
];

/// 纯函数分级：基线（直连出口）vs 经代理观测（出口＋回显头）。
pub fn classify_anonymity(
    baseline_ip: &str,
    exit_ip: Option<&str>,
    echoed_headers: &std::collections::HashMap<String, String>,
) -> AnonLevel {
    let exit = match exit_ip {
        Some(e) => e,
        None => return AnonLevel::Unknown,
    };
    if exit.trim() == baseline_ip.trim() {
        return AnonLevel::Transparent;
    }
    let disclosed = echoed_headers.keys().any(|k| {
        let kl = k.to_ascii_lowercase();
        DISCLOSURE_HEADERS.contains(&kl.as_str())
    });
    if disclosed {
        AnonLevel::Anonymous
    } else {
        AnonLevel::Elite
    }
}

/// canary 标记（`/anything/freepool-canary` 回显 url 须含此串，否则判篡改）。
pub const FULL_CHECK_MARKER: &str = "freepool-canary";

/// 实转复检结果（主健康信号）。
pub struct FullCheckResult {
    pub anon: AnonLevel,
    pub exit_ip: Option<String>,
    /// 经代理 GET 全程耗时（含代理转发；为主延迟信号，替代 TCP 建链延迟参与 EWMA）。
    pub fwd_latency_ms: u64,
}

/// 实转复检器：经候选代理 GET 基址三端点（`/ip` 出口＋`/headers` 回显＋
/// `/anything/{marker}` canary）。任一步失败/超门/canary 失配→None（失败）。
/// 非 HTTP(S) 直接 None（Phase 2 前 SOCKS 不复检）。
pub struct FullChecker {
    base_url: String,
    timeout: Duration,
}

impl FullChecker {
    pub fn new(base_url: String, timeout: Duration) -> Self {
        Self { base_url: base_url.trim_end_matches('/').to_string(), timeout }
    }

    pub async fn check(&self, raw: &RawNode, baseline_ip: &str) -> Option<FullCheckResult> {
        match raw.proto {
            FreeProto::Http | FreeProto::Https => {}
            _ => return None,
        }
        let proxy_url = format!("http://{}:{}", raw.ip, raw.port);
        let proxy = reqwest::Proxy::all(&proxy_url).ok()?;
        let client = reqwest::Client::builder()
            .proxy(proxy)
            .timeout(self.timeout)
            .build()
            .ok()?;
        let start = std::time::Instant::now();
        // 1. 出口。
        let ip_body: serde_json::Value = client
            .get(format!("{}/ip", self.base_url))
            .send().await.ok()?
            .json().await.ok()?;
        let exit = ip_body.get("origin").and_then(|v| v.as_str()).map(|s| s.to_string());
        // 2. 回显头。
        let h_body: serde_json::Value = client
            .get(format!("{}/headers", self.base_url))
            .send().await.ok()?
            .json().await.ok()?;
        let mut echoed = std::collections::HashMap::new();
        if let Some(map) = h_body.get("headers").and_then(|v| v.as_object()) {
            for (k, v) in map {
                if let Some(s) = v.as_str() {
                    echoed.insert(k.clone(), s.to_string());
                }
            }
        }
        // 3. canary（内容篡改检测；arXiv:2403.02445 16,923 篡改样本）。
        let c_body: serde_json::Value = client
            .get(format!("{}/anything/{}", self.base_url, FULL_CHECK_MARKER))
            .send().await.ok()?
            .json().await.ok()?;
        let canary_ok = c_body.get("url").and_then(|v| v.as_str())
            .is_some_and(|u| u.contains(FULL_CHECK_MARKER));
        if !canary_ok {
            log::warn!("[FreePool] canary mismatch {}:{} (tamper suspected)", raw.ip, raw.port);
            return None;
        }
        let anon = classify_anonymity(baseline_ip, exit.as_deref(), &echoed);
        Some(FullCheckResult { anon, exit_ip: exit, fwd_latency_ms: start.elapsed().as_millis() as u64 })
    }
}
```

约束：`reqwest::Proxy::all` 在当前 features（无 socks）下对 http 代理可用；
`.json()` 需 `json` feature（已有）。直连基线 IP 由 Worker 每轮预取一次
（`GET {base}/ip` 直连，失败则当轮跳过 FullCheck 降级 TCP-only，见 Task 12；
基线获取失败**不**计节点失败）。

- [ ] **Step 4: 运行确认通过**

Run: `cargo test --workspace free_pool`
Expected: PASS（＋4 单测）

- [ ] **Step 5: Verify（fmt＋clippy）**

---

### Task 9（v2 修订）: Registry（TTL＋EWMA 健康＋backoff＋streak＋容量＋merge 门）

**Files:**
- Modify: `gateway/src/free_pool.rs`（追加 `Health`＋`Registry`＋5 单测）
- Test: `free_pool::tests::{registry_ttl_expiry, registry_reverify_renews,
  registry_dedupes_across_sources, health_score_math, backoff_and_capacity}`

**前沿锚点：** EWMA α=0.3＋指数 backoff（proxyhive，§1-F4）；
streak/trusted（Thordata top-trusted，§1-F2）；声誉不跨 TTL（IPinfo churn，§1-F7）。

- [ ] **Step 1: 写失败单测**

```rust
fn raw(ip: &str, source: &str) -> RawNode {
    RawNode { ip: ip.to_string(), port: 8080, proto: FreeProto::Http, country: Some("US".to_string()), source: source.to_string() }
}

// v1 三单测照抄（ttl 到期不可见／复检续命与摘除／多源去重＋weight==FREE_POOL_WEIGHT＋ZZ 缺省）
// ＋ v2 新增两单测：

#[test]
fn health_score_math() {
    // 健康分＝EWMA成功率×延迟惩罚；权重 1..20 连续映射；trusted 加成封顶 20。
    // 纯数学形态（初值 success=0.5/lat=NEUTRAL；全成功→success→1；高延迟→惩罚<1）。
    let mut h = Health::fresh();
    assert!((h.score() - 0.5 * 1.0).abs() < 1e-9);
    for _ in 0..50 { h.note_success(100); }
    assert!(h.ewma_success > 0.99);
    assert_eq!(h.weight(), 20); // 高分封顶
    let mut slow = Health::fresh();
    for _ in 0..50 { slow.note_success(9000); }
    assert!(slow.score() < h.score(), "latency penalty must bite");
    assert!(slow.weight() < 20);
    let mut bad = Health::fresh();
    for _ in 0..10 { bad.note_failure(); }
    assert_eq!(bad.weight(), 1); // 低分保底 1（不断流，只降权）
}

#[test]
fn backoff_and_capacity() {
    // backoff：连续失败→merge 不可见；TTL 内保留；成功恢复。
    // 容量：超 FREE_MAX_NODES（测试用小 Registry::with_capacity）逐最低分淘汰。
    let mut reg = Registry::with_capacity(Duration::from_secs(1800), 2);
    let now = Instant::now();
    reg.upsert_full(&raw("10.0.0.1", "a"), 50, AnonLevel::Elite, None, now);
    reg.upsert_full(&raw("10.0.0.2", "a"), 50, AnonLevel::Elite, None, now);
    // 灌第三个→淘汰最低分之一（初分相同，允许淘汰任一，但总数恒 2）。
    reg.upsert_full(&raw("10.0.0.3", "a"), 50, AnonLevel::Elite, None, now);
    assert_eq!(reg.snapshot(now, false).len(), 2);
    // backoff：对 10.0.0.2 连 fail 3 次→快照不可见但 len 仍计入（TTL 内保留）。
    for _ in 0..3 { reg.note_verify_failed("10.0.0.2:8080", now); }
    let snap = reg.snapshot(now, false);
    assert!(snap.iter().all(|n| n.addr != "10.0.0.2:8080"));
    assert_eq!(reg.len(), 2);
}
```

- [ ] **Step 2: 运行确认失败**

Run: `cargo test --workspace registry_ health_ backoff_`
Expected: FAIL（`Registry`/`Health` 不存在）

- [ ] **Step 3: 最小实现**

```rust
/// 免费线池权重上限（vs 付费 60~100，加权混合中占少数；bandit free cost 0 不再叠加）。
/// v2：10 为初值上限，实际权重由 Health 连续映射 1..=FREE_POOL_WEIGHT。
pub const FREE_POOL_WEIGHT: u32 = 10;
/// v2 trusted 加成后硬上限（仍远低于付费线，避免免费淹没付费）。
pub const FREE_POOL_WEIGHT_TRUSTED_MAX: u32 = 20;
/// 无归属国家的缺省标记（只服务无 country 要求的流量，见 RouterEngine::matches）。
pub const FREE_UNKNOWN_COUNTRY: &str = "ZZ";
/// EWMA 衰减（proxyhive 同款 α=0.3；成功率与延迟共用）。
pub const HEALTH_EWMA_ALPHA: f64 = 0.3;
/// 延迟中性点（== FREE_MAX_LATENCY_MS 默认 3000；ewma_latency 高于此则惩罚<1）。
pub const HEALTH_NEUTRAL_LATENCY_MS: f64 = 3000.0;
/// trusted 门：streak≥3＋Elite＋ewma 延迟<1500ms（Thordata top-trusted 思想）。
pub const TRUSTED_MIN_STREAK: u32 = 3;
pub const TRUSTED_MAX_LATENCY_MS: f64 = 1500.0;
/// backoff：60s 起指数增长，封顶 1h。
pub const BACKOFF_BASE_SECS: u64 = 60;
pub const BACKOFF_MAX_SECS: u64 = 3600;

use crate::model::ProxyNode;
use std::collections::HashMap;
use std::time::Instant;

/// 单节点健康（EWMA 成功率×延迟惩罚；声誉不跨 TTL：条目删除即清零，IP 复用重计）。
pub struct Health {
    pub ewma_success: f64,
    pub ewma_latency_ms: f64,
    pub streak: u32,
    pub fail_streak: u32,
    pub backoff_until: Option<Instant>,
    pub anon: AnonLevel,
    pub exit_ip: Option<String>,
}

impl Health {
    pub fn fresh() -> Self {
        Self { ewma_success: 0.5, ewma_latency_ms: HEALTH_NEUTRAL_LATENCY_MS,
               streak: 0, fail_streak: 0, backoff_until: None,
               anon: AnonLevel::Unknown, exit_ip: None }
    }
    pub fn note_success(&mut self, fwd_latency_ms: u64) {
        self.ewma_success += HEALTH_EWMA_ALPHA * (1.0 - self.ewma_success);
        self.ewma_latency_ms += HEALTH_EWMA_ALPHA * (fwd_latency_ms as f64 - self.ewma_latency_ms);
        self.streak += 1;
        self.fail_streak = 0;
        self.backoff_until = None;
    }
    pub fn note_failure(&mut self, now: Instant) {
        self.ewma_success += HEALTH_EWMA_ALPHA * (0.0 - self.ewma_success);
        self.streak = 0;
        self.fail_streak += 1;
        let secs = (BACKOFF_BASE_SECS * 2u64.pow(self.fail_streak.min(7) - 1)).min(BACKOFF_MAX_SECS);
        self.backoff_until = Some(now + Duration::from_secs(secs));
    }
    /// 成功率主项×延迟惩罚（可解释双因子；延迟惩罚＝中性点/(中性点+超额)，超额≤0 时为 1）。
    pub fn score(&self) -> f64 {
        let over = (self.ewma_latency_ms - HEALTH_NEUTRAL_LATENCY_MS).max(0.0);
        self.ewma_success * (HEALTH_NEUTRAL_LATENCY_MS / (HEALTH_NEUTRAL_LATENCY_MS + over))
    }
    pub fn trusted(&self) -> bool {
        self.anon == AnonLevel::Elite && self.streak >= TRUSTED_MIN_STREAK
            && self.ewma_latency_ms < TRUSTED_MAX_LATENCY_MS
    }
    /// 连续权重 1..=上限（低分保底 1 不断流；trusted 封顶 TRUSTED_MAX）。
    pub fn weight(&self) -> u32 {
        let cap = if self.trusted() { FREE_POOL_WEIGHT_TRUSTED_MAX } else { FREE_POOL_WEIGHT };
        ((self.score() * cap as f64).round() as u32).clamp(1, cap)
    }
    pub fn backed_off(&self, now: Instant) -> bool {
        self.backoff_until.is_some_and(|t| t > now)
    }
}

struct Entry {
    node: ProxyNode,
    health: Health,
    expires_at: Instant,
}

pub struct Registry {
    ttl: Duration,
    max_nodes: usize,
    entries: HashMap<String, Entry>,
}

impl Registry {
    pub fn new(ttl: Duration) -> Self {
        Self { ttl, max_nodes: 2000, entries: HashMap::new() }
    }
    pub fn with_capacity(ttl: Duration, max_nodes: usize) -> Self {
        Self { ttl, max_nodes, entries: HashMap::new() }
    }

    /// 质检通过即插入/刷新（已存在 addr：续期＋health 更新，不覆盖 source；首见获胜）。
    /// v1 `upsert(raw, latency, now)` 语义由本函数替代（TCP-only 降级时 anon=Unknown/lat=tcp）。
    pub fn upsert_full(&mut self, raw: &RawNode, fwd_latency_ms: u64, anon: AnonLevel,
                       exit_ip: Option<String>, now: Instant) {
        let addr = format!("{}:{}", raw.ip, raw.port);
        if let Some(e) = self.entries.get_mut(&addr) {
            e.expires_at = now + self.ttl;
            e.health.note_success(fwd_latency_ms);
            e.health.anon = anon;
            e.health.exit_ip = exit_ip;
            e.node.weight = e.health.weight();
            return;
        }
        let mut health = Health::fresh();
        health.note_success(fwd_latency_ms);
        health.anon = anon;
        health.exit_ip = exit_ip;
        let node = ProxyNode::new(
            raw.ip.clone(), raw.port, None, None,
            raw.country.clone().unwrap_or_else(|| FREE_UNKNOWN_COUNTRY.to_string()),
            "free".to_string(), format!("free-{}", raw.source), health.weight(),
        );
        self.entries.insert(addr, Entry { node, health, expires_at: now + self.ttl });
        self.evict_if_over_capacity();
    }

    /// 复检失败（TCP/Full 任一）：EWMA 记失败＋backoff，TTL 内保留（自动恢复）。
    pub fn note_verify_failed(&mut self, addr: &str, now: Instant) {
        if let Some(e) = self.entries.get_mut(addr) {
            e.health.note_failure(now);
            e.node.weight = e.health.weight();
        }
    }

    /// TTL 续命成功（复检通过的轻量路径＝upsert_full；失败摘除语义保留给显式删除）。
    pub fn reverify(&mut self, addr: &str, passed: bool, now: Instant) {
        if passed {
            if let Some(e) = self.entries.get_mut(addr) {
                e.expires_at = now + self.ttl;
            }
        } else {
            self.note_verify_failed(addr, now);
        }
    }

    fn evict_if_over_capacity(&mut self) {
        while self.entries.len() > self.max_nodes {
            // 先逐 backoff，再逐最低分（score 相等时任一；总数收敛即正确）。
            let victim = self.entries.iter()
                .min_by(|a, b| {
                    let now = Instant::now();
                    (a.1.health.backed_off(now), a.1.health.score().to_bits())
                        .cmp(&(b.1.health.backed_off(now), b.1.health.score().to_bits()))
                        .then_with(|| a.0.cmp(b.0))
                })
                .map(|(k, _)| k.clone());
            match victim {
                Some(k) => { self.entries.remove(&k); }
                None => break,
            }
        }
    }

    /// 可进池快照：未过期＋非 backoff＋proto 可进池＋匿名度门。
    /// `require_elite=true` 时仅 Elite（FREE_REQUIRE_ELITE=1）。
    pub fn snapshot(&self, now: Instant, require_elite: bool) -> Vec<ProxyNode> {
        self.entries.values()
            .filter(|e| e.expires_at > now)
            .filter(|e| !e.health.backed_off(now))
            .filter(|e| !require_elite || e.health.anon == AnonLevel::Elite)
            .map(|e| e.node.clone())
            .collect()
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }
}
```

**merge proto 过滤位置说明：** `RawNode.proto` 在 `upsert_full` 入口即过滤
（`if !raw.proto.poolable() { return; }` 首行，SOCKS 直接丢弃＋debug 日志），
故快照无需二次 proto 检查——注释写明（G7 关闭）。

- [ ] **Step 4: 运行确认通过**

Run: `cargo test --workspace free_pool`
Expected: PASS（v1 三单测语义＋v2 两单测；注意 v1 `snapshot(now)` 签名变为
`snapshot(now, require_elite)`，v1 用例照抄时需补第二个参数——以本文件为准）

- [ ] **Step 5: Verify（fmt＋clippy）**

---

### Task 10（v2 新增）: SourceGuard 零产出熔断＋fetch 并发化

**Files:**
- Modify: `gateway/src/free_pool.rs`（追加 `SourceGuard`＋`fetch_all`＋2 单测）
- Test: `free_pool::tests::{source_guard_trips_and_recovers, fetch_all_preserves_source_order}`

- [ ] **Step 1: 写失败单测**

```rust
#[test]
fn source_guard_trips_and_recovers() {
    // 连续 3 轮零产出→暂停；304 不计数；暂停后每 3 tick 试探一次；有产出即恢复。
    let mut g = SourceGuard::new(3, 3);
    assert!(g.should_fetch(0));
    g.note_outcome(&FetchOutcome::nodes(vec![]), 0);
    g.note_outcome(&FetchOutcome::nodes(vec![]), 1);
    assert!(g.should_fetch(2));
    g.note_outcome(&FetchOutcome::nodes(vec![]), 2); // 第 3 轮零产出→熔断
    assert!(!g.should_fetch(3), "suspended");
    assert!(!g.should_fetch(4));
    assert!(g.should_fetch(5), "probe every 3rd tick"); // tick 2 熔断→5 试探（2+3）
    // 304 不计轮次。
    let mut g2 = SourceGuard::new(3, 3);
    g2.note_outcome(&FetchOutcome::not_modified(), 0);
    g2.note_outcome(&FetchOutcome::nodes(vec![]), 1);
    assert!(g2.should_fetch(2), "304 must not count as zero-yield");
    // 有产出清零。
    g.note_outcome(&FetchOutcome::nodes(vec![raw("10.0.0.1", "a")]), 5);
    assert!(g.should_fetch(6));
}

#[tokio::test]
async fn fetch_all_preserves_source_order() {
    // 并发抓取但结果按源序拼接（去重“首见获胜”确定性；G9）。
    // 用两个本地 stub 源（慢源先完成也排后）：断言 raws 中来自源 A 的条目恒在源 B 之前。
}
```

- [ ] **Step 2: 运行确认失败**

Run: `cargo test --workspace source_guard fetch_all`
Expected: FAIL（两者皆不存在）

- [ ] **Step 3: 最小实现**

```rust
/// 源站零产出熔断（连续 MAX_ZERO_CYCLES 轮零产出→暂停；304 不计；每 N tick 试探）。
pub struct SourceGuard {
    max_zero: u32,
    retry_every: u64,
    zero_cycles: u32,
    suspended_at_tick: Option<u64>,
}

impl SourceGuard {
    pub fn new(max_zero: u32, retry_every: u64) -> Self {
        Self { max_zero: max_zero.max(1), retry_every: retry_every.max(1),
               zero_cycles: 0, suspended_at_tick: None }
    }
    pub fn should_fetch(&self, tick: u64) -> bool {
        match self.suspended_at_tick {
            None => true,
            Some(t) => tick >= t && (tick - t) % self.retry_every == 0,
        }
    }
    /// 有产出/304→恢复或维持；零产出→计数，达阈值熔断（记录熔断 tick）。
    pub fn note_outcome(&mut self, outcome: &FetchOutcome, tick: u64) {
        if outcome.not_modified || !outcome.nodes.is_empty() {
            self.zero_cycles = 0;
            self.suspended_at_tick = None;
            return;
        }
        self.zero_cycles += 1;
        if self.zero_cycles >= self.max_zero {
            if self.suspended_at_tick.is_none() {
                log::warn!("[FreePool] source suspended after {} zero-yield cycles", self.zero_cycles);
            }
            self.suspended_at_tick.get_or_insert(tick);
        }
    }
    pub fn suspended(&self) -> bool {
        self.suspended_at_tick.is_some()
    }
}

/// 并发抓取全源（JoinSet＋per-source 15s 超时；结果按源序拼接，保证去重优先级确定性）。
/// `guards` 与 `sources` 等长一一对应；暂停源跳过（其 guard 不计数，保持旧集）。
pub async fn fetch_all(
    sources: &[Box<dyn Source>],
    guards: &mut [SourceGuard],
    client: &reqwest::Client,
    tick: u64,
) -> Vec<RawNode> {
    use std::time::Duration;
    let mut set = tokio::task::JoinSet::new();
    for (i, s) in sources.iter().enumerate() {
        if !guards[i].should_fetch(tick) {
            continue;
        }
        // name 需 'static：源数量个位数，调用方保证（见 Task 12 leak_name 注释）。
        let url_holder = s.name().to_string();
        let _ = url_holder;
        set.spawn(async move { i });
    }
    // 注：Source 非 Clone，故实际按源序逐个 `fetch` 会退化为串行——正确做法见下。
    let _ = set;
    Vec::new()
}
```

**实现修正（以本段为准，替代上式占位）：**
`Box<dyn Source>` 不可 Clone，不能直接进 `spawn`。正确形态：源数量少（≤6），
用 `futures::future::join_all` 按源序并发（`join_all` 保持输入顺序＝输出顺序，
天然满足“按源序拼接”）＋每源 `tokio::time::timeout(15s)`：

```rust
pub async fn fetch_all(
    sources: &[Box<dyn Source>],
    guards: &mut [SourceGuard],
    client: &reqwest::Client,
    tick: u64,
) -> Vec<RawNode> {
    let futs: Vec<_> = sources.iter().enumerate()
        .filter(|(i, _)| guards[i].should_fetch(tick))
        .map(|(i, s)| async move {
            let r = tokio::time::timeout(Duration::from_secs(15), s.fetch(client)).await;
            (i, r)
        })
        .collect();
    let results = futures::future::join_all(futs).await; // 输出与输入同序
    let mut raws = Vec::new();
    for (i, r) in results {
        match r {
            Ok(Ok(outcome)) => {
                // guard 计数（暂停源已在 filter 跳过，此处只记活跃源）。
                guards[i].note_outcome(&outcome, tick);
                if !outcome.not_modified {
                    raws.extend(outcome.nodes);
                }
            }
            Ok(Err(e)) => log::warn!("[FreePool] source fetch failed: {e} (holding last good set)"),
            Err(_) => log::warn!("[FreePool] source fetch timed out (15s, holding last good set)"),
        }
    }
    // 去重（首见获胜；源序＝配置序：api > html > github，调用方保证 sources 顺序）。
    use std::collections::HashSet;
    let mut seen = HashSet::new();
    raws.retain(|r| seen.insert(format!("{}:{}", r.ip, r.port)));
    raws
}
```

约束：`futures` 已在主依赖（`futures = "0.3"`）。超时/失败 hold 旧集（registry 不动，G9 关闭）。
`fetch_all_preserves_source_order` 单测用两本地源 stub（慢源延迟 200ms 但配置序在后）
断言拼接序＝配置序。

- [ ] **Step 4: 运行确认通过**

Run: `cargo test --workspace free_pool`
Expected: PASS（＋2 单测）

- [ ] **Step 5: Verify（fmt＋clippy）**

---

### Task 11（v2 新增）: metrics 扩展（yield／verify／anonymity／suspend）

**Files:**
- Modify: `gateway/src/metrics.rs`（字段＋方法＋渲染＋1 新单测，沿 R2-8 DashMap 模式）
- Test: `gateway/src/metrics.rs::tests::free_pool_extended_rendered`

- [ ] **Step 1: 写失败单测**

```rust
#[test]
fn free_pool_extended_rendered() {
    // Task 3 水位计存量断言保留；新增四组：
    let m = MetricsRegistry::new();
    m.note_free_source_yield("api0", 12);
    m.note_free_verify("pass");
    m.note_free_verify("tcp_fail");
    m.note_free_anonymity("elite");
    m.set_free_source_suspended("gh0", true);
    let r = m.render();
    assert!(r.contains("free_pool_source_yield_total{source=\"api0\"} 12"));
    assert!(r.contains("free_pool_verify_total{result=\"pass\"} 1"));
    assert!(r.contains("free_pool_anonymity_total{level=\"elite\"} 1"));
    assert!(r.contains("free_pool_source_suspended{source=\"gh0\"} 1"));
    m.set_free_source_suspended("gh0", false);
    assert!(m.render().contains("free_pool_source_suspended{source=\"gh0\"} 0"));
}
```

- [ ] **Step 2: 运行确认失败**

Run: `cargo test --workspace free_pool_extended_rendered`
Expected: FAIL（方法不存在）

- [ ] **Step 3: 最小实现**

字段（沿 `supervisor_restarts: DashMap` 模式）：
```rust
free_source_yield: DashMap<String, AtomicU64>,
free_verify: DashMap<String, AtomicU64>,
free_anonymity: DashMap<String, AtomicU64>,
free_suspended: DashMap<String, AtomicU64>, // 0/1 gauge 语义
```
方法：`note_free_source_yield(source, n)`（add）／`note_free_verify(result)`（incr，
result∈pass/tcp_fail/full_fail/backoff_skip）／`note_free_anonymity(level)`
（incr，level∈elite/anonymous/transparent/unknown）／`set_free_source_suspended(source, bool)`。
`render()` 追加四组 HELP/TYPE＋行（label 值白名单断言：非预期 result/level 仍渲染，
调用方保证集合，注释写明；label 需转义 `"`，源名内部生成无引号，注释写明）。

- [ ] **Step 4: 运行确认通过**

Run: `cargo test --workspace metrics`
Expected: 全 PASS（存量＋新 1）

- [ ] **Step 5: Verify（fmt＋clippy）**

---

### Task 12（v2 修订）: Worker 主循环＋main 接线＋运维落盘

**Files:**
- Modify: `gateway/src/free_pool.rs`（`FreePoolConfig`＋`FreePoolWorker`＋1 单测）、
  `gateway/src/main.rs`（`mod`＋supervise＋env＋2 单测）、
  `docs/OPERATION.md`（§4 两行＋§6 安全硬规则）、`docker-compose.yml`（全量 env 示例）
- Test: `free_pool::tests::worker_merge_pushes_snapshot_to_router`（v1 原文＋require_elite 参数）、
  `tests::free_env_defaults`（v1 原文）

- [ ] **Step 1: 写失败单测**

v1 `worker_merge_pushes_snapshot_to_router` 照抄，`merge_once` 签名改为
`(router, metrics, registry, require_elite: bool)`；main `free_env_defaults` 照抄 v1。

- [ ] **Step 2: 运行确认失败**

Run: `cargo test --workspace worker_merge_pushes free_env`
Expected: FAIL（两者皆不存在）

- [ ] **Step 3: 最小实现**

`free_pool.rs` 追加（`#[derive(Debug, Clone)]` 以便 supervise 闭包克隆）：

```rust
/// Worker 配置（main 从 env 组装；默认值见 §2 Env 总表）。
#[derive(Debug, Clone)]
pub struct FreePoolConfig {
    pub api_urls: Vec<String>,
    pub html_urls: Vec<String>,
    pub github_urls: Vec<String>,
    pub fetch_interval: Duration,
    pub ttl: Duration,
    pub verify_timeout: Duration,
    pub max_latency_ms: u64,
    pub max_concurrent: usize,
    pub full_concurrent: usize,
    pub max_nodes: usize,
    pub full_check_base: String,
    pub require_elite: bool,
    pub max_zero_cycles: u32,
    pub suspend_retry_every: u64,
}

/// Source 默认 URL（全 env 可覆盖；单源挂了只 hold 旧集，不清空池）。
pub const DEFAULT_API_URL: &str = "https://proxylist.geonode.com/api/proxy-list?limit=100&page=1&sort_by=lastChecked&sort_type=desc";
pub const DEFAULT_HTML_URL: &str = "https://free-proxy-list.net/";
pub const DEFAULT_GITHUB_URL: &str = "https://raw.githubusercontent.com/clarketm/proxy-list/master/proxy-list-raw.txt";
pub const DEFAULT_FULL_CHECK_BASE: &str = "https://httpbin.org";

fn leak_name(s: String) -> &'static str {
    Box::leak(s.into_boxed_str()) // 源数量个位数，进程级常驻可接受
}

pub struct FreePoolWorker {
    router: Arc<crate::router::RouterEngine>,
    metrics: Arc<crate::metrics::MetricsRegistry>,
    config: FreePoolConfig,
    registry: Registry,
    guards: Vec<SourceGuard>,
    tick: u64,
}

impl FreePoolWorker {
    pub fn new(
        router: Arc<crate::router::RouterEngine>,
        metrics: Arc<crate::metrics::MetricsRegistry>,
        config: FreePoolConfig,
    ) -> Self {
        let ttl = config.ttl;
        let max_nodes = config.max_nodes;
        let n_sources = config.api_urls.len() + config.html_urls.len() + config.github_urls.len();
        let guards = (0..n_sources.max(1))
            .map(|_| SourceGuard::new(config.max_zero_cycles, config.suspend_retry_every))
            .collect();
        Self { router, metrics, config, registry: Registry::with_capacity(ttl, max_nodes),
               guards, tick: 0 }
    }

    /// 单轮：并发抓取（Task 10）→ TCP 初筛（信号量）→ FullCheck 复检（信号量 20，
    /// 基址探活失败则降级 TCP-only，anon=Unknown）→ upsert/失败记 backoff →
    /// 过期自然掉出 → merge（require_elite 门）→ 水位计＋扩展指标。
    pub async fn run_once(&mut self, client: &reqwest::Client) {
        self.tick += 1;
        let tick = self.tick;
        // 1. 组装三类源（配置序＝去重优先级：api > html > github）。
        let mut sources: Vec<Box<dyn Source>> = Vec::new();
        for (i, url) in self.config.api_urls.iter().enumerate() {
            sources.push(Box::new(ApiSource { name: leak_name(format!("api{i}")), url: url.clone() }));
        }
        for (i, url) in self.config.html_urls.iter().enumerate() {
            sources.push(Box::new(HtmlSource { name: leak_name(format!("html{i}")), url: url.clone(), default_proto: FreeProto::Http }));
        }
        for (i, url) in self.config.github_urls.iter().enumerate() {
            sources.push(Box::new(GitHubSource::new(leak_name(format!("gh{i}")), url.clone())));
        }
        if self.guards.len() != sources.len() {
            self.guards = (0..sources.len())
                .map(|_| SourceGuard::new(self.config.max_zero_cycles, self.config.suspend_retry_every))
                .collect();
        }
        // 2. 并发抓取（Task 10；失败 hold 旧集）。
        let raws = fetch_all(&sources, &mut self.guards, client, tick).await;
        for (i, s) in sources.iter().enumerate() {
            self.metrics.set_free_source_suspended(s.name(), self.guards[i].suspended());
        }
        // 3. 直连基线（FullCheck 用；失败→当轮降级 TCP-only）。
        let baseline: Option<String> = reqwest::Client::new()
            .get(format!("{}/ip", self.config.full_check_base))
            .timeout(Duration::from_secs(5))
            .send().await.ok()
            .and_then(|r| r.json::<serde_json::Value>())
            .map(|_| String::new()); // 占位：完整解析见下修正
        let _ = baseline;
        // （基线解析＋两级质检＋merge 见下“展开实现”，以其为准。）
        Self::merge_once(&self.router, &self.metrics, &self.registry, self.config.require_elite);
        log::info!("[FreePool] tick={tick} pool={} interval={:?}", self.registry.len(), self.config.fetch_interval);
    }

    /// 纯合并步（可单测）：快照 → 路由 → 水位计。
    pub fn merge_once(
        router: &Arc<crate::router::RouterEngine>,
        metrics: &Arc<crate::metrics::MetricsRegistry>,
        registry: &Registry,
        require_elite: bool,
    ) {
        let now = Instant::now();
        let snap = registry.snapshot(now, require_elite);
        metrics.set_free_pool_nodes(snap.len() as u64);
        router.replace_vendor_nodes("free-", snap);
    }
}
```

**展开实现（替代上式 Step 3 骨架，以本段为准）：**

```rust
pub async fn run_once(&mut self, client: &reqwest::Client) {
    self.tick += 1;
    let tick = self.tick;
    // 1-2. 同上（组装＋fetch_all＋suspend 指标）。
    // ……
    // 3. 直连基线：GET {base}/ip → origin（5s 超时；失败→None＝降级 TCP-only）。
    let baseline: Option<String> = {
        let r = tokio::time::timeout(Duration::from_secs(5), client.get(format!("{}/ip", self.config.full_check_base)).send()).await;
        match r {
            Ok(Ok(resp)) => resp.json::<serde_json::Value>().await.ok()
                .and_then(|v| v.get("origin").and_then(|o| o.as_str()).map(|s| s.to_string())),
            _ => None,
        }
    };
    if baseline.is_none() {
        log::warn!("[FreePool] baseline unreachable, degrading to TCP-only this tick");
    }
    // 4a. TCP 初筛（信号量 max_concurrent；socks 在 upsert 入口丢弃，此处仍可验以记指标？不——
    //     Phase 1 直接跳过非 poolable，记 verify=backoff_skip，避免无效建链）。
    let verifier = Verifier::new(self.config.verify_timeout, self.config.max_latency_ms);
    let tcp_sem = Arc::new(tokio::sync::Semaphore::new(self.config.max_concurrent.max(1)));
    let mut set = tokio::task::JoinSet::new();
    for raw in raws {
        if !raw.proto.poolable() {
            log::debug!("[FreePool] skip non-poolable proto {:?} {}:{}", raw.proto, raw.ip, raw.port);
            continue;
        }
        let sem = tcp_sem.clone();
        set.spawn(async move {
            if sem.acquire_owned().await.is_err() {
                return (raw, None);
            }
            let latency = verifier.verify(&raw).await;
            (raw, latency)
        });
    }
    // 4b. 通过 TCP 者→FullCheck（信号量 full_concurrent；基线缺失则跳过，anon=Unknown＋tcp 延迟）。
    let full_sem = Arc::new(tokio::sync::Semaphore::new(self.config.full_concurrent.max(1)));
    let checker = FullChecker::new(self.config.full_check_base.clone(), self.config.verify_timeout);
    let now = Instant::now();
    let mut tcp_passed: Vec<(RawNode, u64)> = Vec::new();
    while let Some(joined) = set.join_next().await {
        match joined {
            Ok((raw, Some(ms))) => tcp_passed.push((raw, ms)),
            Ok((raw, None)) => {
                self.metrics.note_free_verify("tcp_fail");
                self.registry.note_verify_failed(&format!("{}:{}", raw.ip, raw.port), now);
            }
            Err(e) => log::warn!("[FreePool] tcp task join failed: {e:?}"),
        }
    }
    let mut fset = tokio::task::JoinSet::new();
    for (raw, tcp_ms) in tcp_passed {
        if baseline.is_none() {
            // 降级：TCP 延迟＋Unknown（REQUIRE_ELITE=1 时此类条目被 merge 门过滤，语义自洽）。
            self.registry.upsert_full(&raw, tcp_ms, AnonLevel::Unknown, None, now);
            self.metrics.note_free_verify("pass");
            self.metrics.note_free_anonymity("unknown");
            continue;
        }
        let sem = full_sem.clone();
        let base = baseline.clone().expect("checked");
        set_spawn_full(&mut fset, sem, checker.clone(), raw, base, tcp_ms);
    }
    while let Some(joined) = fset.join_next().await {
        match joined {
            Ok((raw, Some(res))) => {
                self.registry.upsert_full(&raw, res.fwd_latency_ms, res.anon, res.exit_ip.clone(), now);
                self.metrics.note_free_verify("pass");
                self.metrics.note_free_anonymity(match res.anon {
                    AnonLevel::Elite => "elite", AnonLevel::Anonymous => "anonymous",
                    AnonLevel::Transparent => "transparent", AnonLevel::Unknown => "unknown",
                });
            }
            Ok((raw, None)) => {
                self.metrics.note_free_verify("full_fail");
                self.registry.note_verify_failed(&format!("{}:{}", raw.ip, raw.port), now);
            }
            Err(e) => log::warn!("[FreePool] full task join failed: {e:?}"),
        }
    }
    // 5. 合并＋指标（yield 计数在 fetch_all 内记：按源 span？简化：总量记 source="all"？
    //    不——Task 11 是 per-source：fetch_all 返回后按 raw.source 聚合计数，见下。）
    Self::merge_once(&self.router, &self.metrics, &self.registry, self.config.require_elite);
    log::info!("[FreePool] tick={tick} pool={} interval={:?}", self.registry.len(), self.config.fetch_interval);
}
```

配套要求（实现时一并落地）：
- `FullChecker: Clone`（`#[derive(Clone)]`，字段皆 Clone）。
- `set_spawn_full` 小 helper（许可拿不到→`(raw, None)`，沿 R2-7 `?-in-bool` 教训）。
- yield 聚合：`fetch_all` 返回 `Vec<RawNode>` 后按 `source` 分组
  `note_free_source_yield(source, count)`（含 0？只记>0 源＋暂停源由 suspend gauge 覆盖；
  零产出源不记 yield 行——其熔断由 suspend gauge 可见，注释写明）。
- `run` 常驻循环（supervisor 托管）沿用 v1（`loop { run_once; sleep(fetch_interval) }`）。

`main.rs` 接线（`prewarmer.run()` 后插入；`split_env_list` 复用 v1，增加 scheme 过滤）：

```rust
use free_pool::{FreePoolConfig, FreePoolWorker, DEFAULT_API_URL, DEFAULT_GITHUB_URL, DEFAULT_HTML_URL, DEFAULT_FULL_CHECK_BASE};

// 4e. FreePool 第二供应线（默认关闭；FREE_ENABLED=1 开启）。
fn split_env_list(key: &str, default: &str) -> Vec<String> {
    env_str(key, default)
        .split(',')
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        // SSRF/礼貌护栏：仅 http/https 源（v2 安全项 F1）。
        .filter(|s| s.starts_with("http://") || s.starts_with("https://"))
        .collect()
}
if env_str("FREE_ENABLED", "0") == "1" {
    let mut full_base = env_str("FREE_FULL_CHECK_URL", DEFAULT_FULL_CHECK_BASE);
    if !full_base.starts_with("https://") {
        log::warn!("[FreePool] FREE_FULL_CHECK_URL must be https, falling back to default");
        full_base = DEFAULT_FULL_CHECK_BASE.to_string();
    }
    let free_config = FreePoolConfig {
        api_urls: split_env_list("FREE_API_URLS", DEFAULT_API_URL),
        html_urls: split_env_list("FREE_HTML_URLS", DEFAULT_HTML_URL),
        github_urls: split_env_list("FREE_GITHUB_URLS", DEFAULT_GITHUB_URL),
        fetch_interval: env_secs("FREE_FETCH_INTERVAL_SECS", 600),
        ttl: env_secs("FREE_TTL_SECS", 1800),
        verify_timeout: env_secs("FREE_VERIFY_TIMEOUT_SECS", 3),
        max_latency_ms: env_str("FREE_MAX_LATENCY_MS", "3000").parse::<u64>().unwrap_or(3000),
        max_concurrent: env_str("FREE_MAX_CONCURRENT", "50").parse::<usize>().unwrap_or(50),
        full_concurrent: env_str("FREE_FULL_CONCURRENT", "20").parse::<usize>().unwrap_or(20),
        max_nodes: env_str("FREE_MAX_NODES", "2000").parse::<usize>().unwrap_or(2000),
        full_check_base: full_base,
        require_elite: env_str("FREE_REQUIRE_ELITE", "0") == "1",
        max_zero_cycles: env_str("FREE_SOURCE_MAX_ZERO_CYCLES", "3").parse::<u32>().unwrap_or(3),
        suspend_retry_every: env_str("FREE_SUSPEND_RETRY_EVERY", "3").parse::<u64>().unwrap_or(3),
    };
    let free_router = router.clone();
    let free_metrics = metrics.clone();
    tokio::spawn(async move {
        tokio::time::sleep(startup_jitter()).await;
        log::info!("[FreePool] staggered start (second supply line)");
        supervise("free_pool", free_metrics.clone(), move || {
            let w = FreePoolWorker::new(free_router.clone(), free_metrics.clone(), free_config.clone());
            async move { w.run(reqwest::Client::new()).await }
        })
        .await;
    });
}
```

`docs/OPERATION.md` §4 追加两行：
`| 免费线水位 | `free_pool_nodes_total` / 日志`[FreePool] tick` | 默认关闭（`FREE_ENABLED=1` 开）；水位突降=源站熔断（`free_pool_source_suspended{source}=1`）或质检门限过严（`free_pool_verify_total` 看 fail 分布）；country 缺省 ZZ，只服务无归属要求的流量；`FREE_REQUIRE_ELITE=1` 时仅 Elite 进池 |`
`| 免费线健康 | `free_pool_source_yield_total` / `free_pool_anonymity_total` | yield 骤降=源站挂；transparent 占比突增=源站质量恶化，考虑开 REQUIRE_ELITE；单节点转发延迟看 registry 日志（debug） |`

§6 追加安全硬规则（F1）：
`- 免费线零信任：免费节点**禁止**承载含认证/cookie/支付/银行流量（网关层不强制，租户侧规约：敏感租户绑定 tier≠free 或自建 REQUIRE_ELITE=1 实践）；Transparent 节点永不服务认证租户（REQUIRE_ELITE=1 时 merge 门强制；为 0 时 OPERATION 警告）。`
`- 复检基址必须 https（启动校验，非法回落默认）；抓取源仅 http/https（file/dict/gopher 一律过滤，防 SSRF）。`

`docker-compose.yml` 网关 env 示例追加（§2 全表 1:1）：

```
  #     FREE_ENABLED: "1"
  #     FREE_API_URLS: "https://proxylist.geonode.com/api/proxy-list?limit=100&page=1&sort_by=lastChecked&sort_type=desc"
  #     FREE_HTML_URLS: "https://free-proxy-list.net/"
  #     FREE_GITHUB_URLS: "https://raw.githubusercontent.com/clarketm/proxy-list/master/proxy-list-raw.txt"
  #     FREE_FETCH_INTERVAL_SECS: "600"
  #     FREE_TTL_SECS: "1800"
  #     FREE_VERIFY_TIMEOUT_SECS: "3"
  #     FREE_MAX_LATENCY_MS: "3000"
  #     FREE_MAX_CONCURRENT: "50"
  #     FREE_FULL_CONCURRENT: "20"
  #     FREE_MAX_NODES: "2000"
  #     FREE_FULL_CHECK_URL: "https://httpbin.org"
  #     FREE_REQUIRE_ELITE: "0"
  #     FREE_SOURCE_MAX_ZERO_CYCLES: "3"
  #     FREE_SUSPEND_RETRY_EVERY: "3"
```

- [ ] **Step 4: 运行确认通过**

Run: `cargo test --workspace free_pool free_env`
Expected: PASS（free_pool 目标 20 单测：Task 4/5/6/7 沿用约 9＋Task 8 的 4＋Task 9 的 5＋Task 10 的 2＋本任务 1；main 1；
全仓目标 82＋21=103，±1 以实测为准，EXEC_LOG 如实记录）

- [ ] **Step 5: Verify（fmt＋clippy＋bench 编译）**

Run: `cargo fmt --check`; `cargo clippy --workspace --all-targets -- -D warnings`; `cargo bench --workspace --no-run`

---

### Task 13: 最终门禁＋回归（curl＋链路）

- [ ] **Step 1: 四门全跑**

Run: `cargo fmt --check`（clean）; `cargo clippy --workspace --all-targets -- -D warnings`（零告警）;
`cargo test --workspace`（103±1 过/4 ignored 预期）;
`cargo test --workspace -- --ignored --nocapture`（**先验依赖**：
`docker exec ipproxy-redis redis-cli ping`→PONG 且 CH `/ping`→Ok，无 SKIP 行才算真过；
R2-9 教训：silent-skip 的 ok ≠ 真过）; `cargo bench --workspace --no-run`（编译过）

- [ ] **Step 2: 网关回归（FREE_ENABLED=1 开一轮＋REQUIRE_ELITE=0/1 两档）**

Run: 构建＋detached 拉 mocks＋网关（`FREE_ENABLED=1`），curl 六用例＋缺 Host 400＋/metrics 200；
查 `free_pool_nodes_total`＋`free_pool_verify_total`＋`free_pool_anonymity_total`＋
`free_pool_source_suspended` 行存在；10 流量后 XLEN/CH 涨；日志 `[FreePool] tick` 行。
第二档 `FREE_REQUIRE_ELITE=1` 重启，确认水位≤第一档（Unknown 被过滤）且六用例语义不变。
Expected: 六用例语义不变；tier 隔离单测＋curl 抽查（`tier=residential` 请求不命中 free-*
provider：经日志/arm key 抽查，精确断言由单测承担）；
初期 `free_pool_nodes_total` 可能为 0（源站直连受限即 hold 空集＋基线降级 TCP-only，属正常，日志 warn 可见）

- [ ] **Step 3: 落库（plan 表＋EXEC_LOG append-only＋TASK_PLAN）**

plan 表（本文件 §状态总览）全行置 ✅；EXEC_LOG 追加条目（含单测数/live 真过证据＋网关 PID＋日志文件）；
TASK_PLAN 步骤 10 更新。tier 隔离与 REQUIRE_ELITE 两档结果如实记录（含水位 0 的正常性说明）。

- [ ] **Step 4: Verify（提交确认）**

本仓禁未授权 commit——向用户确认后统一提交（`git add` 限定本计划文件集）。

---

## §3. 状态总览（执行跟踪，随执行动态更新）

| 子项 | 内容 | 优先级 | 状态 | 验收 |
|------|------|--------|------|------|
| Task 1 | free 档经济模型（沿用 v1） | P0 | ⬜ 待开始 | 断言扩展＋存量绿 |
| Task 2 | replace_vendor_nodes（沿用 v1） | P0 | ⬜ 待开始 | 付费 ptr_eq＋二次合并无堆积 |
| Task 3 | 水位计（沿用 v1） | P0 | ⬜ 待开始 | render 常驻行 |
| Task 4 | 骨架＋Source trait FetchOutcome 版 | P0 | ⬜ 待开始 | Geonode 双 port 形态 |
| Task 5 | HtmlSource（沿用 v1） | P0 | ⬜ 待开始 | 4 行/2 行锁定 |
| Task 6 | GitHubSource＋ETag/304 | P0 | ⬜ 待开始 | If-None-Match 断言＋not_modified |
| Task 7 | Verifier TCP 初筛（沿用 v1） | P0 | ⬜ 待开始 | 拒连/socks 双拒 |
| Task 8 | FullCheck＋匿名度＋canary（新增） | P0 | ⬜ 待开始 | 三级矩阵＋篡改检出＋转发延迟 |
| Task 9 | Registry＋EWMA＋backoff＋容量（修订） | P0 | ⬜ 待开始 | 分数数学＋backoff 不可见＋淘汰收敛 |
| Task 10 | SourceGuard＋fetch 并发（新增） | P0 | ⬜ 待开始 | 熔断恢复＋源序确定性 |
| Task 11 | metrics 四组扩展（新增） | P1 | ⬜ 待开始 | render 四组行 |
| Task 12 | Worker＋main 全 env＋运维落盘（修订） | P0 | ⬜ 待开始 | free_pool 20 单测＋main 1 |
| Task 13 | 最终门禁＋两档回归 | 门禁 | ⬜ 待开始 | 103±1＋4 live 真过＋ELITE 两档 |

---

## §4. 不做（v2 explicitly out → Phase 2/3）

- **Phase 2（SOCKS egress，另起计划）：** per-proto reqwest Client（socks feature 新依赖）、
  Pingora upstream SOCKS CONNECT 改造、`ProxyNode.proto` 字段＋全构造点迁移、
  SOCKS 握手探测（Task 7/8 扩展）、按 proto 选路隔离。
- **Phase 3（画像与学习增强）：** 本地 GeoIP（maxminddb 新依赖）＋exit-IP 国家校验；
  免费臂 DomainRisk 上浮＋JA4 漂移门（沿 SPIKE-R2 boringssl 路径）；
  per-tier forgetting（free 短窗）／dLinUCB-change-detection／P2C 选路；
  free 独立套利阈值（池级成功率<50% 降权）；composite 健康（会话保持＋硬目标可达，
  接 ProxyStats 方法学）；CH 落库 free 明细（provider free-* 已天然可分，报表侧做）。
- v1 §不做延续：真供应商 Key 灰度（待用户输入）、Linux 50k 性能验收（待节点）、
  release 全量 bench（bandit 未动，沿用 R2-6 126ns；free 合并为 10min 级后台路径，无数据面承诺）。

## §5. 风险表（v2 新增）

| 风险 | 等级 | 缓解 |
|------|------|------|
| 免费源 ToS／robots（抓取频率） | 中 | 600s 节拍＋ETag＋单源 15s 超时＋OPERATION 记录源站清单；被封即 SourceGuard 熔断可见 |
| 全池污染（某源大量注入可用但恶意节点） | 高 | canary＋https 基址＋REQUIRE_ELITE＋敏感流量禁令＋权重上限 20（爆炸半径有界） |
| httpbin 基址限流／不可达 | 中 | 基线失败降级 TCP-only（当轮 anon=Unknown）；基址 env 可配自建回显服务 |
| 本地 echo/代理 stub 单测脆弱（端口/时序） | 低 | 沿用 OPT-6/Task 6 本地 listener 手法＋127.0.0.1:1 拒连端口；超时 3s 内收敛 |
| `leak_name` 微泄漏（每轮重建 Box::leak） | 低 | v2 修正：Worker 复用同一 sources/guards 装配（tick 内一次；跨轮 name 复用——实现时 guards/sources 提为 Worker 字段缓存，run_once 只在 URL 集合变化时重建；单测不覆盖，注释＋走查验收） |
| free 权重稀释付费流量（无 tier 请求） | 低 | 上限 20 vs 付费 60~100＋trusted 才 20；回归 curl 抽查普通流量仍命中付费大权重大头（方向性，不卡精确值） |

---

## Self-Review（计划自检，已内联修复）

1. **Spec coverage:** 多源插件化→Task 4/5/6；匿名度→Task 8；EWMA/权重→Task 9；
   backoff/恢复→Task 9；零产出熔断→Task 10；容量→Task 9；连通+延迟→Task 7/8；
   全协议→解析全收、进池 HTTP/S（Task 9 过滤）；短 TTL+复检→Task 9/12。
   SOCKS egress 明确 out（Phase 2）。
2. **Placeholder scan:** 无 TBD/TODO；Task 10 初稿占位已展开为 `join_all` 实现；
   Task 12 Step 3 基线占位已展开；HTML 提取器由 v1 单测锁定。
3. **Type consistency:** `RawNode{ip,port,proto,country,source}` 全任务同名；
   `Source::fetch→Result<FetchOutcome,String>`（Task 4 定义，5/6/10/12 引用一致）；
   `Registry::{upsert_full,note_verify_failed,reverify,snapshot(now,require_elite),len}`（Task 9 定义，
   Task 12 调用一致）；`FreePoolConfig: Clone`（Task 12 注明）；
   `replace_vendor_nodes(&self,prefix,&str,nodes:Vec<ProxyNode>)`（Task 2 沿用 v1，Task 12 调用一致）；
   `Health::{score,weight,trusted,backed_off}` 纯函数可单测；`AnonLevel` 四值全任务一致。
4. **存量风险：** `price_per_gb("free")=0`（v1 已评估：tier 只由 FreePool 构造写入）；
   `cost_weight_for_tier("free")=0`（key 隔离，存量臂无 free tier 不受影响）；
   `snapshot(now, require_elite)` 签名变更只影响新模块（存量 router 不动）；
   metrics 新增 DashMap 字段沿 R2-8 模式（render 需处理空表：无数据时只渲染 HELP/TYPE，
   单测覆盖默认空渲染不断言行数，注释写明）。
5. **门禁诚实性（R2-9 教训内化）：** Task 13 强制先验依赖在线＋无 SKIP 行检查；
   EXEC_LOG 单测数以 `cargo test` 实测为准（103±1 为目标非承诺）。
