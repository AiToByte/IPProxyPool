//! Startup configuration parsing and validation.

use std::net::SocketAddr;

const ADMIN_ADDR_ERROR: &str = "ADMIN_ADDR must be a valid host:port listener address";

/// Parse the optional admin bind string without echoing the configured value.
pub(crate) fn parse_admin_addr(raw: Option<&str>) -> Result<String, &'static str> {
    let value = raw
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .unwrap_or("127.0.0.1:9092");
    if value.parse::<SocketAddr>().is_ok() {
        return Ok(value.to_owned());
    }

    let (host, port) = value.rsplit_once(':').ok_or(ADMIN_ADDR_ERROR)?;
    let host_valid = !host.is_empty()
        && if host.starts_with('[') && host.ends_with(']') {
            host[1..host.len() - 1]
                .parse::<std::net::Ipv6Addr>()
                .is_ok()
        } else {
            !host.contains(['[', ']'])
                && !host.contains(':')
                && host
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'-' | b'_'))
        };
    let port_valid = port.parse::<u16>().is_ok();
    if !host_valid || !port_valid {
        return Err(ADMIN_ADDR_ERROR);
    }
    Ok(value.to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn admin_addr_defaults_when_missing_or_empty() {
        assert_eq!(parse_admin_addr(None), Ok("127.0.0.1:9092".to_owned()));
        assert_eq!(parse_admin_addr(Some("")), Ok("127.0.0.1:9092".to_owned()));
        assert_eq!(
            parse_admin_addr(Some("  ")),
            Ok("127.0.0.1:9092".to_owned())
        );
    }

    #[test]
    fn admin_addr_accepts_socket_hosts_and_preserves_original_host() {
        for (raw, expected) in [
            ("127.0.0.1:9092", "127.0.0.1:9092"),
            ("[::1]:9092", "[::1]:9092"),
            ("localhost:9092", "localhost:9092"),
            ("proxy-admin.internal:9092", "proxy-admin.internal:9092"),
            ("localhost:0", "localhost:0"),
            ("127.0.0.1:0", "127.0.0.1:0"),
        ] {
            assert_eq!(parse_admin_addr(Some(raw)), Ok(expected.to_owned()));
        }
    }

    #[test]
    fn admin_addr_rejects_malformed_values_without_echoing_input() {
        for raw in [
            "secret-sentinel",
            "localhost",
            "localhost:65536",
            "[::1:9092",
            "[bad]:9092",
            "host/name:9092",
            "host name:9092",
            "host:9092/path",
        ] {
            let error = parse_admin_addr(Some(raw)).unwrap_err();
            assert!(error.contains("ADMIN_ADDR"));
            assert!(!error.contains(raw));
        }
    }
}
