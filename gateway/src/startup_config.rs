//! Startup configuration parsing and validation.

use std::net::SocketAddr;

const ADMIN_ADDR_ERROR: &str = "ADMIN_ADDR must be a valid host:port listener address";
const CONNECT_ADDR_ERROR: &str = "CONNECT_ADDR must be a loopback IP:port socket address";
const REDIS_URL_ERROR: &str = "REDIS_URL must be a valid Redis URL";
const TENANT_STORE_ERROR: &str = "TENANT_STORE must be memory or redis";

fn is_valid_socket_listener(raw: &str) -> bool {
    raw.parse::<SocketAddr>().is_ok() || {
        let Some((host, port)) = raw.rsplit_once(':') else {
            return false;
        };
        let host_valid = !host.is_empty()
            && !host.contains(['[', ']'])
            && !host.contains(':')
            && host
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'-' | b'_'));
        host_valid && port.parse::<u16>().is_ok()
    }
}

fn parse_required_listener(
    raw: Option<&str>,
    key: &'static str,
    default: &str,
) -> Result<String, &'static str> {
    let value = raw
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .unwrap_or(default);
    if is_valid_socket_listener(value) {
        Ok(value.to_string())
    } else {
        Err(match key {
            "GATEWAY_ADDR" => "GATEWAY_ADDR must be a valid host:port listener address",
            "METRICS_ADDR" => "METRICS_ADDR must be a valid host:port listener address",
            _ => "listener address is invalid",
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum TenantStoreMode {
    Memory,
    Redis,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum StartupConfigEnvVar {
    AdminAddr,
    GatewayAddr,
    MetricsAddr,
    ConnectAddr,
    RedisStreamShards,
    RedisUrl,
    TenantStore,
    APIKey,
    AdminAPIKey,
}

impl StartupConfigEnvVar {
    fn encoding_error(self) -> &'static str {
        match self {
            Self::AdminAddr => "ADMIN_ADDR must be valid Unicode",
            Self::GatewayAddr => "GATEWAY_ADDR must be valid Unicode",
            Self::MetricsAddr => "METRICS_ADDR must be valid Unicode",
            Self::ConnectAddr => "CONNECT_ADDR must be valid Unicode",
            Self::RedisStreamShards => "REDIS_STREAM_SHARDS must be valid Unicode",
            Self::RedisUrl => "REDIS_URL must be valid Unicode",
            Self::TenantStore => "TENANT_STORE must be valid Unicode",
            Self::APIKey => "API_KEY must be valid Unicode",
            Self::AdminAPIKey => "ADMIN_API_KEY must be valid Unicode",
        }
    }
}

#[derive(Default)]
pub(crate) struct StartupConfigInput {
    pub(crate) admin_addr: Option<String>,
    pub(crate) gateway_addr: Option<String>,
    pub(crate) metrics_addr: Option<String>,
    pub(crate) connect_addr: Option<String>,
    pub(crate) redis_stream_shards: Option<String>,
    pub(crate) redis_url: Option<String>,
    pub(crate) tenant_store: Option<String>,
    pub(crate) api_key: Option<String>,
    pub(crate) admin_api_key: Option<String>,
    pub(crate) invalid_environment: Option<StartupConfigEnvVar>,
}

impl StartupConfigInput {
    pub(crate) fn from_environment() -> Self {
        let mut invalid_environment = None;
        let admin_addr = read_env(
            "ADMIN_ADDR",
            StartupConfigEnvVar::AdminAddr,
            &mut invalid_environment,
        );
        let gateway_addr = read_env(
            "GATEWAY_ADDR",
            StartupConfigEnvVar::GatewayAddr,
            &mut invalid_environment,
        );
        let metrics_addr = read_env(
            "METRICS_ADDR",
            StartupConfigEnvVar::MetricsAddr,
            &mut invalid_environment,
        );
        let connect_addr = read_env(
            "CONNECT_ADDR",
            StartupConfigEnvVar::ConnectAddr,
            &mut invalid_environment,
        );
        let redis_stream_shards = read_env(
            "REDIS_STREAM_SHARDS",
            StartupConfigEnvVar::RedisStreamShards,
            &mut invalid_environment,
        );
        let redis_url = read_env(
            "REDIS_URL",
            StartupConfigEnvVar::RedisUrl,
            &mut invalid_environment,
        );
        let tenant_store = read_env(
            "TENANT_STORE",
            StartupConfigEnvVar::TenantStore,
            &mut invalid_environment,
        );
        let api_key = read_env(
            "API_KEY",
            StartupConfigEnvVar::APIKey,
            &mut invalid_environment,
        );
        let admin_api_key = read_env(
            "ADMIN_API_KEY",
            StartupConfigEnvVar::AdminAPIKey,
            &mut invalid_environment,
        );
        Self {
            admin_addr,
            gateway_addr,
            metrics_addr,
            connect_addr,
            redis_stream_shards,
            redis_url,
            tenant_store,
            api_key,
            admin_api_key,
            invalid_environment,
        }
    }
}

fn read_env(
    key: &'static str,
    name: StartupConfigEnvVar,
    invalid_environment: &mut Option<StartupConfigEnvVar>,
) -> Option<String> {
    match std::env::var(key) {
        Ok(value) => Some(value),
        Err(std::env::VarError::NotPresent) => None,
        Err(std::env::VarError::NotUnicode(_)) => {
            invalid_environment.get_or_insert(name);
            None
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct StartupConfig {
    pub(crate) admin_addr: String,
    pub(crate) gateway_addr: String,
    pub(crate) metrics_addr: String,
    pub(crate) connect_addr: Option<SocketAddr>,
    pub(crate) redis_stream_shards: usize,
    pub(crate) redis_url: String,
    pub(crate) tenant_store: TenantStoreMode,
    pub(crate) api_key: Option<String>,
    pub(crate) admin_api_key: Option<String>,
}

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

pub(crate) fn parse_connect_addr(raw: Option<&str>) -> Result<Option<SocketAddr>, &'static str> {
    let Some(value) = raw.map(str::trim).filter(|value| !value.is_empty()) else {
        return Ok(None);
    };
    let addr = value
        .parse::<SocketAddr>()
        .map_err(|_| CONNECT_ADDR_ERROR)?;
    if !addr.ip().is_loopback() {
        return Err(CONNECT_ADDR_ERROR);
    }
    Ok(Some(addr))
}

pub(crate) fn parse_redis_url(raw: Option<&str>, default: &str) -> Result<String, &'static str> {
    let value = raw
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .unwrap_or(default);
    redis::Client::open(value).map_err(|_| REDIS_URL_ERROR)?;
    Ok(value.to_string())
}

pub(crate) fn parse_tenant_store(raw: Option<&str>) -> Result<TenantStoreMode, &'static str> {
    match raw
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .unwrap_or("memory")
        .to_ascii_lowercase()
        .as_str()
    {
        "memory" => Ok(TenantStoreMode::Memory),
        "redis" => Ok(TenantStoreMode::Redis),
        _ => Err(TENANT_STORE_ERROR),
    }
}

pub(crate) fn parse_startup_config(
    input: StartupConfigInput,
    redis_default: &str,
) -> Result<StartupConfig, &'static str> {
    if let Some(name) = input.invalid_environment {
        return Err(name.encoding_error());
    }
    Ok(StartupConfig {
        admin_addr: parse_admin_addr(input.admin_addr.as_deref())?,
        gateway_addr: parse_required_listener(
            input.gateway_addr.as_deref(),
            "GATEWAY_ADDR",
            "127.0.0.1:8916",
        )?,
        metrics_addr: parse_required_listener(
            input.metrics_addr.as_deref(),
            "METRICS_ADDR",
            crate::metrics::METRICS_ADDR,
        )?,
        connect_addr: parse_connect_addr(input.connect_addr.as_deref())?,
        redis_stream_shards: crate::redis_shard::parse_redis_stream_shards(
            input
                .redis_stream_shards
                .ok_or(std::env::VarError::NotPresent),
        )?,
        redis_url: parse_redis_url(input.redis_url.as_deref(), redis_default)?,
        tenant_store: parse_tenant_store(input.tenant_store.as_deref())?,
        api_key: input.api_key,
        admin_api_key: input.admin_api_key,
    })
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
    fn startup_config_rejects_invalid_values_without_external_side_effects() {
        assert!(parse_startup_config(
            StartupConfigInput {
                admin_addr: Some("bad-address".into()),
                redis_stream_shards: Some("1".into()),
                redis_url: Some("redis://127.0.0.1:6379/".into()),
                tenant_store: Some("memory".into()),
                ..Default::default()
            },
            "redis://127.0.0.1:6379/",
        )
        .is_err());
        assert!(parse_startup_config(
            StartupConfigInput {
                connect_addr: Some("0.0.0.0:8917".into()),
                redis_stream_shards: Some("1".into()),
                redis_url: Some("redis://127.0.0.1:6379/".into()),
                tenant_store: Some("memory".into()),
                ..Default::default()
            },
            "redis://127.0.0.1:6379/",
        )
        .is_err());
        assert!(parse_startup_config(
            StartupConfigInput {
                redis_stream_shards: Some("1".into()),
                redis_url: Some("not-a-redis-url".into()),
                tenant_store: Some("memory".into()),
                ..Default::default()
            },
            "redis://127.0.0.1:6379/",
        )
        .is_err());
        assert!(parse_startup_config(
            StartupConfigInput {
                redis_stream_shards: Some("1".into()),
                redis_url: Some("redis://127.0.0.1:6379/".into()),
                tenant_store: Some("unknown".into()),
                ..Default::default()
            },
            "redis://127.0.0.1:6379/",
        )
        .is_err());
    }

    #[test]
    fn startup_config_accepts_defaults_and_valid_boundaries() {
        let config = parse_startup_config(
            StartupConfigInput {
                connect_addr: Some("127.0.0.1:8917".into()),
                tenant_store: Some("REDIS".into()),
                ..Default::default()
            },
            "redis://127.0.0.1:6379/",
        )
        .expect("valid startup defaults");
        assert_eq!(config.admin_addr, "127.0.0.1:9092");
        assert_eq!(config.gateway_addr, "127.0.0.1:8916");
        assert_eq!(config.metrics_addr, crate::metrics::METRICS_ADDR);
        assert_eq!(config.connect_addr.unwrap().to_string(), "127.0.0.1:8917");
        assert_eq!(config.redis_stream_shards, 1);
        assert_eq!(config.tenant_store, TenantStoreMode::Redis);
    }

    #[test]
    fn invalid_unicode_environment_values_are_not_treated_as_missing() {
        let input = StartupConfigInput {
            redis_stream_shards: Some("1".into()),
            redis_url: Some("redis://127.0.0.1:6379/".into()),
            invalid_environment: Some(StartupConfigEnvVar::TenantStore),
            ..Default::default()
        };
        assert_eq!(
            parse_startup_config(input, "redis://127.0.0.1:6379/").unwrap_err(),
            "TENANT_STORE must be valid Unicode"
        );

        let input = StartupConfigInput {
            invalid_environment: Some(StartupConfigEnvVar::RedisStreamShards),
            ..Default::default()
        };
        assert_eq!(
            parse_startup_config(input, "redis://127.0.0.1:6379/").unwrap_err(),
            "REDIS_STREAM_SHARDS must be valid Unicode"
        );
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
