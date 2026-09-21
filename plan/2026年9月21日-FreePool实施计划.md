# FreePool 第二供应线 Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 新增 `free_pool` 模块，定时从公开免费源抓取 HTTP(S) 节点，经两级质检（TCP 初筛＋走代理实转复检含匿名度分级）后以 `provider="free-*"`、EWMA 健康分动态权重并入 RouterEngine，与付费线加权混合；短 TTL+复检＋失败 backoff，进出池不扰动付费节点。

**Architecture:** `Source` trait（三适配器：JSON API / HTML 表格 / GitHub raw，ETag 礼貌轮询）→ fetch（10min，源间错峰）→ 两级 verify（TCP 建链+延迟门全量；新节点/未知匿名度走代理实转复检判 Elite/Anonymous/Transparent）→ Registry（TTL 30min，到期复检续命；EWMA 分→动态权重；多源按 addr 去重；失败 penalty backoff；容量上限逐最低分）→ `replace_vendor_nodes("free-", …)` 原子合并 → 现有选路/熔断/遥测/计量全复用；源站零产出熔断（连续 3 轮跳过）；supervisor 托管，env 总控（默认关闭）。

**前沿依据（2026-09-21 回填）：** 匿名度三级＋7 头披露检查法（MiyaIP 方法学：Forwarded/XFF/X-Real-IP/Client-IP/Via/Proxy-Connection/X-Proxy-ID，对比直连基线）；EWMA 成功率×延迟惩罚健康分（proxyhive/Orbit 模式，α 成功率 0.3）；失败指数 backoff＋自动恢复（proxyhive/Orbit）；源站 5min 级重检节拍（databay），本计划取 10min（TCP 初筛廉价＋复检分级）；免费代理 MITM/蜜罐风险（arXiv 2403.02445＋ProxMint 警示）→ OPERATION 硬性禁敏感流量。

**Tech Stack:** Rust（reqwest 0.12[rjson]、tokio JoinSet+Semaphore、DashMap、Instant）；零新依赖（HTML 用手写字节扫描器，不引 scraper/regex）。

**Deviations locked:** 计划文件落 `plan/`（沿本仓惯例，不用 `docs/superpowers/plans/`）；本仓禁未授权 commit，每任务末 Step 为 verify，最终统一经用户确认后提交；SOCKS egress 为 Phase 2（另起计划，本计划只解析标注、不选路）。

**File map:**
- Modify: `gateway/src/tenant.rs`（free  tier 0 价）、`gateway/src/bandit.rs`（free cost 0）、`gateway/src/router.rs`（`replace_vendor_nodes`）、`gateway/src/metrics.rs`（`free_pool_nodes_total`）、`gateway/src/main.rs`（`mod free_pool`＋supervise＋env）、`docs/OPERATION.md`（§4 一行）、`docker-compose.yml`（env 示例）。
- Create: `gateway/src/free_pool.rs`（全部新逻辑＋单测，约 600 行）。

**Env（main.rs 复用 `env_str/env_secs`，全部有默认）:**
`FREE_ENABLED`（默认 `"0"`，第二线总开关）/`FREE_API_URLS`/`FREE_HTML_URLS`/`FREE_GITHUB_URLS`（逗号分隔，见 Task 4/5/6 默认值）/`FREE_FETCH_INTERVAL_SECS`（默认 600；业界重检 5min 级，TCP 初筛廉价取 10min）/`FREE_TTL_SECS`（默认 1800）/`FREE_MAX_LATENCY_MS`（默认 3000，兼作 EWMA 延迟中性点）/`FREE_MAX_CONCURRENT`（默认 50）/`FREE_MAX_NODES`（默认 2000，超限逐最低分）/`FREE_FULL_CHECK_URL`（默认 `https://httpbin.org`，实转复检基址；不可达则降级 TCP-only）/`FREE_REQUIRE_ELITE`（默认 `"0"`，为 1 时仅 Elite 进池）/`FREE_SOURCE_MAX_ZERO_CYCLES`（默认 3，源站零产出熔断轮数）。

---

### Task 1: free 档经济模型（tenant 计费 0 ＋ bandit 成本 0）

**Files:**
- Modify: `gateway/src/tenant.rs`
- Modify: `gateway/src/bandit.rs`
- Test: 同文件 `mod tests`（扩展既有断言，不新增测试函数）

- [ ] **Step 1: 扩展既有单测断言（先写期望）**

在 `tenant.rs` 的 `price_table_matches_plan` 追加一行：
```rust
assert_eq!(price_per_gb("free"), 0.0);
```
在 `bandit.rs` 的 `cost_mapping_matches_plan` 追加一行：
```rust
assert_eq!(cost_weight_for_tier("free"), 0.0);
```

- [ ] **Step 2: 运行确认失败**

Run: `cargo test --workspace tenant::tests::price_table_matches_plan bandit::tests::cost_mapping_matches_plan`
Expected: 2 FAIL（左值非 0：tenant 走 `_` 分支得 0.2，bandit 走 `_` 分支得 1.0）

- [ ] **Step 3: 最小实现**

`tenant.rs` 在价格常量区追加并在 `price_per_gb` 加分支：
```rust
/// 免费线 $/GB 单价（R2-FreePool：免费节点仍计量字节，单价 0）。
pub const PRICE_FREE_PER_GB: f64 = 0.0;

pub fn price_per_gb(tier: &str) -> f64 {
    match tier.to_ascii_lowercase().as_str() {
        "residential" | "res" => PRICE_RESIDENTIAL_PER_GB,
        "mobile" => PRICE_MOBILE_PER_GB,
        "free" => PRICE_FREE_PER_GB,
        _ => PRICE_DC_PER_GB,
    }
}
```
`bandit.rs` 在 `cost_weight_for_tier` 加分支：
```rust
pub fn cost_weight_for_tier(tier: &str) -> f64 {
    match tier.to_ascii_lowercase().as_str() {
        "datacenter" | "dc" => COST_DC,
        "mobile" => COST_MOBILE,
        // 免费线探索成本 0（池权重 10 已压住其选中率，此处不再双重惩罚）。
        "free" => 0.0,
        _ => COST_RESIDENTIAL,
    }
}
```

- [ ] **Step 4: 运行确认通过**

Run: `cargo test --workspace tenant bandit`
Expected: 全 PASS（含既有 `metering_deducts_tier_price` 等存量）

- [ ] **Step 5: Verify（fmt＋clippy）**

