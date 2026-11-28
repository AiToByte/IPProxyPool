//! FreePool 全部单测（原 `free_pool.rs` 后半段，逐字搬迁）。
//!
//! `use super::*;` 可见父模块的全部条目（含 `pub use` 再导出的子模块条目），
//! 故 `Registry`/`Health`/`Verifier` 等无需逐个 import。

use super::registry::{baseline_bases, cap_intake, intake_cap_limit};
use super::sources::fetch_all;
use super::verify::CHECKER_CLIENT_CAP;
use super::worker::apply_geo_verdict;
use super::*;

#[test]
fn api_source_prefilters_by_anonymity() {
    // NEXT-B1：Geonode `anonymityLevel` 预筛——transparent 声明丢弃（质检预算集中到
    // elite/anonymous；历史 pass 全为 Elite，见步骤 18/19/演示）；字段缺失 fail-open。
    let body = r#"{"data":[
            {"ip":"8.8.8.7","port":"8080","protocols":["http"],"country":"US","anonymityLevel":"elite"},
            {"ip":"1.1.1.9","port":3128,"protocols":["https"],"country":"DE","anonymityLevel":"anonymous"},
            {"ip":"9.9.9.1","port":"8080","protocols":["http"],"country":"US","anonymityLevel":"transparent"},
            {"ip":"9.9.9.2","port":"8080","protocols":["http"],"country":"US","anonymityLevel":"TRANSPARENT"},
            {"ip":"9.9.9.3","port":"8080","protocols":["http"],"country":"US"},
            {"ip":"9.9.9.4","port":"8080","protocols":["socks5"],"country":"US","anonymityLevel":"elite"}
        ]}"#;
    let nodes = ApiSource::parse("geonode", body);
    let ips: Vec<&str> = nodes.iter().map(|n| n.ip.as_str()).collect();
    assert_eq!(ips, vec!["8.8.8.7", "1.1.1.9", "9.9.9.3", "9.9.9.4"]);
}

#[tokio::test]
async fn api_source_fetches_multiple_pages() {
    // NEXT-B2：多页并发合并（page1/2 各 1 节点）；`page` 参数确经 URL 传递
    // （stub 按查询串回不同体）；单页 URL 形态（无 page 参数）补 page=1。
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind");
    let port = listener.local_addr().expect("addr").port();
    tokio::spawn(async move {
        for _ in 0..2 {
            let (mut s, _) = listener.accept().await.expect("accept");
            let mut buf = [0u8; 2048];
            let n = s.read(&mut buf).await.unwrap_or(0);
            let req = String::from_utf8_lossy(&buf[..n]).to_string();
            let page = req
                .split_whitespace()
                .nth(1)
                .unwrap_or("")
                .split("page=")
                .nth(1)
                .unwrap_or("?")
                .chars()
                .take_while(|c| c.is_ascii_digit())
                .collect::<String>();
            let body = match page.as_str() {
                "2" => {
                    r#"{"data":[{"ip":"1.1.1.9","port":3128,"protocols":["https"],"country":"DE"}]}"#
                }
                _ => {
                    r#"{"data":[{"ip":"8.8.8.7","port":"8080","protocols":["http"],"country":"US"}]}"#
                }
            };
            let _ = s
                .write_all(
                    format!(
                        "HTTP/1.1 200 OK\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                        body.len()
                    )
                    .as_bytes(),
                )
                .await;
        }
    });
    let client = reqwest::Client::new();
    let src = ApiSource::new("api", format!("http://127.0.0.1:{port}/list?limit=1")).with_pages(2);
    assert_eq!(src.pages, 2);
    // 缺 page 参数的 URL 补 page=2（含 limit 保留）。
    assert!(src.page_url(2).contains("page=2") && src.page_url(2).contains("limit=1"));
    let outcome = src.fetch(&client).await.expect("fetch pages");
    assert!(!outcome.not_modified);
    let mut ips: Vec<String> = outcome.nodes.iter().map(|n| n.ip.clone()).collect();
    ips.sort();
    assert_eq!(ips, vec!["1.1.1.9".to_string(), "8.8.8.7".to_string()]);
}

#[tokio::test]
async fn b10_first_page_uses_etag_second_fetch_304() {
    // OPT-R4 B10：首页走 conditional_get——首轮存 ETag，次轮首页 304，
    // 首页无产出但不计失败，次页照常合并（整体非 not_modified）。
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind");
    let port = listener.local_addr().expect("addr").port();
    tokio::spawn(async move {
        // 首轮 page1＋page2；次轮 page1（带 If-None-Match）＋page2。
        for _ in 0..4 {
            let (mut s, _) = listener.accept().await.expect("accept");
            let mut buf = [0u8; 2048];
            let n = s.read(&mut buf).await.unwrap_or(0);
            let req = String::from_utf8_lossy(&buf[..n]).to_string();
            let page = req
                .split_whitespace()
                .nth(1)
                .unwrap_or("")
                .split("page=")
                .nth(1)
                .unwrap_or("?")
                .chars()
                .take_while(|c| c.is_ascii_digit())
                .collect::<String>();
            let has_etag = req.to_ascii_lowercase().contains("if-none-match");
            let resp = if page == "1" && has_etag {
                "HTTP/1.1 304 Not Modified\r\nconnection: close\r\n\r\n".to_string()
            } else {
                let body = if page == "2" {
                    r#"{"data":[{"ip":"1.1.1.9","port":3128,"protocols":["https"],"country":"DE"}]}"#
                } else {
                    r#"{"data":[{"ip":"8.8.8.7","port":"8080","protocols":["http"],"country":"US"}]}"#
                };
                format!(
                        "HTTP/1.1 200 OK\r\netag: \"v1\"\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                        body.len()
                    )
            };
            let _ = s.write_all(resp.as_bytes()).await;
        }
    });
    let client = reqwest::Client::new();
    let src = ApiSource::new("api", format!("http://127.0.0.1:{port}/list?limit=1")).with_pages(2);
    let first = src.fetch(&client).await.expect("first fetch");
    assert!(!first.not_modified);
    assert_eq!(first.nodes.len(), 2);
    let second = src.fetch(&client).await.expect("second fetch");
    // 首页 304 无产出，次页照常：整体 Ok 且非 not_modified，只剩次页节点。
    assert!(!second.not_modified);
    assert_eq!(second.nodes.len(), 1);
    assert_eq!(second.nodes[0].ip, "1.1.1.9");
}

#[test]
fn api_source_parses_geonode_shape() {
    let body = r#"{"data":[
            {"ip":"8.8.8.7","port":"8080","protocols":["http"],"country":"US"},
            {"ip":"1.1.1.9","port":3128,"protocols":["https"],"country":"DE"},
            {"ip":"bad","port":"x","protocols":["http"],"country":"US"},
            {"ip":"9.9.9.1","port":"1080","protocols":["socks5"],"country":"US"}
        ]}"#;
    let nodes = ApiSource::parse("geonode", body);
    // 坏行丢弃；socks5 解析保留（Phase 1 由 Verifier 跳过，不在此处丢）。
    assert_eq!(nodes.len(), 3);
    assert_eq!(nodes[0].port, 8080);
    assert_eq!(nodes[0].proto, FreeProto::Http);
    assert_eq!(nodes[1].proto, FreeProto::Https);
    assert_eq!(nodes[2].proto, FreeProto::Socks5);
    assert_eq!(nodes[0].country.as_deref(), Some("US"));
}

#[test]
fn html_source_extracts_ip_ports() {
    let html = r#"<table><tr><td>8.8.8.7</td><td>8080</td><td>yes</td></tr>
            <tr><td>999.1.1.1</td><td>80</td></tr>
            <tr><td>1.1.1.9:3128</td></tr>
            <tr><td>10.0.0.1</td><td>70000</td></tr></table>"#;
    let nodes = HtmlSource::extract("fpl", html, FreeProto::Http);
    // 非法 octet/超范围端口丢弃；`ip:port` 紧凑形态同样识别。
    assert_eq!(nodes.len(), 2);
    assert_eq!((nodes[0].ip.as_str(), nodes[0].port), ("8.8.8.7", 8080));
    assert_eq!((nodes[1].ip.as_str(), nodes[1].port), ("1.1.1.9", 3128));
    assert!(nodes
        .iter()
        .all(|n| n.proto == FreeProto::Http && n.source == "fpl"));
}

#[test]
fn github_source_parses_line_list() {
    let body = "8.8.8.7:8080\n\n# comment\n1.1.1.9:3128 socks5\nbad-line\n10.0.0.1:0\n";
    let nodes = GitHubSource::parse_with("gh", body, FreeProto::Http);
    assert_eq!(nodes.len(), 2);
    assert_eq!(nodes[0].proto, FreeProto::Http);
    assert_eq!(nodes[1].proto, FreeProto::Socks5);
}

#[test]
fn github_parse_honors_default_proto_and_rejects_junk() {
    // NEXT-B3：openproxylist 形态（裸 `ip:port` 无行内标记）按源默认协议；
    // 非法 octet/全零地址丢弃（openproxylist 首行 `0.0.0.0:80` 实锤）。
    let body = "1.12.55.136:2080\n0.0.0.0:80\n999.1.1.1:80\n10.0.0.300:8080\n";
    let nodes = GitHubSource::parse_with("opl", body, FreeProto::Socks5);
    assert_eq!(nodes.len(), 1);
    assert_eq!(nodes[0].ip, "1.12.55.136");
    assert_eq!(nodes[0].proto, FreeProto::Socks5);
    // 行内标记优先于默认值。
    let mixed = GitHubSource::parse_with("opl", "9.9.9.9:1080 socks4\n", FreeProto::Socks5);
    assert_eq!(mixed[0].proto, FreeProto::Socks4);
}

