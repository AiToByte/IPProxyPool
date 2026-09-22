# Phase 2 SOCKS Egress Implementation Plan（2026年9月22日）

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.
>
> **Goal:** 免费/静态 SOCKS4/5 节点可进池、可质检、可被显式 `X-Proxy-Proto: socks5/socks4` 请求选中，
> 经网关内 **HTTP→SOCKS 翻译桥** 出站；默认流量永不触碰 SOCKS 节点（选路隔离＋peer 守卫双保险）；
> 现有 HTTP 数据面行为零变化。
>
> **Architecture:** `EgressProto`（model）→ Router 按 proto 隔离匹配（默认仅 Http）
> → 网关 `proxy_upstream_filter` 短路分支（显式 socks 请求：选路→桥执行→合成响应→ctx 记账，
> logging/计量/遥测/bandit 全复用）→ `socks_handshake`（RFC1928/1929＋SOCKS4 CONNECT，零新依赖）
> → `socks_bridge`（per-node reqwest Client 缓存＋body 上限＋hop 头过滤）
> → free_pool 全收（Verifier 握手/CONNECT 验证＋FullChecker 经 socks 复检＋快照含 socks＋by-proto 水位）
> → prober/pool 跟进（socks 探测＋greeting 预热）→ main env＋OPERATION＋compose。
>
> **Tech Stack:** Rust（reqwest 0.12 `socks` feature——**空 feature，零新 crate**，见勘察；
> tokio TcpStream 手写握手；DashMap Client 缓存沿 prober OPT-5 模式）。
> 零新依赖（不引 tokio-socks 等）。
>
> **Deviations locked（勘察结论，以本节为准）：**
> - v2 §4 曾写“socks feature 新依赖”——实测 `socks = []` 为空 feature，无新增 crate，
>   仅 `Cargo.toml` 加一个 feature 名（本轮唯一依赖面变更）。
> - Pingora 0.6 无 SOCKS connector（全仓 grep 零命中），且 `ProxyHttp` 唯一出口是
>   `upstream_peer() -> HttpPeer`（直连 TCP），**不 fork Pingora**：
>   数据面走 `proxy_upstream_filter` 返回 `Ok(false)`＋合成响应
>   （`write_response_header`/`write_response_body` 皆 public，`proxy_purge.rs` 同款写法；
>   不写则框架回 502；`finish` 后 `logging()` 照常运行——已读 `lib.rs:592-621` 确认）。
> - client→gateway 仍是 HTTP（本网关是 HTTP 正向代理，不做客户端 CONNECT 隧道，explicitly out）。
> - SOCKS4：handshake 模块支持 CONNECT（含 userid）；bridge 经 reqwest `socks4://`（reqwest 原生支持）。
> - 预热对 socks 只做 greeting（不擅自对外 CONNECT，礼貌性）；完整 CONNECT 验证归 free_pool
>   Verifier（目标＝自配 `FREE_FULL_CHECK_URL`，consented）。
>
> **File map:**
> - Modify: `gateway/Cargo.toml`（reqwest 加 `socks` feature）、`gateway/src/model.rs`
>   （`EgressProto`＋`ProxyNode.proto`＋`RoutingSpec.proto`）、`gateway/src/router.rs`
>   （proto 隔离匹配＋1 单测）、`gateway/src/gateway.rs`（`proxy_upstream_filter` 分支＋
>   peer 守卫＋`parse_routing_spec` proto 解析＋单测）、`gateway/src/free_pool.rs`
>   （proto 存入节点＋Verifier/FullChecker socks 路径＋快照含 socks＋单测）、
>   `gateway/src/prober.rs`（socks 探测）、`gateway/src/pool.rs`（socks greeting 预热）、
>   `gateway/src/metrics.rs`（`free_pool_nodes_by_proto`）、`gateway/src/main.rs`
>   （`mod socks_bridge`＋`mod socks_handshake`＋2 env）、`docs/OPERATION.md`、`docker-compose.yml`。
> - Create: `gateway/src/socks_handshake.rs`（握手＋单测）、`gateway/src/socks_bridge.rs`
>   （桥＋Client 缓存＋relay-stub 集成单测）。
>
> ## §2. Env 总表（新增 2 个，其余复用 FREE_*）
>
> | Key | 默认 | 语义 |
> |-----|------|------|
> | `SOCKS_BRIDGE_TIMEOUT_SECS` | `20` | 桥单次出站总超时 |
> | `SOCKS_MAX_BODY_BYTES` | `10485760` | 合成响应 body 上限（10MB，超限按失败计＋warn） |
>
> ---