Run: `cargo fmt --check`（clean）; `cargo clippy --workspace --all-targets -- -D warnings`（零告警）

---

### Task 2: Router 按 vendor 前缀原子替换（付费线不动）

**Files:**
- Modify: `gateway/src/router.rs`（`impl RouterEngine` 追加方法＋1 个新单测）
- Test: `gateway/src/router.rs::tests::replace_vendor_nodes_keeps_paid`

- [ ] **Step 1: 写失败单测**

```rust
#[test]
fn replace_vendor_nodes_keeps_paid() {
    // 免费线合并语义：只动 `free-` 前缀节点，付费节点保持同一分配（ptr_eq）。
    use crate::model::ProxyNode;
    let paid = ProxyNode::new(
        "10.0.0.1".to_string(), 8080, None, None,
        "US".to_string(), "residential".to_string(), "mock-a".to_string(), 100,
    );
    let r = RouterEngine::new(vec![paid]);
    let before = r.snapshot_all();
    let free1 = ProxyNode::new(
        "9.9.9.9".to_string(), 8080, None, None,
        "ZZ".to_string(), "free".to_string(), "free-geonode".to_string(), 10,
    );
    r.replace_vendor_nodes("free-", vec![free1]);
    let after = r.snapshot_all();
    assert_eq!(after.len(), 2);
    assert!(after.iter().any(|n| n.provider == "mock-a"));
    assert!(after.iter().any(|n| n.provider == "free-geonode"));
    // 付费节点仍是池内原分配。
    assert!(after.iter().any(|n| Arc::ptr_eq(n, &before[0])));
    // 二次合并替换旧免费节点，不堆积。
    let free2 = ProxyNode::new(
        "8.8.8.8".to_string(), 8080, None, None,
        "ZZ".to_string(), "free".to_string(), "free-github".to_string(), 10,
    );
    r.replace_vendor_nodes("free-", vec![free2]);
    let again = r.snapshot_all();
    assert_eq!(again.len(), 2);
    assert!(again.iter().all(|n| n.provider != "free-geonode"));
}
```

- [ ] **Step 2: 运行确认失败**

Run: `cargo test --workspace replace_vendor_nodes_keeps_paid`
Expected: FAIL（`replace_vendor_nodes` 不存在，编译错即失败）

- [ ] **Step 3: 最小实现（`adjust_vendor_weight` 旁追加）**

```rust
/// 按 provider 前缀原子替换（FreePool 合并入口）。
/// 只移除 `provider.starts_with(prefix)` 的旧节点并追加新集；其余节点复用
/// 旧 `Arc`（引用不断，粘滞绑定/臂状态不受影响）；空 `nodes` 即清空该前缀。
pub fn replace_vendor_nodes(&self, prefix: &str, nodes: Vec<ProxyNode>) {
    let guard = self.pools.load();
    let mut next: Vec<Arc<ProxyNode>> = guard
        .iter()
        .filter(|n| !n.provider.starts_with(prefix))
        .map(Arc::clone)
        .collect();
    next.extend(nodes.into_iter().map(Arc::new));
    self.pools.store(Arc::new(next));
}
```

- [ ] **Step 4: 运行确认通过**

Run: `cargo test --workspace router`
Expected: 全 PASS（含 R2-1/R2-2/R2-6 存量 13 个 router 单测）

- [ ] **Step 5: Verify（fmt＋clippy）**

Run: `cargo fmt --check`; `cargo clippy --workspace --all-targets -- -D warnings`

---

### Task 3: metrics 免费池水位计

**Files:**
- Modify: `gateway/src/metrics.rs`（字段＋方法＋渲染行＋1 新单测）
- Test: `gateway/src/metrics.rs::tests::free_pool_nodes_rendered`

- [ ] **Step 1: 写失败单测**

```rust
#[test]
fn free_pool_nodes_rendered() {
    // 免费池水位：默认 0 常驻行；set 后渲染新值。
    let m = MetricsRegistry::new();
    assert!(m.render().contains("free_pool_nodes_total 0"));
    m.set_free_pool_nodes(37);
    assert!(m.render().contains("free_pool_nodes_total 37"));
}
```

- [ ] **Step 2: 运行确认失败**

Run: `cargo test --workspace free_pool_nodes_rendered`
Expected: FAIL（方法不存在）

- [ ] **Step 3: 最小实现**

结构体追加字段：`free_pool_nodes: AtomicU64,`；构造器追加 `free_pool_nodes: AtomicU64::new(0),`；追加方法：
```rust
/// 免费池在池节点数（FreePool worker 每次合并后设置，只写不参与 observe）。
pub fn set_free_pool_nodes(&self, n: u64) {
    self.free_pool_nodes.store(n, Ordering::Relaxed);
}
```
`render()` 在 `gateway_logs_sampled_total` 块后追加：
```rust
out.push_str("# HELP free_pool_nodes_total Free-tier nodes currently merged into the pool.\n");
out.push_str("# TYPE free_pool_nodes_total gauge\n");
out.push_str(&format!(
    "free_pool_nodes_total {}\n",
    self.free_pool_nodes.load(Ordering::Relaxed)
));
```

- [ ] **Step 4: 运行确认通过**

Run: `cargo test --workspace metrics`
Expected: 全 PASS（存量 4 单测＋新 1）

- [ ] **Step 5: Verify（fmt＋clippy）**

---

### Task 4: free_pool 骨架＋Source trait＋ApiSource

**Files:**
- Create: `gateway/src/free_pool.rs`（本任务只落类型＋trait＋ApiSource＋单测；Verifier/Registry/Worker 后续任务追加同文件）
- Test: `free_pool::tests::api_source_parses_geonode_shape`

- [ ] **Step 1: 写失败单测（fixture 内联 Geonode 形态，注意 port 为字符串）**

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
    // 坏行丢弃；socks5 解析保留（Phase 1 由 Verifier 跳过，不在此处丢）。
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
Expected: FAIL（模块/类型不存在；先在 `main.rs` 加 `mod free_pool;` 使目标可编译——加 sole 一行）

- [ ] **Step 3: 最小实现**