#[test]
fn uri_source_accepts_strict_uri_feeds() {
    let body = "http://8.8.8.8:8080\nsocks4://1.1.1.1:1080\nsocks5://9.9.9.9:1080\n8.8.4.4:3128\n";
    let nodes = UriSource::parse("uri", body);
    assert_eq!(nodes.len(), 3);
    assert_eq!(
        nodes.iter().map(|n| n.proto).collect::<Vec<_>>(),
        vec![FreeProto::Http, FreeProto::Socks4, FreeProto::Socks5]
    );
}

#[tokio::test]
async fn uri_source_fetches_and_honors_etag_not_modified() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(async move {
        for round in 0..2 {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut request = [0; 2048];
            let size = socket.read(&mut request).await.unwrap_or(0);
            let request = String::from_utf8_lossy(&request[..size]);
            let response = if round == 0 {
                b"HTTP/1.1 200 OK\r\nETag: v1\r\nContent-Length: 20\r\nConnection: close\r\n\r\nhttp://8.8.8.8:8080\n".as_slice()
            } else {
                assert!(request.to_ascii_lowercase().contains("if-none-match: v1"));
                b"HTTP/1.1 304 Not Modified\r\nConnection: close\r\n\r\n".as_slice()
            };
            socket.write_all(response).await.unwrap();
        }
    });
    let source = UriSource::new("uri0", format!("http://127.0.0.1:{port}/list.txt"));
    let client = reqwest::Client::new();
    let first = source.fetch(&client).await.unwrap();
    assert_eq!(first.nodes.len(), 1);
    assert!(!first.not_modified);
    let second = source.fetch(&client).await.unwrap();
    assert!(second.not_modified);
}

#[test]
fn uri_source_accepts_scheme_case_insensitively() {
    let nodes = UriSource::parse("uri", "HTTP://8.8.8.8:80\nSoCkS5://1.1.1.1:1080\n");
    assert_eq!(nodes.len(), 2);
    assert_eq!(nodes[0].proto, FreeProto::Http);
    assert_eq!(nodes[1].proto, FreeProto::Socks5);
}

#[test]
fn uri_source_rejects_special_use_ipv4_ranges() {
    for ip in [
        "192.0.2.1",
        "198.51.100.1",
        "203.0.113.1",
        "100.64.0.1",
        "198.18.0.1",
        "192.0.0.1",
    ] {
        let line = format!("http://{ip}:8080");
        assert!(UriSource::parse("uri", &line).is_empty(), "accepted {ip}");
    }
}

#[test]
fn intake_cap_round_robins_sources_before_refilling_earlier_sources() {
    let raws = (0..4)
        .map(|i| raw(&format!("8.8.8.{i}"), "early"))
        .chain((0..4).map(|i| raw(&format!("1.1.1.{i}"), "late")))
        .collect();
    let admitted = cap_intake(raws, 4, &HashSet::new());
    let sources: Vec<_> = admitted.iter().map(|node| node.source.as_str()).collect();
    assert_eq!(sources, vec!["early", "late", "early", "late"]);
}

#[tokio::test]
async fn paginated_api_rejects_oversized_and_error_responses() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    for response in [
        format!(
            "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
            super::MAX_SOURCE_BODY_BYTES + 1
        ),
        "HTTP/1.1 503 Service Unavailable\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
            .to_string(),
    ] {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let response = response.into_bytes();
        tokio::spawn(async move {
            for _ in 0..2 {
                let (mut socket, _) = listener.accept().await.unwrap();
                let mut request = [0; 2048];
                let _ = socket.read(&mut request).await;
                socket.write_all(&response).await.unwrap();
            }
        });
        let source =
            ApiSource::new("pages", format!("http://127.0.0.1:{port}/?limit=1")).with_pages(2);
        assert!(source.fetch(&reqwest::Client::new()).await.is_err());
    }
}

#[tokio::test]
async fn raw_source_rejects_oversized_and_error_responses() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    for response in [
        format!(
            "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
            super::MAX_SOURCE_BODY_BYTES + 1
        ),
        "HTTP/1.1 503 Service Unavailable\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
            .to_string(),
    ] {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let response = response.into_bytes();
        tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut request = [0; 2048];
            let _ = socket.read(&mut request).await;
            socket.write_all(&response).await.unwrap();
        });
        let source = GitHubSource::new(
            "raw",
            format!("http://127.0.0.1:{port}/list"),
            FreeProto::Http,
        );
        assert!(source.fetch(&reqwest::Client::new()).await.is_err());
    }
}

#[test]
fn uri_source_rejects_credentials_paths_and_invalid_hosts() {
    let body = "http://user:pass@8.8.8.8:80\nhttp://8.8.8.8:80/\nhttp://8.8.8.8:80?x=1\nhttp://8.8.8.8:80#x\nhttp://example.com:80\nhttps://8.8.8.8:443\nsocks5://127.0.0.1:1080\nhttp://8.8.8.8:0\n";
    assert!(UriSource::parse("uri", body).is_empty());
}

#[test]
fn b12_monosans_fixture_parses() {
    // OPT-R4 B12：monosans 实测首行形态（`ip:port` 无标记，靠默认协议）。
    let http = GitHubSource::parse_with(
        "mono",
        "45.151.102.248:10808\n5.129.254.5:8888\n",
        FreeProto::Http,
    );
    assert_eq!(http.len(), 2);
    assert_eq!(http[0].ip, "45.151.102.248");
    assert_eq!(http[0].port, 10808);
    assert!(http.iter().all(|n| n.proto == FreeProto::Http));
    let socks = GitHubSource::parse_with(
        "mono",
        "213.33.186.254:1080\n185.47.55.110:1080\n",
        FreeProto::Socks5,
    );
    assert_eq!(socks.len(), 2);
    assert!(socks.iter().all(|n| n.proto == FreeProto::Socks5));
}

#[test]
fn e3_relayglass_fixture_parses() {
    // OPT-R5 E3：relayglass 双路实测首行（2026-09-25：py200＋curl200；
    // http 152 行／socks5 114 行，裸 `ip:port` 无标记，靠默认协议）。
    let http = GitHubSource::parse_with(
        "relay",
        "152.53.183.107:8081\n213.157.6.50:80\n",
        FreeProto::Http,
    );
    assert_eq!(http.len(), 2);
    assert_eq!(http[0].ip, "152.53.183.107");
    assert_eq!(http[0].port, 8081);
    assert!(http.iter().all(|n| n.proto == FreeProto::Http));
    let socks = GitHubSource::parse_with(
        "relay",
        "169.58.97.115:1080\n185.87.255.54:1080\n",
        FreeProto::Socks5,
    );
    assert_eq!(socks.len(), 2);
    assert_eq!(socks[0].ip, "169.58.97.115");
    assert_eq!(socks[0].port, 1080);
    assert!(socks.iter().all(|n| n.proto == FreeProto::Socks5));
}

#[tokio::test]
async fn github_source_fetches_from_local_server() {
    // 零外部依赖：本地临时 HTTP 服务冒充 raw 仓（沿用 metrics 单测手法换端口）。
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind");
    let port = listener.local_addr().expect("addr").port();
    tokio::spawn(async move {
        let (mut s, _) = listener.accept().await.expect("accept");
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let mut buf = [0u8; 1024];
        let _ = s.read(&mut buf).await;
        let body = "8.8.8.7:8080\n";
        let _ = s
            .write_all(
                format!(
                    "HTTP/1.1 200 OK\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                    body.len()
                )
                .as_bytes(),
            )
            .await;
    });
    let client = reqwest::Client::new();
    let src = GitHubSource::new(
        "gh",
        format!("http://127.0.0.1:{port}/list.txt"),
        FreeProto::Http,
    );
    let outcome = src.fetch(&client).await.expect("fetch");
    assert!(!outcome.not_modified);
    assert_eq!(outcome.nodes.len(), 1);
    assert_eq!(outcome.nodes[0].ip, "8.8.8.7");
}

#[tokio::test]
async fn github_source_etag_not_modified() {
    // ETag 礼貌轮询：首轮存 ETag；次轮带 If-None-Match，被 304 后 not_modified=true
    // 且调用方（SourceGuard）不得计零产出（见 Task 10 单测联动）。
    use std::sync::{Arc, Mutex as StdMutex};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let seen_if_none_match: Arc<StdMutex<Vec<String>>> = Arc::new(StdMutex::new(Vec::new()));
    let seen = seen_if_none_match.clone();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind");
    let port = listener.local_addr().expect("addr").port();
    tokio::spawn(async move {
        for _ in 0..2 {
            let (mut s, _) = listener.accept().await.expect("accept");
            let mut buf = [0u8; 2048];
            let n = s.read(&mut buf).await.unwrap_or(0);
            let req = String::from_utf8_lossy(&buf[..n]).to_string();
            let inm = req
                .lines()
                .find(|l| l.to_ascii_lowercase().starts_with("if-none-match:"))
                .map(|l| {
                    l.split_once(':')
                        .map(|(_, v)| v.trim().to_string())
                        .unwrap_or_default()
                })
                .unwrap_or_default();
            seen.lock().expect("lock").push(inm.clone());
            if inm == "\"v1\"" {
                let _ = s
                    .write_all(b"HTTP/1.1 304 Not Modified\r\nconnection: close\r\n\r\n")
                    .await;
            } else {
                let body = "8.8.8.7:8080\n";
                let _ = s
                        .write_all(
                            format!(
                                "HTTP/1.1 200 OK\r\nETag: \"v1\"\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                                body.len()
                            )
                            .as_bytes(),
                        )
                        .await;
            }
        }
    });
    let client = reqwest::Client::new();
    let src = GitHubSource::new(
        "gh",
        format!("http://127.0.0.1:{port}/list.txt"),
        FreeProto::Http,
    );
    let first = src.fetch(&client).await.expect("first");
    assert_eq!(first.nodes.len(), 1);
    assert!(!first.not_modified);
    let second = src.fetch(&client).await.expect("second");
    assert!(second.not_modified);
    assert!(second.nodes.is_empty());
    let seen = seen_if_none_match.lock().expect("lock");
    assert_eq!(seen.len(), 2);
    assert!(
        seen[1].contains("v1"),
        "second request must carry If-None-Match, got {seen:?}"
    );
}

