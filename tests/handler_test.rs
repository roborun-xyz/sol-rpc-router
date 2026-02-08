use std::{collections::HashMap, sync::Arc};
use std::sync::atomic::AtomicBool;

use axum::{
    body::Body,
    http::{Request, StatusCode},
    middleware,
    routing::{get, post},
    Router,
};
use http_body_util::BodyExt;
use hyper_tls::HttpsConnector;
use hyper_util::client::legacy::Client;
use sol_rpc_router::{
    config::Backend,
    handlers::{extract_rpc_method, health_endpoint, proxy, RpcMethod},
    health::{BackendHealthStatus, HealthState},
    mock::MockKeyStore,
    state::{AppState, RuntimeBackend},
};
use tower::ServiceExt; // for oneshot

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
        weight: 100,
    };

    let runtime_backend = RuntimeBackend {
        config: backend,
        healthy: Arc::new(AtomicBool::new(true)),
    };

    let health_state = Arc::new(HealthState::new(vec!["mock-backend".to_string()]));

    let state = Arc::new(AppState {
        client,
        backends: vec![runtime_backend],
        keystore,
        method_routes: HashMap::new(),
        health_state,
        proxy_timeout_secs: 5,
    });

    let app = Router::new()
        .route("/", post(proxy))
        .with_state(state)
        .layer(middleware::from_fn(extract_rpc_method));

    let req = Request::builder()
        .method("POST")
        .uri("/?api-key=test-key")
        .header("content-type", "application/json")
        .body(Body::from(
            r#"{"jsonrpc":"2.0","method":"eth_blockNumber","params":[],"id":1}"#,
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
    let state = Arc::new(AppState {
        client,
        backends: vec![],
        keystore,
        method_routes: HashMap::new(),
        health_state,
        proxy_timeout_secs: 5,
    });

    let app = Router::new()
        .route("/", post(proxy))
        .with_state(state)
        .layer(middleware::from_fn(extract_rpc_method));

    let req = Request::builder()
        .method("POST")
        .uri("/?api-key=wrong-key")
        .header("content-type", "application/json")
        .body(Body::from(r#"{"jsonrpc":"2.0","method":"test","id":1}"#))
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
    let state = Arc::new(AppState {
        client,
        backends: vec![],
        keystore,
        method_routes: HashMap::new(),
        health_state,
        proxy_timeout_secs: 5,
    });

    let app = Router::new()
        .route("/", post(proxy))
        .with_state(state)
        .layer(middleware::from_fn(extract_rpc_method));

    let req = Request::builder()
        .method("POST")
        .uri("/?api-key=limit-key")
        .header("content-type", "application/json")
        .body(Body::from(r#"{"jsonrpc":"2.0","method":"test","id":1}"#))
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

    let state = Arc::new(AppState {
        client,
        backends: vec![],
        keystore,
        method_routes: HashMap::new(),
        health_state,
        proxy_timeout_secs: 5,
    });

    let app = Router::new()
        .route("/", post(proxy))
        .with_state(state)
        .layer(middleware::from_fn(extract_rpc_method));

    let req = Request::builder()
        .method("POST")
        .uri("/")
        .header("content-type", "application/json")
        .body(Body::from(r#"{"jsonrpc":"2.0","method":"test","id":1}"#))
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
    let state = Arc::new(AppState {
        client,
        backends: vec![],
        keystore,
        method_routes: HashMap::new(),
        health_state,
        proxy_timeout_secs: 5,
    });

    let app = Router::new()
        .route("/", post(proxy))
        .with_state(state)
        .layer(middleware::from_fn(extract_rpc_method));

    let req = Request::builder()
        .method("POST")
        .uri("/?api-key=err-key")
        .header("content-type", "application/json")
        .body(Body::from(r#"{"jsonrpc":"2.0","method":"test","id":1}"#))
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
        weight: 1,
    };

    let runtime_backend = RuntimeBackend {
        config: backend,
        healthy: Arc::new(AtomicBool::new(false)), // Start unhealthy
    };

    let health_state = Arc::new(HealthState::new(vec!["sick-backend".to_string()]));
    // Note: HealthState is for the background loop, RuntimeBackend.healthy is for the hot path.
    // In this test we manually set the atomic boolean.

    let state = Arc::new(AppState {
        client,
        backends: vec![runtime_backend],
        keystore,
        method_routes: HashMap::new(),
        health_state,
        proxy_timeout_secs: 5,
    });

    let app = Router::new()
        .route("/", post(proxy))
        .with_state(state)
        .layer(middleware::from_fn(extract_rpc_method));

    let req = Request::builder()
        .method("POST")
        .uri("/?api-key=test-key")
        .header("content-type", "application/json")
        .body(Body::from(r#"{"jsonrpc":"2.0","method":"test","id":1}"#))
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
        .map(|b| RuntimeBackend {
            config: b.clone(),
            healthy: Arc::new(AtomicBool::new(true)),
        })
        .collect();

    Arc::new(AppState {
        client,
        backends: runtime_backends,
        keystore,
        method_routes: HashMap::new(),
        health_state,
        proxy_timeout_secs: 5,
    })
}

fn test_backends() -> Vec<Backend> {
    vec![
        Backend {
            label: "a".to_string(),
            url: "http://a".to_string(),
            ws_url: None,
            weight: 1,
        },
        Backend {
            label: "b".to_string(),
            url: "http://b".to_string(),
            ws_url: None,
            weight: 1,
        },
    ]
}

#[tokio::test]
async fn test_health_endpoint_all_healthy() {
    let state = make_health_state(&test_backends());
    let app = Router::new()
        .route("/health", get(health_endpoint))
        .with_state(state);

    let req = Request::builder()
        .uri("/health")
        .body(Body::empty())
        .unwrap();

    let response = app.oneshot(req).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);

    let body = response.into_body().collect().await.unwrap().to_bytes();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(json["overall_status"], "healthy");
    assert!(json["backends"][0]["healthy"].as_bool().unwrap());
    assert!(json["backends"][1]["healthy"].as_bool().unwrap());
}

#[tokio::test]
async fn test_health_endpoint_mixed() {
    let state = make_health_state(&test_backends());

    // Update AtomicBool for the hot path (not used by health_endpoint directly, but good practice)
    // Actually health_endpoint reads from HealthState (RwLock), not AtomicBool.
    // The health check loop updates both.
    // So for this test we update HealthState.
    let mut unhealthy = BackendHealthStatus::default();
    unhealthy.healthy = false;
    state.health_state.update_status("b", unhealthy);

    let app = Router::new()
        .route("/health", get(health_endpoint))
        .with_state(state);

    let req = Request::builder()
        .uri("/health")
        .body(Body::empty())
        .unwrap();

    let response = app.oneshot(req).await.unwrap();
    let body = response.into_body().collect().await.unwrap().to_bytes();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(json["overall_status"], "healthy");

    let backends = json["backends"].as_array().unwrap();
    let a = backends.iter().find(|b| b["label"] == "a").unwrap();
    let b = backends.iter().find(|b| b["label"] == "b").unwrap();
    assert!(a["healthy"].as_bool().unwrap());
    assert!(!b["healthy"].as_bool().unwrap());
}

#[tokio::test]
async fn test_health_endpoint_all_unhealthy() {
    let state = make_health_state(&test_backends());
    for label in &["a", "b"] {
        let mut unhealthy = BackendHealthStatus::default();
        unhealthy.healthy = false;
        state.health_state.update_status(label, unhealthy);
    }

    let app = Router::new()
        .route("/health", get(health_endpoint))
        .with_state(state);

    let req = Request::builder()
        .uri("/health")
        .body(Body::empty())
        .unwrap();

    let response = app.oneshot(req).await.unwrap();
    let body = response.into_body().collect().await.unwrap().to_bytes();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(json["overall_status"], "unhealthy");
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