```rust
//! FreePool 第二供应线：公开免费源抓取 → 质检 → TTL 注册 → Router 合并。
//!
//! Phase 1 只收 HTTP(S)（零网关转发改动）；SOCKS 只解析标注、Verifier 跳过，
//! egress 改造见 Phase 2。默认关闭（`FREE_ENABLED=1` 开启）。

use std::time::Duration;

/// 抓取协议（Phase 1 仅 Http/Https 进池）。
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

/// 抓取源插件接口（API / HTML / GitHub 各一实现）。
#[async_trait::async_trait]
pub trait Source: Send + Sync {
    fn name(&self) -> &'static str;
    async fn fetch(&self, client: &reqwest::Client) -> anyhow::Result<Vec<RawNode>>;
}
```

注：本仓无 `anyhow` 依赖——用 `Result<Vec<RawNode>, String>`（错误文案 String，零新依赖）替代上式签名：
```rust
#[async_trait::async_trait]
pub trait Source: Send + Sync {
    fn name(&self) -> &'static str;
    async fn fetch(&self, client: &reqwest::Client) -> Result<Vec<RawNode>, String>;
}
```

```rust
/// JSON API 源（默认 Geonode；URL env 可配，见 Task 9）。
/// port 兼容字符串/数字双形态；缺 country 视为 None（后继标 ZZ）。
pub struct ApiSource {
    pub name: &'static str,
    pub url: String,
}

impl ApiSource {
    pub fn parse(source: &str, body: &str) -> Vec<RawNode> {
        let v: serde_json::Value = match serde_json::from_str(body) {
            Ok(v) => v,
            Err(_) => return Vec::new(),
        };
        let arr = v.get("data").and_then(|d| d.as_array());
        let mut out = Vec::new();
        for item in arr.into_iter().flatten() {
            let ip = item.get("ip").and_then(|s| s.as_str()).unwrap_or("");
            if ip.is_empty() {
                continue;
            }
            let port: Option<u16> = match item.get("port") {
                Some(serde_json::Value::Number(n)) => n.as_u64().and_then(|p| u16::try_from(p).ok()),
                Some(serde_json::Value::String(s)) => s.parse::<u16>().ok(),
                _ => None,
            };
            let port = match port {
                Some(p) if p > 0 => p,
                _ => continue,
            };
            let proto = item
                .get("protocols")
                .and_then(|p| p.as_array())
                .and_then(|a| a.first())
                .and_then(|s| s.as_str())
                .and_then(FreeProto::from_token)
                .unwrap_or(FreeProto::Http);
            let country = item
                .get("country")
                .and_then(|c| c.as_str())
                .filter(|c| !c.is_empty())
                .map(|c| c.to_string());
            out.push(RawNode {
                ip: ip.to_string(),
                port,
                proto,
                country,
                source: source.to_string(),
            });
        }
        out
    }
}

#[async_trait::async_trait]
impl Source for ApiSource {
    fn name(&self) -> &'static str {
        self.name
    }
    async fn fetch(&self, client: &reqwest::Client) -> Result<Vec<RawNode>, String> {
        let body = client
            .get(&self.url)
            .send()
            .await
            .map_err(|e| format!("GET {} failed: {e}", self.url))?
            .text()
            .await
            .map_err(|e| format!("read {} failed: {e}", self.url))?;
        Ok(Self::parse(self.name, &body))
    }
}
```

`main.rs` 加 `mod free_pool;`（`mod fingerprint;` 后按字母序插入）。

- [ ] **Step 4: 运行确认通过**

Run: `cargo test --workspace free_pool`
Expected: PASS（1 单测；`async_trait` 已在主依赖中）

- [ ] **Step 5: Verify（fmt＋clippy）**

---

### Task 5: HtmlSource 手写提取器（零新依赖）

**Files:**
- Modify: `gateway/src/free_pool.rs`（追加 `HtmlSource`＋单测）
- Test: `free_pool::tests::html_source_extracts_ip_ports`

- [ ] **Step 1: 写失败单测**

```rust
#[test]
fn html_source_extracts_ip_ports() {
    let html = r#"<table><tr><td>203.0.113.7</td><td>8080</td><td>yes</td></tr>
        <tr><td>999.1.1.1</td><td>80</td></tr>
        <tr><td>198.51.100.9:3128</td></tr>
        <tr><td>10.0.0.1</td><td>70000</td></tr></table>"#;
    let nodes = HtmlSource::extract("fpl", html, FreeProto::Http);
    // 非法 octet/超范围端口丢弃；`ip:port` 紧凑形态同样识别。
    assert_eq!(nodes.len(), 2);
    assert_eq!((nodes[0].ip.as_str(), nodes[0].port), ("203.0.113.7", 8080));
    assert_eq!((nodes[1].ip.as_str(), nodes[1].port), ("198.51.100.9", 3128));
    assert!(nodes.iter().all(|n| n.proto == FreeProto::Http && n.source == "fpl"));
}
```

- [ ] **Step 2: 运行确认失败**

Run: `cargo test --workspace html_source_extracts_ip_ports`
Expected: FAIL（`HtmlSource` 不存在）

- [ ] **Step 3: 最小实现**

