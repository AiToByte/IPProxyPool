//! GW-1 core data model: proxy nodes, routing spec, per-request context.
//!
//! GW-4 已落地：`tenant_id` / `tenant_account` / `transferred_bytes` 均为
//! 稳定 API（网关鉴权/计量/遥测全链路消费），不再是预留字段。

use std::sync::Arc;
use std::time::Instant;

use crate::tenant::TenantAccount;

/// Upstream egress proxy node metadata.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProxyNode {
    pub ip: String,
    pub port: u16,
    pub username: Option<String>,
    pub password: Option<String>,
    pub country: String,
    /// `datacenter` | `residential` | `mobile`
    pub tier: String,
    pub provider: String,
    pub weight: u32,
}

impl ProxyNode {
    pub fn addr(&self) -> String {
        format!("{}:{}", self.ip, self.port)
    }
}

/// Dynamic routing policy parsed from client request.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RoutingSpec {
    pub country: Option<String>,
    pub session_id: Option<String>,
    pub tier: Option<String>,
    pub target_domain: String,
}

/// Per-request gateway lifecycle context（稳定 API：全字段均被网关消费）。
pub struct ProxyContext {
    pub start_time: Instant,
    pub routing_spec: RoutingSpec,
    pub current_node: Option<ProxyNode>,
    pub retry_count: usize,
    pub max_retries: usize,
    pub client_ip: String,
    /// GW-4 reservation: authenticated tenant id (None until tenant.rs lands).
    pub tenant_id: Option<String>,
    /// GW-4: authenticated tenant account (owns one in-flight slot).
    pub tenant_account: Option<Arc<TenantAccount>>,
    /// GW-4 reservation: streaming byte counter (filled by response_body_filter).
    pub transferred_bytes: u64,
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
        }
    }
}
