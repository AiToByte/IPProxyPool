//! 控制面节点注册表：运行时增删 + 原子持久化（步骤 52）。
//!
//! # 存在的理由
//!
//! 审计报告 [`docs/CAPABILITY-AUDIT.md`] §8 把「控制面 API + 配置热更新」列为
//! 当前最大短板：节点池此前只能靠 `main.rs` 硬编码 seeds 或测试专用的
//! `TEST_POOL_NODES` 注入，**没有运行时增删的公开 API**；而 Windows 侧
//! Pingora 约 305~308s 就会自行退出（OPT-R14），纯内存变更必然随进程消失。
//! 因此「持久化」不是锦上添花，而是这个能力能否成立的前提。
//!
//! # 职责边界
//!
//! - 本模块只管**托管节点**（运维通过 Admin API 增删的节点）：校验、落盘、
//!   内存清单。
//! - `free-*` 节点属于免费池（每轮 merge 重建），**不进本清单、不落盘**；
//!   控制面显式拒绝增删它们，避免“删了 5 秒后又回来”的假操作。
//! - `TEST_POOL_NODES` 注入的节点是测试通道，同样不落盘（进程即弃）。
//! - 真正的选路池由 `RouterEngine` 持有；本模块在落盘成功后调用
//!   `apply` 闭包把变更同步进去。锁序固定为 `NodeRegistry → RouterEngine`，
//!   免费池只拿 `RouterEngine` 锁，故无环。
//!
//! # 写入顺序（先落盘、后生效）
//!
//! ```text
//! 校验 → 写临时文件 + fsync → rename 原子替换 → apply(RouterEngine) → 更新内存
//! ```
//!
//! 先落盘是为了避免「接口返回 500、内存却已经变了」；apply 失败时把旧清单
//! 重新落盘（best-effort 回滚）。若回滚本身也失败，只记 `error!` 并如实返回
//! 500 —— **不谎报成功**。
//!
//! # 为什么落盘必须离开 async 线程（OPT-R18 P2-4）
//!
//! 本模块的三个变更方法原先都是**同步**的，而它们的唯一调用方是 Admin 的
//! axum handler（async）。于是 `write_all + sync_all`（以及 Unix 上的
//! `dir.sync_all()`）直接在 tokio worker 线程上执行：机械盘/网络盘上单次
//! fsync 可达数十毫秒，而 tokio 的 worker 数量等于 CPU 核数——**几次并发
//! 的节点增删就能把整个数据面卡住**（数据面与 Admin 共享同一 runtime）。
//!
//! 修法是标准做法：变更方法改 `async`，把序列化 + 写 + fsync 整体搬进
//! `spawn_blocking`。这里有个**必须留意的约束**：
//!
//! - `parking_lot::Mutex` **不得跨 await 持有**（本仓硬性纪律）。所以不能
//!   "持着 nodes 锁 await 落盘"。
//! - 但若只是"取锁 → 算 next → 放锁 → 落盘 → 再取锁 → 提交"，两个并发
//!   `add` 会基于同一份旧清单各算一份 next，**后者覆盖前者（lost update）**。
//!
//! 故新增一把 `write_gate`（`tokio::sync::Mutex`）串行化整个「读-改-写」序列：
//! `nodes` 锁只覆盖内存读写的极短窗口，`write_gate` 覆盖跨 await 的全过程。
//! 读路径（`snapshot`/`len`）完全不受影响，且**不再被 fsync 阻塞**——这比
//! 修改前更好：原先落盘期间 `snapshot()` 会一直等锁。

use crate::model::{canonical_tier, EgressProto, ProxyNode};
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use std::fmt;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

/// 默认持久化路径（相对进程工作目录；`.gitignore` 已忽略 `data/`）。
pub const DEFAULT_NODES_FILE: &str = "data/nodes.json";

/// 文件格式版本。升级格式时必须递增并写迁移逻辑，避免静默误读旧文件。
pub const NODES_FILE_VERSION: u32 = 1;

/// 托管节点上限。防误操作/脚本死循环把内存与文件同时打爆。
pub const MAX_MANAGED_NODES: usize = 10_000;

/// 权重上限（0 = 摘除；100 = 文档口径的满权）。
pub const MAX_WEIGHT: u32 = 100;

/// 保留的 provider 前缀：免费池每轮 `replace_vendor_nodes("free-", ...)`
/// 会整组重建，控制面写进去也会被静默洗掉。
pub const RESERVED_PROVIDER_PREFIX: &str = "free-";

/// provider 是否属于免费池供给线（前缀判定）。
///
/// **唯一真源**：本函数与 [`RESERVED_PROVIDER_PREFIX`] 同文件，调用方不得就地
/// 写 `"free-"` 字面量——否则“探测抽样把付费节点当免费节点抽样”“免费档凭据
/// 护栏漏判”这类错误不会有任何编译期提示。
///
/// 用 `starts_with` 而非相等：provider 形如 `free-geonode`。
/// 不带连字符的 `"freeproxy"` 会被判为**付费**（全量探测）——这是安全方向：
/// 宁可多探测，不可漏探测 SLA 节点。
pub fn is_free_provider(provider: &str) -> bool {
    provider.starts_with(RESERVED_PROVIDER_PREFIX)
}

