//! Assembles the Axum routers. Used by the binary, the benchmark and the
//! integration tests so all of them run the same middleware in the same order.
//!
//! Request flow for RPC routes:
//! CORS → request id → log → metrics → **auth** → body/method extraction → handler.
//! Authentication runs before the body is read, so unauthenticated or
//! rate-limited clients never make the router buffer a request body.

use std::sync::Arc;

use axum::{
    middleware,
    routing::{get, post},
    Router,
};
use metrics_exporter_prometheus::PrometheusHandle;
use tower_http::cors::CorsLayer;

use crate::{
    handlers::{
        extract_rpc_method, health_endpoint, log_requests, proxy, request_id, require_api_key,
        track_metrics, ws_proxy,
    },
    state::AppState,
};

/// Main listener: JSON-RPC over HTTP, WebSocket upgrades on `/`, and `/health`.
pub fn http_router(state: Arc<AppState>) -> Router {
    let rpc = Router::new()
        .route("/", get(ws_proxy).post(proxy))
        .route("/*path", post(proxy))
        .route_layer(middleware::from_fn(extract_rpc_method))
        .route_layer(middleware::from_fn_with_state(
            state.clone(),
            require_api_key,
        ))
        .with_state(state.clone());

    let health = Router::new()
        .route("/health", get(health_endpoint))
        .with_state(state);

    rpc.merge(health)
        .layer(middleware::from_fn(track_metrics))
        .layer(middleware::from_fn(log_requests))
        .layer(middleware::from_fn(request_id))
        .layer(CorsLayer::permissive())
}

/// Dedicated WebSocket listener (Solana convention: HTTP port + 1).
pub fn ws_router(state: Arc<AppState>) -> Router {
    Router::new()
        .route("/", get(ws_proxy))
        .route_layer(middleware::from_fn_with_state(
            state.clone(),
            require_api_key,
        ))
        .with_state(state)
        .layer(middleware::from_fn(log_requests))
        .layer(middleware::from_fn(request_id))
        .layer(CorsLayer::permissive())
}

/// Prometheus scrape endpoint, served on its own port.
pub fn metrics_router(handle: PrometheusHandle) -> Router {
    Router::new().route("/metrics", get(move || std::future::ready(handle.render())))
}