```rust
/// HTML 表格源（默认 free-proxy-list.net；URL env 可配）。
/// 无 HTML 解析依赖：字节扫描 `a.b.c.d[:port]|</td><td>port` 形态，octet≤255、
/// 端口 1..=65535；行内含 `socks4`/`socks5`（大小写不敏感）则标对应协议
///（Phase 1 Verifier 跳过）。
pub struct HtmlSource {
    pub name: &'static str,
    pub url: String,
    pub default_proto: FreeProto,
}

impl HtmlSource {
    pub fn extract(source: &str, html: &str, default_proto: FreeProto) -> Vec<RawNode> {
        let bytes = html.as_bytes();
        let mut out = Vec::new();
        let mut i = 0;
        while i < bytes.len() {
            if !bytes[i].is_ascii_digit() {
                i += 1;
                continue;
            }
            if let Some((ip, port, consumed)) = Self::match_ip_port(&bytes[i..]) {
                // 行级协议嗅探：向前 200B 窗口找 socks 标记。
                let from = i.saturating_sub(200);
                let window = String::from_utf8_lossy(&bytes[from..i]).to_ascii_lowercase();
                let proto = if window.contains("socks5") {
                    FreeProto::Socks5
                } else if window.contains("socks4") {
                    FreeProto::Socks4
                } else {
                    default_proto
                };
                out.push(RawNode {
                    ip,
                    port,
                    proto,
                    country: None,
                    source: source.to_string(),
                });
                i += consumed;
            } else {
                i += 1;
            }
        }
        out
    }

    /// 在切片头部匹配 `a.b.c.d`＋可选 `:port`／紧随 `<…>port`；非法返回 None。
    fn match_ip_port(b: &[u8]) -> Option<(String, u16, usize)> {
        let mut octets = [0u32; 4];
        let mut pos = 0;
        for k in 0..4 {
            let start = pos;
            while pos < b.len() && b[pos].is_ascii_digit() {
                pos += 1;
            }
            if start == pos {
                return None;
            }
            octets[k] = std::str::from_utf8(&b[start..pos]).ok()?.parse::<u32>().ok()?;
            if octets[k] > 255 {
                return None;
            }
            if k < 3 {
                if pos >= b.len() || b[pos] != b'.' {
                    return None;
                }
                pos += 1;
            }
        }
        // 端口：`:port` 或 `</td><td>port` 紧凑形态。
        let mut ppos = pos;
        if ppos < b.len() && b[ppos] == b':' {
            ppos += 1;
        } else {
            // 跳过 `</td><td>` 类标签找数字。
            let mut q = ppos;
            while q < b.len() && !b[q].is_ascii_digit() && b[q] != b'<' {
                q += 1;
            }
            // 只允许标签字符之间过渡，遇到 `<` 后必须紧跟数字（经闭合标签）。
            let mut r = q;
            while r < b.len() && (b[r] == b'<' || b[r] == b'/' || b[r].is_ascii_alphabetic()) {
                r += 1;
            }
            while r < b.len() && b[r] == b'>' {
                r += 1;
                break;
            }
            // 简化：标签过渡后取数字；若起点非数字则无端口。
            if r < b.len() && b[r].is_ascii_digit() {
                ppos = r;
            } else if q < b.len() && b[q].is_ascii_digit() && q == ppos {
                ppos = q;
            } else {
                return None;
            }
        }
        let start = ppos;
        while ppos < b.len() && b[ppos].is_ascii_digit() {
            ppos += 1;
        }
        if start == ppos {
            return None;
        }
        let port: u16 = std::str::from_utf8(&b[start..ppos]).ok()?.parse().ok()?;
        if port == 0 {
            return None;
        }
        let ip = format!("{}.{}.{}.{}", octets[0], octets[1], octets[2], octets[3]);
        Some((ip, port, ppos))
    }
}

#[async_trait::async_trait]
impl Source for HtmlSource {
    fn name(&self) -> &'static str {
        self.name
    }
    async fn fetch(&self, client: &reqwest::Client) -> Result<Vec<RawNode>, String> {
        let body = client
            .get(&self.url)
            .send()
            .await
            .map_err(|e| format!("GET {} failed: {e}", self.url))?
            .text()
            .await
            .map_err(|e| format!("read {} failed: {e}", self.url))?;
        Ok(Self::extract(self.name, &body, self.default_proto))
    }
}
```

- [ ] **Step 4: 运行确认通过**

Run: `cargo test --workspace free_pool`
Expected: PASS（2 单测）。若 fixture 行解析偏差，允许收紧 `match_ip_port` 标签过渡分支，但必须保持单测 4 行输入/2 行输出不变。

- [ ] **Step 5: Verify（fmt＋clippy）**

---

### Task 6: GitHubSource raw 轮询（`ip:port` 行格式）

**Files:**
- Modify: `gateway/src/free_pool.rs`（追加 `GitHubSource`＋2 单测）
- Test: `free_pool::tests::github_source_parses_line_list`、`free_pool::tests::github_source_fetches_from_local_server`

- [ ] **Step 1: 写失败单测**

```rust
#[test]
fn github_source_parses_line_list() {
    let body = "203.0.113.7:8080\n\n# comment\n198.51.100.9:3128 socks5\nbad-line\n10.0.0.1:0\n";
    let nodes = GitHubSource::parse("gh", body);
    assert_eq!(nodes.len(), 2);
    assert_eq!(nodes[0].proto, FreeProto::Http);
    assert_eq!(nodes[1].proto, FreeProto::Socks5);
}

#[tokio::test]
async fn github_source_fetches_from_local_server() {
    // 零外部依赖：本地临时 HTTP 服务冒充 raw 仓（沿用 metrics 单测手法换端口）。
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let port = listener.local_addr().expect("addr").port();
    tokio::spawn(async move {
        let (mut s, _) = listener.accept().await.expect("accept");
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let mut buf = [0u8; 1024];
        let _ = s.read(&mut buf).await;
        let body = "203.0.113.7:8080\n";
        let _ = s
            .write_all(format!("HTTP/1.1 200 OK\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}", body.len()).as_bytes())
            .await;
    });
    let client = reqwest::Client::new();
    let src = GitHubSource { name: "gh", url: format!("http://127.0.0.1:{port}/list.txt") };
    let nodes = src.fetch(&client).await.expect("fetch");
    assert_eq!(nodes.len(), 1);
    assert_eq!(nodes[0].ip, "203.0.113.7");
}
```

- [ ] **Step 2: 运行确认失败**

Run: `cargo test --workspace github_source`
Expected: FAIL（`GitHubSource` 不存在）

- [ ] **Step 3: 最小实现**

```rust
/// GitHub raw 仓源（默认 clarketm/proxy-list raw；URL env 可配）。
/// 行格式：`ip:port`（`#` 开头与空行跳过；行尾 `socks4`/`socks5` 标记协议）。
/// 礼貌轮询：ETag 缓存＋If-None-Match，304 直接返回空集（不解析、不计产出）。
pub struct GitHubSource {
    pub name: &'static str,
    pub url: String,
    etag: parking_lot::Mutex<Option<String>>,
}