### P2-1: 模型＋选路隔离（EgressProto）

**Files:** `gateway/src/model.rs`（`EgressProto`＋`ProxyNode.proto`＋`RoutingSpec.proto`）、
`gateway/src/router.rs`（matches＋1 新单测）、`gateway/src/gateway.rs`（解析 `X-Proxy-Proto`＋`proto-` token＋1 单测）

- [ ] **Step 1: 写失败单测**

```rust
// router.rs
#[test]
fn proto_isolation_default_http_only() {
    // 默认 spec（无 proto）永不命中 socks 节点；显式 socks5 只命中 socks5；
    // http 显式不命中 socks；socks4 与 socks5 互斥。
}

// gateway.rs parse_routing_spec：
#[test]
fn parse_routing_spec_proto_header_and_token() {
    // X-Proxy-Proto: socks5 → Some(Socks5)；大小写不敏感；
    // Proxy-Auth username token proto-socks4 → Some(Socks4)；
    // 非法值 → None（回落默认 http）；header 优先于 token。
}
```

- [ ] **Step 2: 运行确认失败** Run: `cargo test --workspace proto_isolation parse_routing_spec_proto` Expected: FAIL（类型/字段不存在）
- [ ] **Step 3: 最小实现**

```rust
// model.rs
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EgressProto { Http, Socks5, Socks4 }

impl EgressProto {
    pub fn from_token(tok: &str) -> Option<Self> {
        match tok.to_ascii_lowercase().as_str() {
            "http" | "https" => Some(EgressProto::Http),
            "socks5" | "socks5h" => Some(EgressProto::Socks5),
            "socks4" | "socks4a" => Some(EgressProto::Socks4),
            _ => None,
        }
    }
    pub fn reqwest_scheme(self) -> &'static str {
        match self {
            EgressProto::Http => "http",
            EgressProto::Socks5 => "socks5h", // 远端解析（免费节点场景更稳）
            EgressProto::Socks4 => "socks4",
        }
    }
}
// ProxyNode 加 `pub proto: EgressProto`；::new 保持 8 参（proto 恒 Http，零调用点改动）
// ＋ `pub fn with_proto(mut self, p: EgressProto) -> Self` builder。
// RoutingSpec 加 `pub proto: Option<EgressProto>`（None＝默认 http）。
```

```rust
// router.rs matches 内追加（weight==0 检查之后）：
if let Some(want) = spec.proto {
    if node.proto != want { return false; }
} else if node.proto != EgressProto::Http {
    return false; // 默认流量永不触碰 SOCKS（隔离铁律）
}
```

`parse_routing_spec` 追加：`X-Proxy-Proto` 头 → `EgressProto::from_token`；
Proxy-Auth username token `proto-xxx` 同口径（header 优先，已有 country/session/tier 先例）。

- [ ] **Step 4: 运行确认通过** Run: `cargo test --workspace router gateway model` Expected: 全 PASS（存量 `free_tier_isolation_and_zz_semantics` 等全绿——默认隔离不得改变其断言）
- [ ] **Step 5: Verify（fmt＋clippy）**

---

### P2-2: socks_handshake（RFC1928/1929＋SOCKS4，零新依赖）

**Files:** Create `gateway/src/socks_handshake.rs`（`establish`＋`greet_only`＋4 单测，stub 内联）

- [ ] **Step 1: 写失败单测**（stub 手法沿 Task 6/8：本地 TcpListener 断言握手字节＋回 canned 字节）

```rust
#[tokio::test] async fn socks5_noauth_connect_sends_expected_bytes() // 断言 greeting+CONNECT 字节＋返回已连 stream
#[tokio::test] async fn socks5_userpass_negotiates()                 // 服务端要求 0x02＋账密正确→成功；错账密→Err
#[tokio::test] async fn socks5_refused_and_bad_reply_are_err()       // 拒连端口＋CONNECT reply 非 0x00 → Err（不 panic）
#[tokio::test] async fn socks4_connect_ok()                          // 0x04/0x50 形态＋request granted
```