const MAX_PROVIDER_BYTES: usize = 64;
const MAX_USERNAME_BYTES: usize = 256;
const MAX_PASSWORD_BYTES: usize = 512;

/// 注册表错误。Admin 层按变体映射 HTTP 状态码（400/404/409/500）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RegistryError {
    /// 字段非法（HTTP 400）。
    Validation(String),
    /// 地址已存在（HTTP 409）。
    Duplicate(String),
    /// 地址不在托管清单（HTTP 404）。
    NotFound(String),
    /// 读写/解析持久化文件失败（HTTP 500，或启动失败）。
    Persist(String),
    /// 落盘成功但同步到 RouterEngine 失败（HTTP 500；已尝试回滚文件）。
    Apply(String),
}

impl fmt::Display for RegistryError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Validation(m) => write!(f, "invalid node: {m}"),
            Self::Duplicate(m) => write!(f, "duplicate node: {m}"),
            Self::NotFound(m) => write!(f, "node not found: {m}"),
            Self::Persist(m) => write!(f, "node registry persistence failed: {m}"),
            Self::Apply(m) => write!(f, "node change could not be applied: {m}"),
        }
    }
}

impl std::error::Error for RegistryError {}

/// 磁盘格式。`version` 必填：缺失/不匹配即视为配置损坏，启动失败。
#[derive(Debug, Serialize, Deserialize)]
struct NodesFile {
    version: u32,
    nodes: Vec<PersistedNode>,
}

/// 单个节点的磁盘表示。
///
/// 刻意**不存** `addr`（由 ip+port 派生，存了就有不一致风险）与 `exit_ip`
/// （FullCheck 运行时发现值，不属于运维意图）。
#[derive(Debug, Serialize, Deserialize)]
struct PersistedNode {
    ip: String,
    port: u16,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    username: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    password: Option<String>,
    country: String,
    tier: String,
    provider: String,
    weight: u32,
    #[serde(default = "default_proto_str")]
    proto: String,
}

fn default_proto_str() -> String {
    "http".to_string()
}

fn proto_str(proto: EgressProto) -> &'static str {
    match proto {
        EgressProto::Http => "http",
        EgressProto::Socks5 => "socks5",
        EgressProto::Socks4 => "socks4",
    }
}

impl From<&ProxyNode> for PersistedNode {
    fn from(n: &ProxyNode) -> Self {
        Self {
            ip: n.ip.clone(),
            port: n.port,
            username: n.username.clone(),
            password: n.password.clone(),
            country: n.country.clone(),
            tier: n.tier.clone(),
            provider: n.provider.clone(),
            weight: n.weight,
            proto: proto_str(n.proto).to_string(),
        }
    }
}

impl PersistedNode {
    fn to_node(&self) -> Result<ProxyNode, RegistryError> {
        let proto = EgressProto::from_token(&self.proto).ok_or_else(|| {
            RegistryError::Validation(format!(
                "proto {:?} is not one of http/socks5/socks4",
                self.proto
            ))
        })?;
        let node = ProxyNode::new(
            self.ip.clone(),
            self.port,
            self.username.clone(),
            self.password.clone(),
            self.country.clone(),
            self.tier.clone(),
            self.provider.clone(),
            self.weight,
        )
        .with_proto(proto);
        normalize_and_validate(node)
    }
}

/// 托管节点注册表。`path = None` 表示**关闭持久化**（仅内存）。
#[derive(Debug)]
pub struct NodeRegistry {
    path: Option<PathBuf>,
    nodes: Mutex<Vec<ProxyNode>>,
    /// 串行化「读-改-写」全过程，覆盖跨 await 的落盘。
    ///
    /// 为什么不能只靠 `nodes` 锁：落盘要走 `spawn_blocking`，而
    /// `parking_lot::Mutex` 不得跨 await 持有。若"取锁算完就放锁、落盘后
    /// 再取锁提交"，两个并发 `add` 会基于同一份旧清单各算一份 next，
    /// 后者把前者覆盖掉（lost update）——这正是步骤 52 修过的缺陷类型。
    /// 本锁把整个序列串起来，`nodes` 锁退化为只保护内存读写。
    write_gate: tokio::sync::Mutex<()>,
}

impl NodeRegistry {
    /// 从磁盘加载（文件存在则以文件为准），否则使用 `seeds`。
    ///
    /// 文件存在但解析/校验失败 ⇒ 返回 `Err`，由 `main.rs` **拒绝启动**：
    /// 节点表损坏属误配，静默回退 seeds 会让运维以为配置生效了。
    ///
    /// 保持**同步**：这是启动路径（`main` 尚未进入 runtime 的 hot loop），
    /// 一次性读盘无并发压力，改成 async 只会让装配代码更难读。
    pub fn load(path: Option<PathBuf>, seeds: Vec<ProxyNode>) -> Result<Self, RegistryError> {
        let nodes = match path.as_deref() {
            Some(p) if p.exists() => load_file(p)?,
            _ => seeds,
        };
        ensure_unique_and_bounded(&nodes)?;
        Ok(Self {
            path,
            nodes: Mutex::new(nodes),
            write_gate: tokio::sync::Mutex::new(()),
        })
    }