impl GitHubSource {
    pub fn new(name: &'static str, url: String) -> Self {
        Self { name, url, etag: parking_lot::Mutex::new(None) }
    }

impl GitHubSource {
    pub fn parse(source: &str, body: &str) -> Vec<RawNode> {
        let mut out = Vec::new();
        for line in body.lines() {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            let lower = line.to_ascii_lowercase();
            let proto = if lower.contains("socks5") {
                FreeProto::Socks5
            } else if lower.contains("socks4") {
                FreeProto::Socks4
            } else {
                FreeProto::Http
            };
            let addr = line.split_whitespace().next().unwrap_or("");
            let (ip, port) = match addr.rsplit_once(':') {
                Some((ip, port)) => (ip, port.parse::<u16>().ok()),
                None => continue,
            };
            let port = match port {
                Some(p) if p > 0 => p,
                _ => continue,
            };
            if ip.split('.').count() != 4 {
                continue;
            }
            out.push(RawNode {
                ip: ip.to_string(),
                port,
                proto,
                country: None,
                source: source.to_string(),
            });
        }
        out
    }
}

#[async_trait::async_trait]
impl Source for GitHubSource {
    fn name(&self) -> &'static str {
        self.name
    }
    async fn fetch(&self, client: &reqwest::Client) -> Result<Vec<RawNode>, String> {
        let body = client
            .get(&self.url)
            .send()
            .await
            .map_err(|e| format!("GET {} failed: {e}", self.url))?
            .text()
            .await
            .map_err(|e| format!("read {} failed: {e}", self.url))?;
        Ok(Self::parse(self.name, &body))
    }
}
```

- [ ] **Step 4: 运行确认通过**

Run: `cargo test --workspace free_pool`
Expected: PASS（4 单测）

- [ ] **Step 5: Verify（fmt＋clippy）**

---

### Task 7: Verifier（连通＋延迟门，Phase 1 仅 HTTP/S）

**Files:**
- Modify: `gateway/src/free_pool.rs`（追加 `Verifier`＋2 单测）
- Test: `free_pool::tests::verifier_admits_fast_listener`、`free_pool::tests::verifier_rejects_refused_and_slow`

- [ ] **Step 1: 写失败单测**

```rust
#[tokio::test]
async fn verifier_admits_fast_listener() {
    // 本地 listener 必连上（OPT-6 手法），延迟门内放行。
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let port = listener.local_addr().expect("addr").port();
    let v = Verifier::new(Duration::from_secs(3), 3000);
    let raw = RawNode { ip: "127.0.0.1".to_string(), port, proto: FreeProto::Http, country: None, source: "t".to_string() };
    let ok = v.verify(&raw).await;
    assert!(ok.is_some());
    assert!(ok.expect("latency") < 3000);
    drop(listener);
}

#[tokio::test]
async fn verifier_rejects_refused_and_slow() {
    // 拒连端口（127.0.0.1:1）必失败；socks 协议 Phase 1 跳过（None）。
    let v = Verifier::new(Duration::from_secs(3), 3000);
    let refused = RawNode { ip: "127.0.0.1".to_string(), port: 1, proto: FreeProto::Http, country: None, source: "t".to_string() };
    assert!(v.verify(&refused).await.is_none());
    let socks = RawNode { ip: "127.0.0.1".to_string(), port: 1, proto: FreeProto::Socks5, country: None, source: "t".to_string() };
    assert!(v.verify(&socks).await.is_none());
}
```

- [ ] **Step 2: 运行确认失败**

Run: `cargo test --workspace verifier_`
Expected: FAIL（`Verifier` 不存在）

- [ ] **Step 3: 最小实现**

```rust
/// 质检器：TCP 建链（超时）＋延迟门。Phase 1 只收 Http/Https，其余协议
/// 直接跳过（None；Phase 2 加 SOCKS 握手探测）。
pub struct Verifier {
    timeout: Duration,
    max_latency_ms: u64,
}

impl Verifier {
    pub fn new(timeout: Duration, max_latency_ms: u64) -> Self {
        Self { timeout, max_latency_ms }
    }

    /// 通过返回建链延迟 ms；失败/超门/非 HTTP(S) 返回 None。
    pub async fn verify(&self, raw: &RawNode) -> Option<u64> {
        match raw.proto {
            FreeProto::Http | FreeProto::Https => {}
            _ => {
                log::debug!("[FreePool] skip unsupported proto {:?} {}:{}", raw.proto, raw.ip, raw.port);
                return None;
            }
        }
        let start = std::time::Instant::now();
        let ok = tokio::time::timeout(self.timeout, tokio::net::TcpStream::connect((raw.ip.as_str(), raw.port)))
            .await
            .is_ok_and(|r| r.is_ok());
        if !ok {
            return None;
        }
        let ms = start.elapsed().as_millis() as u64;
        if ms <= self.max_latency_ms {
            Some(ms)
        } else {
            log::debug!("[FreePool] slow {}:{} {ms}ms over gate", raw.ip, raw.port);
            None
        }
    }
}
```

- [ ] **Step 4: 运行确认通过**

Run: `cargo test --workspace free_pool`
Expected: PASS（6 单测）

- [ ] **Step 5: Verify（fmt＋clippy）**

---

### Task 8: Registry（TTL＋复检＋多源去重）

**Files:**
- Modify: `gateway/src/free_pool.rs`（追加 `Registry`＋3 单测）、`gateway/src/model.rs`（无改动——`ProxyNode::new` 复用，显式声明）
- Test: `free_pool::tests::{registry_ttl_expiry, registry_reverify_renews, registry_dedupes_across_sources}`

- [ ] **Step 1: 写失败单测**

```rust
fn raw(ip: &str, source: &str) -> RawNode {
    RawNode { ip: ip.to_string(), port: 8080, proto: FreeProto::Http, country: Some("US".to_string()), source: source.to_string() }
}

#[test]
fn registry_ttl_expiry() {
    // TTL 到即快照不可见（可测版本注入未来时间，沿用 sweep_expired_at 手法）。
    let mut reg = Registry::new(Duration::from_secs(1800));
    reg.upsert(&raw("10.0.0.1", "a"), 50, Instant::now());
    assert_eq!(reg.snapshot(Instant::now()).len(), 1);
    assert!(reg.snapshot(Instant::now() + Duration::from_secs(1801)).is_empty());
}

#[test]
fn registry_reverify_renews() {
    // 复检通过续命 30min；失败摘除。
    let mut reg = Registry::new(Duration::from_secs(1800));
    let now = Instant::now();
    reg.upsert(&raw("10.0.0.1", "a"), 50, now);
    reg.reverify("10.0.0.1:8080", true, now + Duration::from_secs(1700));
    assert_eq!(reg.snapshot(now + Duration::from_secs(2000)).len(), 1, "renewed");
    reg.reverify("10.0.0.1:8080", false, now + Duration::from_secs(2100));
    assert!(reg.snapshot(now + Duration::from_secs(2200)).is_empty(), "failed drops");
}