- [ ] **Step 2: 运行确认失败** Run: `cargo test --workspace socks5_ socks4_` Expected: FAIL（模块不存在；先在 main.rs 加两行 `mod` 使目标可编译）
- [ ] **Step 3: 最小实现**

```rust
/// 经 SOCKS4/5 建到 target 的出站流（调用方持有超时：`timeout()` 包本函数）。
/// methods: 先 offer [NO_AUTH]；服务端回 0x02 且有账密则 RFC1929 子协商；回 0xFF → Err。
/// CONNECT: Socks5 按 target 是否 IP 选 ATYP（IPv4/DOMAIN，不组 IPv6——本仓节点皆 v4/hostname）；
/// Socks4: 0x04 头＋userid（无账密时空串；0x4a 结尾域名形态 Socks4a 顺手支持）。
pub async fn establish(
    proxy_ip: &str, proxy_port: u16,
    username: Option<&str>, password: Option<&str>,
    proto: EgressProto, target_host: &str, target_port: u16,
) -> Result<tokio::net::TcpStream, String>

/// greeting-only（prewarmer 用：证明端口说 SOCKS，不对外 CONNECT）。
pub async fn greet_only(proxy_ip: &str, proxy_port: u16, proto: EgressProto) -> Result<(), String>
```

约束：错误文案 String（沿 free_pool 惯例）；读写用 `AsyncReadExt/AsyncWriteExt`＋`read_exact`
（短读即 Err）；IPv6 target → String Err（显式不支持，注释写明）。

- [ ] **Step 4: 运行确认通过** Run: `cargo test --workspace socks_handshake` Expected: PASS（4 单测）
- [ ] **Step 5: Verify（fmt＋clippy）**

---

### P2-3: socks_bridge（reqwest socks Client 缓存＋执行＋合成件）

**Files:** `gateway/Cargo.toml`（reqwest features 加 `socks`）；Create `gateway/src/socks_bridge.rs`
（`SocksBridge`＋`BridgeRequest/Response`＋hop 头表＋3 单测，relay-stub 集成 1 个）

- [ ] **Step 1: 写失败单测**

```rust
#[test] fn hop_headers_stripped() // connection/transfer-encoding/keep-alive/proxy-*/trailer/upgrade 被过滤；x-custom 保留
#[test] fn proxy_url_for_node_shape() // socks5h://[user:pass@]ip:port；http 节点回 http://…（桥只接 socks，http 进来即 None）
#[tokio::test] async fn bridge_relays_through_stub_to_mock() // 本地 relay-stub（握手→CONNECT→copy_bidirectional 到 mock HTTP）＋mock；桥 GET→mock body；status/headers 透回
```

- [ ] **Step 2: 运行确认失败** Run: `cargo test --workspace hop_headers_stripped proxy_url_for_node bridge_relays` Expected: FAIL
- [ ] **Step 3: 最小实现**

```rust
/// P2 数据面桥：显式 socks 请求的出站执行器（Client 按 node.addr 缓存复用，沿 prober OPT-5 模式＋TTL 淘汰）。
pub struct SocksBridge {
    clients: DashMap<String, CachedClient>, // key = node.addr
    timeout: Duration,   // SOCKS_BRIDGE_TIMEOUT_SECS
    max_body: u64,       // SOCKS_MAX_BODY_BYTES
}
pub struct BridgeRequest { pub method: String, pub url: String, pub headers: Vec<(String, String)>, pub body: Option<bytes::Bytes> }
pub struct BridgeResponse { pub status: u16, pub headers: Vec<(String, String)>, pub body: bytes::Bytes }

impl SocksBridge {
    pub fn new(timeout: Duration, max_body: u64) -> Self;
    /// 执行（reqwest socks5h/socks4 proxy；body 超限/超时/建 Client 失败 → Err(String)；调用方记 failed_addrs＋重试）。
    pub async fn fetch(&self, node: &ProxyNode, req: BridgeRequest) -> Result<BridgeResponse, String>;
    pub fn evict_idle(&self) -> usize; // main 滴答复用 sweep 节拍（签名沿 prober）
}
```