    /// 仅内存模式（单测与 `NODES_FILE=""` 场景）。
    #[cfg(test)]
    pub fn in_memory(seeds: Vec<ProxyNode>) -> Self {
        Self {
            path: None,
            nodes: Mutex::new(seeds),
            write_gate: tokio::sync::Mutex::new(()),
        }
    }

    /// 当前托管节点快照（启动装配、Admin 查询辅助用）。
    pub fn snapshot(&self) -> Vec<ProxyNode> {
        self.nodes.lock().clone()
    }

    /// 托管节点数。
    pub fn len(&self) -> usize {
        self.nodes.lock().len()
    }

    /// 是否没有任何托管节点。
    #[cfg(test)]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// 持久化路径；`None` 表示关闭持久化。
    pub fn path(&self) -> Option<&Path> {
        self.path.as_deref()
    }

    /// 新增节点：校验 → 落盘 → `apply` 同步 Router → 更新内存。
    ///
    /// 返回**归一化后**的节点（country 大写、tier 归一、addr 重建）。
    ///
    /// `async` 的原因见模块头「为什么落盘必须离开 async 线程」。
    pub async fn add<F>(&self, raw: ProxyNode, apply: F) -> Result<ProxyNode, RegistryError>
    where
        F: FnOnce(&ProxyNode) -> Result<(), String>,
    {
        let node = normalize_and_validate(raw)?;
        let _gate = self.write_gate.lock().await;

        // 阶段 1：内存校验 + 计算新清单。持 `nodes` 锁的时间只有几次比较与
        // 一次 clone，**没有任何 I/O**，因此不会阻塞 `snapshot()`。
        let previous = {
            let guard = self.nodes.lock();
            if guard.len() >= MAX_MANAGED_NODES {
                return Err(RegistryError::Validation(format!(
                    "managed node limit reached ({MAX_MANAGED_NODES})"
                )));
            }
            if guard.iter().any(|n| n.addr == node.addr) {
                return Err(RegistryError::Duplicate(node.addr.clone()));
            }
            guard.clone()
        };
        let mut next = previous.clone();
        next.push(node.clone());

        // 阶段 2：落盘（阻塞 I/O 已搬进 blocking 线程池）。
        self.persist(next.clone()).await?;

        // 阶段 3：apply + 提交。
        if let Err(e) = apply(&node) {
            // 落盘已成功但路由层拒绝：把旧清单写回去，保持“接口报错 = 状态未变”。
            if let Err(rb) = self.persist(previous).await {
                log::error!(
                    "[NodeRegistry] rollback after apply failure also failed: {rb}; \
                     disk and memory may diverge (node={})",
                    node.addr
                );
            }
            return Err(RegistryError::Apply(e));
        }

        *self.nodes.lock() = next;
        Ok(node)
    }

    /// 删除托管节点：落盘 → `apply` 从 Router 移除 → 更新内存。
    ///
    /// `apply` 返回 false（Router 里已找不到）视为 Apply 错误并回滚文件。
    pub async fn remove<F>(&self, addr: &str, apply: F) -> Result<ProxyNode, RegistryError>
    where
        F: FnOnce(&str) -> bool,
    {
        let _gate = self.write_gate.lock().await;

        let (previous, removed, idx) = {
            let guard = self.nodes.lock();
            let idx = guard
                .iter()
                .position(|n| n.addr == addr)
                .ok_or_else(|| RegistryError::NotFound(addr.to_string()))?;
            (guard.clone(), guard[idx].clone(), idx)
        };
        let mut next = previous.clone();
        next.remove(idx);
        self.persist(next.clone()).await?;

        if !apply(addr) {
            if let Err(rb) = self.persist(previous).await {
                log::error!(
                    "[NodeRegistry] rollback after remove failure also failed: {rb}; \
                     disk and memory may diverge (node={addr})"
                );
            }
            return Err(RegistryError::Apply(format!(
                "router no longer contains {addr}"
            )));
        }

        *self.nodes.lock() = next;
        Ok(removed)
    }

    /// 把 `provider + country` 维度的调权**持久化到托管节点**，再应用到 Router。
    ///
    /// 返回被持久化修改的托管节点数（`free-*` / 测试节点不计）。没有匹配项时
    /// 不写文件（避免无意义 fsync），但仍执行 `apply`（调权对全部节点生效）。
    pub async fn set_group_weight<F>(
        &self,
        provider: &str,
        country: &str,
        weight: u32,
        apply: F,
    ) -> Result<usize, RegistryError>
    where
        F: FnOnce(&str, &str, u32) -> Result<(), String>,
    {
        if weight > MAX_WEIGHT {
            return Err(RegistryError::Validation(format!(
                "weight {weight} exceeds {MAX_WEIGHT}"
            )));
        }

        let _gate = self.write_gate.lock().await;

        let (previous, next, changed) = {
            let guard = self.nodes.lock();
            let mut next = guard.clone();
            let mut changed = 0usize;
            for n in &mut next {
                if n.provider == provider && n.country.eq_ignore_ascii_case(country) {
                    n.weight = weight;
                    changed += 1;
                }
            }
            (guard.clone(), next, changed)
        };

        if changed > 0 {
            self.persist(next.clone()).await?;
        }

        if let Err(e) = apply(provider, country, weight) {
            if changed > 0 {
                if let Err(rb) = self.persist(previous).await {
                    log::error!(
                        "[NodeRegistry] rollback after weight apply failure also failed: {rb}; \
                         disk and memory may diverge ({provider}/{country})"
                    );
                }
            }
            return Err(RegistryError::Apply(e));
        }

        if changed > 0 {
            *self.nodes.lock() = next;
        }
        Ok(changed)
    }

