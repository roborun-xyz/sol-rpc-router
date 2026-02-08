use std::{net::SocketAddr, sync::{atomic::AtomicBool, Arc}};

use axum::{
    middleware,
    routing::{get, post},
    Router,
};
use clap::Parser;
use hyper_tls::HttpsConnector;
use hyper_util::client::legacy::Client;
use metrics_exporter_prometheus::PrometheusBuilder;
use sol_rpc_router::{
    config::load_config,
    handlers::{extract_rpc_method, health_endpoint, log_requests, proxy, track_metrics, ws_proxy},
    health::{health_check_loop, HealthState},
    keystore::RedisKeyStore,
    state::{AppState, RuntimeBackend},
};
use tracing::{error, info};

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

    // Initialize Prometheus recorder
    let builder = PrometheusBuilder::new();
    let handle = builder
        .install_recorder()
        .expect("failed to install Prometheus recorder");

    // Parse command-line arguments
    let args = Args::parse();

    // Load configuration from TOML file
    let config = load_config(&args.config).expect("Failed to load router configuration");

    info!("Loaded configuration from: {}", args.config);
    info!("Redis URL configured: {}", config.redis_url);

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

    // Initialize runtime backends with atomic health status
    let runtime_backends: Vec<RuntimeBackend> = config
        .backends
        .iter()
        .map(|b| RuntimeBackend {
            config: b.clone(),
            healthy: Arc::new(AtomicBool::new(true)), // Default to healthy
        })
        .collect();

    // Initialize health state
    let backend_labels: Vec<String> = config.backends.iter().map(|b| b.label.clone()).collect();
    let health_state = Arc::new(HealthState::new(backend_labels));

    let https = HttpsConnector::new();
    let client = Client::builder(hyper_util::rt::TokioExecutor::new()).build(https);

    // Initialize Redis KeyStore
    let keystore = match RedisKeyStore::new(&config.redis_url) {
        Ok(ks) => ks,
        Err(e) => {
            error!("Failed to initialize Redis KeyStore: {}", e);
            std::process::exit(1);
        }
    };

    let state = Arc::new(AppState {
        client: client.clone(),
        backends: runtime_backends.clone(),
        keystore: Arc::new(keystore),
        method_routes: config.method_routes,
        health_state: health_state.clone(),
        proxy_timeout_secs: config.proxy.timeout_secs,
    });

    // Spawn background health check task
    let health_check_client = client.clone();
    let health_check_backends = runtime_backends; // Move the runtime backends here
    let health_check_config = config.health_check.clone();
    let health_check_state = health_state.clone();

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
            health_check_state,
            health_check_config,
        )
        .await;
    });

    // HTTP server (JSON-RPC over HTTP)
    let http_app = Router::new()
        .route("/", post(proxy))
        .route("/*path", post(proxy))
        .route("/health", get(health_endpoint))
        .route("/metrics", get(move || std::future::ready(handle.render())))
        .with_state(state.clone())
        .layer(middleware::from_fn(track_metrics))
        .layer(middleware::from_fn(log_requests))
        .layer(middleware::from_fn(extract_rpc_method));

    // WebSocket server (following Solana convention: WS port = HTTP port + 1)
    let ws_app = Router::new()
        .route("/", get(ws_proxy))
        .with_state(state)
        .layer(middleware::from_fn(log_requests));

    let http_addr = SocketAddr::from(([0, 0, 0, 0], config.port));
    let ws_port = config
        .port
        .checked_add(1)
        .expect("WebSocket port overflow: HTTP port cannot be 65535");
    let ws_addr = SocketAddr::from(([0, 0, 0, 0], ws_port));

    info!("HTTP server listening on http://{}", http_addr);
    info!("WebSocket server listening on ws://{}", ws_addr);
    info!("Health monitoring endpoint: http://{}/health", http_addr);
    info!("Metrics endpoint: http://{}/metrics", http_addr);

    // Start both servers concurrently
    let http_server = async {
        axum::serve(
            tokio::net::TcpListener::bind(http_addr)
                .await
                .expect("Failed to bind HTTP server"),
            http_app.into_make_service_with_connect_info::<SocketAddr>(),
        )
        .await
        .expect("HTTP server error");
    };

    let ws_server = async {
        axum::serve(
            tokio::net::TcpListener::bind(ws_addr)
                .await
                .expect("Failed to bind WebSocket server"),
            ws_app.into_make_service_with_connect_info::<SocketAddr>(),
        )
        .await
        .expect("WebSocket server error");
    };

    tokio::join!(http_server, ws_server);
}