/// 通用 ETag stub：首轮 200＋ETag＋给定 body；次轮见 If-None-Match 含 v1 即 304。
/// 返回（端口，收到的 If-None-Match 记录）。R3-5 供 Api/Html 礼貌轮询断言。
async fn spawn_etag_stub(
    body_200: &'static str,
) -> (u16, std::sync::Arc<std::sync::Mutex<Vec<String>>>) {
    use std::sync::{Arc, Mutex as StdMutex};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let seen: Arc<StdMutex<Vec<String>>> = Arc::new(StdMutex::new(Vec::new()));
    let seen2 = seen.clone();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind");
    let port = listener.local_addr().expect("addr").port();
    tokio::spawn(async move {
        for _ in 0..2 {
            let (mut s, _) = listener.accept().await.expect("accept");
            let mut buf = [0u8; 4096];
            let n = s.read(&mut buf).await.unwrap_or(0);
            let req = String::from_utf8_lossy(&buf[..n]).to_string();
            let inm = req
                .lines()
                .find(|l| l.to_ascii_lowercase().starts_with("if-none-match:"))
                .map(|l| {
                    l.split_once(':')
                        .map(|(_, v)| v.trim().to_string())
                        .unwrap_or_default()
                })
                .unwrap_or_default();
            seen2.lock().expect("lock").push(inm.clone());
            if inm.contains("v1") {
                let _ = s
                    .write_all(b"HTTP/1.1 304 Not Modified\r\nconnection: close\r\n\r\n")
                    .await;
            } else {
                let _ = s
                        .write_all(
                            format!(
                                "HTTP/1.1 200 OK\r\nETag: \"v1\"\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body_200}",
                                body_200.len()
                            )
                            .as_bytes(),
                        )
                        .await;
            }
        }
    });
    (port, seen)
}

#[tokio::test]
async fn api_source_etag_not_modified() {
    // R3-5：ApiSource 礼貌轮询（对齐 GitHub）：首轮存 ETag，次轮 304→not_modified。
    let body = r#"{"data":[{"ip":"8.8.8.7","port":"8080","protocols":["http"],"country":"US"}]}"#;
    let (port, seen) = spawn_etag_stub(body).await;
    let client = reqwest::Client::new();
    let src = ApiSource::new("api", format!("http://127.0.0.1:{port}/api"));
    let first = src.fetch(&client).await.expect("first");
    assert_eq!(first.nodes.len(), 1);
    assert!(!first.not_modified);
    let second = src.fetch(&client).await.expect("second");
    assert!(second.not_modified);
    assert!(second.nodes.is_empty());
    let seen = seen.lock().expect("lock");
    assert_eq!(seen.len(), 2);
    assert!(seen[1].contains("v1"));
}

#[tokio::test]
async fn html_source_etag_not_modified() {
    // R3-5：HtmlSource 礼貌轮询同理。
    let body = "<table><tr><td>8.8.8.7</td><td>8080</td></tr></table>";
    let (port, seen) = spawn_etag_stub(body).await;
    let client = reqwest::Client::new();
    let src = HtmlSource::new(
        "html",
        format!("http://127.0.0.1:{port}/list"),
        FreeProto::Http,
    );
    let first = src.fetch(&client).await.expect("first");
    assert_eq!(first.nodes.len(), 1);
    assert!(!first.not_modified);
    let second = src.fetch(&client).await.expect("second");
    assert!(second.not_modified);
    assert!(second.nodes.is_empty());
    let seen = seen.lock().expect("lock");
    assert_eq!(seen.len(), 2);
    assert!(seen[1].contains("v1"));
}

#[tokio::test]
async fn verifier_admits_fast_listener() {
    // 本地 listener 必连上（OPT-6 手法），延迟门内放行。
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind");
    let port = listener.local_addr().expect("addr").port();
    let v = Verifier::new(Duration::from_secs(3), 3000);
    let raw = RawNode {
        ip: "127.0.0.1".to_string(),
        port,
        proto: FreeProto::Http,
        country: None,
        source: "t".to_string(),
    };
    let ok = v.verify(&raw).await;
    assert!(ok.is_some());
    assert!(ok.expect("latency") < 3000);
    drop(listener);
}

#[tokio::test]
async fn verifier_rejects_refused_and_slow() {
    // 拒连端口（127.0.0.1:1）必失败；socks 协议 Phase 1 跳过（None）。
    let v = Verifier::new(Duration::from_secs(3), 3000);
    let refused = RawNode {
        ip: "127.0.0.1".to_string(),
        port: 1,
        proto: FreeProto::Http,
        country: None,
        source: "t".to_string(),
    };
    assert!(v.verify(&refused).await.is_none());
    let socks = RawNode {
        ip: "127.0.0.1".to_string(),
        port: 1,
        proto: FreeProto::Socks5,
        country: None,
        source: "t".to_string(),
    };
    assert!(v.verify(&socks).await.is_none());
}

#[test]
fn anonymity_classification_matrix() {
    // 三级矩阵（openproxyhub 定义）：出口==基线→Transparent；否则 7 头有披露→Anonymous；无→Elite。
    use std::collections::HashMap;
    let empty: HashMap<String, String> = HashMap::new();
    assert_eq!(
        classify_anonymity("1.1.1.1", Some("1.1.1.1"), &empty),
        AnonLevel::Transparent
    );
    assert_eq!(
        classify_anonymity("1.1.1.1", Some("9.9.9.9"), &empty),
        AnonLevel::Elite
    );
    let mut disclosed = HashMap::new();
    disclosed.insert("via".to_string(), "1.0 proxy".to_string());
    // OPT-R4 B2：无害 Via（值不含基线）不再误杀 Elite（旧口径判 Anonymous）。
    assert_eq!(
        classify_anonymity("1.1.1.1", Some("9.9.9.9"), &disclosed),
        AnonLevel::Elite
    );
    let mut leaked = HashMap::new();
    leaked.insert("via".to_string(), "1.0 1.1.1.1".to_string());
    assert_eq!(
        classify_anonymity("1.1.1.1", Some("9.9.9.9"), &leaked),
        AnonLevel::Anonymous
    );
    let mut upper = HashMap::new();
    upper.insert("X-Forwarded-For".to_string(), "1.1.1.1".to_string());
    assert_eq!(
        classify_anonymity("1.1.1.1", Some("9.9.9.9"), &upper),
        AnonLevel::Anonymous
    );
    // 出口未知（代理失败）→Unknown，调用方按失败计。
    assert_eq!(
        classify_anonymity("1.1.1.1", None, &empty),
        AnonLevel::Unknown
    );
}

#[test]
fn b1_bogon_ips_never_routable() {
    // OPT-R4 B1：私网/回环/链路本地/组播/未指定/畸形一律不可公网回连。
    for bad in [
        "10.0.0.1",
        "10.255.255.255",
        "172.16.0.1",
        "172.31.255.255",
        "192.168.1.1",
        "127.0.0.1",
        "0.0.0.0",
        "169.254.10.20",
        "224.0.0.1",
        "240.0.0.1",
        "",
        "not-an-ip",
        "1.2.3",
        "1.2.3.4.5",
        "1.2.3.999",
        "::1",
        "example.com",
    ] {
        assert!(!is_routable_ip(bad), "{bad} must not routable");
    }
    for good in ["8.8.8.8", "1.1.1.1", "9.9.9.9", "172.15.0.1", "172.32.0.1"] {
        assert!(is_routable_ip(good), "{good} must routable");
    }
    // 边界：172.15/172.32 不是私网；x.y.z.0 主机位零放行。
    assert!(is_routable_ip("172.15.0.1"));
    assert!(is_routable_ip("172.32.0.1"));
    assert!(!is_routable_ip("192.0.2.1"));
    assert!(!is_routable_ip("198.51.100.1"));
    assert!(!is_routable_ip("203.0.113.7"));
    assert!(!is_routable_ip("100.64.0.1"));
    assert!(!is_routable_ip("198.18.0.1"));
}

#[test]
fn b1_parsers_drop_bogon_pre_tcp() {
    // OPT-R4 B1：三源 parse 均丢弃 bogon，不进候选。
    let api_body = r#"{"data":[
            {"ip":"8.8.8.8","port":"8080","protocols":["http"],"country":"US"},
            {"ip":"10.0.0.1","port":"8080","protocols":["http"],"country":"US"},
            {"ip":"192.168.1.1","port":3128,"protocols":["http"],"country":"US"}
        ]}"#;
    let nodes = ApiSource::parse("api-test", api_body);
    assert_eq!(nodes.len(), 1);
    assert_eq!(nodes[0].ip, "8.8.8.8");
    let raw_body = "8.8.8.8:8080\n10.0.0.5:3128 socks5\n127.0.0.1:8888\n";
    let raws = GitHubSource::parse_with("gh-test", raw_body, FreeProto::Http);
    assert_eq!(raws.len(), 1);
    assert_eq!(raws[0].ip, "8.8.8.8");
    let html = "<td>8.8.4.4</td><td>8080</td><td>10.1.2.3</td><td>3128</td>";
    let hrefs = HtmlSource::extract("html-test", html, FreeProto::Http);
    assert!(hrefs.iter().all(|r| r.ip == "8.8.4.4"));
    assert!(!hrefs.is_empty());
}

