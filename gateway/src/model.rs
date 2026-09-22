//! GW-1 core data model: proxy nodes, routing spec, per-request context.
//!
//! GW-4 已落地：`tenant_id` / `tenant_account` / `transferred_bytes` 均为
//! 稳定 API（网关鉴权/计量/遥测全链路消费），不再是预留字段。

use std::sync::Arc;
use std::time::Instant;

use crate::bandit::VectorD;
use crate::tenant::TenantAccount;

/// Upstream egress proxy node metadata.
///
/// R2-6：`addr`（`ip:port`）构造时预存，选路/排除/臂表 key 比较走 `&str`
/// 零分配；统一经 [`ProxyNode::new`] 构造，保证与 `ip:port` 一致。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProxyNode {
    pub ip: String,
    pub port: u16,
    /// 预存 `format!("{ip}:{port}")`（R2-6：构造时一次分配，读路径复用）。
    pub addr: String,
    pub username: Option<String>,
    pub password: Option<String>,
    pub country: String,
    /// `datacenter` | `residential` | `mobile`
    pub tier: String,
    pub provider: String,
    pub weight: u32,
    /// P2 出站协议（默认 Http；SOCKS 节点经 `with_proto` 标注，走翻译桥出站）。
    pub proto: EgressProto,
    /// R3-2 真 egress 出口 IP（FullCheck 实测；None＝未知/经典 HTTP 节点，
    /// 此时遥测 out_ip 回落 `ip`，行为冻结）。
    pub exit_ip: Option<String>,
}

/// 出站协议（P2 SOCKS egress）。
///
/// - `Http`：经典 HTTP 正向代理（存量全部节点＋免费 http/https 映射到此）；
/// - `Socks5`/`Socks4`：经网关内翻译桥出站；默认选路永不命中（见 `RouterEngine::matches` 隔离）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EgressProto {
    Http,
    Socks5,
    Socks4,
}

impl EgressProto {
    /// 解析客户端 `X-Proxy-Proto` 头／`proto-` token（大小写不敏感；非法值 None）。
    pub fn from_token(tok: &str) -> Option<Self> {
        match tok.to_ascii_lowercase().as_str() {
            "http" | "https" => Some(EgressProto::Http),
            "socks5" | "socks5h" => Some(EgressProto::Socks5),
            "socks4" | "socks4a" => Some(EgressProto::Socks4),
            _ => None,
        }
    }
}

impl ProxyNode {
    /// 全字段构造（`addr` 按 `ip:port` 自动预存，保证一致）。
    /// 8 参数与字段 1:1 对应（builder 属过度设计，参考 `reload_nodes` 惯例放行）。
    /// `proto` 缺省 Http（P2：存量调用点零改动；SOCKS 节点经 `with_proto` 标注）。
    /// `exit_ip` 缺省 None（R3-2：遥测回落 `ip`，存量行为冻结）。
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        ip: String,
        port: u16,
        username: Option<String>,
        password: Option<String>,
        country: String,
        tier: String,
        provider: String,
        weight: u32,
    ) -> Self {
        let addr = format!("{ip}:{port}");
        Self {
            ip,
            port,
            addr,
            username,
            password,
            country,
            tier,
            provider,
            weight,
            proto: EgressProto::Http,
            exit_ip: None,
        }
    }

    /// P2：标注出站协议（free_pool Registry／静态装配用）。
    pub fn with_proto(mut self, proto: EgressProto) -> Self {
        self.proto = proto;
        self
    }

    /// R3-2：标注真 egress 出口（free_pool Registry 从 FullCheck 结果同步；
    /// 遥测 out_ip 与隔离匹配消费）。
    pub fn with_exit_ip(mut self, exit_ip: Option<String>) -> Self {
        self.exit_ip = exit_ip;
        self
    }
}

/// Dynamic routing policy parsed from client request.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RoutingSpec {
    pub country: Option<String>,
    pub session_id: Option<String>,
    pub tier: Option<String>,
    pub target_domain: String,
    /// P2：显式出站协议约束（`X-Proxy-Proto` 头／`proto-` token）。
    /// `None`＝默认 http（SOCKS 节点永不命中，见 `RouterEngine::matches`）。
    pub proto: Option<EgressProto>,
}

/// Per-request gateway lifecycle context（稳定 API：全字段均被网关消费）。
pub struct ProxyContext {
    pub start_time: Instant,
    pub routing_spec: RoutingSpec,
    /// R2-6：当前节点以 `Arc` 持有（选路返回即引用，落 ctx 不再克隆整节点）。
    pub current_node: Option<Arc<ProxyNode>>,
    pub retry_count: usize,
    pub max_retries: usize,
    pub client_ip: String,
    /// GW-4 reservation: authenticated tenant id (None until tenant.rs lands).
    pub tenant_id: Option<String>,
    /// GW-4: authenticated tenant account (owns one in-flight slot).
    pub tenant_account: Option<Arc<TenantAccount>>,
    /// GW-4 reservation: streaming byte counter (filled by response_body_filter).
    pub transferred_bytes: u64,
    /// R2-2：本请求已失败的 `ip:port` 集合（`fail_to_connect` 记录，
    /// `upstream_peer` 重选时排除，避免原地打死节点重试）。
    pub failed_addrs: Vec<String>,
    /// R2-6：本请求已算好的 bandit 上下文（`upstream_peer` 无状态路径计算一次，
    /// `logging` 复用同一向量做 reward 更新：省一次 `extract_context`
    ///（含时钟 syscall），且选学一致；粘滞路径为 None（logging 回落现算）。
    pub bandit_context: Option<VectorD>,
}

impl Default for ProxyContext {
    fn default() -> Self {
        Self {
            start_time: Instant::now(),
            routing_spec: RoutingSpec::default(),
            current_node: None,
            retry_count: 0,
            max_retries: 3,
            client_ip: String::new(),
            tenant_id: None,
            tenant_account: None,
            transferred_bytes: 0,
            failed_addrs: Vec::new(),
            bandit_context: None,
        }
    }
}