约束：`fetch` 内 `Client::builder().proxy(Proxy::all(proxy_url)).timeout()`（proxy_url 含 userinfo 编码复用
prober `encode_userinfo`——将其提为 `pub(crate)`，不复制第二份）；
响应体 `bytes_stream` 逐 chunk 累加，超 `max_body` 即 Err（防内存爆）；
hop 头表常量 `HOP_HEADERS: [&str; 7]`（connection/transfer-encoding/keep-alive/proxy-authenticate/
proxy-authorization/trailer/upgrade＋te 小写比较）。

- [ ] **Step 4: 运行确认通过** Run: `cargo test --workspace socks_bridge` Expected: PASS（3 单测；relay-stub 内联 ~50 行）
- [ ] **Step 5: Verify（fmt＋clippy）**

---

### P2-4: 网关 filter 分支＋peer 守卫

**Files:** `gateway/src/gateway.rs`（`proxy_upstream_filter` override＋`upstream_peer` socks 守卫＋
`SmartProxyGateway.bridge: Option<Arc<SocksBridge>>` 字段＋构造点同步＋2 单测）；
`gateway/src/main.rs`（bridge 装配＋evict 滴答——P2-7 一并落地，此处只留字段与 Default None）

- [ ] **Step 1: 写失败单测**

```rust
#[test] fn upstream_peer_refuses_socks_node() // socks 节点直达 upstream_peer → Err（503 映射由 Pingora 完成；单测只断 Err）
#[test] fn socks_request_build_shape() // BridgeRequest 组装纯函数：method/absolute-url/host 头保留＋X-API-Key 已脱敏（沿用 request_filter 后的 header）
```

注：filter 全链路（选路→桥→合成）由 P2-8 本地 E2E 覆盖（curl 级），此处只锁纯函数与守卫。

- [ ] **Step 2: 运行确认失败** Run: `cargo test --workspace upstream_peer_refuses socks_request_build` Expected: FAIL
- [ ] **Step 3: 最小实现**

```rust
async fn proxy_upstream_filter(&self, session: &mut Session, ctx: &mut Self::CTX) -> Result<bool> {
    let want_socks = ctx.routing_spec.proto.is_some_and(|p| p != EgressProto::Http);
    if !want_socks { return Ok(true); } // HTTP 快路径零改动
    let bridge = match self.bridge { Some(ref b) => b.clone(), None => return Err(503-error) };
    // 选路（含 failed_addrs 排除＋粘滞复用 select_node_excluding）→ 逐个 bridge.fetch（≤max_retries+1 个不同节点）→
    // 首个成功：写合成响应（status＋过滤头＋分 chunk body，transferred_bytes 累加）→ ctx.current_node/bandit_context 落定 → Ok(false)。
    // 全失败：record_failed_addr＋retry_count 累加 → Err(503)（logging 照常：status 0/503＋reward 低＋遥测 error_type）。
}
```

`upstream_peer` 首行守卫（防御性，正常走不到——filter 已短路＋router 默认隔离）：
```rust
if let Some(spec_proto) = ctx.routing_spec.proto { ... } // 无需；守卫对象是 node：
// selection 返回后：if node.proto != Http && filter 未处理 → Err(503)（注释写明双保险）。
```
注：filter 与 peer 间无状态传递——filter 内直接完成选路（`select_node_excluding` 公开方法复用），
`ctx.current_node` 由 filter 设置，peer 对 socks 请求不可达（filter恒返回 false/Err）。

合成响应细节：`ResponseHeader::build(status, None)`＋`insert_header`（非法头跳过＋warn）；
body 分 32KB chunk `write_response_body(chunk, false)`，尾 `write_response_body(empty, true)`；
任一步写失败 → Err（Pingora 关连接，logging 照常）。

- [ ] **Step 4: 运行确认通过** Run: `cargo test --workspace gateway` Expected: 全 PASS（存量重试/隔离/采样单测全绿）
- [ ] **Step 5: Verify（fmt＋clippy）**

---

### P2-5: free_pool 全收 SOCKS（质检＋快照＋水位）

**Files:** `gateway/src/free_pool.rs`（`upsert_full` 去 poolable 门＋proto 存入＋Verifier socks 路径＋
FullChecker socks 路径＋4 单测）；`gateway/src/metrics.rs`（`free_pool_nodes_by_proto{proto}`＋1 单测）

- [ ] **Step 1: 写失败单测**