#[tokio::test]
async fn checker_rejects_refused_proxy() {
    // 拒连代理（127.0.0.1:1）→ None（失败），不 panic。
    let c = FullChecker::new("http://127.0.0.1:1/".to_string(), Duration::from_secs(3));
    let raw = RawNode {
        ip: "127.0.0.1".to_string(),
        port: 1,
        proto: FreeProto::Http,
        country: None,
        source: "t".to_string(),
    };
    assert!(c.check(&raw, "9.9.9.9").await.is_none());
}

/// 本地微型 HTTP 代理 stub（单测内联 ~40 行）：读绝对 URI 首行，按 path
/// 返回固定 JSON（`/ip` 出口／`/headers` 回显空头／`/anything` canary）。
/// `tamper=true` 时 canary 的 url 缺 marker（模拟内容篡改）。
async fn spawn_stub_proxy(tamper: bool) -> u16 {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind");
    let port = listener.local_addr().expect("addr").port();
    tokio::spawn(async move {
        // 一次 check 发 3 请求（server 回 connection:close，故 3 连接）。
        for _ in 0..3 {
            let (mut s, _) = match listener.accept().await {
                Ok(v) => v,
                Err(_) => return,
            };
            let mut buf = [0u8; 4096];
            let n = s.read(&mut buf).await.unwrap_or(0);
            let req = String::from_utf8_lossy(&buf[..n]).to_string();
            let path = req
                .lines()
                .next()
                .unwrap_or("")
                .split_whitespace()
                .nth(1)
                .unwrap_or("/")
                .to_string();
            // 绝对 URI（经代理）与源形态（直连）双兼容：截 path。
            let path = path
                .split_once("://")
                .map(|(_, rest)| rest.find('/').map(|i| &rest[i..]).unwrap_or("/"))
                .unwrap_or(&path)
                .to_string();
            let body = if path.starts_with("/ip") {
                r#"{"origin":"10.9.9.9"}"#.to_string()
            } else if path.starts_with("/headers") {
                r#"{"headers":{}}"#.to_string()
            } else if path.contains(FULL_CHECK_MARKER) {
                if tamper {
                    r#"{"url":"tampered"}"#.to_string()
                } else {
                    // 显式位置参数（不用内联作用域捕获：raw 字符串大括号＋捕获式混写
                    // 在部分 rust-analyzer 版本误报，见硬化记录）。
                    format!(r#"{{"url":"http://x/anything/{}"}}"#, FULL_CHECK_MARKER)
                }
            } else {
                r#"{}"#.to_string()
            };
            let _ = s
                    .write_all(
                        format!(
                            "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                            body.len()
                        )
                        .as_bytes(),
                    )
                    .await;
        }
    });
    port
}

#[tokio::test]
async fn checker_detects_tampered_canary() {
    // 篡改 stub（canary url 缺 marker）→ check 判失败（None）。
    let stub = spawn_stub_proxy(true).await;
    let c = FullChecker::new(format!("http://127.0.0.1:{stub}"), Duration::from_secs(3));
    let raw = RawNode {
        ip: "127.0.0.1".to_string(),
        port: stub,
        proto: FreeProto::Http,
        country: None,
        source: "t".to_string(),
    };
    assert!(c.check(&raw, "1.2.3.4").await.is_none());
}

#[test]
fn checker_reuses_client_per_proxy() {
    // NEXT-B4：同代理 URL 复用 Client（逐节点重建浪费连接池；沿 prober OPT-5）。
    let c = FullChecker::new("http://127.0.0.1:1".to_string(), Duration::from_secs(3));
    assert!(c.client_for("http://127.0.0.1:9").is_some());
    assert!(c.client_for("http://127.0.0.1:9").is_some());
    assert_eq!(c.client_count(), 1);
    assert!(c.client_for("http://127.0.0.1:10").is_some());
    assert_eq!(c.client_count(), 2);
}

#[tokio::test]
async fn checker_falls_back_to_second_base() {
    // NEXT-B4：首基址拒连→次基址成功即 pass（单基址抖动不再判整节点失败）。
    let good = spawn_stub_proxy(false).await;
    let c = FullChecker::new("http://127.0.0.1:1".to_string(), Duration::from_secs(3))
        .with_fallback_bases(vec![format!("http://127.0.0.1:{good}")]);
    let raw = RawNode {
        ip: "127.0.0.1".to_string(),
        port: good,
        proto: FreeProto::Http,
        country: None,
        source: "t".to_string(),
    };
    let res = c.check(&raw, "1.2.3.4").await.expect("fallback pass");
    assert_eq!(res.anon, AnonLevel::Elite);
}

#[tokio::test]
async fn checker_records_forward_latency() {
    // 正常 stub：出口 10.9.9.9 ≠ 基线 → Elite；转发延迟 < 3000ms。
    let stub = spawn_stub_proxy(false).await;
    let c = FullChecker::new(format!("http://127.0.0.1:{stub}"), Duration::from_secs(3));
    let raw = RawNode {
        ip: "127.0.0.1".to_string(),
        port: stub,
        proto: FreeProto::Http,
        country: None,
        source: "t".to_string(),
    };
    let res = c.check(&raw, "1.2.3.4").await.expect("pass");
    assert_eq!(res.anon, AnonLevel::Elite);
    assert_eq!(res.exit_ip.as_deref(), Some("10.9.9.9"));
    assert!(res.fwd_latency_ms < 3000);
}

fn raw(ip: &str, source: &str) -> RawNode {
    RawNode {
        ip: ip.to_string(),
        port: 8080,
        proto: FreeProto::Http,
        country: Some("US".to_string()),
        source: source.to_string(),
    }
}

#[test]
fn registry_ttl_expiry() {
    // TTL 到即快照不可见（可测版本注入未来时间，沿用 sweep_expired_at 手法）。
    let mut reg = Registry::new(Duration::from_secs(1800));
    reg.upsert_full(
        &raw("10.0.0.1", "a"),
        50,
        AnonLevel::Elite,
        None,
        Instant::now(),
    );
    assert_eq!(reg.snapshot(Instant::now(), false).len(), 1);
    assert!(reg
        .snapshot(Instant::now() + Duration::from_secs(1801), false)
        .is_empty());
}

#[test]
fn registry_stores_exit_ip() {
    // R3-2：upsert 的 exit_ip 落到快照节点（遥测 out_ip 用）；刷新即更新（exit 轮转不保留旧值）。
    let mut reg = Registry::new(Duration::from_secs(1800));
    let now = Instant::now();
    reg.upsert_full(
        &raw("10.0.0.1", "a"),
        50,
        AnonLevel::Elite,
        Some("10.9.9.9".to_string()),
        now,
    );
    let snap = reg.snapshot(now, false);
    assert_eq!(snap[0].exit_ip.as_deref(), Some("10.9.9.9"));
    reg.upsert_full(
        &raw("10.0.0.1", "a"),
        60,
        AnonLevel::Elite,
        Some("10.9.9.10".to_string()),
        now,
    );
    let snap2 = reg.snapshot(now, false);
    assert_eq!(snap2.len(), 1);
    assert_eq!(snap2[0].exit_ip.as_deref(), Some("10.9.9.10"));
}

#[test]
fn enforce_maps_mismatch_to_failure() {
    // P4-1：enforce 开＋Mismatch→backoff（快照不可见但条目保留，沿 note_verify_failed 语义，
    // TTL 内自动恢复）；enforce 关＋Mismatch→仍在池（只记指标，Phase 3 行为）。
    use crate::geo::GeoVerdict;
    use crate::metrics::MetricsRegistry;
    let now = Instant::now();
    let m = MetricsRegistry::new();
    let mut reg = Registry::new(Duration::from_secs(1800));
    reg.upsert_full(
        &raw("10.0.0.1", "a"),
        50,
        AnonLevel::Elite,
        Some("10.9.9.9".to_string()),
        now,
    );
    apply_geo_verdict(&mut reg, &m, "10.0.0.1:8080", GeoVerdict::Mismatch, true);
    assert!(reg.snapshot(now, false).is_empty());
    assert_eq!(reg.len(), 1);
    assert!(m
        .render()
        .contains("free_pool_verify_total{result=\"geo_fail\"} 1"));
    let mut reg2 = Registry::new(Duration::from_secs(1800));
    reg2.upsert_full(
        &raw("10.0.0.2", "a"),
        50,
        AnonLevel::Elite,
        Some("10.9.9.9".to_string()),
        now,
    );
    apply_geo_verdict(&mut reg2, &m, "10.0.0.2:8080", GeoVerdict::Mismatch, false);
    assert_eq!(reg2.snapshot(now, false).len(), 1);
}

#[test]
fn registry_reverify_renews() {
    // 复检：失败记 backoff（TTL 内保留），成功路径由 upsert_full 续期。
    let mut reg = Registry::new(Duration::from_secs(1800));
    let now = Instant::now();
    reg.upsert_full(&raw("10.0.0.1", "a"), 50, AnonLevel::Elite, None, now);
    // TTL 1800s：2000s 处已过期不可见；1000s 处（TTL 内）可见。
    assert_eq!(
        reg.snapshot(now + Duration::from_secs(2000), false).len(),
        0
    );
    // 新鲜条目在 TTL 内可见。
    assert_eq!(
        reg.snapshot(now + Duration::from_secs(1000), false).len(),
        1
    );
    // 失败→backoff：快照不可见但条目保留（len 不变）。
    reg.reverify("10.0.0.1:8080", false, now + Duration::from_secs(1000));
    assert!(reg
        .snapshot(now + Duration::from_secs(1000), false)
        .is_empty());
    assert_eq!(reg.len(), 1);
}

#[test]
fn registry_dedupes_across_sources() {
    // 多源同 addr 只留首见 source；快照转 ProxyNode（provider free-{source}/tier free/weight/country 缺省 ZZ）。
    let mut reg = Registry::new(Duration::from_secs(1800));
    let now = Instant::now();
    reg.upsert_full(&raw("10.0.0.1", "a"), 50, AnonLevel::Elite, None, now);
    reg.upsert_full(&raw("10.0.0.1", "b"), 60, AnonLevel::Elite, None, now);
    let snap = reg.snapshot(now, false);
    assert_eq!(snap.len(), 1);
    assert_eq!(snap[0].provider, "free-a");
    assert_eq!(snap[0].tier, "free");
    assert_eq!(snap[0].addr, "10.0.0.1:8080");
    let mut no_country = raw("10.0.0.2", "a");
    no_country.country = None;
    reg.upsert_full(&no_country, 50, AnonLevel::Elite, None, now);
    // 快照为 HashMap 迭代序（不定），按 addr 查找断言 ZZ 缺省。
    let snap = reg.snapshot(now, false);
    assert_eq!(snap.len(), 2);
    let zz = snap
        .iter()
        .find(|n| n.addr == "10.0.0.2:8080")
        .expect("zz node");
    assert_eq!(zz.country, "ZZ");
}

#[test]
fn health_score_math() {
    // 健康分＝EWMA成功率×延迟惩罚；权重 1..20 连续映射；trusted 加成封顶 20。
    let mut h = Health::fresh();
    assert!((h.score() - 0.5).abs() < 1e-9);
    // Elite＋快＋全成功→trusted→封顶 20。
    h.anon = AnonLevel::Elite;
    for _ in 0..50 {
        h.note_success(100);
    }
    assert!(h.ewma_success > 0.99);
    assert!(h.trusted());
    assert_eq!(h.weight(), 20);
    // 高延迟→惩罚生效（Unknown 非 trusted，cap 10）。
    let mut slow = Health::fresh();
    for _ in 0..50 {
        slow.note_success(9000);
    }
    assert!(slow.score() < h.score(), "latency penalty must bite");
    assert!(slow.weight() < 20);
    // 连败→保底 1（不断流，只降权）。
    let mut bad = Health::fresh();
    for _ in 0..10 {
        bad.note_failure(Instant::now());
    }
    assert_eq!(bad.weight(), 1);
}

#[test]
fn composite_rewards_streak() {
    // P3-3：composite＝score×(0.5＋0.5×min(1,streak/3))。
    // fresh（streak 0）→ 0.25（score 本体 0.5 冻结）；50 连成功→factor 1→composite==score；
    // 单次成功（streak 1）介于两者之间；连败比同 score 更低（方向不断精确值）。
    let fresh = Health::fresh();
    assert!((fresh.composite() - 0.25).abs() < 1e-9);
    let mut hot = Health::fresh();
    hot.anon = AnonLevel::Elite;
    for _ in 0..50 {
        hot.note_success(100);
    }
    assert!((hot.composite() - hot.score()).abs() < 1e-9);
    let mut warm = Health::fresh();
    warm.note_success(100);
    assert!(warm.composite() > fresh.composite());
    assert!(warm.composite() < warm.score());
    assert!(warm.composite() < hot.composite());
}

#[test]
fn backoff_and_capacity() {
    // backoff：连续失败→merge 不可见；TTL 内保留；成功恢复。
    // 容量：超量逐最低分淘汰（backoff 优先）。
    let mut reg = Registry::with_capacity(Duration::from_secs(1800), 2);
    let now = Instant::now();
    reg.upsert_full(&raw("10.0.0.1", "a"), 50, AnonLevel::Elite, None, now);
    reg.upsert_full(&raw("10.0.0.2", "a"), 50, AnonLevel::Elite, None, now);
    // 灌第三个→淘汰最低分之一（初分相同，允许淘汰任一，但总数恒 2）。
    reg.upsert_full(&raw("10.0.0.3", "a"), 50, AnonLevel::Elite, None, now);
    assert_eq!(reg.snapshot(now, false).len(), 2);
    // backoff：连 fail 3 次→快照不可见但 len 仍计入（TTL 内保留）。
    // 注：容量淘汰后 10.0.0.2 可能已被逐出；取快照现存任一节点做 backoff。
    let victim = reg.snapshot(now, false)[0].addr.clone();
    for _ in 0..3 {
        reg.note_verify_failed(&victim, now);
    }
    let snap = reg.snapshot(now, false);
    assert!(snap.iter().all(|n| n.addr != victim));
    assert_eq!(reg.len(), 2);
}

#[test]
fn source_guard_trips_and_recovers() {
    // 连续 3 轮零产出→暂停；304 不计数；暂停后每 3 tick 试探一次；有产出即恢复。
    let mut g = SourceGuard::new(3, 3);
    assert!(g.should_fetch(0));
    g.note_outcome(&FetchOutcome::nodes(vec![]), 0);
    g.note_outcome(&FetchOutcome::nodes(vec![]), 1);
    assert!(g.should_fetch(2));
    g.note_outcome(&FetchOutcome::nodes(vec![]), 2); // 第 3 轮零产出→熔断
    assert!(!g.should_fetch(3), "suspended");
    assert!(!g.should_fetch(4));
    assert!(g.should_fetch(5), "probe every 3rd tick"); // tick 2 熔断→5 试探（2+3）
                                                        // 304 不计轮次。
    let mut g2 = SourceGuard::new(3, 3);
    g2.note_outcome(&FetchOutcome::not_modified(), 0);
    g2.note_outcome(&FetchOutcome::nodes(vec![]), 1);
    assert!(g2.should_fetch(2), "304 must not count as zero-yield");
    // 有产出清零。
    g.note_outcome(&FetchOutcome::nodes(vec![raw("10.0.0.1", "a")]), 5);
    assert!(g.should_fetch(6));
}

#[test]
fn degraded_baseline_keeps_anonymity() {
    // REVIEW-R2 Q3：基线缺失降级续命只续 TTL，不得把存量 Elite 洗成 Unknown、
    // exit_ip 洗成 None、权重打回初值（同文件子模块，直读 entries 断言）。
    let mut reg = Registry::with_capacity(Duration::from_secs(1800), 10);
    let now = Instant::now();
    let raw9 = RawNode {
        ip: "10.0.0.9".to_string(),
        port: 8080,
        proto: FreeProto::Http,
        country: Some("US".to_string()),
        source: "a".to_string(),
    };
    reg.upsert_full(
        &raw9,
        120,
        AnonLevel::Elite,
        Some("9.9.9.9".to_string()),
        now,
    );
    let addr = "10.0.0.9:8080".to_string();
    let w0 = reg.snapshot(now, false)[0].weight;
    reg.renew_ttl(&addr, now);
    let e = reg.entries.get(&addr).expect("entry kept");
    assert_eq!(e.health.anon, AnonLevel::Elite);
    assert_eq!(e.health.exit_ip.as_deref(), Some("9.9.9.9"));
    assert_eq!(e.node.exit_ip.as_deref(), Some("9.9.9.9"));
    assert_eq!(reg.snapshot(now, false)[0].weight, w0);
}

#[test]
fn shared_client_builds_without_system_proxy() {
    // VPN-IMMUNE：构造不断言行为（env 行为单测会污染并行测试，计划显式不出）；
    // 行为由 H4 live 三路对照覆盖（基线＝直连出口）。
    let _ = shared_client();
}

#[test]
fn eviction_is_deterministic() {
    // REVIEW-R2 Q7：同分平局一律淘汰 addr 最小者（HashMap 迭代随机，不得决定受害者；
    // 排名依据与权重同基 composite，原 score() 与 weight() 脱钩一并对齐）。
    // 逐个插入时溢出点集合为 {.3,.1,.2} 全平局→确定淘汰 .1，幸存 {.2,.3}；
    // 10 轮独立注册表同断言（修前随机受害者，10 轮全中概率 (1/3)^10≈0，几乎必红）。
    for _ in 0..10 {
        let mut reg = Registry::with_capacity(Duration::from_secs(1800), 2);
        let now = Instant::now();
        for ip in ["10.0.0.3", "10.0.0.1", "10.0.0.2"] {
            reg.upsert_full(&raw(ip, "a"), 50, AnonLevel::Elite, None, now);
        }
        let mut addrs: Vec<String> = reg
            .snapshot(now, false)
            .iter()
            .map(|n| n.addr.clone())
            .collect();
        addrs.sort();
        assert_eq!(
            addrs,
            vec!["10.0.0.2:8080".to_string(), "10.0.0.3:8080".to_string()]
        );
    }
}

#[test]
fn intake_cap_bounds_and_covers() {
    // OPT-R4 B8：加权截断（确定性）：超限按 Elite 历史＋源序取 TopN，小池原样。
    let mut big = Vec::new();
    for i in 0..500 {
        big.push(raw(&format!("10.1.{}.{}", i / 254, i % 254 + 1), "a"));
    }
    let empty: HashSet<String> = HashSet::new();
    let capped = cap_intake(big, 100, &empty);
    assert_eq!(capped.len(), 100);
    assert!(capped.iter().all(|r| r.source == "a"));
    let small = vec![raw("10.0.0.1", "a")];
    assert_eq!(cap_intake(small, 100, &empty).len(), 1);
}

#[test]
fn b3_intake_cap_formula() {
    // OPT-R4 B3：cap＝max_nodes×factor，保底 100；factor 0 兜底 1；乘法饱和。
    assert_eq!(intake_cap_limit(2000, 2), 4000);
    assert_eq!(intake_cap_limit(2000, 1), 2000);
    assert_eq!(intake_cap_limit(2000, 0), 2000);
    assert_eq!(intake_cap_limit(10, 2), 100);
    assert_eq!(intake_cap_limit(usize::MAX, 2), usize::MAX);
}

#[test]
fn b7_baseline_bases_dedup_order() {
    // OPT-R4 B7：主基址打头＋fallback 去重保序。
    let v = baseline_bases(
        "https://a",
        &[
            "https://b".to_string(),
            "https://a".to_string(),
            "https://c".to_string(),
        ],
    );
    assert_eq!(v, vec!["https://a", "https://b", "https://c"]);
    assert_eq!(baseline_bases("https://a", &[]), vec!["https://a"]);
}

#[test]
fn b5_fail_fingerprint_window() {
    // OPT-R4 B5：未进池 addr 失败留指纹，窗口内跳过初筛，窗外过期。
    let mut reg = Registry::with_capacity(Duration::from_secs(1800), 100);
    let now = Instant::now();
    assert!(!reg.failed_recently("1.2.3.4:8080", now, Duration::from_secs(60)));
    reg.note_verify_failed("1.2.3.4:8080", now);
    assert!(reg.failed_recently("1.2.3.4:8080", now, Duration::from_secs(60)));
    let past = now.checked_sub(Duration::from_secs(3600)).unwrap_or(now);
    let mut reg2 = Registry::with_capacity(Duration::from_secs(1800), 100);
    reg2.note_verify_failed("5.6.7.8:8080", past);
    // OPT-R15 注记：`Instant` 基准距今不足 1h 时（Windows 开机初期实测
    // `Instant::now().elapsed()` 仅 1.7µs），`checked_sub` 返 None ⇒ past==now，
    // 此时「窗口外」断言在本机恒不成立。这不是实现缺陷，而是 Instant 基准
    // 太近导致无法构造过去时刻。改为基准可用时断言窗口外，基准不可用时
    // 只断言窗口内语义，保证两种环境下该测试都稳定。
    if now.checked_sub(Duration::from_secs(3600)).is_some() {
        assert!(!reg2.failed_recently("5.6.7.8:8080", now, Duration::from_secs(60)));
    }
    assert!(reg2.failed_recently("5.6.7.8:8080", now, Duration::from_secs(7200)));
    assert!(!reg.failed_recently("9.9.9.9:1", now, Duration::from_secs(3600)));
}

/// OPT-R15 回归：`Instant` 基准距今不足 `idle` 时（Windows 开机初期
/// `Instant::now().elapsed()` 可能只有微秒级），`checked_sub` 必返 None。
/// 旧写法 `now.checked_sub(idle).unwrap_or(now) <= t` 在该场景下退化成
/// `now <= t` 恒假 ⇒ 客户端缓存被整体清空。此测试直接对纯函数
/// `is_idle_over` 断言边界，**不依赖真实时钟**，任何环境下都稳定。
#[test]
fn b5_is_idle_over_is_clock_base_independent() {
    let now = Instant::now();
    let idle = Duration::from_secs(600);
    // 刚用过 ⇒ 未空闲超期，**必须**被保留。
    assert!(!is_idle_over(now, now, idle));
    // 明确空闲 11 分钟 ⇒ 超期。
    let eleven_ago = now - Duration::from_secs(660);
    assert!(is_idle_over(eleven_ago, now, idle));
    // 边界：Windows 上 `Instant` 内部精度约 15.6ms，`now - idle` 精确回推会
    // 引入截断误差（实测得 599.98s），故用「明确超出 1 秒」构造边界，
    // 既覆盖 `>=` 语义又不受时钟精度影响。
    let just_over = now - (idle + Duration::from_secs(1));
    assert!(is_idle_over(just_over, now, idle));
    // 明确未超期（idle 减 1 秒）。
    let just_under = now - (idle - Duration::from_secs(1));
    assert!(!is_idle_over(just_under, now, idle));
    // t 晚于 now（时钟回拨 / 基准错乱）⇒ saturating 返回 0 ⇒ 判为未超期，
    // 不会 panic，也不会误淘汰。
    assert!(!is_idle_over(now + Duration::from_secs(600), now, idle));
    // 关键回归点：把「基准距今仅 idle 的 1/10」这一最危险情形显式构造出来——
    // 旧实现在该情形下 checked_sub 返 None 后会把谓词变成恒假；
    // 新实现只依赖相对量，结论恒定。
    assert!(!is_idle_over(now, now, idle));
}

#[test]
fn b5_client_cache_evict_keeps_fresh_entries() {
    let c = FullChecker::new("https://example.com".to_string(), Duration::from_secs(3));
    assert!(c.client_for("http://10.20.0.1:8080").is_some());
    assert!(c.client_for("http://10.20.0.2:8080").is_some());
    assert_eq!(c.client_count(), 2);
    for i in 0..(CHECKER_CLIENT_CAP as u32 + 10u32) {
        let url = format!("http://10.21.{}.{}:8080", i / 254, i % 254 + 1);
        assert!(c.client_for(&url).is_some());
    }
    // 全部条目都是刚插入的（空闲 0s < 10min）⇒ 淘汰谓词为假 ⇒ 必须原样保留。
    // 旧实现在 Instant 基准过近时会退化为「全清」，此处即会失败。
    assert_eq!(
        c.client_count(),
        CHECKER_CLIENT_CAP + 12,
        "刚插入的客户端条目被误淘汰 —— 空闲判定退化为恒真"
    );
}

#[test]
fn b6_checker_client_cache_reuses_and_bounds() {
    // OPT-R4 B6：同代理复用不增长；1100 个不同代理后仍有界（2×cap 整清兜底）。
    let c = FullChecker::new("https://example.com".to_string(), Duration::from_secs(3));
    assert!(c.client_for("http://10.9.0.1:8080").is_some());
    assert_eq!(c.client_count(), 1);
    assert!(c.client_for("http://10.9.0.1:8080").is_some());
    assert_eq!(c.client_count(), 1);
    for i in 0..1100u32 {
        let url = format!("http://10.10.{}.{}:8080", i / 254, i % 254 + 1);
        assert!(c.client_for(&url).is_some());
    }
    assert!(c.client_count() <= CHECKER_CLIENT_CAP * 2);
}

#[test]
fn b8_weighted_truncation_prefers_elite_then_source_order() {
    // Elite history wins first, with first-seen source order as the tie-breaker.
    let mut raws = Vec::new();
    for i in 0..10 {
        raws.push(raw(&format!("10.7.0.{i}"), "gh0"));
    }
    for i in 0..10 {
        raws.push(raw(&format!("10.8.0.{i}"), "api0"));
    }
    let elite: HashSet<String> = ["10.7.0.3:8080", "10.8.0.9:8080"]
        .iter()
        .map(|s| s.to_string())
        .collect();
    let capped = cap_intake(raws, 3, &elite);
    assert_eq!(capped.len(), 3);
    assert_eq!(capped[0].ip, "10.7.0.3");
    assert_eq!(capped[1].ip, "10.8.0.9");
    assert_eq!(capped[2].ip, "10.7.0.0");
}

struct FailSource {
    name: &'static str,
}

#[async_trait::async_trait]
impl Source for FailSource {
    fn name(&self) -> &'static str {
        self.name
    }

    async fn fetch(&self, _client: &reqwest::Client) -> Result<FetchOutcome, String> {
        Err("boom".to_string())
    }
}

