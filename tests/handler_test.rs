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
use sol_rpc_router::{
    config::Backend,
    handlers::{extract_rpc_method, proxy},
    health::HealthState,
    mock::MockKeyStore,
    state::AppState,
};
use std::collections::HashMap;
use std::sync::Arc;
use tower::ServiceExt; // for oneshot

async fn start_mock_backend() -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();

    tokio::spawn(async move {
        let app = Router::new().route("/", post(|| async { "{\"jsonrpc\":\"2.0\",\"result\":\"ok\",\"id\":1}" }));
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

    let health_state = Arc::new(HealthState::new(vec!["mock-backend".to_string()]));
    let mut label_to_url = HashMap::new();
    label_to_url.insert("mock-backend".to_string(), backend_url);

    let state = Arc::new(AppState {
        client,
        backends: vec![backend],
        keystore,
        method_routes: HashMap::new(),
        label_to_url,
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
        .body(Body::from(r#"{"jsonrpc":"2.0","method":"eth_blockNumber","params":[],"id":1}"#))
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
        label_to_url: HashMap::new(),
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
    keystore.rate_limited_keys.lock().unwrap().push("limit-key".to_string());

    let health_state = Arc::new(HealthState::new(vec![]));
    let state = Arc::new(AppState {
        client,
        backends: vec![],
        keystore,
        method_routes: HashMap::new(),
        label_to_url: HashMap::new(),
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