    /// 原子落盘（`async` 外壳）。`path = None` 时为空操作。
    ///
    /// 序列化 + 写临时文件 + fsync + rename 全部在 `spawn_blocking` 里完成：
    /// 序列化虽小，但 `MAX_MANAGED_NODES = 10000` 时也是几十 KB 的 CPU 工作，
    /// 一起搬走更彻底。
    async fn persist(&self, nodes: Vec<ProxyNode>) -> Result<(), RegistryError> {
        let Some(path) = self.path.clone() else {
            return Ok(());
        };

        // `JoinError` 只在 blocking 任务 panic 或 runtime 关闭时出现。两种情况
        // 都不能当成"落盘成功"，故统一映射为 `Persist` 错误。
        tokio::task::spawn_blocking(move || write_nodes_atomic(&path, &nodes))
            .await
            .map_err(|e| RegistryError::Persist(format!("blocking writer task failed: {e}")))?
    }
}

/// 原子写节点文件（**同步、阻塞**，只在 `spawn_blocking` 里调用）。
///
/// 与 `NodeRegistry` 解耦成自由函数的原因：`spawn_blocking` 要求闭包 `'static`，
/// 借用 `&self` 无法跨线程；同时这也让"阻塞 I/O"在类型层面就与 async 代码分离。
fn write_nodes_atomic(path: &Path, nodes: &[ProxyNode]) -> Result<(), RegistryError> {
    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() {
            fs::create_dir_all(parent).map_err(|e| {
                RegistryError::Persist(format!("create parent dir {}: {e}", parent.display()))
            })?;
        }
    }

    let file = NodesFile {
        version: NODES_FILE_VERSION,
        nodes: nodes.iter().map(PersistedNode::from).collect(),
    };
    let data = serde_json::to_vec_pretty(&file)
        .map_err(|e| RegistryError::Persist(format!("serialize: {e}")))?;

    let tmp = tmp_path(path);
    write_private_file(&tmp, &data)
        .map_err(|e| RegistryError::Persist(format!("write temp file {}: {e}", tmp.display())))?;

    // 同目录 rename 是原子的；Rust 在 Windows 上走
    // `MoveFileExW(MOVEFILE_REPLACE_EXISTING)`，可覆盖已有文件。
    if let Err(e) = fs::rename(&tmp, path) {
        let _ = fs::remove_file(&tmp);
        return Err(RegistryError::Persist(format!(
            "atomic rename {} -> {}: {e}",
            tmp.display(),
            path.display()
        )));
    }

    // Unix：fsync 父目录，确保 rename 本身也落盘（崩溃后不丢）。
    #[cfg(unix)]
    if let Some(parent) = path.parent() {
        if let Ok(dir) = fs::File::open(parent) {
            let _ = dir.sync_all();
        }
    }

    Ok(())
}

/// 临时文件名与目标同目录（保证 rename 不跨挂载点）。
///
/// 唯一性用 **pid + 进程内单调序号**，两个维度都要：
///
/// - `pid`：防**多实例**互踩（两个网关进程共用同一个 `NODES_FILE`）。
/// - 序号：防**同进程内并发写**互踩。只用 pid 时，两次并发写会指向同一个
///   临时文件，先完成者的 `rename` 把它搬走，后者随即报
///   `os error 2 (file not found)`——实测踩到过（把 `write_gate` 摘掉后
///   立刻复现）。`write_gate` 确实串行化了写，但**原子写本身不该依赖外部
///   锁才正确**：这层耦合是隐形的，将来任何人加一条新的写路径都会撞上。
fn tmp_path(path: &Path) -> PathBuf {
    static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let seq = SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let mut s = path.as_os_str().to_os_string();
    s.push(format!(".tmp.{}.{}", std::process::id(), seq));
    PathBuf::from(s)
}

/// 写临时文件并 fsync；Unix 下权限固定 `0600`（文件含上游代理凭据）。
fn write_private_file(path: &Path, data: &[u8]) -> std::io::Result<()> {
    #[cfg(unix)]
    let mut file = {
        use std::os::unix::fs::OpenOptionsExt;
        fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(path)?
    };
    #[cfg(not(unix))]
    let mut file = fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .open(path)?;

    file.write_all(data)?;
    file.sync_all()?;
    Ok(())
}