```rust
#[test] fn registry_stores_socks_proto() // socks5 RawNode upsert→快照含节点且 proto==Socks5（find by addr 断言，沿 ZZ 手法）
#[tokio::test] async fn verifier_socks_handshake_ok_and_refused() // 对 stub-socks5（greeting→CONNECT canned 成功）Some；127.0.0.1:1 None
#[tokio::test] async fn fullcheck_via_socks_relay_reports_elite() // relay-stub（CONNECT 到本地 echo 基址）→Some＋anon==Elite（基线传 echo 观测 IP 之外值）
#[test] fn metrics_nodes_by_proto_rendered() // set_free_pool_nodes_proto("socks5",2)→行存在
```

- [ ] **Step 2: 运行确认失败** Run: `cargo test --workspace registry_stores_socks verifier_socks_handshake fullcheck_via_socks nodes_by_proto` Expected: FAIL
- [ ] **Step 3: 最小实现**
  - `upsert_full`：删 `poolable` 门，`ProxyNode::…with_proto(map FreeProto→EgressProto)`（Https→Http；Socks4/5 直映；注释写明 socks4 服务走 reqwest 原生）。
  - `Verifier::verify`：socks→`timeout(establish(CONNECT到 check_target))` 成功返回握手耗时（延迟门同 HTTP）；
    `Verifier` 加 `socks_target: Option<(String,u16)>` 字段（Worker 传入 full_check_base 解析出的 host:443；
    None 则 greeting-only 降级——单测覆盖两档）。
  - `FullChecker::check`：socks→`reqwest::Proxy::all(socks5h://…)` Client（单次构建，10min 节拍可接受，注释写明）；
    后续三端点逻辑零改动（匿名度/canary 同权）。
  - Worker `run_once` 4a：删 `poolable` 跳过（socks 正常进 TCP/握手初筛；`backoff_skip` 语义保留给采集中断）。
  - metrics：`free_nodes_by_proto: DashMap`＋`set_free_pool_nodes_proto(proto,&str,n)`＋render gauge 组；
    Worker merge 后按 snapshot 聚合 proto 计数设置。
- [ ] **Step 4: 运行确认通过** Run: `cargo test --workspace free_pool metrics` Expected: 全 PASS
- [ ] **Step 5: Verify（fmt＋clippy）**

---

### P2-6: prober＋pool 跟进

**Files:** `gateway/src/prober.rs`（`proxy_url_for` socks5h 形态＋`client_for` 零改动复用＋1 单测）；
`gateway/src/pool.rs`（预热 socks→greeting-only＋1 单测）

- [ ] **Step 1: 写失败单测**

```rust
#[test] fn proxy_url_for_socks_shape() // socks5 节点→socks5h://ip:port（含账密编码复用）；http 形态不变
#[tokio::test] async fn warm_socks_greeting_only() // 对 stub-socks5 greeting 成功→connected 计；127.0.0.1:1→failed（不 panic）
```

- [ ] **Step 2: 运行确认失败** Run: `cargo test --workspace proxy_url_for_socks warm_socks` Expected: FAIL
- [ ] **Step 3: 最小实现**（`proxy_url_for` 按 `node.proto` 切 scheme；`encode_userinfo` 提 `pub(crate)` 供 bridge 复用；
  pool warm 分支：socks→`greet_only`，http→原 TCP；`WarmStats` 口径不变）
- [ ] **Step 4: 运行确认通过** Run: `cargo test --workspace prober pool` Expected: 全 PASS
- [ ] **Step 5: Verify（fmt＋clippy）**

---

### P2-7: main 接线＋运维落盘

**Files:** `gateway/src/main.rs`（`mod socks_bridge/socks_handshake`＋bridge 装配＋evict 滴答＋env 2 个）；
`docs/OPERATION.md`（§4 一行＋§6 一行）；`docker-compose.yml`（2 env 示例）

- [ ] **Step 1: 最小实现**（无新单测，复用既有 env 单测形态；bridge 常驻 `Arc` 注入 `SmartProxyGateway` 构造点——
  构造点在 main 唯一＋gateway 单测 helper（helper 传 None，注释写明 socks 单测由 E2E 承担））

