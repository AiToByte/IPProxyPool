//! GW-6 传输层连接器抽象：默认 tokio，Linux 下预留 io_uring 接入点（SPIKE-R2 待放行）。
//! Windows 构建不受影响（io_uring 实现仅在 #[cfg(unix)] 下存在，且不启用 tokio-uring crate）。
//! 当前所有构造均降级为 tokio::net::TcpStream，行为与修改前逐字节一致。

use async_trait::async_trait;
use std::io;
use std::time::Duration;
use tokio::net::TcpStream;

/// GW-6 TCP 连接器抽象：统一建链入口，默认 tokio，未来可无缝替换为 io_uring。
/// 所有实现必须 `Send + Sync`，以便在后台 worker 中跨线程共享。
#[async_trait]
pub trait TcpConnector: Send + Sync {
    /// 建立 TCP 连接，返回流。`addr` 形如 `"ip:port"`。
    async fn connect(&self, addr: &str) -> io::Result<TcpStream>;
    /// 单次建链超时参考值（由调用方自行包 `tokio::time::timeout`）。
    /// 当前由预热器按 `PREWARM_TCP_TIMEOUT` 外部包超时，此字段预留供未来 io_uring 实现直接使用。
    #[allow(dead_code)]
    fn timeout(&self) -> Duration;
}

/// 默认实现：`tokio::net::TcpStream`（全平台可用，Windows/Linux 均正常）。
#[derive(Debug, Clone, Copy, Default)]
pub struct TokioTcpConnector {
    /// 单次建链超时参考值（当前由预热器按 `PREWARM_TCP_TIMEOUT` 外部包超时，此字段预留供未来 io_uring 实现直接使用）。
    #[allow(dead_code)]
    pub timeout: Duration,
}

#[async_trait]
impl TcpConnector for TokioTcpConnector {
    async fn connect(&self, addr: &str) -> io::Result<TcpStream> {
        TcpStream::connect(addr).await
    }
    fn timeout(&self) -> Duration {
        self.timeout
    }
}

/// GW-6 io_uring 实现占位（Linux 专用，SPIKE-R2 待放行）。
///
/// 当前**不启用** `tokio-uring` crate（避免 Windows 构建破坏，且与
/// Pingora 内部事件循环集成有风险），仅预留结构体名称与 trait 实现骨架。
/// 未来接入时仅需替换此处 `connect` 内部为 `tokio_uring::net::TcpStream::connect`，
/// 其余调用点无需改动。
#[cfg(unix)]
#[derive(Debug, Clone, Copy, Default)]
pub struct UringTcpConnector {
    pub timeout: Duration,
}

#[cfg(unix)]
#[async_trait]
impl TcpConnector for UringTcpConnector {
    async fn connect(&self, addr: &str) -> io::Result<TcpStream> {
        // TODO(SPIKE-R2): 替换为 tokio_uring::net::TcpStream::connect(addr)
        // 当前降级为 tokio 实现，保证构建与行为不变。
        TcpStream::connect(addr).await
    }
    fn timeout(&self) -> Duration {
        self.timeout
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn tokio_connector_connect_refused_is_err() {
        // 不存在端口的连接应快速失败，不 panic。
        let connector = TokioTcpConnector::default();
        let res = connector.connect("127.0.0.1:1").await;
        assert!(res.is_err(), "连接拒绝端口应返回错误，got {:?}", res);
    }

    #[test]
    fn connector_timeout_default() {
        let c = TokioTcpConnector::default();
        assert_eq!(c.timeout(), Duration::from_secs(0));
    }
}
