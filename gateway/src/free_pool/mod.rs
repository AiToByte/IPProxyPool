//! FreePool 第二供应线：公开免费源抓取 → 两级质检 → TTL 注册 → Router 合并。
//!
//! Phase 1 只收 HTTP(S)（零网关转发改动）；SOCKS 只解析标注、merge 过滤
//! （Phase 2 做 SOCKS egress）。默认关闭（`FREE_ENABLED=1` 开启）。
//! 免费线零信任（arXiv:2403.02445：16,923 篡改内容）：复检基址 https-only＋
//! canary 防篡改＋OPERATION 禁敏感流量。
//!
//! # 子模块划分（OPT-R18 P2-1 第二批）
//!
//! 本文件曾是 3727 行的单文件，四类职责混在一起：抓取源、质检、注册表状态、
//! 后台 worker。改动任何一处都要在 3700 行里定位上下文。现按职责拆分：
//!
//! | 子模块 | 职责 |
//! | --- | --- |
//! | `sources.rs` | 三类抓取源（JSON API / HTML / GitHub 裸列表）＋礼貌轮询缓存 |
//! | `verify.rs` | 两级质检：`Verifier`（轻量 TCP/协议）、`FullChecker`（canary 全量）＋匿名度分级 |
//! | `registry.rs` | 池状态：`Health`（EWMA）、`Registry`（TTL/淘汰/熔断）、`SourceGuard`、intake 配额 |
//! | `worker.rs` | 后台 worker 与配置（env 组装、周期抓取、geo 裁决、Router 合并） |
//! | `tests.rs` | 全部单测（原文件后半段） |
//!
//! **这是纯搬移**：所有条目逐字迁移，行为零变更。
//!
//! ## 两处必须留意的 Rust 语义（拆分时踩到）
//!
//! ① **子模块经 `use super::*;` 继承父模块的私有字段与自由函数**——私有项对其
//! 定义模块的**全部后代**可见。故父模块的私有 `use`（`EgressProto`/`ProxyNode`/
//! `RouterEngine`/`HashMap`/`HashSet`/`Arc`）仍是子模块名字的来源，不必每个
//! 子模块重复 import。
//! ② **`pub use` 再导出是为了保住对外路径**（`crate::free_pool::Registry` 等）。
//! 与 `router` 那次拆分不同：那里搬的是**固有方法**（调用方写 `engine.foo()`
//! 即可，Rust 不要求 import，故再导出反而触发 `unused_imports`）；这里搬的是
//! **类型与常量**，不 `pub use` 的话外部调用点（`main.rs` 的
//! `use free_pool::{FreePoolConfig, ...}`）全部要改成深层路径。
//! ③ 少数私有项被 `tests.rs` 用到（`is_idle_over` / `intake_cap_limit` /
//! `cap_intake` / `baseline_bases` / `Registry::entries` / `Entry::health`），
//! 已放宽为 `pub(super)`——`pub(super)` 的作用域是父模块**及其全部后代**，
//! 正好覆盖 `free_pool::tests`，且不扩大到 crate 级。

use std::time::Duration;
use std::time::Instant;

/// Maximum decoded response body accepted from any public free-proxy source.
pub const MAX_SOURCE_BODY_BYTES: usize = 2 * 1024 * 1024;
/// 抓取协议（P2：全协议进池；出站形态见 `egress_of`，选路隔离见 `RouterEngine::matches`）。
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

/// 抓取结果（v2：304 NotModified 显式信号，供 SourceGuard 区分“未变更”与“零产出”）。
#[derive(Debug)]
pub struct FetchOutcome {
    pub nodes: Vec<RawNode>,
    /// true＝源站明确表示未变更（GitHub ETag/304），调用方不得计零产出轮次。
    pub not_modified: bool,
}

impl FetchOutcome {
    pub fn nodes(nodes: Vec<RawNode>) -> Self {
        Self {
            nodes,
            not_modified: false,
        }
    }

    pub fn not_modified() -> Self {
        Self {
            nodes: Vec::new(),
            not_modified: true,
        }
    }
}