/// 读取并校验节点文件。
fn load_file(path: &Path) -> Result<Vec<ProxyNode>, RegistryError> {
    let raw = fs::read_to_string(path)
        .map_err(|e| RegistryError::Persist(format!("read {}: {e}", path.display())))?;
    let file: NodesFile = serde_json::from_str(&raw)
        .map_err(|e| RegistryError::Persist(format!("parse {}: {e}", path.display())))?;
    if file.version != NODES_FILE_VERSION {
        return Err(RegistryError::Persist(format!(
            "{}: unsupported version {} (expected {NODES_FILE_VERSION})",
            path.display(),
            file.version
        )));
    }

    let mut out = Vec::with_capacity(file.nodes.len());
    for (i, p) in file.nodes.iter().enumerate() {
        let node = p
            .to_node()
            .map_err(|e| RegistryError::Persist(format!("{}: nodes[{i}]: {e}", path.display())))?;
        out.push(node);
    }
    ensure_unique_and_bounded(&out)?;
    Ok(out)
}

/// 唯一性 + 上限校验（加载 seeds / 文件时用）。
fn ensure_unique_and_bounded(nodes: &[ProxyNode]) -> Result<(), RegistryError> {
    if nodes.len() > MAX_MANAGED_NODES {
        return Err(RegistryError::Validation(format!(
            "{} nodes exceed limit {MAX_MANAGED_NODES}",
            nodes.len()
        )));
    }
    let mut seen = HashSet::with_capacity(nodes.len());
    for n in nodes {
        if !seen.insert(n.addr.as_str()) {
            return Err(RegistryError::Duplicate(n.addr.clone()));
        }
    }
    Ok(())
}

/// 归一化 + 全字段校验。返回的节点保证：IPv4、port>0、country 大写两字母、
/// tier 为三档之一、provider 非空且不占 `free-`、weight≤100、凭据成对。
fn normalize_and_validate(mut node: ProxyNode) -> Result<ProxyNode, RegistryError> {
    // IP：只接受 IPv4。`ProxyNode.addr` 是 `ip:port` 字符串，IPv6 会产生
    // `::1:8080` 这类歧义；本仓既有节点也全是 IPv4，故如实拒绝而非假装支持。
    let ip = match node.ip.trim().parse::<std::net::IpAddr>() {
        Ok(std::net::IpAddr::V4(v4)) => v4.to_string(),
        Ok(std::net::IpAddr::V6(_)) => {
            return Err(RegistryError::Validation(
                "IPv6 is not supported (`addr` is ip:port and would be ambiguous)".to_string(),
            ))
        }
        Err(_) => {
            return Err(RegistryError::Validation(format!(
                "{:?} is not a valid IPv4 address",
                node.ip
            )))
        }
    };
    if node.port == 0 {
        return Err(RegistryError::Validation(
            "port must be in 1..=65535".to_string(),
        ));
    }
    node.ip = ip;
    node.addr = format!("{}:{}", node.ip, node.port);

    let country = node.country.trim();
    if country.len() != 2 || !country.chars().all(|c| c.is_ascii_alphabetic()) {
        return Err(RegistryError::Validation(format!(
            "country {country:?} must be 2 ASCII letters (e.g. US)"
        )));
    }
    node.country = country.to_ascii_uppercase();

    let tier = canonical_tier(node.tier.trim());
    if !matches!(tier.as_str(), "datacenter" | "residential" | "mobile") {
        return Err(RegistryError::Validation(format!(
            "tier {tier:?} must be datacenter/residential/mobile (free is managed by FreePool)"
        )));
    }
    node.tier = tier;

    let provider = node.provider.trim();
    if provider.is_empty() {
        return Err(RegistryError::Validation("provider is empty".to_string()));
    }
    if provider.len() > MAX_PROVIDER_BYTES {
        return Err(RegistryError::Validation(format!(
            "provider exceeds {MAX_PROVIDER_BYTES} bytes"
        )));
    }
    if provider.chars().any(char::is_control) {
        return Err(RegistryError::Validation(
            "provider contains control characters".to_string(),
        ));
    }
    if provider.eq_ignore_ascii_case("free")
        || provider
            .to_ascii_lowercase()
            .starts_with(RESERVED_PROVIDER_PREFIX)
    {
        return Err(RegistryError::Validation(format!(
            "provider prefix {RESERVED_PROVIDER_PREFIX:?} is reserved by FreePool"
        )));
    }
    node.provider = provider.to_string();

    if node.weight > MAX_WEIGHT {
        return Err(RegistryError::Validation(format!(
            "weight {} exceeds {MAX_WEIGHT}",
            node.weight
        )));
    }

    match (&node.username, &node.password) {
        (None, None) => {}
        (Some(u), password) => {
            if u.is_empty() {
                return Err(RegistryError::Validation("username is empty".to_string()));
            }
            if u.len() > MAX_USERNAME_BYTES {
                return Err(RegistryError::Validation(format!(
                    "username exceeds {MAX_USERNAME_BYTES} bytes"
                )));
            }
            if let Some(p) = password {
                if p.len() > MAX_PASSWORD_BYTES {
                    return Err(RegistryError::Validation(format!(
                        "password exceeds {MAX_PASSWORD_BYTES} bytes"
                    )));
                }
            }
        }
        (None, Some(_)) => {
            return Err(RegistryError::Validation(
                "password requires username (credentials must be paired)".to_string(),
            ))
        }
    }

    Ok(node)
}