```rust
let socks_bridge = Arc::new(SocksBridge::new(
    env_secs("SOCKS_BRIDGE_TIMEOUT_SECS", 20),
    env_str("SOCKS_MAX_BODY_BYTES", "10485760").parse::<u64>().unwrap_or(10 * 1024 * 1024),
));
// gateway 构造字段 bridge: Some(socks_bridge.clone())；后台 60s 滴答 evict_idle（与 sweep 同节拍，计数 debug 日志）。
```

OPERATION §4：`| SOCKS 桥 | free_pool_nodes_by_proto{proto} / 日志[SocksBridge] | 仅显式 X-Proxy-Proto: socks5/socks4 请求走桥；默认流量永不命中 socks（router 默认隔离＋peer 守卫）；body>上限 502＋warn |`
§6：`- SOCKS 桥零信任延续：socks 节点同样禁敏感流量（与免费线同规）；relay 只连验证/请求目标，不做 CONNECT 扫描。`
compose：`SOCKS_BRIDGE_TIMEOUT_SECS: "20"`＋`SOCKS_MAX_BODY_BYTES: "10485760"`（注释态）。

- [ ] **Step 2: 运行确认通过** Run: `cargo test --workspace` Expected: 全 PASS（目标 107＋P2 新增约 14＝121±2，以实测为准）
- [ ] **Step 3: Verify（fmt＋clippy＋bench 编译）**

---

### P2-8: 最终门禁＋回归（存量 curl＋本地确定性 SOCKS E2E）

- [ ] **Step 1: 四门全跑** `cargo fmt --check`；`cargo clippy --workspace --all-targets -- -D warnings`；
  `cargo test --workspace`（121±2 过/4 ignored）；`cargo test -- --ignored`（先验 Redis PONG＋CH Ok，无 SKIP）；
  `cargo bench --no-run`（编译过）
- [ ] **Step 2: 存量回归（默认面零变化）** mocks＋网关（默认 env，不开 FREE）：六用例＋缺 Host 400＋/metrics 200 全绿；
  断言 `free_pool_nodes_total` 缺席或 0 且普通流量仍命中付费（socks 零干扰）。
- [ ] **Step 3: 本地确定性 SOCKS E2E（全 localhost，无外网依赖）**
  1. 起 mock（:8888 `mock-a-us` 复用）＋socks5 relay stub（:1099→:8888，`log/` 脚本，常驻日志）＋
     list server（:18080 吐 `127.0.0.1:1099 socks5`）。
  2. 网关 env：`FREE_ENABLED=1 FREE_GITHUB_URLS=http://127.0.0.1:18080/list.txt`
     `FREE_API_URLS=http://127.0.0.1:1/x FREE_HTML_URLS=http://127.0.0.1:1/x`
     `FREE_FETCH_INTERVAL_SECS=60 FREE_TTL_SECS=300`（其余默认）。
  3. 等 tick=1：断言 `free_pool_nodes_total 1`＋`free_pool_nodes_by_proto{proto="socks5"} 1`。
  4. `curl -H 'X-Proxy-Proto: socks5'` → 期望 mock body（整链：fetch→握手→FullCheck经socks→merge→filter→桥→relay→mock→合成）。
  5. 默认 curl（无 proto 头）→ 仍命中付费（隔离证据）；`tier=residential`＋socks5 头→ 503/无命中（双约束互斥证据，状态码如实记录）。
  6. 10 流量 → XLEN/CH 涨（链路活）。
  Expected: 4 绿；任一步红即修（桥/握手/merge 三段日志定位）。
- [ ] **Step 4: 落库** plan 本表全✅＋EXEC_LOG append-only（含四门数/live 真过＋两 PID＋日志文件＋E2E 三断言）＋
  TASK_PLAN 步骤 11 ✅；禁未授权 commit——用户确认后统一提交。

---

## §3. 状态总览