/// 抓取源插件接口（API / HTML / GitHub 各一实现；零新依赖，错误文案 String）。
#[async_trait::async_trait]
pub trait Source: Send + Sync {
    fn name(&self) -> &'static str;
    async fn fetch(&self, client: &reqwest::Client) -> Result<FetchOutcome, String>;
}

/// Read a successful source response with a hard cap on decoded bytes.
async fn read_source_body(resp: reqwest::Response) -> Result<String, String> {
    if !resp.status().is_success() {
        return Err(format!("source returned HTTP {}", resp.status()));
    }
    if resp
        .content_length()
        .is_some_and(|len| len > MAX_SOURCE_BODY_BYTES as u64)
    {
        return Err("source response exceeds size limit".to_string());
    }
    let mut stream = resp.bytes_stream();
    let mut body = Vec::new();
    use futures::StreamExt;
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|_| "source response read failed".to_string())?;
        if body.len().saturating_add(chunk.len()) > MAX_SOURCE_BODY_BYTES {
            return Err("source response exceeds size limit".to_string());
        }
        body.extend_from_slice(&chunk);
    }
    String::from_utf8(body).map_err(|_| "source response is not UTF-8".to_string())
}

/// 条件 GET（R3-5 礼貌轮询共享件；GitHubSource 自有实现不动，存量冻结）。
/// 发 `If-None-Match`／`If-Modified-Since`（有缓存才带）；304→`Ok(None)`
/// （调用方包 `not_modified`，SourceGuard 不计数）；200→更新缓存并 `Ok(Some(body))`。
async fn conditional_get(
    client: &reqwest::Client,
    url: &str,
    etag: &parking_lot::Mutex<Option<String>>,
    last_modified: &parking_lot::Mutex<Option<String>>,
) -> Result<Option<String>, String> {
    let mut req = client.get(url);
    if let Some(e) = etag.lock().clone() {
        req = req.header("If-None-Match", e);
    }
    if let Some(m) = last_modified.lock().clone() {
        req = req.header("If-Modified-Since", m);
    }
    let resp = req
        .send()
        .await
        .map_err(|e| format!("GET {url} failed: {e}"))?;
    if resp.status() == reqwest::StatusCode::NOT_MODIFIED {
        return Ok(None);
    }
    let new_etag = resp
        .headers()
        .get("etag")
        .and_then(|v| v.to_str().ok())
        .map(str::to_owned);
    let new_modified = resp
        .headers()
        .get("last-modified")
        .and_then(|v| v.to_str().ok())
        .map(str::to_owned);
    let body = read_source_body(resp).await?;
    *etag.lock() = new_etag;
    *last_modified.lock() = new_modified;
    Ok(Some(body))
}

/// JSON API 源（默认 Geonode；URL env 可配，见 Task 12）。
/// port 兼容字符串/数字双形态；缺 country 视为 None（后继标 ZZ）。
/// R3-5 礼貌轮询：ETag/Last-Modified 缓存＋304（对齐 GitHubSource）。
/// OPT-R4 B1：可公网回连 IP 判定——过滤 bogon/私网/回环/链路本地/组播/未指定。
/// 只认 IPv4 点分四段（与三源 parse 口径一致；IPv6/主机名/畸形一律 false，
/// 调用方跳过）。x.y.z.0 主机位零保持放行（代理端口可配，非 bogon）。
pub fn is_routable_ip(ip: &str) -> bool {
    let mut parts = ip.trim().split('.');
    let mut octets = [0u8; 4];
    for o in octets.iter_mut() {
        match parts.next().and_then(|s| s.parse::<u8>().ok()) {
            Some(v) => *o = v,
            None => return false,
        }
    }
    if parts.next().is_some() {
        return false;
    }
    let [a, b, c, _] = octets;
    if a == 0 || a == 10 || a == 127 || a >= 224 {
        return false;
    }
    if (a == 100 && (64..=127).contains(&b))
        || (a == 169 && b == 254)
        || (a == 172 && (16..=31).contains(&b))
        || (a == 192 && b == 0)
        || (a == 192 && b == 88 && c == 99)
        || (a == 192 && b == 168)
        || (a == 198 && (18..=19).contains(&b))
        || (a == 198 && b == 51 && c == 100)
        || (a == 203 && b == 0 && c == 113)
    {
        return false;
    }
    true
}
// 共享 import（子模块经 `use super::*;` 继承；见文件头「Rust 语义」①）。
use crate::model::EgressProto;
use crate::model::ProxyNode;
use crate::router::RouterEngine;
use std::collections::HashMap;
use std::collections::HashSet;
use std::sync::Arc;