#[tokio::test]
async fn failed_source_trips_guard() {
    // REVIEW-R2 Q4：传输失败（Err/超时）视同零产出计入熔断；连续 3 轮即暂停。
    let bad: Box<dyn Source> = Box::new(FailSource { name: "bad" });
    let sources = vec![bad];
    let mut guards = vec![SourceGuard::new(3, 3)];
    let client = reqwest::Client::new();
    for tick in 0..3 {
        let raws = fetch_all(
            &sources,
            &mut guards,
            &client,
            tick,
            Duration::from_secs(15),
            None,
        )
        .await;
        assert!(raws.is_empty());
    }
    assert!(
        !guards[0].should_fetch(3),
        "persistently failing source must suspend"
    );
}

struct StubSource {
    name: &'static str,
    nodes: Vec<RawNode>,
    delay_ms: u64,
}

#[async_trait::async_trait]
impl Source for StubSource {
    fn name(&self) -> &'static str {
        self.name
    }

    async fn fetch(&self, _client: &reqwest::Client) -> Result<FetchOutcome, String> {
        tokio::time::sleep(Duration::from_millis(self.delay_ms)).await;
        Ok(FetchOutcome::nodes(self.nodes.clone()))
    }
}

#[tokio::test]
async fn fetch_all_preserves_source_order() {
    // 并发抓取但结果按源序拼接（去重“首见获胜”确定性；G9）。
    // 慢源（200ms）配置在前，快源（0ms）在后：慢源条目恒在前。
    let slow: Box<dyn Source> = Box::new(StubSource {
        name: "slow",
        nodes: vec![raw("10.0.0.1", "slow")],
        delay_ms: 200,
    });
    let fast: Box<dyn Source> = Box::new(StubSource {
        name: "fast",
        nodes: vec![raw("10.0.0.2", "fast")],
        delay_ms: 0,
    });
    let sources = vec![slow, fast];
    let mut guards = vec![SourceGuard::new(3, 3), SourceGuard::new(3, 3)];
    let client = reqwest::Client::new();
    let raws = fetch_all(
        &sources,
        &mut guards,
        &client,
        0,
        Duration::from_secs(15),
        None,
    )
    .await;
    assert_eq!(raws.len(), 2);
    assert_eq!(raws[0].source, "slow");
    assert_eq!(raws[1].source, "fast");
}

