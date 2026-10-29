use std::sync::Arc;

use arc_swap::ArcSwap;
use axum::{
    body::Body,
    http::{Request, StatusCode},
    middleware,
    routing::post,
    Router,
};
use http_body_util::BodyExt;
use hyper_tls::HttpsConnector;
use hyper_util::client::legacy::Client;
use metrics_exporter_prometheus::PrometheusBuilder;
use sol_rpc_router::{
    config::Backend,
    handlers::{extract_rpc_method, RpcMethod},
    health::{BackendHealthStatus, HealthState},
    mock::MockKeyStore,
    router::{http_router, metrics_router},
    state::{AppState, RouterState, RuntimeBackend},
};
use tower::ServiceExt; // for oneshot

fn make_app_state(
    client: Client<HttpsConnector<hyper_util::client::legacy::connect::HttpConnector>, Body>,
    keystore: Arc<MockKeyStore>,
    backends: Vec<RuntimeBackend>,
    health_state: Arc<HealthState>,
) -> Arc<AppState> {
    let router_state = RouterState::simple(backends, health_state);

    Arc::new(AppState::new(
        client,
        keystore,
        Arc::new(ArcSwap::from_pointee(router_state)),
    ))
}

async fn start_mock_backend() -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();

    tokio::spawn(async move {
        let app = Router::new().route(
            "/",
            post(|| async { "{\"jsonrpc\":\"2.0\",\"result\":\"ok\",\"id\":1}" }),
        );
        axum::serve(listener, app).await.unwrap();
    });

    format!("http://{}", addr)
}

#[tokio::test]
async fn test_proxy_handler_success() {
    let backend_url = start_mock_backend().await;

    let https = HttpsConnector::new();
    let client = Client::builder(hyper_util::rt::TokioExecutor::new()).build(https);
    let keystore = Arc::new(MockKeyStore::new());
    keystore.add_key("test-key", "tester", 100);

    let backend = Backend {
        label: "mock-backend".to_string(),
        url: backend_url.clone(),
        ws_url: None,
        max_rps: 0,
        weight: 100,
    };

    let runtime_backend = RuntimeBackend::new(backend, true);

    let health_state = Arc::new(HealthState::new(vec!["mock-backend".to_string()]));
    let state = make_app_state(client, keystore, vec![runtime_backend], health_state);

    let app = http_router(state);

    let req = Request::builder()
        .method("POST")
        .uri("/?api-key=test-key")
        .header("content-type", "application/json")
        .body(Body::from(
            r#"{"jsonrpc":"2.0","method":"getSlot","params":[],"id":1}"#,
        ))
        .unwrap();

    let response = app.oneshot(req).await.unwrap();

    assert_eq!(response.status(), StatusCode::OK);

    let body = response.into_body().collect().await.unwrap().to_bytes();
    let body_str = String::from_utf8(body.to_vec()).unwrap();
    assert!(body_str.contains("result"));
}