/// OPT-R15：判定某条目是否已空闲超过 `idle`，**只依赖相对时长**。
///
/// 为什么不写成 `t + idle <= now`：那样需要 `checked_add`，而 `Instant` 基准在
/// Windows 上是系统启动时刻，开机初期做加法越界会 panic。也不写成
/// `now.checked_sub(idle).unwrap_or(now) <= t`：Windows 上 `Instant::now().elapsed()`
/// 可能只有微秒级（实测刚开机时仅 1.7µs），`checked_sub` 会返 None，一旦返 None
/// 就退化成「cutoff == now」，把 `retain` 的谓词变成恒假，从而清空**全部**客户端缓存。
///
/// `saturating_duration_since` 在 `t` 晚于 `now` 时返回 0（不会 panic），
/// 因此本函数对「时钟基准任意近」「t 为未来值」都安全。
fn is_idle_over(last_used: Instant, now: Instant, idle: Duration) -> bool {
    now.saturating_duration_since(last_used) >= idle
}

mod registry;
mod sources;
mod verify;
mod worker;

// 对外路径保持不变的再导出。
//
// **只登记真正被外部或 `tests` 用到的条目**：本模块在 `main.rs` 里是
// `mod free_pool;`（crate 私有），故 `pub use` 的可见性止于 crate 内；
// 未被任何人使用的再导出会被 `unused_imports` 判红（`clippy -D warnings` 会拦）。
// 曾一次性把子模块的全部 `pub` 条目都再导出，结果 10 个常量被判 unused——
// 它们只在 `registry.rs` 内部使用，不需要对外。
// `cfg_attr(not(test), allow(unused_imports))` 的理由：这些再导出的消费方是
// **子模块（经 `use super::*`）与 `tests.rs`**，而 `tests.rs` 只在 `cfg(test)`
// 下编译。于是非测试构建里会有一批"没人用"的再导出被判 unused——但它们
// **不是死代码**：删掉会让测试或兄弟模块找不到名字。
// 与 `router.rs` 拆分时用 `#[cfg_attr(not(test), allow(dead_code))]` 同理：
// 属性只放宽"非测试构建"，测试构建下仍然必须真的被用上（否则照样报错）。
#[cfg_attr(not(test), allow(unused_imports))]
pub use registry::{Health, Registry, SourceGuard};
#[cfg_attr(not(test), allow(unused_imports))]
pub use sources::{ApiSource, GitHubSource, HtmlSource, UriSource, API_MAX_PAGES};
#[cfg_attr(not(test), allow(unused_imports))]
pub use verify::{
    classify_anonymity, egress_of, socks_target_of_base, AnonLevel, FullCheckResult, FullChecker,
    Verifier, FULL_CHECK_MARKER,
};
pub use worker::{
    FreePoolConfig, FreePoolWorker, DEFAULT_API_URL, DEFAULT_FULL_CHECK_BASE, DEFAULT_GITHUB_URL,
    DEFAULT_HTML_URL, DEFAULT_URI_URL,
};

/// VPN-IMMUNE：共享出站 Client（抓取＋基线）：显式禁用系统代理。
/// reqwest 默认跟随 OS 系统代理（Windows 注册表 Clash `127.0.0.1:7890`），
/// 操作员开 VPN 即污染基线/抓取；`no_proxy()` 后行为与操作员 VPN 状态无关。
/// 显式代理客户端（桥/复检/探针）本就免疫（`proxy()` 置 auto_sys_proxy=false，
/// reqwest 0.12.28 `async_impl/client.rs:1414-1418` 实锤）。
pub fn shared_client() -> reqwest::Client {
    reqwest::Client::builder()
        .no_proxy()
        .build()
        .expect("shared client builds")
}

#[cfg(test)]
mod tests;