#[tokio::test]
async fn b4_dedup_keeps_proto_variants() {
    // OPT-R4 B4：同址不同协议不得互挤（旧 ip:port 键会丢 socks5 Elite）。
    let http_node = raw("10.9.9.9", "api0");
    let socks_node = RawNode {
        port: 8080,
        proto: FreeProto::Socks5,
        ..raw("10.9.9.9", "gh0")
    };
    let a: Box<dyn Source> = Box::new(StubSource {
        name: "api0",
        nodes: vec![http_node],
        delay_ms: 0,
    });
    let b: Box<dyn Source> = Box::new(StubSource {
        name: "gh0",
        nodes: vec![socks_node],
        delay_ms: 0,
    });
    let sources = vec![a, b];
    let mut guards = vec![SourceGuard::new(3, 3), SourceGuard::new(3, 3)];
    let client = reqwest::Client::new();
    let raws = fetch_all(
        &sources,
        &mut guards,
        &client,
        0,
        Duration::from_secs(15),
        None,
    )
    .await;
    assert_eq!(raws.len(), 2);
    assert!(raws.iter().any(|r| r.proto == FreeProto::Socks5));
    assert!(raws.iter().any(|r| r.proto == FreeProto::Http));
    // 真重复（同址同协议）仍去重，首见（api0）获胜。
    let dup: Box<dyn Source> = Box::new(StubSource {
        name: "gh1",
        nodes: vec![raw("10.9.9.9", "gh1")],
        delay_ms: 0,
    });
    let sources2: Vec<Box<dyn Source>> = vec![
        Box::new(StubSource {
            name: "api0",
            nodes: vec![raw("10.9.9.9", "api0")],
            delay_ms: 0,
        }),
        dup,
    ];
    let mut guards2 = vec![SourceGuard::new(3, 3), SourceGuard::new(3, 3)];
    let raws2 = fetch_all(
        &sources2,
        &mut guards2,
        &client,
        1,
        Duration::from_secs(15),
        None,
    )
    .await;
    assert_eq!(raws2.len(), 1);
    assert_eq!(raws2[0].source, "api0");
}

