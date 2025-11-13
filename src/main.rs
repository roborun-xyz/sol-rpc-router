use axum::{
    body::Body,
    extract::{Query, State},
    http::{Request, StatusCode, Uri},
    response::IntoResponse,
    routing::any,
    Router,
};
use axum::{
    extract::ConnectInfo,
    middleware::{self, Next},
    response::Response,
};
use dotenv::dotenv;
use hyper_util::client::legacy::connect::HttpConnector;
use hyper_util::client::legacy::Client;
use serde::Deserialize;
use std::env;
use std::net::SocketAddr;
use std::sync::Arc;
use tracing::info;
use tracing_subscriber;

#[derive(Clone)]
struct AppState {
    client: Client<HttpConnector, Body>,
    backend: String,
    api_keys: Vec<String>,
}

#[derive(Deserialize)]
struct Params {
    #[serde(rename = "api-key")]
    api_key: Option<String>,
}

pub async fn log_requests(
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
    req: Request<Body>,
    next: Next,
) -> Response {
    let method = req.method().clone();
    let path = req.uri().path().to_string();

    let start = std::time::Instant::now();
    let response = next.run(req).await;
    let duration = start.elapsed();

    info!("{} {} {} {:?}", method, path, addr, duration);

    response
}

async fn proxy(
    State(state): State<Arc<AppState>>,
    Query(params): Query<Params>,
    mut req: Request<Body>,
) -> impl IntoResponse {
    match params.api_key {
        Some(ref key) if state.api_keys.contains(key) => {}
        _ => return (StatusCode::UNAUTHORIZED, "Unauthorized").into_response(),
    }

    // Rebuild URI (remove ?api-key=...)
    let mut uri_string = format!(
        "{}{}",
        state.backend,
        req.uri()
            .path_and_query()
            .map(|x| x.as_str())
            .unwrap_or("/")
    );
    if let Some(pos) = uri_string.find("?api-key=") {
        uri_string.truncate(pos);
    }
    *req.uri_mut() = uri_string.parse::<Uri>().unwrap();

    // Forward request
    match state.client.request(req).await {
        Ok(resp) => resp.into_response(),
        Err(err) => (StatusCode::BAD_GATEWAY, format!("Proxy error: {}", err)).into_response(),
    }
}

#[tokio::main]
async fn main() {
    // Load environment variables from .env file
    dotenv().ok();

    tracing_subscriber::fmt::init();

    // Read configuration from environment variables
    let backend = env::var("BACKEND_URL").expect("BACKEND_URL environment variable must be set");
    let api_keys_str = env::var("API_KEYS").expect("API_KEYS environment variable must be set");
    let api_keys: Vec<String> = api_keys_str
        .split(',')
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect();
    
    if api_keys.is_empty() {
        panic!("API_KEYS must contain at least one valid API key");
    }
    
    let port: u16 = env::var("PORT")
        .unwrap_or_else(|_| "28899".to_string())
        .parse()
        .expect("PORT must be a valid number");

    let state = Arc::new(AppState {
        client: Client::builder(hyper_util::rt::TokioExecutor::new()).build(HttpConnector::new()),
        backend,
        api_keys,
    });

    let app = Router::new()
        .route("/", any(proxy))
        .route("/*path", any(proxy))
        .with_state(state)
        .layer(middleware::from_fn(log_requests));

    let addr = SocketAddr::from(([0, 0, 0, 0], port));
    println!("Listening on http://{}", addr);

    axum::serve(
        tokio::net::TcpListener::bind(addr).await.unwrap(),
        app.into_make_service_with_connect_info::<SocketAddr>(),
    )
    .await
    .unwrap();
}