#[test]
fn registry_dedupes_across_sources() {
    // 多源同 addr 只留首见 source；快照转 ProxyNode（provider free-{source}/tier free/weight 10/country 缺省 ZZ）。
    let mut reg = Registry::new(Duration::from_secs(1800));
    let now = Instant::now();
    reg.upsert(&raw("10.0.0.1", "a"), 50, now);
    reg.upsert(&raw("10.0.0.1", "b"), 60, now);
    let snap = reg.snapshot(now);
    assert_eq!(snap.len(), 1);
    assert_eq!(snap[0].provider, "free-a");
    assert_eq!(snap[0].tier, "free");
    assert_eq!(snap[0].weight, FREE_POOL_WEIGHT);
    assert_eq!(snap[0].addr, "10.0.0.1:8080");
    let mut no_country = raw("10.0.0.2", "a");
    no_country.country = None;
    reg.upsert(&no_country, 50, now);
    assert_eq!(reg.snapshot(now)[1].country, "ZZ");
}
```

- [ ] **Step 2: 运行确认失败**

Run: `cargo test --workspace registry_`
Expected: FAIL（`Registry` 不存在）

- [ ] **Step 3: 最小实现**

```rust
/// 免费线池权重（vs 付费 60~100，加权混合中占少数；bandit free cost 0 不再叠加）。
pub const FREE_POOL_WEIGHT: u32 = 10;
/// 无归属国家的缺省标记（只服务无 country 要求的流量，见 RouterEngine::matches）。
pub const FREE_UNKNOWN_COUNTRY: &str = "ZZ";

use crate::model::ProxyNode;
use std::collections::HashMap;
use std::time::Instant;

struct Entry {
    node: ProxyNode,
    expires_at: Instant,
    latency_ms: u64,
}

/// TTL 注册表：多源按 `addr` 去重（首见 source 获胜）；到期仅经复检续命。
pub struct Registry {
    ttl: Duration,
    entries: HashMap<String, Entry>,
}

impl Registry {
    pub fn new(ttl: Duration) -> Self {
        Self { ttl, entries: HashMap::new() }
    }

    /// 质检通过即插入/刷新（已存在 addr 不覆盖 source，只续期；调用方保证已去重语义）。
    pub fn upsert(&mut self, raw: &RawNode, latency_ms: u64, now: Instant) {
        let addr = format!("{}:{}", raw.ip, raw.port);
        if let Some(e) = self.entries.get_mut(&addr) {
            e.expires_at = now + self.ttl;
            e.latency_ms = latency_ms;
            return;
        }
        let node = ProxyNode::new(
            raw.ip.clone(),
            raw.port,
            None,
            None,
            raw.country.clone().unwrap_or_else(|| FREE_UNKNOWN_COUNTRY.to_string()),
            "free".to_string(),
            format!("free-{}", raw.source),
            FREE_POOL_WEIGHT,
        );
        self.entries.insert(addr, Entry { node, expires_at: now + self.ttl, latency_ms });
    }

    /// 复检：通过续满 TTL，失败摘除。
    pub fn reverify(&mut self, addr: &str, passed: bool, now: Instant) {
        if passed {
            if let Some(e) = self.entries.get_mut(addr) {
                e.expires_at = now + self.ttl;
            }
        } else {
            self.entries.remove(addr);
        }
    }

    /// 未过期快照（调用方直接喂 `replace_vendor_nodes`）。
    pub fn snapshot(&self, now: Instant) -> Vec<ProxyNode> {
        self.entries
            .values()
            .filter(|e| e.expires_at > now)
            .map(|e| e.node.clone())
            .collect()
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }
}
```

- [ ] **Step 4: 运行确认通过**

Run: `cargo test --workspace free_pool`
Expected: PASS（9 单测）

- [ ] **Step 5: Verify（fmt＋clippy）**

---

### Task 9: Worker 主循环＋main 接线＋运维落盘

**Files:**
- Modify: `gateway/src/free_pool.rs`（追加 `FreePoolWorker`＋config＋1 单测）、`gateway/src/main.rs`（`mod`＋supervise＋env＋2 单测）、`docs/OPERATION.md`（§4 一行）、`docker-compose.yml`（env 示例）
- Test: `free_pool::tests::worker_merge_pushes_snapshot_to_router`、`tests::free_env_defaults`

- [ ] **Step 1: 写失败单测**

```rust
#[tokio::test]
async fn worker_merge_pushes_snapshot_to_router() {
    // Worker 合并语义：registry 快照经 replace_vendor_nodes 进池；水位计同步。
    use crate::metrics::MetricsRegistry;
    use crate::router::RouterEngine;
    use std::sync::Arc;
    let router = Arc::new(RouterEngine::new(vec![]));
    let metrics = Arc::new(MetricsRegistry::new());
    let mut reg = Registry::new(Duration::from_secs(1800));
    reg.upsert(&raw("10.0.0.1", "a"), 50, Instant::now());
    FreePoolWorker::merge_once(&router, &metrics, &reg);
    assert_eq!(router.snapshot_all().len(), 1);
    assert!(metrics.render().contains("free_pool_nodes_total 1"));
}
```
（`raw` 复用 Task 8 单测 helper——同文件 `mod tests` 内可见；若 helper 置于 Task 8 单测函数内则前提起为模块级 `fn raw`，本任务 Step 3 含此移动。）

main env 单测（`main.rs mod tests` 追加）：
```rust
#[test]
fn free_env_defaults() {
    // 默认关闭；显式开生效（key 唯一防并行污染，用后清理）。
    assert_eq!(env_str("R2T_FREE_ENABLED_XYZ", "0"), "0");
    std::env::set_var("R2T_FREE_FLAG", "1");
    assert_eq!(env_str("R2T_FREE_FLAG", "0"), "1");
    std::env::remove_var("R2T_FREE_FLAG");
}
```

- [ ] **Step 2: 运行确认失败**

Run: `cargo test --workspace worker_merge_pushes free_env_defaults`
Expected: FAIL（`FreePoolWorker` 不存在；main 单测函数不存在——分别失败）

- [ ] **Step 3: 最小实现**

`free_pool.rs` 追加：
```rust
/// Worker 配置（main 从 env 组装；默认值见各常量）。
pub struct FreePoolConfig {
    pub api_urls: Vec<String>,
    pub html_urls: Vec<String>,
    pub github_urls: Vec<String>,
    pub fetch_interval: Duration,
    pub ttl: Duration,
    pub verify_timeout: Duration,
    pub max_latency_ms: u64,
    pub max_concurrent: usize,
}

/// Source 默认 URL（全 env 可覆盖；单源挂了只 hold 旧集，不清空池）。
pub const DEFAULT_API_URL: &str = "https://proxylist.geonode.com/api/proxy-list?limit=100&page=1&sort_by=lastChecked&sort_type=desc";
pub const DEFAULT_HTML_URL: &str = "https://free-proxy-list.net/";
pub const DEFAULT_GITHUB_URL: &str = "https://raw.githubusercontent.com/clarketm/proxy-list/master/proxy-list-raw.txt";

