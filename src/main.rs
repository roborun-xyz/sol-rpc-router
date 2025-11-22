use axum::{
    body::{to_bytes, Body},
    extract::{Query, State},
    http::{Request, StatusCode, Uri},
    response::IntoResponse,
    routing::post,
    Router,
};
use axum::{
    extract::ConnectInfo,
    middleware::{self, Next},
    response::Response,
};
use clap::Parser;
use hyper_tls::HttpsConnector;
use hyper_util::client::legacy::connect::HttpConnector;
use hyper_util::client::legacy::Client;
use rand::Rng;
use serde::Deserialize;
use std::collections::HashMap;
use std::fs;
use std::net::SocketAddr;
use std::sync::Arc;
use tracing::info;
use tracing_subscriber;

const MAX_BODY_SIZE: usize = 10 * 1024 * 1024; // 10 MB

/// RPC Proxy with weighted load balancing and method-based routing
#[derive(Parser, Debug)]
#[command(name = "rpc-proxy")]
#[command(about = "High-performance RPC proxy with load balancing", long_about = None)]
struct Args {
    /// Path to configuration file
    #[arg(short, long, default_value = "config.toml")]
    config: String,
}

#[derive(Clone)]
struct RpcMethod(String);

#[derive(Clone)]
struct SelectedBackend(String);

#[derive(Debug, Deserialize, Clone)]
struct Config {
    port: u16,
    api_keys: Vec<String>,
    backends: Vec<Backend>,
    #[serde(default)]
    method_routes: HashMap<String, String>,
}

#[derive(Debug, Deserialize, Clone)]
struct Backend {
    url: String,
    #[serde(default = "default_weight")]
    weight: u32,
}

fn default_weight() -> u32 {
    1
}

#[derive(Clone)]
struct AppState {
    client: Client<HttpsConnector<HttpConnector>, Body>,
    backends: Vec<Backend>,
    total_weight: u32,
    api_keys: Vec<String>,
    method_routes: HashMap<String, String>,
}

impl AppState {
    fn select_backend(&self, rpc_method: Option<&str>) -> &str {
        // Check method-specific routing first
        if let Some(method) = rpc_method {
            if let Some(backend_url) = self.method_routes.get(method) {
                info!("Method {} routed to {}", method, backend_url);
                return backend_url;
            }
        }

        // Weighted random selection
        let mut rng = rand::thread_rng();
        let mut random_weight = rng.gen_range(0..self.total_weight);

        for backend in &self.backends {
            if random_weight < backend.weight {
                return &backend.url;
            }
            random_weight -= backend.weight;
        }

        // Fallback (should never reach here if weights are valid)
        &self.backends[0].url
    }
}

#[derive(Deserialize)]
struct Params {
    #[serde(rename = "api-key")]
    api_key: Option<String>,
}

fn load_config(config_path: &str) -> Result<Config, Box<dyn std::error::Error>> {
    if !std::path::Path::new(config_path).exists() {
        return Err(format!("Configuration file not found: {}", config_path).into());
    }

    // Read TOML file directly to preserve case sensitivity
    let contents = fs::read_to_string(config_path)?;
    let config: Config = toml::from_str(&contents)?;

    // Validation
    if config.api_keys.is_empty() {
        return Err("At least one API key must be configured".into());
    }
    if config.backends.is_empty() {
        return Err("At least one backend must be configured".into());
    }
    for backend in &config.backends {
        if backend.weight == 0 {
            return Err(format!("Backend {} has invalid weight 0", backend.url).into());
        }
    }

    Ok(config)
}

pub async fn extract_rpc_method(mut req: Request<Body>, next: Next) -> Response {
    // Read body, extract "method" field, then reconstruct the request
    let (parts, body) = req.into_parts();
    let body_bytes = match to_bytes(body, MAX_BODY_SIZE).await {
        Ok(bytes) => bytes,
        Err(_) => {
            // If body read fails, pass empty body downstream
            return next.run(Request::from_parts(parts, Body::empty())).await;
        }
    };

    // Try to extract "method" from JSON
    if let Ok(json) = serde_json::from_slice::<serde_json::Value>(&body_bytes) {
        if let Some(method) = json.get("method").and_then(|m| m.as_str()) {
            req = Request::from_parts(parts, Body::from(body_bytes.clone()));
            req.extensions_mut().insert(RpcMethod(method.to_string()));
            return next.run(req).await;
        }
    }

    // If no method found, reconstruct request with original body
    req = Request::from_parts(parts, Body::from(body_bytes));
    next.run(req).await
}

