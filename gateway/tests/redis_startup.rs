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

// Build RESP lengths from bytes so a typo cannot turn a policy test into a protocol failure.
fn array(values: &[&str]) -> Vec<u8> {
    let mut result = format!("*{}\r\n", values.len()).into_bytes();
    for value in values {
        result.extend(format!("${}\r\n{value}\r\n", value.len()).as_bytes());
    }
    result
}

fn scan_page(cursor: u64, keys: &[&str]) -> Vec<u8> {
    let mut result = format!("*2\r\n${}\r\n{cursor}\r\n", cursor.to_string().len()).into_bytes();
    result.extend(array(keys));
    result
}

type Step = (Vec<String>, Vec<u8>);

fn step(command: &[&str], reply: Vec<u8>) -> Step {
    (command.iter().map(|s| (*s).to_owned()).collect(), reply)
}

fn restore_script(index: &[&str], scanned: &[&str], hashes: &[(&str, Vec<u8>)]) -> Vec<Step> {
    let mut script = vec![
        step(&["SMEMBERS", "ipp:tenant:index"], array(index)),
        step(
            &["SCAN", "0", "MATCH", "ipp:tenant:*", "COUNT", "128"],
            scan_page(0, scanned),
        ),
    ];
    for (key, reply) in hashes {
        script.push(step(&["HGETALL", key], reply.clone()));
    }
    script
}

async fn serve_restore(listener: TcpListener, script: Vec<Step>) {
    let (stream, _) = tokio::time::timeout(TEST_TIMEOUT, listener.accept())
        .await
        .expect("gateway must connect")
        .expect("accept Redis fixture");
    let stream = stream.into_std().expect("convert fixture socket");
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
        for (expected, response) in script {
            loop {
                let frame = parser
                    .parse_value(&mut reader)
                    .expect("restore script ended prematurely");
                let command: Vec<String> = redis::from_redis_value(&frame).expect("decode command");
                if command.first().map(String::as_str) == Some("CLIENT") {
                    reader
                        .get_mut()
                        .write_all(b"+OK\r\n")
                        .expect("handshake response");
                    continue;
                }
                assert_eq!(command, expected, "unexpected restore command");
                reader
                    .get_mut()
                    .write_all(&response)
                    .expect("restore response");
                break;
            }
        }
        // Restoration is complete. Close deliberately; background workers are outside this script.
    })
    .await
    .expect("join restore fixture");
}

fn valid_hash() -> Vec<u8> {
    array(&["balance_usd", "42.5", "total_bytes", "7", "is_active", "0"])
}

async fn run_restore(script: Vec<Step>) -> (Output, String) {
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind fixture");
    let url = format!(
        "redis://{}/",
        listener.local_addr().expect("fixture address")
    );
    let ((), result) = tokio::join!(
        serve_restore(listener, script),
        check_startup("redis", &url)
    );
    result
}

fn assert_rejected(output: Output, logs: String) {
    assert_eq!(output.status.code(), Some(1), "{logs}");
    assert!(logs.contains("failed to load Redis tenant state"), "{logs}");
    assert!(!logs.contains("write-through flusher online"), "{logs}");
}

fn assert_started(output: Output, logs: String) {
    assert_eq!(output.status.code(), Some(0), "{logs}");
    assert!(logs.contains("persisted tenant state loaded"), "{logs}");
    assert!(logs.contains("write-through flusher online"), "{logs}");
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
    let script = vec![step(
        &["SMEMBERS", "ipp:tenant:index"],
        b"-ERR redis-error-detail-sentinel\r\n".to_vec(),
    )];
    let (output, logs) = run_restore(script).await;
    assert!(!logs.contains(ERROR_SENTINEL), "raw Redis error leaked");
    assert_rejected(output, logs);
}

#[tokio::test]
async fn required_redis_starts_with_unknown_unindexed_orphan() {
    let script = restore_script(
        &["default_key"],
        &["ipp:tenant:legacy"],
        &[
            ("ipp:tenant:default_key", valid_hash()),
            ("ipp:tenant:legacy", valid_hash()),
        ],
    );
    let (output, logs) = run_restore(script).await;
    assert!(logs.contains("orphan"), "orphan diagnostic missing: {logs}");
    assert_started(output, logs);
}

#[tokio::test]
async fn required_redis_aborts_when_configured_hash_is_unindexed() {
    let script = restore_script(
        &[],
        &["ipp:tenant:default_key"],
        &[("ipp:tenant:default_key", valid_hash())],
    );
    let (output, logs) = run_restore(script).await;
    assert_rejected(output, logs);
}

#[tokio::test]
async fn required_redis_aborts_when_only_unknown_orphan_exists() {
    let script = restore_script(
        &[],
        &["ipp:tenant:legacy"],
        &[("ipp:tenant:legacy", valid_hash())],
    );
    let (output, logs) = run_restore(script).await;
    assert_rejected(output, logs);
}

#[tokio::test]
async fn required_redis_aborts_startup_when_indexed_hash_is_malformed() {
    let script = restore_script(
        &["default_key"],
        &[],
        &[("ipp:tenant:default_key", array(&["balance_usd", "NaN"]))],
    );
    let (output, logs) = run_restore(script).await;
    assert!(
        logs.contains("\u{65e0}\u{6cd5}\u{89e3}\u{6790}"),
        "must reach hash decoding: {logs}"
    );
    assert_rejected(output, logs);
}

#[tokio::test]
async fn required_redis_aborts_startup_when_indexed_hash_is_missing() {
    let script = restore_script(
        &["default_key"],
        &[],
        &[("ipp:tenant:default_key", array(&[]))],
    );
    let (output, logs) = run_restore(script).await;
    assert_rejected(output, logs);
}

#[tokio::test]
async fn required_redis_starts_with_truly_empty_store() {
    let (output, logs) = run_restore(restore_script(&[], &[], &[])).await;
    assert_started(output, logs);
}

#[tokio::test]
async fn required_redis_starts_after_tenant_state_load_succeeds() {
    let script = restore_script(
        &["default_key"],
        &[],
        &[("ipp:tenant:default_key", valid_hash())],
    );
    let (output, logs) = run_restore(script).await;
    assert_started(output, logs);
}

#[tokio::test]
async fn required_redis_scans_continuation_pages_and_deduplicates_keys() {
    let script = vec![
        step(&["SMEMBERS", "ipp:tenant:index"], array(&["default_key"])),
        step(
            &["SCAN", "0", "MATCH", "ipp:tenant:*", "COUNT", "128"],
            scan_page(19, &["ipp:tenant:index", "ipp:tenant:default_key"]),
        ),
        step(
            &["SCAN", "19", "MATCH", "ipp:tenant:*", "COUNT", "128"],
            scan_page(0, &["ipp:tenant:default_key"]),
        ),
        step(&["HGETALL", "ipp:tenant:default_key"], valid_hash()),
    ];
    let (output, logs) = run_restore(script).await;
    assert_started(output, logs);
}

#[tokio::test]
async fn memory_mode_does_not_require_redis_at_startup() {
    let (output, logs) = check_startup("memory", UNAVAILABLE_REDIS).await;
    assert_eq!(output.status.code(), Some(0), "{logs}");
    assert!(!logs.contains("write-through flusher online"), "{logs}");
}
