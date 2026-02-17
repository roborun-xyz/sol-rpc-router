use std::{net::SocketAddr, sync::Arc};

use axum::{
    body::{to_bytes, Body},
    extract::{
        ws::{Message, WebSocket, WebSocketUpgrade},
        ConnectInfo, Query, State,
    },
    http::{Request, StatusCode, Uri},
    middleware::Next,
    response::{IntoResponse, Response},
    Json,
};
use futures_util::{SinkExt, StreamExt};
use metrics::{counter, histogram};
use serde::{Deserialize, Serialize};
use tokio::time::{timeout, Duration};
use tokio_tungstenite::{connect_async, tungstenite::Message as TungsteniteMessage};
use tracing::{error, info, warn};

use crate::state::AppState;

const MAX_BODY_SIZE: usize = 10 * 1024 * 1024; // 10 MB

#[derive(Clone)]
pub struct RpcMethod(pub String);

#[derive(Clone)]
pub struct SelectedBackend(pub String);

#[derive(Deserialize)]
struct MethodProbe<'a> {
    method: Option<&'a str>,
}

#[derive(Deserialize)]
pub struct Params {
    #[serde(rename = "api-key")]
    pub api_key: Option<String>,
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

    // Optimize: Partial Zero-Copy Deserialization
    // Instead of parsing the full JSON (which allocates for params),
    // we use a struct that only captures 'method' and borrows the string from the buffer.
    if let Ok(probe) = serde_json::from_slice::<MethodProbe>(&body_bytes) {
        if let Some(method) = probe.method {
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

pub async fn track_metrics(req: Request<Body>, next: Next) -> Response {
    let start = std::time::Instant::now();
    let method = req.method().to_string();

    // Try to get RPC method if already extracted
    let rpc_method = req
        .extensions()
        .get::<RpcMethod>()
        .map(|m| m.0.clone())
        .unwrap_or_else(|| "unknown".to_string());

    let response = next.run(req).await;

    let duration = start.elapsed().as_secs_f64();
    let status = response.status().as_u16().to_string();

    let backend = response
        .extensions()
        .get::<SelectedBackend>()
        .map(|b| b.0.clone())
        .unwrap_or_else(|| "none".to_string());

    histogram!("rpc_request_duration_seconds", "rpc_method" => rpc_method.clone(), "backend" => backend.clone()).record(duration);
    counter!("rpc_requests_total", "method" => method, "status" => status, "rpc_method" => rpc_method, "backend" => backend).increment(1);

    response
}

pub async fn proxy(
    State(state): State<Arc<AppState>>,
    Query(params): Query<Params>,
    mut req: Request<Body>,
) -> impl IntoResponse {
    let api_key = match params.api_key {
        Some(k) => k,
        None => {
            info!("No API key provided");
            return (StatusCode::UNAUTHORIZED, "Unauthorized").into_response();
        }
    };

    match state.keystore.validate_key(&api_key).await {
        Ok(Some(_info)) => {
            // Valid key
        }
        Ok(None) => {
            info!("Invalid API key presented (prefix={}...)", &api_key[..api_key.len().min(6)]);
            return (StatusCode::UNAUTHORIZED, "Unauthorized").into_response();
        }
        Err(e) => {
            if e == "Rate limit exceeded" {
                warn!("API key rate limited (prefix={}...)", &api_key[..api_key.len().min(6)]);
                return (StatusCode::TOO_MANY_REQUESTS, "Rate limit exceeded").into_response();
            } else {
                error!("Key validation error: {}", e);
                return (StatusCode::INTERNAL_SERVER_ERROR, "Internal Server Error")
                    .into_response();
            }
        }
    }

    // Get RPC method from extension (set by extract_rpc_method middleware)
    let rpc_method = req.extensions().get::<RpcMethod>().map(|m| m.0.as_str());

    // Select backend based on method routing or weighted random
    let (backend_label, backend_url) = match state.select_backend(rpc_method) {
        Some(selection) => selection,
        None => {
            tracing::error!("No healthy backends available for request");
            return (
                StatusCode::SERVICE_UNAVAILABLE,
                "No healthy backends available",
            )
                .into_response();
        }
    };

    // Rebuild URI: strip api-key from query params while preserving others
    let path = req.uri().path();
    let cleaned_query = req
        .uri()
        .query()
        .map(|q| {
            q.split('&')
                .filter(|p| !p.starts_with("api-key="))
                .collect::<Vec<_>>()
                .join("&")
        })
        .unwrap_or_default();

    let cleaned_request_path = if cleaned_query.is_empty() {
        path.to_string()
    } else {
        format!("{}?{}", path, cleaned_query)
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

    // Ensure we have a valid URI
    let parsed_uri = match uri_string.parse::<Uri>() {
        Ok(uri) => uri,
        Err(e) => {
            error!("Failed to parse backend URI '{}': {}", uri_string, e);
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                "Invalid backend configuration",
            )
                .into_response();
        }
    };

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
    let proxy_timeout = state.state.load().proxy_timeout_secs;
    let result = timeout(
        Duration::from_secs(proxy_timeout),
        state.client.request(req),
    )
    .await;

    match result {
        Ok(Ok(mut resp)) => {
            // Store selected backend label in response extensions for logging
            resp.extensions_mut()
                .insert(SelectedBackend(backend_label.to_string()));
            resp.into_response()
        }
        Ok(Err(err)) => {
            info!("Backend request failed: {} (error type: {:?})", err, err);
            (StatusCode::BAD_GATEWAY, format!("Proxy error: {}", err)).into_response()
        }
        Err(_) => (
            StatusCode::GATEWAY_TIMEOUT,
            format!(
                "Upstream request timed out after {}s",
                proxy_timeout
            ),
        )
            .into_response(),
    }
}

#[derive(Serialize)]
pub struct HealthResponse {
    pub overall_status: String,
    pub backends: Vec<BackendHealth>,
}

#[derive(Serialize)]
pub struct BackendHealth {
    pub label: String,
    pub healthy: bool,
    pub last_check: Option<String>,
    pub consecutive_failures: u32,
    pub consecutive_successes: u32,
    pub last_error: Option<String>,
}

pub async fn health_endpoint(State(state): State<Arc<AppState>>) -> impl IntoResponse {
    let current_state = state.state.load();
    let all_statuses = current_state.health_state.get_all_statuses();

    let mut backends = Vec::new();
    let mut any_healthy = false;

    for backend in &current_state.backends {
        let status = all_statuses
            .get(&backend.config.label)
            .cloned()
            .unwrap_or_default();

        if status.healthy {
            any_healthy = true;
        }

        backends.push(BackendHealth {
            label: backend.config.label.clone(),
            healthy: status.healthy,
            last_check: status.last_check_time.map(|t| format!("{:?}", t)),
            consecutive_failures: status.consecutive_failures,
            consecutive_successes: status.consecutive_successes,
            last_error: status.last_error,
        });
    }

    let overall_status = if any_healthy { "healthy" } else { "unhealthy" };

    let response = HealthResponse {
        overall_status: overall_status.to_string(),
        backends,
    };

    Json(response)
}

pub async fn ws_proxy(
    ws: WebSocketUpgrade,
    State(state): State<Arc<AppState>>,
    Query(params): Query<Params>,
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
) -> impl IntoResponse {
    let api_key = match params.api_key {
        Some(k) => k,
        None => {
            info!("WebSocket: No API key provided from {}", addr);
            return (StatusCode::UNAUTHORIZED, "Unauthorized").into_response();
        }
    };

    // Validate API key
    match state.keystore.validate_key(&api_key).await {
        Ok(Some(_)) => {
            // Authorized
        }
        Ok(None) => {
            info!("WebSocket: Invalid API key from {} (prefix={}...)", addr, &api_key[..api_key.len().min(6)]);
            return (StatusCode::UNAUTHORIZED, "Unauthorized").into_response();
        }
        Err(e) => {
            if e == "Rate limit exceeded" {
                warn!(
                    "WebSocket: API key rate limited from {} (prefix={}...)",
                    addr, &api_key[..api_key.len().min(6)]
                );
                return (StatusCode::TOO_MANY_REQUESTS, "Rate limit exceeded").into_response();
            }
            error!("WebSocket: Key validation error: {}", e);
            return (StatusCode::INTERNAL_SERVER_ERROR, "Internal Server Error").into_response();
        }
    }

    // Select a backend with WebSocket support
    let (backend_label, backend_ws_url) = match state.select_ws_backend() {
        Some(selection) => selection,
        None => {
            error!("No healthy WebSocket backends available");
            return (
                StatusCode::SERVICE_UNAVAILABLE,
                "No healthy WebSocket backends available",
            )
                .into_response();
        }
    };

    let backend_label = backend_label.to_string();
    let backend_ws_url = backend_ws_url.to_string();

    info!(
        "WebSocket: {} upgrading connection, backend={}",
        addr, backend_label
    );

    ws.on_upgrade(move |client_socket| {
        handle_ws_connection(client_socket, backend_ws_url, backend_label, addr)
    })
    .into_response()
}

async fn handle_ws_connection(
    client_socket: WebSocket,
    backend_url: String,
    backend_label: String,
    client_addr: SocketAddr,
) {
    // Connect to the backend WebSocket
    let backend_socket = match connect_async(&backend_url).await {
        Ok((socket, _)) => socket,
        Err(e) => {
            error!(
                "WebSocket: Failed to connect to backend {} ({}): {}",
                backend_label, backend_url, e
            );
            return;
        }
    };

    info!(
        "WebSocket: {} connected to backend {}",
        client_addr, backend_label
    );

    // Split both connections
    let (mut client_write, mut client_read) = client_socket.split();
    let (mut backend_write, mut backend_read) = backend_socket.split();

    // Forward client -> backend
    let client_to_backend = async {
        while let Some(msg) = client_read.next().await {
            match msg {
                Ok(Message::Text(text)) => {
                    if backend_write
                        .send(TungsteniteMessage::Text(text))
                        .await
                        .is_err()
                    {
                        break;
                    }
                }
                Ok(Message::Binary(data)) => {
                    if backend_write
                        .send(TungsteniteMessage::Binary(data))
                        .await
                        .is_err()
                    {
                        break;
                    }
                }
                Ok(Message::Ping(data)) => {
                    if backend_write
                        .send(TungsteniteMessage::Ping(data))
                        .await
                        .is_err()
                    {
                        break;
                    }
                }
                Ok(Message::Pong(data)) => {
                    if backend_write
                        .send(TungsteniteMessage::Pong(data))
                        .await
                        .is_err()
                    {
                        break;
                    }
                }
                Ok(Message::Close(_)) | Err(_) => break,
            }
        }
    };

    // Forward backend -> client
    let backend_to_client = async {
        while let Some(msg) = backend_read.next().await {
            match msg {
                Ok(TungsteniteMessage::Text(text)) => {
                    if client_write.send(Message::Text(text)).await.is_err() {
                        break;
                    }
                }
                Ok(TungsteniteMessage::Binary(data)) => {
                    if client_write.send(Message::Binary(data)).await.is_err() {
                        break;
                    }
                }
                Ok(TungsteniteMessage::Ping(data)) => {
                    if client_write.send(Message::Ping(data)).await.is_err() {
                        break;
                    }
                }
                Ok(TungsteniteMessage::Pong(data)) => {
                    if client_write.send(Message::Pong(data)).await.is_err() {
                        break;
                    }
                }
                Ok(TungsteniteMessage::Close(_)) | Ok(TungsteniteMessage::Frame(_)) | Err(_) => {
                    break
                }
            }
        }
    };

    // Run both directions concurrently, stop when either ends
    tokio::select! {
        _ = client_to_backend => {
            // Client side ended; send close to backend
            let _ = backend_write.send(TungsteniteMessage::Close(None)).await;
        },
        _ = backend_to_client => {
            // Backend side ended; send close to client
            let _ = client_write.send(Message::Close(None)).await;
        },
    }

    info!(
        "WebSocket: {} disconnected from backend {}",
        client_addr, backend_label
    );
}
