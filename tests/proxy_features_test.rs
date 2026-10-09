//! Integration tests for header auth, failover, fan-out, blocked methods,
//! batch requests and request ids. Every backend is an in-process axum server
//! bound to 127.0.0.1:0.

use std::{
    collections::HashMap,
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    },
};

use arc_swap::ArcSwap;
use axum::{
    body::{Body, Bytes},
    http::{HeaderMap, Request, StatusCode},
    middleware,
    routing::{get, post},
    Router,
};
use http_body_util::BodyExt;
use hyper_tls::HttpsConnector;
use hyper_util::client::legacy::Client;
use sol_rpc_router::{
    config::{Backend, ProxyConfig},
    handlers::{
        extract_rpc_method, health_endpoint, proxy, request_id, ATTEMPTS_HEADER, BACKEND_HEADER,
        REQUEST_ID_HEADER,
    },
    health::HealthState,
    mock::MockKeyStore,
    state::{AppState, RouterState, RuntimeBackend},
};
use tower::ServiceExt;

type Handler = Arc<dyn Fn(&HeaderMap, &Bytes) -> (StatusCode, String) + Send + Sync>;

/// A mock backend with a hit counter and a programmable response.
struct MockBackend {
    url: String,
    hits: Arc<AtomicUsize>,
}

async fn start_backend(handler: Handler) -> MockBackend {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let hits = Arc::new(AtomicUsize::new(0));
    let hits_clone = hits.clone();

    tokio::spawn(async move {
        let app = Router::new().route(
            "/",
            post(move |headers: HeaderMap, body: Bytes| {
                let handler = handler.clone();
                let hits = hits_clone.clone();
                async move {
                    hits.fetch_add(1, Ordering::SeqCst);
                    let (status, body) = handler(&headers, &body);
                    (status, [("content-type", "application/json")], body)
                }
            }),
        );
        axum::serve(listener, app).await.unwrap();
    });

    MockBackend {
        url: format!("http://{}", addr),
        hits,
    }
}

fn ok_backend() -> Handler {
    Arc::new(|_, _| {
        (
            StatusCode::OK,
            r#"{"jsonrpc":"2.0","result":123,"id":1}"#.to_string(),
        )
    })
}

fn status_backend(status: StatusCode) -> Handler {
    Arc::new(move |_, _| (status, format!("{{\"error\":\"{}\"}}", status.as_u16())))
}

/// A URL nothing listens on: bind, read the port, drop the listener.
async fn dead_backend_url() -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    drop(listener);
    format!("http://{}", addr)
}

fn backend(label: &str, url: &str, weight: u32) -> RuntimeBackend {
    RuntimeBackend::new(
        Backend {
            label: label.to_string(),
            url: url.to_string(),
            ws_url: None,
            weight,
        },
        true,
    )
}

struct TestApp {
    router: Router,
    #[allow(dead_code)]
    keystore: Arc<MockKeyStore>,
}

fn build_app(
    backends: Vec<RuntimeBackend>,
    method_routes: HashMap<String, String>,
    proxy_cfg: ProxyConfig,
) -> TestApp {
    let https = HttpsConnector::new();
    let client = Client::builder(hyper_util::rt::TokioExecutor::new()).build(https);
    let keystore = Arc::new(MockKeyStore::new());
    keystore.add_key("test-key", "tester", 100);

    let labels = backends.iter().map(|b| b.config.label.clone()).collect();
    let health_state = Arc::new(HealthState::new(labels));

    let mut router_state = RouterState::simple(backends, health_state);
    router_state.method_routes = method_routes;
    router_state.fanout_methods = proxy_cfg.fanout_methods.iter().cloned().collect();
    router_state.blocked_methods = proxy_cfg.blocked_methods.iter().cloned().collect();
    router_state.proxy = proxy_cfg;

    let state = Arc::new(AppState {
        client,
        keystore: keystore.clone(),
        state: Arc::new(ArcSwap::from_pointee(router_state)),
    });

    let router = Router::new()
        .route("/", post(proxy))
        .route("/health", get(health_endpoint))
        .with_state(state)
        .layer(middleware::from_fn(extract_rpc_method))
        .layer(middleware::from_fn(request_id));

    TestApp { router, keystore }
}