#[tokio::test]
async fn worker_merge_pushes_snapshot_to_router() {
    // Worker 合并语义：registry 快照经 replace_vendor_nodes 进池；水位计同步。
    use crate::metrics::MetricsRegistry;
    use crate::router::RouterEngine;
    use std::sync::Arc;
    let router = Arc::new(RouterEngine::new(vec![]));
    let metrics = Arc::new(MetricsRegistry::new());
    let mut reg = Registry::new(Duration::from_secs(1800));
    reg.upsert_full(
        &raw("10.0.0.1", "a"),
        50,
        AnonLevel::Elite,
        None,
        Instant::now(),
    );
    FreePoolWorker::merge_once(&router, &metrics, &reg, false);
    assert_eq!(router.snapshot_all().len(), 1);
    assert!(metrics.render().contains("free_pool_nodes_total 1"));
}

// ---------------------------------------------------------------------
// OPT-R18 P2-3：供给 SLO（出口多样性 / 年龄中位数）
// ---------------------------------------------------------------------

/// 空池 ⇒ `None`（渲染 NaN），**不是** `Some(0.0)`。
///
/// 这条不是吹毛求疵：0 会被读成"节点刚进池"，而真相是"池是空的"，
/// 两者对应的运维动作完全相反（前者等着就行，后者要立刻查供给）。
#[test]
fn registry_median_age_none_when_empty() {
    let reg = Registry::new(Duration::from_secs(1800));
    assert_eq!(reg.median_age_secs(Instant::now(), false), None);
}

/// 中位数口径（而非均值），且过滤条件与 `snapshot()` 完全一致。
///
/// 年龄取 1000 / 101 / 100 秒：**中位数 101、均值 400.33**。故意让两者
/// 分叉，这样"实现成均值"会被立刻抓出来——只拿等差年龄去测，两种口径
/// 会给出同一个数，测试等于没写。
#[test]
fn registry_median_age_is_median_not_mean() {
    let t0 = Instant::now();
    let mut reg = Registry::new(Duration::from_secs(1800));
    for (ip, born) in [("10.0.0.1", 0u64), ("10.0.0.2", 899), ("10.0.0.3", 900)] {
        reg.upsert_full(
            &raw(ip, "a"),
            50,
            AnonLevel::Elite,
            None,
            t0 + Duration::from_secs(born),
        );
    }
    let now = t0 + Duration::from_secs(1000);
    assert_eq!(
        reg.median_age_secs(now, false),
        Some(101.0),
        "年龄 1000/101/100 的中位数是 101；得到 400.33 说明实现成了均值"
    );
}

/// 偶数个条目 ⇒ 取中间两个的均值（标准中位数定义）。
#[test]
fn registry_median_age_even_count_averages_middle_two() {
    let t0 = Instant::now();
    let mut reg = Registry::new(Duration::from_secs(1800));
    // 年龄 100 / 300 ⇒ 中位数 200。
    reg.upsert_full(&raw("10.0.0.1", "a"), 50, AnonLevel::Elite, None, t0);
    reg.upsert_full(
        &raw("10.0.0.2", "a"),
        50,
        AnonLevel::Elite,
        None,
        t0 + Duration::from_secs(200),
    );
    let now = t0 + Duration::from_secs(300);
    assert_eq!(reg.median_age_secs(now, false), Some(200.0));
}

/// `require_elite` 与 `snapshot()` 同口径：非 Elite 条目不得进入年龄统计。
#[test]
fn registry_median_age_respects_elite_filter() {
    let t0 = Instant::now();
    let mut reg = Registry::new(Duration::from_secs(1800));
    reg.upsert_full(&raw("10.0.0.1", "a"), 50, AnonLevel::Elite, None, t0);
    reg.upsert_full(&raw("10.0.0.2", "a"), 50, AnonLevel::Transparent, None, t0);
    let now = t0 + Duration::from_secs(60);
    assert_eq!(reg.snapshot(now, true).len(), 1);
    assert_eq!(reg.median_age_secs(now, true), Some(60.0));
    assert_eq!(reg.snapshot(now, false).len(), 2);
    assert_eq!(reg.median_age_secs(now, false), Some(60.0));
}

/// 过期条目不得进入年龄统计（否则"池里只剩 1 个活节点"会被平均成一个
/// 看起来很健康的年龄）。
#[test]
fn registry_median_age_excludes_expired() {
    let t0 = Instant::now();
    let ttl = Duration::from_secs(60);
    let mut reg = Registry::new(ttl);
    reg.upsert_full(&raw("10.0.0.1", "a"), 50, AnonLevel::Elite, None, t0);
    let now = t0 + Duration::from_secs(120);
    assert!(reg.snapshot(now, false).is_empty());
    assert_eq!(reg.median_age_secs(now, false), None);
}

/// 端到端：`merge_once` 必须把三个供给 SLO 数字同时写进指标。
///
/// 关键断言是**节点数 3 但出口只有 2 个**——两个节点共用同一个 `exit_ip`
/// 时，"池里有 3 个节点"是假象：WAF 侧只看到 2 个身份。这正是这道指标
/// 存在的理由，也是只有它才能回答的问题。
#[tokio::test]
async fn merge_once_publishes_supply_slo() {
    use crate::metrics::MetricsRegistry;
    use crate::router::RouterEngine;
    use std::sync::Arc;
    let router = Arc::new(RouterEngine::new(vec![]));
    let metrics = Arc::new(MetricsRegistry::new());
    let mut reg = Registry::new(Duration::from_secs(1800));
    let now = Instant::now();
    // 两个节点共用出口 10.9.9.9；第三个节点出口未知 ⇒ 回落自身 ip。
    reg.upsert_full(
        &raw("10.0.0.1", "a"),
        50,
        AnonLevel::Elite,
        Some("10.9.9.9".to_string()),
        now,
    );
    reg.upsert_full(
        &raw("10.0.0.2", "a"),
        50,
        AnonLevel::Elite,
        Some("10.9.9.9".to_string()),
        now,
    );
    reg.upsert_full(&raw("10.0.0.3", "a"), 50, AnonLevel::Elite, None, now);

    FreePoolWorker::merge_once(&router, &metrics, &reg, false);
    let r = metrics.render();
    assert!(r.contains("free_pool_nodes_total 3"));
    assert!(
        r.contains("free_pool_distinct_exit_ips 2"),
        "两个节点共用出口时必须只算 1 个身份：\n{r}"
    );
    // 年龄是 ~0 秒但**不是 NaN**（有节点就一定有年龄）。
    assert!(
        !r.contains("free_pool_median_age_seconds NaN"),
        "非空池不得渲染 NaN：\n{r}"
    );
}

#[tokio::test]
async fn semaphore_caps_concurrent_verify() {
    // REVIEW-R2 Q1：spawn 内许可必须持有跨过 await（`pool.rs:92` 形态）；
    // 丢弃写法（`if acquire_owned().await.is_err()` 临时值）即时释放，并发无上限。
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;
    let sem = Arc::new(tokio::sync::Semaphore::new(2));
    let live = Arc::new(AtomicUsize::new(0));
    let high = Arc::new(AtomicUsize::new(0));
    let mut set = tokio::task::JoinSet::new();
    for _ in 0..8 {
        let (s, l, h) = (sem.clone(), live.clone(), high.clone());
        set.spawn(async move {
            // 许可持有跨过 await（丢弃写法即时释放，高水位==任务数，见本单测红灯记录）。
            let Ok(_permit) = s.acquire_owned().await else {
                return;
            };
            let n = l.fetch_add(1, Ordering::SeqCst) + 1;
            h.fetch_max(n, Ordering::SeqCst);
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
            l.fetch_sub(1, Ordering::SeqCst);
        });
    }
    while set.join_next().await.is_some() {}
    assert!(
        high.load(Ordering::SeqCst) <= 2,
        "concurrency escaped cap: high={}",
        high.load(Ordering::SeqCst)
    );
}

