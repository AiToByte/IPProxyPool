use std::io::{BufReader, Write};
use std::process::{Output, Stdio};
use std::time::Duration;

use tokio::net::TcpListener;
use tokio::process::Command;

const TEST_TIMEOUT: Duration = Duration::from_secs(15);
const UNAVAILABLE_REDIS: &str = "redis://127.0.0.1:0/";
const ERROR_SENTINEL: &str = "redis-error-detail-sentinel";

async fn check_startup(mode: &str, redis_url: &str) -> (Output, String) {
    let mut command = Command::new(env!("CARGO_BIN_EXE_pingora-proxy-gateway"));
    command.env_clear();
    // Winsock needs SystemRoot to locate the installed service providers.
    #[cfg(windows)]
    if let Some(system_root) = std::env::var_os("SystemRoot") {
        command.env("SystemRoot", system_root);
    }
    let child = command
        .arg("--test")
        .env("RUST_LOG", "info")
        .env("ADMIN_ADDR", "127.0.0.1:0")
        .env("GATEWAY_ADDR", "127.0.0.1:0")
        .env("METRICS_ADDR", "127.0.0.1:0")
        .env("NODES_FILE", "")
        .env("FREE_ENABLED", "0")
        .env("CLICKHOUSE_URL", "http://127.0.0.1:0")
        .env("TENANT_STORE", mode)
        .env("REDIS_URL", redis_url)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .expect("spawn gateway startup");
    let output = tokio::time::timeout(TEST_TIMEOUT, child.wait_with_output())
        .await
        .expect("gateway startup timed out")
        .expect("read startup output");
    let logs = String::from_utf8_lossy(&output.stderr).into_owned();
    (output, logs)
}

async fn answer_tenant_index(listener: TcpListener, answer: &'static [u8]) {
    let (stream, _) = tokio::time::timeout(TEST_TIMEOUT, listener.accept())
        .await
        .expect("gateway did not connect to Redis fixture")
        .expect("accept Redis connection");
    let stream = stream.into_std().expect("convert Redis fixture socket");
    tokio::task::spawn_blocking(move || {
        stream.set_nonblocking(false).expect("blocking socket");
        stream
            .set_read_timeout(Some(TEST_TIMEOUT))
            .expect("read timeout");
        stream
            .set_write_timeout(Some(TEST_TIMEOUT))
            .expect("write timeout");
        let mut reader = BufReader::new(stream);
        let mut parser = redis::Parser::new();
        loop {
            let frame = parser
                .parse_value(&mut reader)
                .expect("parse Redis request");
            let command: Vec<String> =
                redis::from_redis_value(&frame).expect("decode Redis request");
            match command.first().map(String::as_str) {
                Some("CLIENT") => reader
                    .get_mut()
                    .write_all(b"+OK\r\n")
                    .expect("CLIENT reply"),
                Some("SMEMBERS") => {
                    assert_eq!(command.as_slice(), ["SMEMBERS", "ipp:tenant:index"]);
                    reader.get_mut().write_all(answer).expect("index reply");
                    return;
                }
                _ => panic!("unexpected Redis fixture command"),
            }
        }
    })
    .await
    .expect("join Redis fixture task");
}

#[tokio::test]
async fn required_redis_aborts_startup_when_unavailable() {
    let (output, logs) = check_startup("redis", UNAVAILABLE_REDIS).await;
    assert_eq!(output.status.code(), Some(1), "{logs}");
    assert!(
        logs.contains("TENANT_STORE=redis requires a Redis connection"),
        "{logs}"
    );
    assert!(!logs.contains("write-through flusher online"), "{logs}");
}

#[tokio::test]
async fn required_redis_aborts_startup_when_tenant_load_fails() {
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind Redis fixture");
    let url = format!(
        "redis://{}/",
        listener.local_addr().expect("fixture address")
    );
    let ((), (output, logs)) = tokio::join!(
        answer_tenant_index(listener, b"-ERR redis-error-detail-sentinel\r\n"),
        check_startup("redis", &url),
    );
    assert_eq!(output.status.code(), Some(1), "{logs}");
    assert!(logs.contains("failed to load Redis tenant state"), "{logs}");
    assert!(!logs.contains(ERROR_SENTINEL), "raw Redis error leaked");
    assert!(!logs.contains("write-through flusher online"), "{logs}");
}

#[tokio::test]
async fn required_redis_starts_after_tenant_state_load_succeeds() {
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind Redis fixture");
    let url = format!(
        "redis://{}/",
        listener.local_addr().expect("fixture address")
    );
    let ((), (output, logs)) = tokio::join!(
        answer_tenant_index(listener, b"*0\r\n"),
        check_startup("redis", &url),
    );
    assert_eq!(output.status.code(), Some(0), "{logs}");
    assert!(logs.contains("[Tenant] persistence=redis"), "{logs}");
    assert!(logs.contains("persisted tenant state loaded"), "{logs}");
    assert!(logs.contains("write-through flusher online"), "{logs}");
}

#[tokio::test]
async fn memory_mode_does_not_require_redis_at_startup() {
    let (output, logs) = check_startup("memory", UNAVAILABLE_REDIS).await;
    assert_eq!(output.status.code(), Some(0), "{logs}");
    assert!(!logs.contains("write-through flusher online"), "{logs}");
}
