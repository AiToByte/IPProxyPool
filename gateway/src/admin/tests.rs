//! `admin` 全部单测（原文件后半段，逐字搬迁）。
//!
//! 这里保留原有的**显式 import**（`use super::{error_response, ...}`）而不是
//! `use super::*`：admin 的 DTO 类型很多，显式列出能看出"这个测试到底依赖哪几个
//! 入口"，比通配更难写错。

use super::{error_response, AdminApiDoc, AdminState, AutotuneConfig, NodeRegistry};
use crate::router::RouterEngine;
use crate::tenant::TenantManager;
use crate::{metrics::MetricsRegistry, model::ProxyNode};
use axum::body::Body;
use axum::http::StatusCode;
use axum::http::{header::CONTENT_TYPE, Request};
use std::sync::Arc;
use tower::ServiceExt;
use utoipa::OpenApi;

/// 使用独立测试进程安装 logger，避免与并行测试或已有全局 logger 冲突。
#[test]
fn audit_logging_redacts_credentials_in_real_records() {
    let output = std::process::Command::new(std::env::current_exe().expect("测试程序"))
        .args([
            "--exact",
            "admin::tests::audit_logging_capture_child",
            "--nocapture",
            "--test-threads=1",
        ])
        .env("IPP_ADMIN_AUDIT_CAPTURE_CHILD", "1")
        .output()
        .expect("启动日志捕获子进程");
    assert!(
        output.status.success(),
        "日志行为回归失败：\n{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

struct AuditCapture(std::sync::Mutex<Vec<(log::Level, String)>>);

impl log::Log for AuditCapture {
    fn enabled(&self, _: &log::Metadata<'_>) -> bool {
        true
    }

    fn log(&self, record: &log::Record<'_>) {
        let message = record.args().to_string();
        if message.starts_with("[admin] ") {
            self.0
                .lock()
                .expect("捕获锁")
                .push((record.level(), message));
        }
    }

    fn flush(&self) {}
}

#[tokio::test]
async fn audit_logging_capture_child() {
    if std::env::var_os("IPP_ADMIN_AUDIT_CAPTURE_CHILD").is_none() {
        return;
    }
    static CAPTURE: AuditCapture = AuditCapture(std::sync::Mutex::new(Vec::new()));
    log::set_logger(&CAPTURE).expect("子进程独占 logger");
    log::set_max_level(log::LevelFilter::Info);
    let app = super::router(test_state(vec![], &["tenant-secret-P0", "tenant/secret"]));
    // 同一条真实审计链路覆盖成功、认证拒绝、租户不存在、未匹配和方法错误。
    let cases = [
        (
            "GET",
            "tenant-secret-P0",
            true,
            200,
            "/api/v1/tenants/{api_key}",
        ),
        (
            "GET",
            "tenant-%73ecret-P0",
            true,
            200,
            "/api/v1/tenants/{api_key}",
        ),
        (
            "GET",
            "tenant%2fsecret",
            true,
            200,
            "/api/v1/tenants/{api_key}",
        ),
        (
            "PATCH",
            "tenant-secret-P0/balance",
            true,
            200,
            "/api/v1/tenants/{api_key}/balance",
        ),
        (
            "PATCH",
            "tenant-%73ecret-P0/active",
            true,
            200,
            "/api/v1/tenants/{api_key}/active",
        ),
        (
            "GET",
            "tenant-secret-P0",
            false,
            401,
            "/api/v1/tenants/{api_key}",
        ),
        (
            "GET",
            "tenant-%73ecret-P0",
            false,
            401,
            "/api/v1/tenants/{api_key}",
        ),
        (
            "GET",
            "missing-secret-P0",
            true,
            404,
            "/api/v1/tenants/{api_key}",
        ),
        (
            "GET",
            "missing-%73ecret-P0",
            true,
            404,
            "/api/v1/tenants/{api_key}",
        ),
        (
            "GET",
            "tenant-secret-P0/unknown/tenant-secret-P0",
            true,
            404,
            "/[unmatched]",
        ),
        (
            "GET",
            "tenant-%73ecret-P0/unknown",
            false,
            404,
            "/[unmatched]",
        ),
        (
            "POST",
            "tenant-secret-P0",
            true,
            405,
            "/api/v1/tenants/{api_key}",
        ),
    ];
    for (index, (method, tail, authenticated, status, path)) in cases.into_iter().enumerate() {
        // 查询参数和两种 Admin 认证头的值也不能进入日志。
        let uri = format!("/api/v1/tenants/{tail}?api_key=query-secret-P0");
        let mut request = Request::builder().method(method).uri(uri);
        if authenticated {
            request = request.header("Authorization", format!("Admin {KEY}"));
        }
        let body = if method == "PATCH" {
            request = request.header(CONTENT_TYPE, "application/json");
            Body::from(r#"{"balance_usd":3.5,"active":true}"#)
        } else {
            Body::empty()
        };
        let response = app
            .clone()
            .oneshot(request.body(body).expect("请求"))
            .await
            .expect("响应");
        assert_eq!(response.status().as_u16(), status, "{method} {tail}");
        let records = CAPTURE.0.lock().expect("捕获锁");
        if records.is_empty() {
            panic!("第 {index} 次请求必须产生真实审计日志");
        }
        let (level, message) = records.last().expect("必须产生真实审计日志");
        assert_eq!(records.len(), 1, "每次请求恰有一条审计记录");
        assert_eq!(
            *level,
            if status < 400 {
                log::Level::Info
            } else {
                log::Level::Warn
            }
        );
        assert!(
            message.starts_with(&format!("[admin] {method} {path} -> {status} (")),
            "实际日志：{message}"
        );
        assert!(message.ends_with(&format!("ms, key={authenticated})")));
        for secret in [
            KEY,
            "tenant-secret-P0",
            "tenant-%73ecret-P0",
            "tenant/secret",
            "tenant%2fsecret",
            "missing-secret-P0",
            "missing-%73ecret-P0",
            "query-secret-P0",
        ] {
            assert!(!message.contains(secret), "整条日志不得泄漏凭据：{message}");
        }
        drop(records);
        CAPTURE.0.lock().expect("捕获锁").clear();
    }
}

/// 结构编译验证（历史保留）+ 步骤 52 新端点必须进 OpenAPI。
#[test]
fn openapi_doc_builds() {
    let doc = AdminApiDoc::openapi();
    let nodes = doc
        .paths
        .paths
        .get("/api/v1/nodes")
        .expect("collection path");
    assert!(nodes.get.is_some());
    assert!(
        nodes.post.is_some(),
        "POST /api/v1/nodes must be documented"
    );
    let item = doc
        .paths
        .paths
        .get("/api/v1/nodes/{addr}")
        .expect("item path");
    assert!(
        item.delete.is_some(),
        "DELETE /api/v1/nodes/{{addr}} must be documented"
    );
}

#[test]
fn auth_middleware_rejects_missing_key() {
    // 仅验证结构编译；运行时认证逻辑由下方 oneshot 全链路测试覆盖。
    let _state = AdminState {
        router: Arc::new(RouterEngine::new(vec![])),
        nodes: Arc::new(NodeRegistry::in_memory(vec![])),
        tenant_mgr: Arc::new(TenantManager::new()),
        metrics: Arc::new(MetricsRegistry::new()),
        admin_api_key: "test-key".to_string(),
        autotune: Arc::new(AutotuneConfig::new()),
    };
    assert!(!_state.admin_api_key.is_empty());
}

/// 统一错误体形状：code 必须**等于 HTTP 状态码**（文档 §4 契约）。
#[test]
fn error_response_carries_status_and_message() {
    let (status, body) = error_response(StatusCode::NOT_FOUND, "boom");
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(body.code, 404);
    assert_eq!(body.error, "boom");
}

// ---- 以下为 axum oneshot 全链路契约测试（本轮契约修复的锁定回归）----

const KEY: &str = "test-key";

fn test_state(nodes: Vec<ProxyNode>, tenants: &[&str]) -> AdminState {
    let mgr = TenantManager::new();
    for key in tenants {
        mgr.register_tenant("t", key, 10_000, 10_000, 1);
    }
    AdminState {
        router: Arc::new(RouterEngine::new(nodes.clone())),
        nodes: Arc::new(NodeRegistry::in_memory(nodes)),
        tenant_mgr: Arc::new(mgr),
        metrics: Arc::new(MetricsRegistry::new()),
        admin_api_key: KEY.to_string(),
        autotune: Arc::new(AutotuneConfig::new()),
    }
}

fn test_node(ip: &str, port: u16, country: &str, tier: &str) -> ProxyNode {
    ProxyNode::new(
        ip.to_string(),
        port,
        None,
        None,
        country.to_string(),
        tier.to_string(),
        "mock-a".to_string(),
        100,
    )
}

async fn send_json(
    app: axum::Router,
    method: &str,
    uri: &str,
    body: Option<&str>,
) -> (StatusCode, serde_json::Value) {
    let mut builder = Request::builder()
        .method(method)
        .uri(uri)
        .header("X-Admin-Key", KEY);
    if body.is_some() {
        // axum 的 Json extractor 强制要求 application/json。
        builder = builder.header(CONTENT_TYPE, "application/json");
    }
    let req = builder
        .body(match body {
            Some(b) => Body::from(b.to_string()),
            None => Body::empty(),
        })
        .expect("request");
    let res = app.oneshot(req).await.expect("oneshot");
    let status = res.status();
    let bytes = axum::body::to_bytes(res.into_body(), usize::MAX)
        .await
        .expect("body");
    let value = if bytes.is_empty() {
        serde_json::Value::Null
    } else {
        serde_json::from_slice(&bytes).expect("body must be valid JSON (错误体契约)")
    };
    (status, value)
}

/// 契约 ①：错误必须带 JSON body `{error, code}`（401 场景：无 key）。
#[tokio::test]
async fn missing_admin_key_returns_json_401() {
    let app = super::router(test_state(vec![], &[]));
    let req = Request::builder()
        .method("GET")
        .uri("/api/v1/health")
        .body(Body::empty())
        .expect("request");
    let res = app.oneshot(req).await.expect("oneshot");
    assert_eq!(res.status(), StatusCode::UNAUTHORIZED);
    let bytes = axum::body::to_bytes(res.into_body(), usize::MAX)
        .await
        .expect("body");
    let v: serde_json::Value = serde_json::from_slice(&bytes).expect("错误体必须是 JSON");
    assert_eq!(v["code"], 401);
    assert!(v["error"].as_str().unwrap().contains("admin key"));
}

#[tokio::test]
async fn empty_configured_admin_key_rejects_empty_credentials() {
    let mut state = test_state(vec![], &[]);
    state.admin_api_key.clear();
    let app = super::router(state);

    for (header_name, header_value) in [
        ("X-Admin-Key", ""),
        ("X-Admin-Key", "unexpected-key"),
        ("Authorization", "Admin "),
        ("Authorization", "Admin unexpected-key"),
    ] {
        let req = Request::builder()
            .method("GET")
            .uri("/api/v1/health")
            .header(header_name, header_value)
            .body(Body::empty())
            .expect("request");
        let res = app.clone().oneshot(req).await.expect("response");
        assert_eq!(
            res.status(),
            StatusCode::UNAUTHORIZED,
            "header={header_name}"
        );
    }
}

/// 契约 ②：`/nodes` 列表是**裸数组**（首版包装对象 `{nodes:[...]}` 与文档、
/// 前端三页面契约不符，dashboard/nodes 页面按数组消费直接崩——本测试锁死）。
#[tokio::test]
async fn nodes_list_is_bare_array() {
    let app = super::router(test_state(
        vec![test_node("127.0.0.1", 39_001, "US", "residential")],
        &[],
    ));
    let (status, v) = send_json(app, "GET", "/api/v1/nodes", None).await;
    assert_eq!(status, StatusCode::OK);
    let arr = v.as_array().expect("/nodes 必须返回裸数组");
    assert_eq!(arr.len(), 1);
    assert_eq!(arr[0]["addr"], "127.0.0.1:39001");
    assert_eq!(arr[0]["country"], "US");
}

/// 契约 ③：`/tenants` 列表是**裸数组**；PATCH balance **真实落库**
///（首版只改本地副本，GET 回读即回滚——本测试锁死 round-trip）。
#[tokio::test]
async fn tenants_list_is_bare_array_and_balance_persists() {
    let app = super::router(test_state(vec![], &["tenant-key-1"]));
    let (status, v) = send_json(app.clone(), "GET", "/api/v1/tenants", None).await;
    assert_eq!(status, StatusCode::OK);
    let arr = v.as_array().expect("/tenants 必须返回裸数组");
    assert_eq!(arr.len(), 1);
    assert_eq!(arr[0]["api_key"], "tenant-key-1");

    // PATCH balance 3.5 → 200，body 即更新后的 detail。
    let (status, v) = send_json(
        app.clone(),
        "PATCH",
        "/api/v1/tenants/tenant-key-1/balance",
        Some(r#"{"balance_usd": 3.5}"#),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert!((v["balance_usd"].as_f64().unwrap() - 3.5).abs() < 1e-9);

    // 回读：新值必须还在（落库修复的验收点）。
    let (status, v) = send_json(app.clone(), "GET", "/api/v1/tenants/tenant-key-1", None).await;
    assert_eq!(status, StatusCode::OK);
    assert!(
        (v["balance_usd"].as_f64().unwrap() - 3.5).abs() < 1e-9,
        "PATCH 后回读必须仍是 3.5"
    );

    // 幽灵 key ⇒ 404 JSON 错误体。
    let (status, v) = send_json(
        app.clone(),
        "PATCH",
        "/api/v1/tenants/ghost-key/balance",
        Some(r#"{"balance_usd": 1.0}"#),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(v["code"], 404);
    assert!(v["error"].as_str().unwrap().contains("tenant"));

    // 非法 JSON ⇒ 400 JSON 错误体（400 面的实装验证）。
    let (status, v) = send_json(
        app.clone(),
        "PATCH",
        "/api/v1/tenants/tenant-key-1/balance",
        Some("{oops"),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(v["code"], 400);
}

/// 契约 ④：隔离 → 断路器快照可见 → DELETE 真解除（隔离条目清零）。
/// 首版 DELETE 是空操作，隔离只能等 sweep 过期——本测试锁死实装语义。
#[tokio::test]
async fn quarantine_set_then_delete_really_removes() {
    let node = test_node("127.0.0.2", 39_002, "JP", "datacenter");
    let state = test_state(vec![node], &[]);
    let router_for_assert = state.router.clone();
    let app = super::router(state);

    // POST 隔离 600s。
    let (status, _) = send_json(
        app.clone(),
        "POST",
        "/api/v1/nodes/127.0.0.2:39002/quarantine",
        Some(r#"{"ttl_secs": 600}"#),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(router_for_assert.quarantine_len(), 1, "隔离必须真的落表");

    // 两级口径验证：外层 domain 数 = 1，内层 ip 条目数 = 1。
    assert_eq!(router_for_assert.quarantine_domains(), 1);

    // DELETE 解除 → 隔离条目清零（含外层回收）。
    let (status, v) = send_json(
        app,
        "DELETE",
        "/api/v1/nodes/127.0.0.2:39002/quarantine",
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(v["addr"], "127.0.0.2:39002");
    assert_eq!(
        router_for_assert.quarantine_len(),
        0,
        "DELETE 必须真的解除隔离"
    );
    assert_eq!(
        router_for_assert.quarantine_domains(),
        0,
        "空内层必须回收外层"
    );
}

/// 契约 ⑤：调权真实生效（provider+country 维度整组）+ 归一化 country 过滤。
#[tokio::test]
async fn node_weight_patch_applies_and_detail_route_works() {
    let app = super::router(test_state(
        vec![test_node("127.0.0.3", 39_003, "GB", "mobile")],
        &[],
    ));
    // 详情路由（含 ':' 的路径段）。
    let (status, v) = send_json(app.clone(), "GET", "/api/v1/nodes/127.0.0.3:39003", None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(v["addr"], "127.0.0.3:39003");

    // 调权 → 200 且详情里 weight 立即变化。
    let (status, _) = send_json(
        app.clone(),
        "PATCH",
        "/api/v1/nodes/127.0.0.3:39003/weight",
        Some(r#"{"weight": 42}"#),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let (status, v) = send_json(app, "GET", "/api/v1/nodes/127.0.0.3:39003", None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(v["weight"], 42, "调权必须真实生效于节点视图");

    // 不存在的节点 ⇒ 404（JSON 错误体）。
    let app = super::router(test_state(vec![], &[]));
    let (status, v) = send_json(app, "GET", "/api/v1/nodes/1.2.3.4:80", None).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(v["code"], 404);

    // 未匹配路径 ⇒ fallback 404（JSON 错误体，此前是空 body）。
    let (status, v) = send_json(
        super::router(test_state(vec![], &[])),
        "GET",
        "/api/v1/nope",
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert!(v["error"]
        .as_str()
        .unwrap()
        .contains("unknown admin endpoint"));
}

// ---- OPT-R17：超时层与审计层（补齐此前"注释声称存在、代码里没有"的两层）----

/// 超时层**真的会掐断**超时的请求（超时层的红测有效性证明）。
///
/// # 为什么必须实测，而不是断言常量
///
/// 首版写的是 `assert!(ADMIN_TIMEOUT_SECS > 0)`。它是**假绿**：
/// `ADMIN_TIMEOUT_SECS` 是 `const u64 = 30`，`30 > 0` 在编译期即确定，
/// 断言永不为假，也抓不到任何回归（把它改小同样不会红，只会重新编译）。
/// CI 的 clippy 以 `assertions_on_constants` lint 判红（实测 run，
/// rust 1.99）——**该 lint 抓到的是真缺陷，不是风格问题**。
///
/// 现改为：用 `build_router` 注入 1ms 超时 + 一个真会挂起 10s 的 handler，
/// 断言得到 **408**。这样"超时层生效"是被**行为**证明的 ——
/// 有人删掉 `TimeoutLayer` 或把它挪到 handler 之后，本测试立即失败。
#[tokio::test]
async fn timeout_layer_actually_cuts_off_a_hanging_handler() {
    // 1ms 超时 + 挂起 10s 的 handler ⇒ 必然 408。
    // 用真实时间（不用 `start_paused`）：`oneshot` 在内存里直接驱动
    // Service，`TimeoutLayer` 内部走 tokio 的 real-time timer，
    // 暂停时钟会让 timeout 永不触发、测试挂到 CI 超时。
    //
    // 慢路由经 `build_router_with` 注入到 `/api/v1` **内部** —— 挂在 nest
    // 之外的话根本不经过 TimeoutLayer，测试会误判成「超时层失效」。
    let app = super::build_router_with(
        test_state(vec![], &[]),
        std::time::Duration::from_millis(1),
        super::Router::new().route(
            "/slow",
            axum::routing::get(|| async {
                tokio::time::sleep(std::time::Duration::from_secs(10)).await;
                axum::Json(serde_json::json!({"never": true}))
            }),
        ),
    );
    let req = Request::builder()
        .method("GET")
        .uri("/api/v1/slow")
        .header("X-Admin-Key", KEY)
        .body(Body::empty())
        .expect("request");
    let res = app.oneshot(req).await.expect("oneshot");
    assert_eq!(
        res.status(),
        StatusCode::REQUEST_TIMEOUT,
        "挂起 10s 的请求在 1ms 超时下必须被掐断为 408 —— \
             若返回 200，说明 TimeoutLayer 未生效或位置错误"
    );
}

/// 快速路径**不受**超时层误伤（防止"防护"变成"关停"）。
///
/// 与上一条成对：上一条证明超时层会掐断慢请求，这一条证明正常的内存操作
/// 仍能及时返回。两条缺一，超时层就可能整体失效或整体误伤。
#[tokio::test]
async fn timeout_layer_does_not_break_the_fast_path() {
    let app = super::router(test_state(
        vec![test_node("127.0.0.1", 39_001, "US", "residential")],
        &[],
    ));
    let req = Request::builder()
        .method("GET")
        .uri("/api/v1/health")
        .header("X-Admin-Key", KEY)
        .body(Body::empty())
        .expect("request");
    let res = app.oneshot(req).await.expect("oneshot");
    assert_eq!(
        res.status(),
        StatusCode::OK,
        "生产超时（{}s）下，健康检查必须仍是 200（否则是误伤而非防护）",
        super::ADMIN_TIMEOUT_SECS
    );
}

/// 超时层覆盖**未认证**请求（层序回归）。
///
/// 若日后有人把 `TimeoutLayer` 挪到 `auth_middleware` 内侧，
/// 未认证的挂起请求将不受 timeout 约束。本测试锁定"任何路径都过 timeout"。
#[tokio::test]
async fn timeout_layer_wraps_auth_middleware() {
    // 无 Key ⇒ 401（证明认证仍生效，timeout 未把认证吞掉）。
    let app = super::router(test_state(vec![], &[]));
    let req = Request::builder()
        .method("GET")
        .uri("/api/v1/health")
        .body(Body::empty())
        .expect("request");
    let res = app.oneshot(req).await.expect("oneshot");
    assert_eq!(res.status(), StatusCode::UNAUTHORIZED);

    // 带 Key ⇒ 200（证明 timeout 层未破坏认证后的正常路径）。
    let app = super::router(test_state(vec![], &[]));
    let req = Request::builder()
        .method("GET")
        .uri("/api/v1/health")
        .header("X-Admin-Key", KEY)
        .body(Body::empty())
        .expect("request");
    let res = app.oneshot(req).await.expect("oneshot");
    assert_eq!(res.status(), StatusCode::OK);
}

/// 审计层对**失败**请求同样放行（401/404 不被审计层吞掉或改写）。
///
/// 审计层是最外层，如果它的实现有误（例如把响应体吃掉、或对错误状态
/// 提前返回），会表现为"契约①/④ 的错误体测试突然挂了"。本测试显式
/// 锁定：加了审计层后，错误体契约与状态码**保持不变**。
// ===== 步骤 52：控制面节点增删 =====

#[tokio::test]
async fn add_and_delete_node_round_trip() {
    let app = super::router(test_state(vec![], &[]));
    let body = r#"{"ip":"203.0.113.10","port":8080,"country":"us","tier":"residential","provider":"vendor-a","weight":100,"proto":"http"}"#;
    let (status, v) = send_json(app.clone(), "POST", "/api/v1/nodes", Some(body)).await;
    assert_eq!(status, StatusCode::CREATED, "resp={v}");
    assert_eq!(v["addr"], "203.0.113.10:8080");
    assert_eq!(v["country"], "US", "country 必须归一为大写");
    assert_eq!(v["tier"], "residential");
    assert_eq!(v["provider"], "vendor-a");

    // 新增即生效：详情立即可见。
    let (status, v) = send_json(app.clone(), "GET", "/api/v1/nodes/203.0.113.10:8080", None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(v["weight"], 100);

    // 删除 → 详情 404，再删也是 404（幂等语义如实返回）。
    let (status, v) = send_json(
        app.clone(),
        "DELETE",
        "/api/v1/nodes/203.0.113.10:8080",
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "resp={v}");
    assert_eq!(v["removed"], true);

    let (status, v) = send_json(app.clone(), "GET", "/api/v1/nodes/203.0.113.10:8080", None).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(v["code"], 404);

    let (status, _) = send_json(app, "DELETE", "/api/v1/nodes/203.0.113.10:8080", None).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn add_node_rejects_duplicate_and_invalid_fields() {
    let app = super::router(test_state(
        vec![test_node("127.0.0.5", 39_005, "US", "datacenter")],
        &[],
    ));

    // 实时池同址（含 seeds / free-* / 测试注入）⇒ 409，而不是落出重复节点。
    let dup = r#"{"ip":"127.0.0.5","port":39005,"country":"US","tier":"datacenter","provider":"other","weight":100}"#;
    let (status, v) = send_json(app.clone(), "POST", "/api/v1/nodes", Some(dup)).await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert_eq!(v["code"], 409);

    // 坏 IP / IPv6 / 保留 provider / 越界权重 / 未配对凭据 / 未知 proto ⇒ 400。
    for body in [
        r#"{"ip":"not-an-ip","port":8080,"country":"US","tier":"datacenter","provider":"v","weight":100}"#,
        r#"{"ip":"::1","port":8080,"country":"US","tier":"datacenter","provider":"v","weight":100}"#,
        r#"{"ip":"203.0.113.11","port":8080,"country":"US","tier":"datacenter","provider":"free-gh0","weight":100}"#,
        r#"{"ip":"203.0.113.12","port":8080,"country":"US","tier":"datacenter","provider":"v","weight":101}"#,
        r#"{"ip":"203.0.113.13","port":8080,"country":"US","tier":"datacenter","provider":"v","weight":100,"password":"only"}"#,
        r#"{"ip":"203.0.113.14","port":8080,"country":"US","tier":"datacenter","provider":"v","weight":100,"proto":"ftp"}"#,
    ] {
        let (status, v) = send_json(app.clone(), "POST", "/api/v1/nodes", Some(body)).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "body={body} resp={v}");
        assert_eq!(v["code"], 400);
    }
}

#[tokio::test]
async fn delete_free_pool_node_is_rejected_with_actionable_error() {
    let free = ProxyNode::new(
        "127.0.0.9".to_string(),
        39_009,
        None,
        None,
        "ZZ".to_string(),
        "free".to_string(),
        "free-gh0".to_string(),
        10,
    );
    let app = super::router(test_state(vec![free], &[]));
    let (status, v) = send_json(app, "DELETE", "/api/v1/nodes/127.0.0.9:39009", None).await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert_eq!(v["code"], 409);
    assert!(
        v["error"].as_str().unwrap().contains("FreePool"),
        "409 必须说明为什么删不掉: {v}"
    );
}

#[tokio::test]
async fn delete_unmanaged_test_node_is_rejected() {
    let mut state = test_state(vec![], &[]);
    // 模拟 TEST_POOL_NODES：实时池里有，托管清单里没有。
    state.router = Arc::new(RouterEngine::new(vec![test_node(
        "127.0.0.8",
        39_008,
        "US",
        "datacenter",
    )]));
    let app = super::router(state);
    let (status, v) = send_json(app, "DELETE", "/api/v1/nodes/127.0.0.8:39008", None).await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert!(
        v["error"]
            .as_str()
            .unwrap()
            .contains("not operator-managed"),
        "409 必须说明该节点不受控制面管理: {v}"
    );
}

#[tokio::test]
async fn weight_patch_persists_for_managed_nodes() {
    let path = std::env::temp_dir().join(format!(
        "ipp-admin-weight-{}-{}.json",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("clock")
            .as_nanos()
    ));
    let node = test_node("127.0.0.6", 39_006, "US", "datacenter");
    let registry =
        Arc::new(NodeRegistry::load(Some(path.clone()), vec![node.clone()]).expect("registry"));
    let state = AdminState {
        router: Arc::new(RouterEngine::new(vec![node])),
        nodes: registry,
        tenant_mgr: Arc::new(TenantManager::new()),
        metrics: Arc::new(MetricsRegistry::new()),
        admin_api_key: KEY.to_string(),
        autotune: Arc::new(AutotuneConfig::new()),
    };
    let app = super::router(state);
    let (status, _) = send_json(
        app,
        "PATCH",
        "/api/v1/nodes/127.0.0.6:39006/weight",
        Some(r#"{"weight":0}"#),
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    // 关键：重启视角重新加载文件，weight=0（坏节点不会满血复活）。
    let reloaded = NodeRegistry::load(Some(path.clone()), vec![]).expect("reload");
    assert_eq!(reloaded.snapshot()[0].weight, 0, "PATCH 调权必须持久化");

    let _ = std::fs::remove_file(&path);
    let mut tmp = path.as_os_str().to_os_string();
    tmp.push(format!(".tmp.{}", std::process::id()));
    let _ = std::fs::remove_file(std::path::PathBuf::from(tmp));
}

#[tokio::test]
async fn audit_layer_preserves_error_contract() {
    // 401：错误体仍是 {error, code:401}。
    let app = super::router(test_state(vec![], &[]));
    let req = Request::builder()
        .method("GET")
        .uri("/api/v1/health")
        .body(Body::empty())
        .expect("request");
    let res = app.oneshot(req).await.expect("oneshot");
    assert_eq!(res.status(), StatusCode::UNAUTHORIZED);
    let bytes = axum::body::to_bytes(res.into_body(), usize::MAX)
        .await
        .expect("body");
    let v: serde_json::Value = serde_json::from_slice(&bytes).expect("错误体必须是 JSON");
    assert_eq!(v["code"], 401);

    // 404 fallback：审计层必须放行到 fallback 才有这个 JSON 404。
    let app = super::router(test_state(vec![], &[]));
    let req = Request::builder()
        .method("GET")
        .uri("/api/v1/nope")
        .header("X-Admin-Key", KEY)
        .body(Body::empty())
        .expect("request");
    let res = app.oneshot(req).await.expect("oneshot");
    assert_eq!(res.status(), StatusCode::NOT_FOUND);
    let bytes = axum::body::to_bytes(res.into_body(), usize::MAX)
        .await
        .expect("body");
    let v: serde_json::Value = serde_json::from_slice(&bytes).expect("错误体必须是 JSON");
    assert_eq!(v["code"], 404);
}