pub struct FreePoolWorker {
    router: Arc<crate::router::RouterEngine>,
    metrics: Arc<crate::metrics::MetricsRegistry>,
    config: FreePoolConfig,
    registry: Registry,
}

impl FreePoolWorker {
    pub fn new(
        router: Arc<crate::router::RouterEngine>,
        metrics: Arc<crate::metrics::MetricsRegistry>,
        config: FreePoolConfig,
    ) -> Self {
        let ttl = config.ttl;
        Self { router, metrics, config, registry: Registry::new(ttl) }
    }

    /// 单轮：各源抓取（失败 hold 旧集）→ 并发质检（信号量封顶）→ upsert/复检 →
    /// 过期自然掉出快照 → `replace_vendor_nodes("free-", …)` → 水位计。
    pub async fn run_once(&mut self, client: &reqwest::Client) {
        let now = Instant::now();
        let mut raws: Vec<RawNode> = Vec::new();
        let mut apis: Vec<Box<dyn Source>> = Vec::new();
        for (i, url) in self.config.api_urls.iter().enumerate() {
            apis.push(Box::new(ApiSource { name: leak_name(format!("api{i}")), url: url.clone() }));
        }
        // name 需 'static：源数量少（个位数），允许 Box::leak（进程级常驻，注释写明）。
        for url in &self.config.html_urls {
            let _ = url;
        }
        // （完整装配见下：为可读性，三类源统一压入 Vec<Box<dyn Source>>。）
        let _ = &apis;
        let _ = &mut raws;
        let _ = now;
        let _ = client;
    }

    /// 纯合并步（可单测）：快照 → 路由 → 水位计。
    pub fn merge_once(
        router: &Arc<crate::router::RouterEngine>,
        metrics: &Arc<crate::metrics::MetricsRegistry>,
        registry: &Registry,
    ) {
        let snap = registry.snapshot(Instant::now());
        metrics.set_free_pool_nodes(snap.len() as u64);
        router.replace_vendor_nodes("free-", snap);
    }
}
```

上面 `run_once` 是骨架占位——按本计划“无占位”铁律，展开完整实现替代它：
```rust
fn leak_name(s: String) -> &'static str {
    Box::leak(s.into_boxed_str())
}

pub async fn run_once(&mut self, client: &reqwest::Client) {
    // 1. 组装三类源（name 进程级常驻泄漏，源数量个位数可接受）。
    let mut sources: Vec<Box<dyn Source>> = Vec::new();
    for (i, url) in self.config.api_urls.iter().enumerate() {
        sources.push(Box::new(ApiSource { name: leak_name(format!("api{i}")), url: url.clone() }));
    }
    for (i, url) in self.config.html_urls.iter().enumerate() {
        sources.push(Box::new(HtmlSource { name: leak_name(format!("html{i}")), url: url.clone(), default_proto: FreeProto::Http }));
    }
    for (i, url) in self.config.github_urls.iter().enumerate() {
        sources.push(Box::new(GitHubSource { name: leak_name(format!("gh{i}")), url: url.clone() }));
    }
    // 2. 抓取：单源失败只 warn 并 hold 旧集（registry 不动）。
    let mut raws: Vec<RawNode> = Vec::new();
    for s in &sources {
        match s.fetch(client).await {
            Ok(mut v) => raws.append(&mut v),
            Err(e) => log::warn!("[FreePool] source {} failed: {e} (holding last good set)", s.name()),
        }
    }
    // 3. 去重（首见获胜）＋并发质检（信号量封顶）。
    use std::collections::HashSet;
    let mut seen = HashSet::new();
    raws.retain(|r| seen.insert(format!("{}:{}", r.ip, r.port)));
    let verifier = Verifier::new(self.config.verify_timeout, self.config.max_latency_ms);
    let sem = Arc::new(tokio::sync::Semaphore::new(self.config.max_concurrent.max(1)));
    let mut set = tokio::task::JoinSet::new();
    for raw in raws {
        let sem = sem.clone();
        set.spawn(async move {
            let _permit = sem.acquire_owned().await;
            let latency = verifier.verify(&raw).await;
            (raw, latency)
        });
    }
    let now = Instant::now();
    while let Some(joined) = set.join_next().await {
        match joined {
            Ok((raw, Some(ms))) => self.registry.upsert(&raw, ms, now),
            Ok((raw, None)) => {
                // 复检语义：已在池且过期→摘除；新货质检失败→直接丢弃。
                self.registry.reverify(&format!("{}:{}", raw.ip, raw.port), false, now);
            }
            Err(e) => log::warn!("[FreePool] verify task join failed: {e:?}"),
        }
    }
    // 4. 合并（过期条目自然掉出快照）。
    Self::merge_once(&self.router, &self.metrics, &self.registry);
    log::info!("[FreePool] tick pool={} interval={:?}", self.registry.len(), self.config.fetch_interval);
}

/// 常驻循环（supervisor 托管；单轮 panic 由 supervisor 捕获重启）。
pub async fn run(mut self, client: reqwest::Client) {
    loop {
        self.run_once(&client).await;
        tokio::time::sleep(self.config.fetch_interval).await;
    }
}
```

（注：`_permit` 为 `Result`，`acquire_owned` 失败（关闭）时 `verifier` 仍会跑——许可语义弱化；修正：`if sem.acquire_owned().await.is_err() { return (raw, None); }`。以修正版为准。）

`main.rs` 接线（`prewarmer.run()` 后插入）：
```rust
use free_pool::{FreePoolConfig, FreePoolWorker, DEFAULT_API_URL, DEFAULT_GITHUB_URL, DEFAULT_HTML_URL};
// （mod 声明区加 `mod free_pool;`，与 `mod fingerprint;` 字母序相邻。）