| 子项 | 内容 | 优先级 | 状态 | 验收 |
|------|------|--------|------|------|
| P2-1 | 模型＋选路隔离 | P0 | ✅ 已完成（2026-09-22：EgressProto＋with_proto＋matches 默认隔离＋header/token 解析；109 过） | 默认排 socks＋显式命中＋解析 |
| P2-2 | socks_handshake | P0 | ✅ 已完成（2026-09-22：RFC1928/1929＋SOCKS4/4a＋greet_only；4 单测字节断言；113 过） | 4 单测（字节断言＋拒连） |
| P2-3 | socks_bridge | P0 | ✅ 已完成（2026-09-22：Client 缓存＋hop 过滤＋body 上限＋relay 透传；3 单测；116 过。诚实记录：`bytes_stream` 需 reqwest `stream` feature，lock 仅增 wasm 目标 `wasm-streams`（不进 native 编译），native 零新 crate） | relay 透传＋hop 过滤＋上限 |
| P2-4 | 网关 filter＋守卫 | P0 | ✅ 已完成（2026-09-22：filter 短路＋合成响应＋peer 守卫＋粘滞 proto 复核；3 单测；119 过。诚实记录：合成头名须 owned String，`&str` 短借用不满足 `IntoCaseHeaderName`） | 守卫＋组装＋存量绿 |
| P2-5 | free_pool 全收 | P0 | ✅ 已完成（2026-09-22：poolable 门拆除＋proto 入池＋Verifier 握手/CONNECT＋FullChecker 经 socks＋by-proto 水位；5 单测；124 过。诚实记录：`poolable()` 已删除（概念退役）；合成头名沿 P2-4 须 owned） | socks 进池＋复检＋by-proto |
| P2-6 | prober＋pool | P1 | ✅ 已完成（2026-09-22：proxy_url 按 proto 切 scheme＋warm 三路候选＋socks greeting-only；3 单测；127 过。诚实记录：P2-6 实现中发现 warm 默认 spec 天然漏探 socks，已并取修复） | socks 探测＋预热 |
| P2-7 | main＋运维 | P1 | ✅ 已完成（2026-09-22：bridge 装配＋sweep 同节拍 evict＋2 env＋OPERATION§4/§6＋compose；127 过＋bench 编译） | 121±2＋env 落盘 |
| P2-8 | 门禁＋E2E | 门禁 | ✅ 已完成（2026-09-22：127 过/4 真 live＋存量 curl 零变化＋本地 E2E 五断言全绿，见 EXEC_LOG 步骤 11 条） | 四门＋本地 E2E 三断言 |

## §4. 不做（explicitly out）

- 客户端 CONNECT 隧道（client→gateway 保持 HTTP；UDP ASSOCIATE 不做）。
- SOCKS 节点服务默认/无约束流量（隔离铁律，无开关）。
- Pingora fork / 自定义 connector（零侵入是硬约束）。
- IPv6 CONNECT 目标（handshake 显式 Err；节点 v6 仍可 TCP 建链？不——Verifier 对 v6 socks 直接 None，注释写明）。
- bridge 连接池跨节点复用之外的优化（per-node Client 缓存已够 10min 节拍＋数据面按需）。

## Self-Review

1. **Spec coverage:** SOCKS 解析→P2-1/ free 全收→P2-5／握手探测→P2-2+P2-6／按 proto 选路隔离→P2-1＋P2-4 守卫／reqwest per-proto Client→P2-3＋P2-6／Pingora 改造→P2-4（filter 短路，正向声明非 connector）／画像 free 明细→by-proto 水位（报表侧已可分）。
2. **Placeholder scan:** 无 TBD/TODO；relay-stub 与 E2E 脚本行为由单测/E2E 断言锁定。
3. **Type consistency:** `EgressProto{Http,Socks5,Socks4}`（model 单源；free_pool `FreeProto→EgressProto` 映射函数一处）；
   `RoutingSpec.proto: Option<EgressProto>`（None＝默认 http）；
   `SocksBridge::fetch(&ProxyNode, BridgeRequest)->Result<BridgeResponse,String>`；
   `socks_handshake::establish/greet_only -> Result<_, String>`。
4. **存量风险：** `ProxyNode::new` 8 参不变（proto 缺省 Http）——30＋ 调用点零改动；
   matches 默认分支新增 `!= Http → false`——存量节点全 Http，行为冻结（`free_tier_isolation` 单测守护）；
   `SmartProxyGateway` 加 `bridge: Option<Arc<SocksBridge>>`——gateway 单测 helper 传 None，
   main 传 Some（mocks 下 socks 请求回 503，属正确——无 socks 节点时无候选本就 503）。
5. **门禁诚实性：** P2-8 先验依赖＋无 SKIP 检查（R2-9 教训）；E2E 全 localhost（外网抖动零影响）；
   单测数以实测为准（121±2 为目标非承诺）。