#[tokio::test]
async fn test_proxy_handler_unauthorized() {
    let https = HttpsConnector::new();
    let client = Client::builder(hyper_util::rt::TokioExecutor::new()).build(https);
    let keystore = Arc::new(MockKeyStore::new());
    // No keys added

    let health_state = Arc::new(HealthState::new(vec![]));
    let state = make_app_state(client, keystore, vec![], health_state);

    let app = http_router(state);

    let req = Request::builder()
        .method("POST")
        .uri("/?api-key=wrong-key")
        .header("content-type", "application/json")
        .body(Body::from(r#"{"jsonrpc":"2.0","method":"getSlot","id":1}"#))
        .unwrap();

    let response = app.oneshot(req).await.unwrap();

    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn test_proxy_handler_rate_limited() {
    let https = HttpsConnector::new();
    let client = Client::builder(hyper_util::rt::TokioExecutor::new()).build(https);
    let keystore = Arc::new(MockKeyStore::new());
    keystore.add_key("limit-key", "tester", 10);
    keystore
        .rate_limited_keys
        .lock()
        .unwrap()
        .push("limit-key".to_string());

    let health_state = Arc::new(HealthState::new(vec![]));
    let state = make_app_state(client, keystore, vec![], health_state);

    let app = http_router(state);

    let req = Request::builder()
        .method("POST")
        .uri("/?api-key=limit-key")
        .header("content-type", "application/json")
        .body(Body::from(r#"{"jsonrpc":"2.0","method":"getSlot","id":1}"#))
        .unwrap();

    let response = app.oneshot(req).await.unwrap();

    assert_eq!(response.status(), StatusCode::TOO_MANY_REQUESTS);
}

// --- Proxy error path tests ---

#[tokio::test]
async fn test_proxy_no_api_key() {
    let https = HttpsConnector::new();
    let client = Client::builder(hyper_util::rt::TokioExecutor::new()).build(https);
    let keystore = Arc::new(MockKeyStore::new());
    let health_state = Arc::new(HealthState::new(vec![]));
    let state = make_app_state(client, keystore, vec![], health_state);

    let app = http_router(state);

    let req = Request::builder()
        .method("POST")
        .uri("/")
        .header("content-type", "application/json")
        .body(Body::from(r#"{"jsonrpc":"2.0","method":"getSlot","id":1}"#))
        .unwrap();

    let response = app.oneshot(req).await.unwrap();
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn test_proxy_keystore_internal_error() {
    let https = HttpsConnector::new();
    let client = Client::builder(hyper_util::rt::TokioExecutor::new()).build(https);
    let keystore = Arc::new(MockKeyStore::new());
    keystore.add_key("err-key", "tester", 100);
    keystore.set_error("err-key", "Redis connection failed");

    let health_state = Arc::new(HealthState::new(vec![]));
    let state = make_app_state(client, keystore, vec![], health_state);

    let app = http_router(state);

    let req = Request::builder()
        .method("POST")
        .uri("/?api-key=err-key")
        .header("content-type", "application/json")
        .body(Body::from(r#"{"jsonrpc":"2.0","method":"getSlot","id":1}"#))
        .unwrap();

    let response = app.oneshot(req).await.unwrap();
    assert_eq!(response.status(), StatusCode::INTERNAL_SERVER_ERROR);
}

#[tokio::test]
async fn test_proxy_no_healthy_backends() {
    let backend_url = start_mock_backend().await;

    let https = HttpsConnector::new();
    let client = Client::builder(hyper_util::rt::TokioExecutor::new()).build(https);
    let keystore = Arc::new(MockKeyStore::new());
    keystore.add_key("test-key", "tester", 100);

    let backend = Backend {
        label: "sick-backend".to_string(),
        url: backend_url.clone(),
        ws_url: None,
        max_rps: 0,
        weight: 1,
    };

    let runtime_backend = RuntimeBackend::new(backend, false); // Start unhealthy

    let health_state = Arc::new(HealthState::new(vec!["sick-backend".to_string()]));
    let state = make_app_state(client, keystore, vec![runtime_backend], health_state);

    let app = http_router(state);

    let req = Request::builder()
        .method("POST")
        .uri("/?api-key=test-key")
        .header("content-type", "application/json")
        .body(Body::from(r#"{"jsonrpc":"2.0","method":"getSlot","id":1}"#))
        .unwrap();

    let response = app.oneshot(req).await.unwrap();
    assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
}

// --- Health endpoint tests ---

fn make_health_state(backends: &[Backend]) -> Arc<AppState> {
    let https = HttpsConnector::new();
    let client = Client::builder(hyper_util::rt::TokioExecutor::new()).build(https);
    let keystore = Arc::new(MockKeyStore::new());
    let labels: Vec<String> = backends.iter().map(|b| b.label.clone()).collect();
    let health_state = Arc::new(HealthState::new(labels));

    let runtime_backends = backends
        .iter()
        .map(|b| RuntimeBackend::new(b.clone(), true))
        .collect();

    make_app_state(client, keystore, runtime_backends, health_state)
}

fn test_backends() -> Vec<Backend> {
    vec![
        Backend {
            label: "a".to_string(),
            url: "http://a".to_string(),
            ws_url: None,
            max_rps: 0,
            weight: 1,
        },
        Backend {
            label: "b".to_string(),
            url: "http://b".to_string(),
            ws_url: None,
            max_rps: 0,
            weight: 1,
        },
    ]
}

async fn get_json(app: Router, uri: &str) -> (StatusCode, serde_json::Value) {
    let req = Request::builder().uri(uri).body(Body::empty()).unwrap();
    let response = app.oneshot(req).await.unwrap();
    let status = response.status();
    let body = response.into_body().collect().await.unwrap().to_bytes();
    (status, serde_json::from_slice(&body).unwrap())
}

/// The metrics listener's router, with a recorder that is never installed
/// globally so tests stay independent.
fn test_metrics_router(state: Arc<AppState>) -> Router {
    let handle = PrometheusBuilder::new().build_recorder().handle();
    metrics_router(handle, state)
}

fn mark_unhealthy(state: &Arc<AppState>, label: &str) {
    let status = BackendHealthStatus {
        healthy: false,
        consecutive_failures: 3,
        last_error: Some("connect to upstream.internal.example:8899 failed".into()),
        ..Default::default()
    };
    state.state.load().health_state.update_status(label, status);
}

#[tokio::test]
async fn test_public_health_all_healthy() {
    let state = make_health_state(&test_backends());
    let (status, json) = get_json(http_router(state), "/health").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(json, serde_json::json!({ "status": "ok" }));
}

#[tokio::test]
async fn test_public_health_mixed_hides_backend_details() {
    let state = make_health_state(&test_backends());
    mark_unhealthy(&state, "b");

    let req = Request::builder()
        .uri("/health")
        .body(Body::empty())
        .unwrap();
    let response = http_router(state).oneshot(req).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let body = response.into_body().collect().await.unwrap().to_bytes();
    let text = std::str::from_utf8(&body).unwrap();
    assert_eq!(text, r#"{"status":"ok"}"#);
    for leak in [
        "\"a\"",
        "\"b\"",
        "upstream.internal",
        "consecutive",
        "backends",
    ] {
        assert!(!text.contains(leak), "public /health leaked {leak}: {text}");
    }
}

#[tokio::test]
async fn test_public_health_all_unhealthy() {
    let state = make_health_state(&test_backends());
    mark_unhealthy(&state, "a");
    mark_unhealthy(&state, "b");

    let (status, json) = get_json(http_router(state), "/health").await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(json, serde_json::json!({ "status": "unavailable" }));
}

#[tokio::test]
async fn test_detailed_health_all_healthy() {
    let state = make_health_state(&test_backends());
    let (status, json) = get_json(test_metrics_router(state), "/health").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(json["overall_status"], "healthy");
    assert!(json["backends"][0]["healthy"].as_bool().unwrap());
    assert!(json["backends"][1]["healthy"].as_bool().unwrap());
}

#[tokio::test]
async fn test_detailed_health_mixed() {
    let state = make_health_state(&test_backends());
    mark_unhealthy(&state, "b");

    let (status, json) = get_json(test_metrics_router(state), "/health").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(json["overall_status"], "healthy");
    assert_eq!(json["healthy_backends"], 1);

    let backends = json["backends"].as_array().unwrap();
    let a = backends.iter().find(|b| b["label"] == "a").unwrap();
    let b = backends.iter().find(|b| b["label"] == "b").unwrap();
    assert!(a["healthy"].as_bool().unwrap());
    assert!(!b["healthy"].as_bool().unwrap());
    assert_eq!(b["consecutive_failures"], 3);
    assert_eq!(
        b["last_error"],
        "connect to upstream.internal.example:8899 failed"
    );
}

#[tokio::test]
async fn test_detailed_health_all_unhealthy() {
    let state = make_health_state(&test_backends());
    mark_unhealthy(&state, "a");
    mark_unhealthy(&state, "b");

    let (status, json) = get_json(test_metrics_router(state), "/health").await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(json["overall_status"], "unhealthy");
}

#[tokio::test]
async fn test_metrics_listener_still_serves_metrics() {
    let state = make_health_state(&test_backends());
    let req = Request::builder()
        .uri("/metrics")
        .body(Body::empty())
        .unwrap();
    let response = test_metrics_router(state).oneshot(req).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
}

// --- extract_rpc_method middleware tests ---

#[tokio::test]
async fn test_extract_rpc_method_valid_json() {
    let app = Router::new()
        .route(
            "/",
            post(|req: Request<Body>| async move {
                match req.extensions().get::<RpcMethod>() {
                    Some(m) => m.0.clone(),
                    None => "none".to_string(),
                }
            }),
        )
        .layer(middleware::from_fn(extract_rpc_method));

    let req = Request::builder()
        .method("POST")
        .uri("/")
        .header("content-type", "application/json")
        .body(Body::from(r#"{"method":"getSlot"}"#))
        .unwrap();

    let response = app.oneshot(req).await.unwrap();
    let body = response.into_body().collect().await.unwrap().to_bytes();
    assert_eq!(String::from_utf8(body.to_vec()).unwrap(), "getSlot");
}

#[tokio::test]
async fn test_extract_rpc_method_no_method_field() {
    let app = Router::new()
        .route(
            "/",
            post(|req: Request<Body>| async move {
                match req.extensions().get::<RpcMethod>() {
                    Some(m) => m.0.clone(),
                    None => "none".to_string(),
                }
            }),
        )
        .layer(middleware::from_fn(extract_rpc_method));

    let req = Request::builder()
        .method("POST")
        .uri("/")
        .header("content-type", "application/json")
        .body(Body::from(r#"{"id":1}"#))
        .unwrap();

    let response = app.oneshot(req).await.unwrap();
    let body = response.into_body().collect().await.unwrap().to_bytes();
    assert_eq!(String::from_utf8(body.to_vec()).unwrap(), "none");
}

#[tokio::test]
async fn test_extract_rpc_method_invalid_json() {
    let app = Router::new()
        .route(
            "/",
            post(|req: Request<Body>| async move {
                match req.extensions().get::<RpcMethod>() {
                    Some(m) => m.0.clone(),
                    None => "none".to_string(),
                }
            }),
        )
        .layer(middleware::from_fn(extract_rpc_method));

    let req = Request::builder()
        .method("POST")
        .uri("/")
        .header("content-type", "application/json")
        .body(Body::from("not json at all"))
        .unwrap();

    let response = app.oneshot(req).await.unwrap();
    let body = response.into_body().collect().await.unwrap().to_bytes();
    assert_eq!(String::from_utf8(body.to_vec()).unwrap(), "none");
}

// --- request body read errors ---

fn body_reader_app() -> Router {
    Router::new()
        .route("/", post(|| async { "ok" }))
        .layer(middleware::from_fn(extract_rpc_method))
}

async fn error_code(response: axum::response::Response) -> (StatusCode, serde_json::Value) {
    let status = response.status();
    let body = response.into_body().collect().await.unwrap().to_bytes();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(json["jsonrpc"], "2.0");
    assert_eq!(json["error"]["code"], -32600);
    (status, json)
}

#[tokio::test]
async fn test_body_too_large_returns_413() {
    let req = Request::builder()
        .method("POST")
        .uri("/")
        .body(Body::from(vec![b' '; 10 * 1024 * 1024 + 1]))
        .unwrap();

    let (status, json) = error_code(body_reader_app().oneshot(req).await.unwrap()).await;
    assert_eq!(status, StatusCode::PAYLOAD_TOO_LARGE);
    assert_eq!(json["error"]["message"], "Request body too large");
}

#[tokio::test]
async fn test_streamed_body_too_large_returns_413() {
    // No size hint, so the limit is only hit while streaming.
    let chunks = futures_util::stream::iter(
        (0..11).map(|_| Ok::<_, std::io::Error>(vec![b' '; 1024 * 1024])),
    );
    let req = Request::builder()
        .method("POST")
        .uri("/")
        .body(Body::from_stream(chunks))
        .unwrap();

    let (status, _) = error_code(body_reader_app().oneshot(req).await.unwrap()).await;
    assert_eq!(status, StatusCode::PAYLOAD_TOO_LARGE);
}

#[tokio::test]
async fn test_body_read_error_returns_400() {
    // Simulates a client that disconnects mid-upload.
    let chunks = futures_util::stream::iter(vec![
        Ok(bytes::Bytes::from_static(br#"{"method":"#)),
        Err(std::io::Error::new(
            std::io::ErrorKind::ConnectionReset,
            "connection reset",
        )),
    ]);
    let req = Request::builder()
        .method("POST")
        .uri("/")
        .body(Body::from_stream(chunks))
        .unwrap();

    let (status, json) = error_code(body_reader_app().oneshot(req).await.unwrap()).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(json["error"]["message"], "Failed to read request body");
}

#[tokio::test(start_paused = true)]
async fn test_body_read_timeout_returns_408() {
    // A body that never finishes; paused time jumps straight to the timeout.
    let chunks = futures_util::stream::pending::<Result<bytes::Bytes, std::io::Error>>();
    let req = Request::builder()
        .method("POST")
        .uri("/")
        .body(Body::from_stream(chunks))
        .unwrap();

    let (status, json) = error_code(body_reader_app().oneshot(req).await.unwrap()).await;
    assert_eq!(status, StatusCode::REQUEST_TIMEOUT);
    assert_eq!(json["error"]["message"], "Timed out reading request body");
}