fn rpc_request(uri: &str, method: &str) -> Request<Body> {
    Request::builder()
        .method("POST")
        .uri(uri)
        .header("content-type", "application/json")
        .body(Body::from(format!(
            r#"{{"jsonrpc":"2.0","method":"{}","params":[],"id":1}}"#,
            method
        )))
        .unwrap()
}

async fn body_json(resp: axum::response::Response) -> serde_json::Value {
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    serde_json::from_slice(&bytes)
        .unwrap_or_else(|_| panic!("body is not JSON: {}", String::from_utf8_lossy(&bytes)))
}

fn fast_proxy(max_retries: u32) -> ProxyConfig {
    ProxyConfig {
        timeout_secs: 2,
        max_retries,
        ..Default::default()
    }
}

// --- Authentication -------------------------------------------------------

#[tokio::test]
async fn auth_via_x_api_key_header() {
    let b = start_backend(ok_backend()).await;
    let app = build_app(vec![backend("b", &b.url, 1)], HashMap::new(), fast_proxy(0));

    let mut req = rpc_request("/", "getSlot");
    req.headers_mut()
        .insert("x-api-key", "test-key".parse().unwrap());
    let resp = app.router.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(b.hits.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn auth_via_bearer_token() {
    let b = start_backend(ok_backend()).await;
    let app = build_app(vec![backend("b", &b.url, 1)], HashMap::new(), fast_proxy(0));

    let mut req = rpc_request("/", "getSlot");
    req.headers_mut()
        .insert("authorization", "Bearer test-key".parse().unwrap());
    let resp = app.router.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
}

#[tokio::test]
async fn auth_failure_returns_jsonrpc_error_with_id() {
    let app = build_app(vec![], HashMap::new(), fast_proxy(0));
    let resp = app
        .router
        .oneshot(rpc_request("/?api-key=nope", "getSlot"))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    let json = body_json(resp).await;
    assert_eq!(json["jsonrpc"], "2.0");
    assert_eq!(json["id"], 1);
    assert_eq!(json["error"]["code"], -32001);
}

#[tokio::test]
async fn rate_limited_returns_retry_after() {
    let app = build_app(vec![], HashMap::new(), fast_proxy(0));
    app.keystore.add_key("limited", "tester", 1);
    app.keystore
        .rate_limited_keys
        .lock()
        .unwrap()
        .push("limited".to_string());

    let resp = app
        .router
        .oneshot(rpc_request("/?api-key=limited", "getSlot"))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::TOO_MANY_REQUESTS);
    assert_eq!(resp.headers().get("retry-after").unwrap(), "1");
    let json = body_json(resp).await;
    assert_eq!(json["error"]["code"], -32005);
}

#[tokio::test]
async fn router_credentials_are_not_forwarded_upstream() {
    // Backend rejects anything that still carries router credentials.
    let strict: Handler = Arc::new(|headers, _| {
        if headers.contains_key("x-api-key") || headers.contains_key("authorization") {
            (StatusCode::IM_A_TEAPOT, "leaked".into())
        } else {
            (StatusCode::OK, r#"{"result":"clean"}"#.into())
        }
    });
    let b = start_backend(strict).await;
    let app = build_app(vec![backend("b", &b.url, 1)], HashMap::new(), fast_proxy(0));

    let mut req = rpc_request("/?api-key=test-key", "getSlot");
    req.headers_mut()
        .insert("x-api-key", "test-key".parse().unwrap());
    req.headers_mut()
        .insert("authorization", "Bearer test-key".parse().unwrap());
    let resp = app.router.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
}

// --- Failover ---------------------------------------------------------------

#[tokio::test]
async fn failover_on_5xx_to_another_backend() {
    let bad = start_backend(status_backend(StatusCode::SERVICE_UNAVAILABLE)).await;
    let good = start_backend(ok_backend()).await;

    // Pin getSlot to the bad backend so the first attempt is deterministic.
    let mut routes = HashMap::new();
    routes.insert("getSlot".to_string(), "bad".to_string());

    let app = build_app(
        vec![backend("bad", &bad.url, 1), backend("good", &good.url, 1)],
        routes,
        fast_proxy(1),
    );

    let resp = app
        .router
        .oneshot(rpc_request("/?api-key=test-key", "getSlot"))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(resp.headers().get(BACKEND_HEADER).unwrap(), "good");
    assert_eq!(resp.headers().get(ATTEMPTS_HEADER).unwrap(), "2");
    assert_eq!(bad.hits.load(Ordering::SeqCst), 1);
    assert_eq!(good.hits.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn failover_on_connection_refused() {
    let dead = dead_backend_url().await;
    let good = start_backend(ok_backend()).await;

    let mut routes = HashMap::new();
    routes.insert("getSlot".to_string(), "dead".to_string());

    let app = build_app(
        vec![backend("dead", &dead, 1), backend("good", &good.url, 1)],
        routes,
        fast_proxy(1),
    );

    let resp = app
        .router
        .oneshot(rpc_request("/?api-key=test-key", "getSlot"))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(resp.headers().get(BACKEND_HEADER).unwrap(), "good");
    assert_eq!(good.hits.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn no_retry_when_max_retries_is_zero() {
    let bad = start_backend(status_backend(StatusCode::SERVICE_UNAVAILABLE)).await;
    let good = start_backend(ok_backend()).await;

    let mut routes = HashMap::new();
    routes.insert("getSlot".to_string(), "bad".to_string());

    let app = build_app(
        vec![backend("bad", &bad.url, 1), backend("good", &good.url, 1)],
        routes,
        fast_proxy(0),
    );

    let resp = app
        .router
        .oneshot(rpc_request("/?api-key=test-key", "getSlot"))
        .await
        .unwrap();
    // Upstream status passes through untouched.
    assert_eq!(resp.status(), StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(resp.headers().get(ATTEMPTS_HEADER).unwrap(), "1");
    assert_eq!(good.hits.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn non_retryable_status_passes_through() {
    let bad = start_backend(status_backend(StatusCode::BAD_REQUEST)).await;
    let good = start_backend(ok_backend()).await;

    let mut routes = HashMap::new();
    routes.insert("getSlot".to_string(), "bad".to_string());

    let app = build_app(
        vec![backend("bad", &bad.url, 1), backend("good", &good.url, 1)],
        routes,
        fast_proxy(3),
    );

    let resp = app
        .router
        .oneshot(rpc_request("/?api-key=test-key", "getSlot"))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    assert_eq!(good.hits.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn all_backends_dead_returns_502_jsonrpc_error() {
    let dead1 = dead_backend_url().await;
    let dead2 = dead_backend_url().await;
    let app = build_app(
        vec![backend("d1", &dead1, 1), backend("d2", &dead2, 1)],
        HashMap::new(),
        fast_proxy(5),
    );

    let resp = app
        .router
        .oneshot(rpc_request("/?api-key=test-key", "getSlot"))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_GATEWAY);
    // Only two backends exist, so only two attempts regardless of max_retries.
    assert_eq!(resp.headers().get(ATTEMPTS_HEADER).unwrap(), "2");
    let json = body_json(resp).await;
    assert_eq!(json["error"]["code"], -32011);
    assert_eq!(json["id"], 1);
}

// --- Fan-out ----------------------------------------------------------------

#[tokio::test]
async fn fanout_returns_first_successful_result_and_hits_everyone() {
    let erroring: Handler = Arc::new(|_, _| {
        (
            StatusCode::OK,
            r#"{"jsonrpc":"2.0","error":{"code":-32002,"message":"Blockhash not found"},"id":1}"#
                .into(),
        )
    });
    let bad = start_backend(erroring).await;
    let good = start_backend(ok_backend()).await;
    let dead = dead_backend_url().await;

    let cfg = ProxyConfig {
        timeout_secs: 2,
        fanout_methods: vec!["sendTransaction".into()],
        ..Default::default()
    };
    let app = build_app(
        vec![
            backend("bad", &bad.url, 1),
            backend("good", &good.url, 1),
            backend("dead", &dead, 1),
        ],
        HashMap::new(),
        cfg,
    );

    let resp = app
        .router
        .oneshot(rpc_request("/?api-key=test-key", "sendTransaction"))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(resp.headers().get(BACKEND_HEADER).unwrap(), "good");
    let json = body_json(resp).await;
    assert_eq!(json["result"], 123);

    // Spawned sends keep going; give them a moment to land.
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    assert_eq!(bad.hits.load(Ordering::SeqCst), 1);
    assert_eq!(good.hits.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn fanout_surfaces_upstream_error_when_nobody_succeeds() {
    let erroring: Handler = Arc::new(|_, _| {
        (
            StatusCode::OK,
            r#"{"jsonrpc":"2.0","error":{"code":-32002,"message":"Blockhash not found"},"id":1}"#
                .into(),
        )
    });
    let b1 = start_backend(erroring.clone()).await;
    let b2 = start_backend(erroring).await;

    let cfg = ProxyConfig {
        timeout_secs: 2,
        fanout_methods: vec!["sendTransaction".into()],
        ..Default::default()
    };
    let app = build_app(
        vec![backend("b1", &b1.url, 1), backend("b2", &b2.url, 1)],
        HashMap::new(),
        cfg,
    );

    let resp = app
        .router
        .oneshot(rpc_request("/?api-key=test-key", "sendTransaction"))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let json = body_json(resp).await;
    assert_eq!(json["error"]["message"], "Blockhash not found");
}

#[tokio::test]
async fn fanout_methods_do_not_fan_out_other_methods() {
    let b1 = start_backend(ok_backend()).await;
    let b2 = start_backend(ok_backend()).await;

    let cfg = ProxyConfig {
        timeout_secs: 2,
        fanout_methods: vec!["sendTransaction".into()],
        ..Default::default()
    };
    let app = build_app(
        vec![backend("b1", &b1.url, 1), backend("b2", &b2.url, 1)],
        HashMap::new(),
        cfg,
    );

    let resp = app
        .router
        .oneshot(rpc_request("/?api-key=test-key", "getSlot"))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    assert_eq!(
        b1.hits.load(Ordering::SeqCst) + b2.hits.load(Ordering::SeqCst),
        1
    );
}

// --- Blocked methods & batches ---------------------------------------------

#[tokio::test]
async fn blocked_method_is_rejected_before_upstream() {
    let b = start_backend(ok_backend()).await;
    let cfg = ProxyConfig {
        blocked_methods: vec!["getProgramAccounts".into()],
        ..Default::default()
    };
    let app = build_app(vec![backend("b", &b.url, 1)], HashMap::new(), cfg);

    let resp = app
        .router
        .oneshot(rpc_request("/?api-key=test-key", "getProgramAccounts"))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::FORBIDDEN);
    let json = body_json(resp).await;
    assert_eq!(json["error"]["code"], -32601);
    assert_eq!(b.hits.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn batch_with_blocked_method_is_rejected() {
    let b = start_backend(ok_backend()).await;
    let cfg = ProxyConfig {
        blocked_methods: vec!["getProgramAccounts".into()],
        ..Default::default()
    };
    let app = build_app(vec![backend("b", &b.url, 1)], HashMap::new(), cfg);

    let req = Request::builder()
        .method("POST")
        .uri("/?api-key=test-key")
        .header("content-type", "application/json")
        .body(Body::from(
            r#"[{"jsonrpc":"2.0","method":"getSlot","id":1},{"jsonrpc":"2.0","method":"getProgramAccounts","id":2}]"#,
        ))
        .unwrap();
    let resp = app.router.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::FORBIDDEN);
    assert_eq!(b.hits.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn batch_request_is_forwarded() {
    let echo: Handler = Arc::new(|_, body| {
        let v: serde_json::Value = serde_json::from_slice(body).unwrap();
        assert!(v.is_array(), "backend should receive the batch intact");
        (
            StatusCode::OK,
            r#"[{"result":1,"id":1},{"result":2,"id":2}]"#.into(),
        )
    });
    let b = start_backend(echo).await;
    let app = build_app(vec![backend("b", &b.url, 1)], HashMap::new(), fast_proxy(0));

    let req = Request::builder()
        .method("POST")
        .uri("/?api-key=test-key")
        .header("content-type", "application/json")
        .body(Body::from(
            r#"[{"jsonrpc":"2.0","method":"getSlot","id":1},{"jsonrpc":"2.0","method":"getBlockHeight","id":2}]"#,
        ))
        .unwrap();
    let resp = app.router.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let json = body_json(resp).await;
    assert_eq!(json.as_array().unwrap().len(), 2);
}

// --- Request ids & health ---------------------------------------------------

#[tokio::test]
async fn request_id_is_generated_and_forwarded() {
    let echo: Handler = Arc::new(|headers, _| {
        let id = headers
            .get(REQUEST_ID_HEADER)
            .map(|v| v.to_str().unwrap().to_string())
            .unwrap_or_default();
        (StatusCode::OK, format!(r#"{{"result":"{}"}}"#, id))
    });
    let b = start_backend(echo).await;
    let app = build_app(vec![backend("b", &b.url, 1)], HashMap::new(), fast_proxy(0));

    // Generated when absent...
    let resp = app
        .router
        .clone()
        .oneshot(rpc_request("/?api-key=test-key", "getSlot"))
        .await
        .unwrap();
    let generated = resp
        .headers()
        .get(REQUEST_ID_HEADER)
        .unwrap()
        .to_str()
        .unwrap()
        .to_string();
    assert_eq!(generated.len(), 16);
    let json = body_json(resp).await;
    assert_eq!(json["result"], generated, "upstream saw the same id");

    // ...and preserved when the client supplies one.
    let mut req = rpc_request("/?api-key=test-key", "getSlot");
    req.headers_mut()
        .insert(REQUEST_ID_HEADER, "client-trace-42".parse().unwrap());
    let resp = app.router.oneshot(req).await.unwrap();
    assert_eq!(
        resp.headers().get(REQUEST_ID_HEADER).unwrap(),
        "client-trace-42"
    );
    let json = body_json(resp).await;
    assert_eq!(json["result"], "client-trace-42");
}

#[tokio::test]
async fn health_endpoint_reports_version_and_counts() {
    let app = build_app(
        vec![backend("a", "http://a", 1), backend("b", "http://b", 1)],
        HashMap::new(),
        fast_proxy(0),
    );
    let req = Request::builder()
        .uri("/health")
        .body(Body::empty())
        .unwrap();
    let resp = app.router.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let json = body_json(resp).await;
    assert_eq!(json["version"], env!("CARGO_PKG_VERSION"));
    assert_eq!(json["healthy_backends"], 2);
    assert_eq!(json["total_backends"], 2);
    assert!(json["backends"][0]["slot"].is_null());
}

#[tokio::test]
async fn sub_path_and_extra_query_are_forwarded() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let app = Router::new().route(
            "/v1/extra",
            post(|req: Request<Body>| async move {
                let q = req.uri().query().unwrap_or("").to_string();
                (StatusCode::OK, format!(r#"{{"result":"{}"}}"#, q))
            }),
        );
        axum::serve(listener, app).await.unwrap();
    });

    let backend_url = format!("http://{}/v1", addr);
    let https = HttpsConnector::new();
    let client = Client::builder(hyper_util::rt::TokioExecutor::new()).build(https);
    let keystore = Arc::new(MockKeyStore::new());
    keystore.add_key("test-key", "tester", 100);
    let health_state = Arc::new(HealthState::new(vec!["b".into()]));
    let state = Arc::new(AppState {
        client,
        keystore,
        state: Arc::new(ArcSwap::from_pointee(RouterState::simple(
            vec![backend("b", &backend_url, 1)],
            health_state,
        ))),
    });
    let router = Router::new()
        .route("/*path", post(proxy))
        .with_state(state)
        .layer(middleware::from_fn(extract_rpc_method));

    let resp = router
        .oneshot(rpc_request("/extra?api-key=test-key&foo=bar", "getSlot"))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let json = body_json(resp).await;
    assert_eq!(json["result"], "foo=bar");
}

#[tokio::test]
async fn upstream_cookies_are_not_passed_to_clients() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let app = Router::new().route(
            "/",
            post(|| async {
                (
                    StatusCode::OK,
                    [
                        ("set-cookie", "__cf_bm=secret; Path=/"),
                        ("content-type", "application/json"),
                        ("x-upstream-custom", "kept"),
                    ],
                    r#"{"result":1}"#,
                )
            }),
        );
        axum::serve(listener, app).await.unwrap();
    });

    let app = build_app(
        vec![backend("b", &format!("http://{}", addr), 1)],
        HashMap::new(),
        fast_proxy(0),
    );
    let resp = app
        .router
        .oneshot(rpc_request("/?api-key=test-key", "getSlot"))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    assert!(resp.headers().get("set-cookie").is_none());
    assert_eq!(resp.headers().get("x-upstream-custom").unwrap(), "kept");
    assert_eq!(
        resp.headers().get("content-type").unwrap(),
        "application/json"
    );
}