#[test]
fn merge_applies_arbitrage_factor_across_ticks() {
    // NEXT-B6：审计 scale 沉入合并——复检快照（权重重置）重合并后缩放仍保持，
    // 不再被 `replace_vendor_nodes` 全量覆盖洗掉。
    use crate::metrics::MetricsRegistry;
    use crate::router::RouterEngine;
    use std::sync::Arc;
    let router = Arc::new(RouterEngine::new(vec![]));
    let metrics = Arc::new(MetricsRegistry::new());
    let mut reg = Registry::new(Duration::from_secs(1800));
    reg.upsert_full(
        &raw("10.0.0.9", "a"),
        50,
        AnonLevel::Elite,
        None,
        Instant::now(),
    );
    let w0 = reg.snapshot(Instant::now(), false)[0].weight;
    assert!(w0 > 1);
    FreePoolWorker::merge_once(&router, &metrics, &reg, false);
    router.scale_vendor_weights("free-a", "US", 0.5);
    FreePoolWorker::merge_once(&router, &metrics, &reg, false);
    let w1 = router
        .snapshot_all()
        .iter()
        .find(|n| n.ip == "10.0.0.9")
        .map(|n| n.weight)
        .unwrap();
    assert_eq!(w1, ((w0 as f64 * 0.5).round() as u32).max(1));
    // 复检快照权重重置为 w0 → 重合并 → 缩放仍在（修前回到 w0，即红）。
    FreePoolWorker::merge_once(&router, &metrics, &reg, false);
    let w2 = router
        .snapshot_all()
        .iter()
        .find(|n| n.ip == "10.0.0.9")
        .map(|n| n.weight)
        .unwrap();
    assert_eq!(w2, w1);
}

fn socks_raw(ip: &str, source: &str) -> RawNode {
    RawNode {
        ip: ip.to_string(),
        port: 1080,
        proto: FreeProto::Socks5,
        country: None,
        source: source.to_string(),
    }
}

#[test]
fn registry_stores_socks_proto() {
    // P2-5：socks 节点正常进池（poolable 门已拆），快照携带 proto＝Socks5。
    use crate::model::EgressProto;
    let mut reg = Registry::new(Duration::from_secs(1800));
    let now = Instant::now();
    reg.upsert_full(&socks_raw("9.9.9.9", "gh"), 50, AnonLevel::Elite, None, now);
    let snap = reg.snapshot(now, false);
    assert_eq!(snap.len(), 1);
    assert_eq!(snap[0].proto, EgressProto::Socks5);
    assert_eq!(snap[0].tier, "free");
}

/// CONNECT 常成功 stub（greeting→CONNECT→回成功；再读带 500ms 超时以兼容 greeting-only）。
async fn spawn_connect_ok_stub() -> u16 {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind");
    let port = listener.local_addr().expect("addr").port();
    tokio::spawn(async move {
        let (mut s, _) = match listener.accept().await {
            Ok(v) => v,
            Err(_) => return,
        };
        let mut head = [0u8; 2];
        if s.read_exact(&mut head).await.is_err() {
            return;
        }
        let mut methods = vec![0u8; head[1] as usize];
        if s.read_exact(&mut methods).await.is_err() {
            return;
        }
        if s.write_all(&[0x05, 0x00]).await.is_err() {
            return;
        }
        // CONNECT 头 4 字节（500ms 读不到＝greeting-only，正常关闭）。
        let mut req4 = [0u8; 4];
        let got = tokio::time::timeout(Duration::from_millis(500), s.read_exact(&mut req4)).await;
        if got.is_err() {
            return;
        }
        let rest_len = match req4[3] {
            0x01 => 4 + 2,
            0x03 => {
                let mut l = [0u8; 1];
                if s.read_exact(&mut l).await.is_err() {
                    return;
                }
                l[0] as usize + 2
            }
            _ => return,
        };
        let mut rest = vec![0u8; rest_len];
        if s.read_exact(&mut rest).await.is_err() {
            return;
        }
        let _ = s
            .write_all(&[0x05, 0x00, 0x00, 0x01, 0, 0, 0, 0, 0, 0])
            .await;
    });
    port
}

#[tokio::test]
async fn verifier_socks_handshake_ok_and_refused() {
    // 有 target→CONNECT 验证 Some；无 target→greeting-only 降级 Some；拒连 None。
    let stub = spawn_connect_ok_stub().await;
    let raw = socks_raw("127.0.0.1", "t");
    let raw = RawNode { port: stub, ..raw };
    let v = Verifier::new(Duration::from_secs(3), 3000)
        .with_socks_target("127.0.0.1".to_string(), stub);
    assert!(v.verify(&raw).await.is_some());
    let stub2 = spawn_connect_ok_stub().await;
    let raw2 = RawNode {
        port: stub2,
        ..socks_raw("127.0.0.1", "t")
    };
    let v2 = Verifier::new(Duration::from_secs(3), 3000);
    assert!(v2.verify(&raw2).await.is_some());
    let refused = RawNode {
        port: 1,
        ..socks_raw("127.0.0.1", "t")
    };
    assert!(v.verify(&refused).await.is_none());
}

#[test]
fn check_target_parses_base() {
    // full_check_base → CONNECT 验证目标（host＋端口，缺省 443/80）。
    assert_eq!(
        socks_target_of_base("https://httpbin.org"),
        Some(("httpbin.org".to_string(), 443))
    );
    assert_eq!(
        socks_target_of_base("http://127.0.0.1:18080/"),
        Some(("127.0.0.1".to_string(), 18080))
    );
    assert_eq!(socks_target_of_base("gopher://x/"), None);
    assert_eq!(socks_target_of_base(""), None);
}

#[tokio::test]
async fn fullcheck_via_socks_relay_reports_elite() {
    // 本地 echo 基址（/ip＋/headers＋/anything 三连 accept）＋relay-stub
    // （CONNECT 到 echo 端口后双向管道）→经 socks 复检 Some＋Elite。
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let echo = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind");
    let echo_port = echo.local_addr().expect("addr").port();
    tokio::spawn(async move {
        for _ in 0..3 {
            let (mut s, _) = match echo.accept().await {
                Ok(v) => v,
                Err(_) => return,
            };
            let mut buf = [0u8; 4096];
            let n = s.read(&mut buf).await.unwrap_or(0);
            let req = String::from_utf8_lossy(&buf[..n]).to_string();
            let path = req
                .lines()
                .next()
                .unwrap_or("")
                .split_whitespace()
                .nth(1)
                .unwrap_or("/")
                .to_string();
            let body = if path.starts_with("/ip") {
                r#"{"origin":"10.9.9.9"}"#.to_string()
            } else if path.starts_with("/headers") {
                r#"{"headers":{}}"#.to_string()
            } else {
                // 同上：显式位置参数。
                format!(r#"{{"url":"http://x/anything/{}"}}"#, FULL_CHECK_MARKER)
            };
            let _ = s
                    .write_all(
                        format!(
                            "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                            body.len()
                        )
                        .as_bytes(),
                    )
                    .await;
        }
    });
    let relay = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind");
    let relay_port = relay.local_addr().expect("addr").port();
    tokio::spawn(async move {
        for _ in 0..3 {
            let (mut s, _) = match relay.accept().await {
                Ok(v) => v,
                Err(_) => return,
            };
            let mut head = [0u8; 2];
            if s.read_exact(&mut head).await.is_err() {
                continue;
            }
            let mut methods = vec![0u8; head[1] as usize];
            if s.read_exact(&mut methods).await.is_err() {
                continue;
            }
            if s.write_all(&[0x05, 0x00]).await.is_err() {
                continue;
            }
            let mut req4 = [0u8; 4];
            if s.read_exact(&mut req4).await.is_err() {
                continue;
            }
            let (host, port) = match req4[3] {
                0x01 => {
                    let mut b = [0u8; 6];
                    if s.read_exact(&mut b).await.is_err() {
                        continue;
                    }
                    (
                        std::net::IpAddr::from([b[0], b[1], b[2], b[3]]).to_string(),
                        u16::from_be_bytes([b[4], b[5]]),
                    )
                }
                0x03 => {
                    let mut l = [0u8; 1];
                    if s.read_exact(&mut l).await.is_err() {
                        continue;
                    }
                    let mut b = vec![0u8; l[0] as usize + 2];
                    if s.read_exact(&mut b).await.is_err() {
                        continue;
                    }
                    let n = b.len();
                    (
                        String::from_utf8_lossy(&b[..n - 2]).to_string(),
                        u16::from_be_bytes([b[n - 2], b[n - 1]]),
                    )
                }
                _ => continue,
            };
            let mut up = match tokio::net::TcpStream::connect((host.as_str(), port)).await {
                Ok(v) => v,
                Err(_) => continue,
            };
            if s.write_all(&[0x05, 0x00, 0x00, 0x01, 0, 0, 0, 0, 0, 0])
                .await
                .is_err()
            {
                continue;
            }
            let _ = tokio::io::copy_bidirectional(&mut s, &mut up).await;
        }
    });
    let checker = FullChecker::new(
        format!("http://127.0.0.1:{echo_port}"),
        Duration::from_secs(5),
    );
    let raw = RawNode {
        port: relay_port,
        ..socks_raw("127.0.0.1", "t")
    };
    let res = checker.check(&raw, "1.2.3.4").await.expect("pass");
    assert_eq!(res.anon, AnonLevel::Elite);
    assert_eq!(res.exit_ip.as_deref(), Some("10.9.9.9"));
}