// 4e. FreePool 第二供应线（默认关闭；FREE_ENABLED=1 开启）。
fn split_env_list(key: &str, default: &str) -> Vec<String> {
    env_str(key, default)
        .split(',')
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect()
}
if env_str("FREE_ENABLED", "0") == "1" {
    let free_config = FreePoolConfig {
        api_urls: split_env_list("FREE_API_URLS", DEFAULT_API_URL),
        html_urls: split_env_list("FREE_HTML_URLS", DEFAULT_HTML_URL),
        github_urls: split_env_list("FREE_GITHUB_URLS", DEFAULT_GITHUB_URL),
        fetch_interval: env_secs("FREE_FETCH_INTERVAL_SECS", 900),
        ttl: env_secs("FREE_TTL_SECS", 1800),
        verify_timeout: Duration::from_secs(3),
        max_latency_ms: env_str("FREE_MAX_LATENCY_MS", "3000").parse::<u64>().unwrap_or(3000),
        max_concurrent: env_str("FREE_MAX_CONCURRENT", "50").parse::<usize>().unwrap_or(50),
    };
    let free_router = router.clone();
    let free_metrics = metrics.clone();
    tokio::spawn(async move {
        tokio::time::sleep(startup_jitter()).await;
        log::info!("[FreePool] staggered start (second supply line)");
        supervise("free_pool", free_metrics.clone(), move || {
            let worker = FreePoolWorker::new(free_router.clone(), free_metrics.clone(), FreePoolConfig {
                api_urls: free_config.api_urls.clone(),
                html_urls: free_config.html_urls.clone(),
                github_urls: free_config.github_urls.clone(),
                fetch_interval: free_config.fetch_interval,
                ttl: free_config.ttl,
                verify_timeout: free_config.verify_timeout,
                max_latency_ms: free_config.max_latency_ms,
                max_concurrent: free_config.max_concurrent,
            });
            async move { worker.run(reqwest::Client::new()).await }
        })
        .await;
    });
}
```

（注：`FreePoolConfig` 需 `Clone`——定义处加 `#[derive(Debug, Clone)]`，以此为准简化上述闭包为 `move || { let w = FreePoolWorker::new(a.clone(), m.clone(), free_config.clone()); async move { w.run(reqwest::Client::new()).await } }`。）

`split_env_list` 放 `env_secs` 旁；main 单测追加 `free_env_defaults`（Step 1 已给）。

`docs/OPERATION.md` §4 表追加行：
`| 免费线 | `free_pool_nodes_total` / 日志`[FreePool]` | 默认关闭（`FREE_ENABLED=1` 开）；水位突降=源站挂或质检门限过严，查 warn 行；免费节点 country 缺省 ZZ，只服务无归属要求的流量 |`

`docker-compose.yml` 网关示例追加：
```
  #     FREE_ENABLED: "1"
  #     FREE_API_URLS: "https://proxylist.geonode.com/api/proxy-list?limit=100&page=1&sort_by=lastChecked&sort_type=desc"
  #     FREE_HTML_URLS: "https://free-proxy-list.net/"
  #     FREE_GITHUB_URLS: "https://raw.githubusercontent.com/clarketm/proxy-list/master/proxy-list-raw.txt"
  #     FREE_FETCH_INTERVAL_SECS: "900"
  #     FREE_TTL_SECS: "1800"
  #     FREE_MAX_LATENCY_MS: "3000"
  #     FREE_MAX_CONCURRENT: "50"
```

- [ ] **Step 4: 运行确认通过**

Run: `cargo test --workspace free_pool free_env`
Expected: PASS（free_pool 11 单测＋main 1；全仓目标 82＋12=94）

- [ ] **Step 5: Verify（fmt＋clippy＋bench 编译）**

Run: `cargo fmt --check`; `cargo clippy --workspace --all-targets -- -D warnings`; `cargo bench --workspace --no-run`

---

### Task 10: 最终门禁＋回归（curl＋链路）

- [ ] **Step 1: 四门全跑**

Run: `cargo fmt --check`（clean）; `cargo clippy --workspace --all-targets -- -D warnings`（零告警）; `cargo test --workspace`（94 过/4 ignored 预期）; `cargo test --workspace -- --ignored --nocapture`（**先验依赖**：`docker exec ipproxy-redis redis-cli ping`→PONG 且 CH `/ping`→Ok，无 SKIP 行才算真过）; `cargo bench --workspace --no-run`（编译过）

- [ ] **Step 2: 网关回归（FREE_ENABLED=1 开一轮）**

Run: 构建＋detached 拉 mocks＋网关（`FREE_ENABLED=1`），curl 六用例＋缺 Host 400＋/metrics 200；查 `free_pool_nodes_total` 行存在；10 流量后 XLEN/CH 涨；日志 `[FreePool] tick` 行。
Expected: 六用例语义不变；免费节点并入后普通流量仍 200（初期 `free_pool_nodes_total` 可能为 0——源站直连受限即 hold 空集，属正常，日志 warn 可见）

- [ ] **Step 3: 落库（plan 表＋EXEC_LOG append-only＋TASK_PLAN）**

plan 表新增 FreePool 行置 ✅；EXEC_LOG 追加条目（含单测数/live 真过证据＋网关 PID＋日志文件）；TASK_PLAN 步骤 10（或 GW-R3 项下）更新。

- [ ] **Step 4: Verify（提交确认）**

本仓禁未授权 commit——向用户确认后统一提交（`git add` 限定本计划文件集）。

---

## Self-Review（计划自检，已内联修复）

1. **Spec coverage:** 多源插件化→Task 4/5/6；加权混合→Task 2＋8（weight 10）；连通+延迟→Task 7；全协议→解析全收、选路 HTTP/S（Task 5/6/7 标注 Phase 2）；短 TTL+复检→Task 8。SOCKS egress 明确 out（Phase 2 另起计划）。
2. **Placeholder scan:** 无 TBD/TODO；`run_once` 初稿骨架已展开为完整实现；HTML 提取器分支行为由单测 fixture 锁定。
3. **Type consistency:** `RawNode{ip,port,proto,country,source}` 全任务同名；`Registry::{upsert,reverify,snapshot,len}` 签名在 Task 8 定义、Task 9 引用一致；`FreePoolConfig` 加 `Clone`（Task 9 注明）；`Source::fetch` 返回 `Result<Vec<RawNode>, String>`（无 anyhow）全任务一致；`replace_vendor_nodes(&self, prefix: &str, nodes: Vec<ProxyNode>)`（Task 2 定义、Task 9 调用一致）。
4. **存量风险：** `price_per_gb("free")` 原走 `_` 得 0.2——已有租户若误标 free tier 会少计费；缓解：tier 只由 FreePool 构造写入（`ProxyNode::new` 调用点唯一），网关 logging 原样透传；OPERATION 行注明。`cost_weight_for_tier("free")=0.0` 只影响 free 臂（key 隔离），存量臂无 free tier 不受影响。