/// `NODES_FILE` 语义（纯函数，便于单测）：
///
/// - 未设置 ⇒ 默认 [`DEFAULT_NODES_FILE`]；
/// - 空串/纯空白 ⇒ `None`（关闭持久化）；
/// - 其它 ⇒ 该路径（trim 后）。
pub fn path_from_env(raw: Option<&str>) -> Option<PathBuf> {
    match raw {
        None => Some(PathBuf::from(DEFAULT_NODES_FILE)),
        Some(s) if s.trim().is_empty() => None,
        Some(s) => Some(PathBuf::from(s.trim())),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::{SystemTime, UNIX_EPOCH};

    fn node(ip: &str, port: u16) -> ProxyNode {
        ProxyNode::new(
            ip.to_string(),
            port,
            None,
            None,
            "us".to_string(),
            "dc".to_string(),
            "vendor-a".to_string(),
            100,
        )
    }

    fn temp_file(tag: &str) -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock")
            .as_nanos();
        std::env::temp_dir().join(format!(
            "ipp-node-registry-{}-{}-{}.json",
            std::process::id(),
            tag,
            nanos
        ))
    }

    fn cleanup(path: &Path) {
        let _ = fs::remove_file(path);
        // 临时文件名含单调序号，无法预测 ⇒ 扫同目录下所有同前缀的残留。
        if let (Some(dir), Some(name)) = (path.parent(), path.file_name()) {
            let prefix = format!("{}.tmp.", name.to_string_lossy());
            if let Ok(entries) = fs::read_dir(dir) {
                for e in entries.flatten() {
                    if e.file_name().to_string_lossy().starts_with(&prefix) {
                        let _ = fs::remove_file(e.path());
                    }
                }
            }
        }
    }

    #[test]
    fn normalizes_country_tier_and_addr() {
        let out = normalize_and_validate(node("203.0.113.9", 8080)).expect("valid");
        assert_eq!(out.country, "US");
        assert_eq!(out.tier, "datacenter");
        assert_eq!(out.addr, "203.0.113.9:8080");
    }

    #[test]
    fn rejects_invalid_fields() {
        let mut bad_ip = node("not-an-ip", 8080);
        assert!(matches!(
            normalize_and_validate(bad_ip.clone()),
            Err(RegistryError::Validation(_))
        ));

        bad_ip = node("::1", 8080);
        assert!(matches!(
            normalize_and_validate(bad_ip),
            Err(RegistryError::Validation(m)) if m.contains("IPv6")
        ));

        let mut zero_port = node("203.0.113.9", 0);
        assert!(matches!(
            normalize_and_validate(zero_port.clone()),
            Err(RegistryError::Validation(_))
        ));
        zero_port.port = 8080;

        let mut bad_country = node("203.0.113.9", 8080);
        bad_country.country = "USA".to_string();
        assert!(matches!(
            normalize_and_validate(bad_country),
            Err(RegistryError::Validation(_))
        ));

        let mut bad_tier = node("203.0.113.9", 8080);
        bad_tier.tier = "free".to_string();
        assert!(matches!(
            normalize_and_validate(bad_tier),
            Err(RegistryError::Validation(m)) if m.contains("FreePool")
        ));

        let mut bad_weight = node("203.0.113.9", 8080);
        bad_weight.weight = MAX_WEIGHT + 1;
        assert!(matches!(
            normalize_and_validate(bad_weight),
            Err(RegistryError::Validation(_))
        ));

        let mut bad_provider = node("203.0.113.9", 8080);
        bad_provider.provider = "free-gh0".to_string();
        assert!(matches!(
            normalize_and_validate(bad_provider),
            Err(RegistryError::Validation(m)) if m.contains("reserved")
        ));
    }

    #[test]
    fn rejects_unpaired_credentials() {
        let mut n = node("203.0.113.9", 8080);
        n.password = Some("secret".to_string());
        assert!(matches!(
            normalize_and_validate(n),
            Err(RegistryError::Validation(m)) if m.contains("paired")
        ));

        let mut ok = node("203.0.113.9", 8080);
        ok.username = Some("user".to_string());
        ok.password = Some("secret".to_string());
        assert!(normalize_and_validate(ok).is_ok());
    }

    #[tokio::test]
    async fn add_persists_and_reloads() {
        let path = temp_file("add");
        let reg = NodeRegistry::load(Some(path.clone()), vec![]).expect("load");
        let calls = AtomicUsize::new(0);
        let added = reg
            .add(node("203.0.113.10", 8080), |n| {
                calls.fetch_add(1, Ordering::SeqCst);
                assert_eq!(n.addr, "203.0.113.10:8080");
                Ok(())
            })
            .await
            .expect("add");
        assert_eq!(added.country, "US");
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        assert_eq!(reg.len(), 1);

        let reloaded = NodeRegistry::load(Some(path.clone()), vec![]).expect("reload");
        let snap = reloaded.snapshot();
        assert_eq!(snap.len(), 1);
        assert_eq!(snap[0].addr, "203.0.113.10:8080");
        assert_eq!(snap[0].country, "US");
        assert_eq!(snap[0].proto, EgressProto::Http);

        cleanup(&path);
    }

    #[tokio::test]
    async fn add_duplicate_is_rejected_before_apply() {
        let reg = NodeRegistry::in_memory(vec![]);
        reg.add(node("203.0.113.10", 8080), |_| Ok(()))
            .await
            .expect("first");
        let mut applied = false;
        let err = reg
            .add(node("203.0.113.10", 8080), |_| {
                applied = true;
                Ok(())
            })
            .await
            .expect_err("duplicate");
        assert!(matches!(err, RegistryError::Duplicate(_)));
        assert!(!applied, "duplicate must not reach the router");
        assert_eq!(reg.len(), 1);
    }

    #[tokio::test]
    async fn remove_persists_and_reloads() {
        let path = temp_file("remove");
        let reg = NodeRegistry::load(Some(path.clone()), vec![]).expect("load");
        reg.add(node("203.0.113.10", 8080), |_| Ok(()))
            .await
            .expect("add");
        reg.add(node("203.0.113.11", 8081), |_| Ok(()))
            .await
            .expect("add");

        let removed = reg
            .remove("203.0.113.10:8080", |addr| addr == "203.0.113.10:8080")
            .await
            .expect("remove");
        assert_eq!(removed.addr, "203.0.113.10:8080");
        assert_eq!(reg.len(), 1);

        let reloaded = NodeRegistry::load(Some(path.clone()), vec![]).expect("reload");
        assert_eq!(reloaded.len(), 1);
        assert_eq!(reloaded.snapshot()[0].addr, "203.0.113.11:8081");

        cleanup(&path);
    }

    #[tokio::test]
    async fn remove_unknown_returns_not_found() {
        let reg = NodeRegistry::in_memory(vec![]);
        let err = reg
            .remove("203.0.113.10:8080", |_| true)
            .await
            .expect_err("missing");
        assert!(matches!(err, RegistryError::NotFound(_)));
    }

    #[tokio::test]
    async fn persist_failure_leaves_memory_unchanged() {
        let path = temp_file("persist-dir");
        // 先按“文件不存在”加载（得到空清单），再把目标路径变成目录：
        // 落盘时 rename(file -> directory) 必然失败，而内存必须保持为空。
        let reg = NodeRegistry::load(Some(path.clone()), vec![]).expect("load");
        fs::create_dir_all(&path).expect("mkdir");
        let err = reg
            .add(node("203.0.113.10", 8080), |_| Ok(()))
            .await
            .expect_err("persist must fail");
        assert!(matches!(err, RegistryError::Persist(_)));
        assert!(reg.is_empty(), "memory must not change when disk fails");
        let _ = fs::remove_dir_all(&path);
        cleanup(&path);
    }

    #[tokio::test]
    async fn apply_failure_rolls_back_file() {
        let path = temp_file("rollback");
        let reg = NodeRegistry::load(Some(path.clone()), vec![]).expect("load");
        let err = reg
            .add(node("203.0.113.10", 8080), |_| {
                Err("router says no".to_string())
            })
            .await
            .expect_err("apply must fail");
        assert!(matches!(err, RegistryError::Apply(_)));
        assert!(reg.is_empty());

        let reloaded = NodeRegistry::load(Some(path.clone()), vec![]).expect("reload");
        assert!(
            reloaded.is_empty(),
            "failed apply must not leave the node on disk"
        );

        cleanup(&path);
    }

    #[test]
    fn corrupt_and_mismatched_files_are_rejected() {
        let path = temp_file("corrupt");
        fs::write(&path, "{not json").expect("write");
        let err = NodeRegistry::load(Some(path.clone()), vec![]).expect_err("corrupt");
        assert!(matches!(err, RegistryError::Persist(_)));
        cleanup(&path);

        let path = temp_file("version");
        fs::write(&path, r#"{"version": 99, "nodes": []}"#).expect("write");
        let err = NodeRegistry::load(Some(path.clone()), vec![]).expect_err("version");
        assert!(matches!(err, RegistryError::Persist(m) if m.contains("unsupported version")));
        cleanup(&path);
    }

    #[tokio::test]
    async fn group_weight_persists_only_managed_nodes() {
        let path = temp_file("weight");
        let reg = NodeRegistry::load(Some(path.clone()), vec![]).expect("load");
        reg.add(node("203.0.113.10", 8080), |_| Ok(()))
            .await
            .expect("add");

        let grouped = AtomicUsize::new(0);
        let changed = reg
            .set_group_weight("vendor-a", "US", 42, |p, c, w| {
                grouped.fetch_add(1, Ordering::SeqCst);
                assert_eq!((p, c, w), ("vendor-a", "US", 42));
                Ok(())
            })
            .await
            .expect("weight");
        assert_eq!(changed, 1);
        assert_eq!(grouped.load(Ordering::SeqCst), 1);

        let reloaded = NodeRegistry::load(Some(path.clone()), vec![]).expect("reload");
        assert_eq!(reloaded.snapshot()[0].weight, 42);

        // 无托管匹配项：仍应用，但不写文件。
        let changed = reg
            .set_group_weight("free-gh0", "ZZ", 0, |_, _, _| Ok(()))
            .await
            .expect("free weight");
        assert_eq!(changed, 0);

        cleanup(&path);
    }

    #[test]
    fn path_from_env_semantics() {
        assert_eq!(path_from_env(None), Some(PathBuf::from(DEFAULT_NODES_FILE)));
        assert_eq!(path_from_env(Some("")), None);
        assert_eq!(path_from_env(Some("   ")), None);
        assert_eq!(
            path_from_env(Some("  custom/nodes.json  ")),
            Some(PathBuf::from("custom/nodes.json"))
        );
    }

    // ---------------------------------------------------------------------
    // OPT-R18 P2-4：落盘必须离开 async 线程
    // ---------------------------------------------------------------------

    /// **确定性**证明：落盘被派发到 blocking 线程池，而不是在 worker 上跑。
    ///
    /// 手法：把 runtime 的 `max_blocking_threads` 压到 **1**，先派一个"钉子"
    /// 任务占住那唯一的 blocking 线程（它阻塞在 std channel 上），再发起
    /// `add()`。此时：
    ///
    /// - 若落盘走 `spawn_blocking` ⇒ 写任务**排队**，`add()` 处于 pending
    ///   ⇒ `is_finished()` 为 false；
    /// - 若落盘是同步的（回退成修复前的写法）⇒ `add()` 根本不碰 blocking
    ///   线程池，第一次 poll 就跑完了 ⇒ `is_finished()` 为 true ⇒ **判红**。
    ///
    /// 用 `max_blocking_threads(1)` 而不是"数 512 个线程"，是为了让这个
    /// 断言与平台/核数无关，且不制造 512 个 OS 线程。
    #[test]
    fn add_offloads_write_to_blocking_pool() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .max_blocking_threads(1)
            .enable_all()
            .build()
            .expect("runtime");
        let path = temp_file("offload");
        let reg = NodeRegistry::load(Some(path.clone()), vec![]).expect("load");

        rt.block_on(async {
            let (started_tx, started_rx) = std::sync::mpsc::channel::<()>();
            let (release_tx, release_rx) = std::sync::mpsc::channel::<()>();
            // 钉子：占住唯一的 blocking 线程，直到收到 release。
            let hog = tokio::task::spawn_blocking(move || {
                let _ = started_tx.send(());
                let _ = release_rx.recv();
            });
            // 等钉子**真的开始跑**，否则 add 的写任务可能先抢到空位。
            // 这里阻塞的是 current_thread runtime 的线程，钉子在其他线程，
            // 故不会自锁。
            started_rx.recv().expect("hog started");

            let handle =
                tokio::spawn(async move { reg.add(node("203.0.113.20", 8080), |_| Ok(())).await });

            // 让 add 任务至少被 poll 一次（跑到 await 点）。
            for _ in 0..8 {
                tokio::task::yield_now().await;
            }
            assert!(
                !handle.is_finished(),
                "add() completed without ever waiting on the blocking pool —— \
                 落盘又回到 worker 线程上了（P2-4 回退）"
            );

            let _ = release_tx.send(());
            let _ = hog.await;
            handle
                .await
                .expect("join")
                .expect("add must succeed once the blocking pool frees up");
        });

        let reloaded = NodeRegistry::load(Some(path.clone()), vec![]).expect("reload");
        assert_eq!(reloaded.len(), 1, "写任务排队后仍必须真正落盘");
        cleanup(&path);
    }

    /// 并发 `add` 不得丢更新。
    ///
    /// 这是 async 重构**引入的新风险**：落盘跨 await 后，"读-算-提交"之间
    /// 出现了真实让出点。若没有 `write_gate` 串行化，N 个并发 add 会各自
    /// 基于同一份旧清单计算 next，最后一个提交者把其余全部覆盖。
    ///
    /// 用真实文件（而非 `in_memory`）：`path = None` 时 `persist` 立即返回、
    /// 没有让出点，反而测不出竞态。
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn concurrent_adds_do_not_lose_updates() {
        const N: usize = 16;
        let path = temp_file("concurrent");
        let reg =
            std::sync::Arc::new(NodeRegistry::load(Some(path.clone()), vec![]).expect("load"));

        let mut handles = Vec::with_capacity(N);
        for i in 0..N {
            let reg = reg.clone();
            handles.push(tokio::spawn(async move {
                reg.add(
                    node(&format!("203.0.113.{}", 100 + i), 9000 + i as u16),
                    |_| Ok(()),
                )
                .await
            }));
        }
        for h in handles {
            h.await.expect("join").expect("every add must succeed");
        }

        assert_eq!(reg.len(), N, "并发 add 丢更新：写串行化（write_gate）失效");
        let reloaded = NodeRegistry::load(Some(path.clone()), vec![]).expect("reload");
        assert_eq!(reloaded.len(), N, "磁盘上必须也有全部 {N} 个节点");
        cleanup(&path);
    }
}