pub async fn log_requests(
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
    req: Request<Body>,
    next: Next,
) -> Response {
    let method = req.method().clone();
    let path = req.uri().path().to_string();
    let rpc_method = req.extensions().get::<RpcMethod>().cloned();

    let start = std::time::Instant::now();
    let response = next.run(req).await;
    let duration = start.elapsed();

    // Extract backend from response extensions (set by proxy handler)
    let backend = response.extensions().get::<SelectedBackend>().cloned();

    match (rpc_method, backend) {
        (Some(RpcMethod(m)), Some(SelectedBackend(b))) => info!(
            "{} {} {} {:?} rpc_method={} backend={}",
            method, path, addr, duration, m, b
        ),
        (Some(RpcMethod(m)), None) => info!(
            "{} {} {} {:?} rpc_method={}",
            method, path, addr, duration, m
        ),
        (None, Some(SelectedBackend(b))) => {
            info!("{} {} {} {:?} backend={}", method, path, addr, duration, b)
        }
        (None, None) => info!("{} {} {} {:?}", method, path, addr, duration),
    }

    response
}

async fn proxy(
    State(state): State<Arc<AppState>>,
    Query(params): Query<Params>,
    mut req: Request<Body>,
) -> impl IntoResponse {
    match params.api_key {
        Some(ref key) if state.api_keys.contains(key) => {}
        Some(ref key) => {
            info!("API key '{}' is invalid", key);
            return (StatusCode::UNAUTHORIZED, "Unauthorized").into_response();
        }
        None => {
            info!("No API key provided");
            return (StatusCode::UNAUTHORIZED, "Unauthorized").into_response();
        }
    }

    // Get RPC method from extension (set by extract_rpc_method middleware)
    let rpc_method = req.extensions().get::<RpcMethod>().map(|m| m.0.as_str());

    // Select backend based on method routing or weighted random
    let backend_url = state.select_backend(rpc_method);

    // Rebuild URI (remove ?api-key=... from request)
    let request_path_and_query = req
        .uri()
        .path_and_query()
        .map(|x| x.as_str())
        .unwrap_or("/");

    // Remove api-key from the incoming request's query parameters
    let cleaned_request_path = if let Some(pos) = request_path_and_query.find("?api-key=") {
        &request_path_and_query[..pos]
    } else {
        request_path_and_query
    };

    // Build URI with selected backend
    let uri_string = if cleaned_request_path == "/" {
        // For root path requests, don't add trailing slash
        backend_url.trim_end_matches('/').to_string()
    } else if backend_url.ends_with('/') && cleaned_request_path.starts_with('/') {
        // Avoid double slashes
        format!("{}{}", backend_url, &cleaned_request_path[1..])
    } else {
        format!("{}{}", backend_url, cleaned_request_path)
    };
    let parsed_uri = uri_string.parse::<Uri>().unwrap();

    // Update Host header to match the backend
    if let Some(host) = parsed_uri.host() {
        let host_value = if let Some(port) = parsed_uri.port_u16() {
            format!("{}:{}", host, port)
        } else {
            host.to_string()
        };
        req.headers_mut()
            .insert("host", host_value.parse().unwrap());
    }

    *req.uri_mut() = parsed_uri;

    // Forward request
    match state.client.request(req).await {
        Ok(mut resp) => {
            // Store selected backend in response extensions for logging
            resp.extensions_mut()
                .insert(SelectedBackend(backend_url.to_string()));
            resp.into_response()
        }
        Err(err) => {
            info!("Backend request failed: {} (error type: {:?})", err, err);
            (StatusCode::BAD_GATEWAY, format!("Proxy error: {}", err)).into_response()
        }
    }
}

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt::init();

    // Parse command-line arguments
    let args = Args::parse();

    // Load configuration from TOML file
    let config = load_config(&args.config).expect("Failed to load configuration");

    info!("Loaded configuration from: {}", args.config);
    info!("Loaded {} backends", config.backends.len());
    for backend in &config.backends {
        info!("  - {} (weight: {})", backend.url, backend.weight);
    }

    if !config.method_routes.is_empty() {
        info!("Method routing overrides:");
        for (method, url) in &config.method_routes {
            info!("  - {} -> {}", method, url);
        }
    }

    let total_weight: u32 = config.backends.iter().map(|b| b.weight).sum();

    let https = HttpsConnector::new();
    let state = Arc::new(AppState {
        client: Client::builder(hyper_util::rt::TokioExecutor::new()).build(https),
        backends: config.backends,
        total_weight,
        api_keys: config.api_keys,
        method_routes: config.method_routes,
    });

    let app = Router::new()
        .route("/", post(proxy))
        .route("/*path", post(proxy))
        .with_state(state)
        .layer(middleware::from_fn(log_requests))
        .layer(middleware::from_fn(extract_rpc_method));

    let addr = SocketAddr::from(([0, 0, 0, 0], config.port));
    info!("Listening on http://{}", addr);

    axum::serve(
        tokio::net::TcpListener::bind(addr).await.unwrap(),
        app.into_make_service_with_connect_info::<SocketAddr>(),
    )
    .await
    .unwrap();
}
