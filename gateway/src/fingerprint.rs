//! GW-3 gateway-grade baseline fingerprint hardening.
//!
//! Scope is deliberately transport/upstream-option level (Chrome header
//! alignment + idle/TFO/keepalive/recv-buf), per the frozen plan: no
//! uTLS/Boring ClientHello surgery in this round.
//!
//! Deviations from manual-A3 (verified against pingora-core 0.6.0 source):
//! - `recv_buffer_size`/`send_buffer_size` do not exist in 0.6 `PeerOptions`;
//!   only `tcp_recv_buf` does → 64KB lands there (send side uses OS default).
//! - `TcpKeepalive` gains a `user_timeout` field on Linux → constructor is
//!   `cfg`-gated so the same code builds on Windows dev and Linux perf boxes.

use pingora::http::RequestHeader;
use pingora_core::upstreams::peer::HttpPeer;
use std::time::Duration;

/// Upstream idle reuse window (TLS session ticket stays warm).
pub const UPSTREAM_IDLE_SECS: u64 = 90;
/// 64KB socket window, aligned with modern broadband TCP windows.
pub const TCP_WINDOW_BYTES: usize = 64 * 1024;

pub struct FingerprintHardener;

impl FingerprintHardener {
    /// Apply the Chrome-stable transport profile to an egress peer.
    pub fn apply_chrome_profile(peer: &mut HttpPeer) {
        // 1. Long idle reuse (session tickets stay hot).
        peer.options.idle_timeout = Some(Duration::from_secs(UPSTREAM_IDLE_SECS));
        // 2. TCP Fast Open (0-RTT data on repeat routes).
        peer.options.tcp_fast_open = true;
        // 3. Dead-peer probing (idle 60s / every 10s / 3 strikes).
        peer.options.tcp_keepalive = Some(tcp_keepalive_60_10_3());
        // 4. 64KB receive window (0.6 has no send-side knob).
        peer.options.tcp_recv_buf = Some(TCP_WINDOW_BYTES);
    }

    /// Align upstream request headers with Chrome 124+ (`sec-ch-ua` family +
    /// `sec-fetch-*` + upgrade-insecure-requests: 8 headers total).
    pub fn align_http2_headers(headers: &mut RequestHeader) {
        let _ = headers.insert_header(
            "sec-ch-ua",
            r#""Chromium";v="124", "Google Chrome";v="124", "Not-A.Brand";v="99""#,
        );
        let _ = headers.insert_header("sec-ch-ua-mobile", "?0");
        let _ = headers.insert_header("sec-ch-ua-platform", r#""Windows""#);
        let _ = headers.insert_header("sec-fetch-dest", "document");
        let _ = headers.insert_header("sec-fetch-mode", "navigate");
        let _ = headers.insert_header("sec-fetch-site", "none");
        let _ = headers.insert_header("sec-fetch-user", "?1");
        let _ = headers.insert_header("upgrade-insecure-requests", "1");
    }
}

fn tcp_keepalive_60_10_3() -> pingora_core::protocols::TcpKeepalive {
    #[cfg(target_os = "linux")]
    {
        pingora_core::protocols::TcpKeepalive {
            idle: Duration::from_secs(60),
            interval: Duration::from_secs(10),
            count: 3,
            user_timeout: Duration::from_secs(30),
        }
    }
    #[cfg(not(target_os = "linux"))]
    {
        pingora_core::protocols::TcpKeepalive {
            idle: Duration::from_secs(60),
            interval: Duration::from_secs(10),
            count: 3,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_peer() -> HttpPeer {
        HttpPeer::new(
            "127.0.0.1:8080".to_string(),
            false,
            "example.com".to_string(),
        )
    }

    #[test]
    fn chrome_profile_sets_transport_options() {
        let mut peer = test_peer();
        FingerprintHardener::apply_chrome_profile(&mut peer);
        assert_eq!(
            peer.options.idle_timeout,
            Some(Duration::from_secs(UPSTREAM_IDLE_SECS))
        );
        assert!(peer.options.tcp_fast_open);
        let ka = peer.options.tcp_keepalive.expect("keepalive");
        assert_eq!(ka.idle, Duration::from_secs(60));
        assert_eq!(ka.interval, Duration::from_secs(10));
        assert_eq!(ka.count, 3);
        assert_eq!(peer.options.tcp_recv_buf, Some(TCP_WINDOW_BYTES));
    }

    #[test]
    fn aligns_eight_chrome_headers() {
        let mut h = RequestHeader::build("GET", b"/", None).unwrap();
        FingerprintHardener::align_http2_headers(&mut h);
        for name in [
            "sec-ch-ua",
            "sec-ch-ua-mobile",
            "sec-ch-ua-platform",
            "sec-fetch-dest",
            "sec-fetch-mode",
            "sec-fetch-site",
            "sec-fetch-user",
            "upgrade-insecure-requests",
        ] {
            assert!(
                h.headers.get(name).is_some(),
                "missing aligned header {name}"
            );
        }
        assert_eq!(
            h.headers
                .get("sec-ch-ua-mobile")
                .and_then(|v| v.to_str().ok()),
            Some("?0")
        );
    }
}
