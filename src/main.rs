mod config;
mod handlers;
mod health;
mod state;

use config::load_config;
use handlers::{extract_rpc_method, health_endpoint, log_requests, proxy};
use health::{health_check_loop, HealthState};
use state::AppState;

use axum::{
    middleware,
    routing::{get, post},
    Router,
};
use clap::Parser;
use hyper_tls::HttpsConnector;
use hyper_util::client::legacy::Client;
use std::{collections::HashMap, net::SocketAddr, sync::Arc};
use tracing::info;
use tracing_subscriber;

#[derive(Parser, Debug)]
#[command(name = "rpc-router")]
#[command(about = "RPC router with load balancing and health monitoring", long_about = None)]
struct Args {
    /// Path to configuration file
    #[arg(short, long, default_value = "config.toml")]
    config: String,
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
        info!(
            "  - [{}] {} (weight: {})",
            backend.label, backend.url, backend.weight
        );
    }

    if !config.method_routes.is_empty() {
        info!("Method routing overrides:");
        for (method, label) in &config.method_routes {
            info!("  - {} -> {}", method, label);
        }
    }

    // Build label-to-URL mapping
    let label_to_url: HashMap<String, String> = config
        .backends
        .iter()
        .map(|b| (b.label.clone(), b.url.clone()))
        .collect();

    // Initialize health state
    let backend_labels: Vec<String> = config.backends.iter().map(|b| b.label.clone()).collect();
    let health_state = Arc::new(HealthState::new(backend_labels));

    let https = HttpsConnector::new();
    let client = Client::builder(hyper_util::rt::TokioExecutor::new()).build(https);

    let state = Arc::new(AppState {
        client: client.clone(),
        backends: config.backends.clone(),
        api_keys: config.api_keys,
        method_routes: config.method_routes,
        label_to_url,
        health_state: health_state.clone(),
    });

    // Spawn background health check task
    let health_check_client = client.clone();
    let health_check_backends = config.backends.clone();
    let health_check_config = config.health_check.clone();

    tokio::spawn(async move {
        info!(
            "Starting health check loop (interval: {}s, timeout: {}s, method: {})",
            health_check_config.interval_secs,
            health_check_config.timeout_secs,
            health_check_config.method
        );
        health_check_loop(
            health_check_client,
            health_check_backends,
            health_state,
            health_check_config,
        )
        .await;
    });

    let app = Router::new()
        .route("/", post(proxy))
        .route("/*path", post(proxy))
        .route("/health", get(health_endpoint))
        .with_state(state)
        .layer(middleware::from_fn(log_requests))
        .layer(middleware::from_fn(extract_rpc_method));

    let addr = SocketAddr::from(([0, 0, 0, 0], config.port));
    info!("Listening on http://{}", addr);
    info!("Health monitoring endpoint: http://{}/health", addr);

    axum::serve(
        tokio::net::TcpListener::bind(addr).await.unwrap(),
        app.into_make_service_with_connect_info::<SocketAddr>(),
    )
    .await
    .unwrap();
}
