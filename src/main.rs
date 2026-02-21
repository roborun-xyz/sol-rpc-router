use std::{net::SocketAddr, sync::{atomic::AtomicBool, Arc}};

use arc_swap::ArcSwap;
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
    state::{AppState, RouterState, RuntimeBackend},
};
use tokio::signal::unix::{signal, SignalKind};
use tower_http::cors::CorsLayer;
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

    // Initialize Prometheus recorder with histogram buckets
    // Using set_buckets makes the exporter emit true Prometheus histograms (_bucket/_sum/_count)
    // instead of summaries, which is required for histogram_quantile() in Grafana.
    let builder = PrometheusBuilder::new()
        .set_buckets(&[0.001, 0.005, 0.01, 0.025, 0.05, 0.1, 0.25, 0.5, 1.0, 2.5, 5.0, 10.0])
        .expect("failed to set histogram buckets");
    let handle = builder
        .install_recorder()
        .expect("failed to install Prometheus recorder");

    // Parse command-line arguments
    let args = Args::parse();

    // Load configuration from TOML file
    let config = load_config(&args.config).expect("Failed to load router configuration");

    info!("Loaded configuration from: {}", args.config);
    info!("Redis URL configured (host redacted)");

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

    let initial_router_state = RouterState {
        backends: runtime_backends,
        method_routes: config.method_routes,
        health_state: health_state.clone(),
        proxy_timeout_secs: config.proxy.timeout_secs,
        health_check_config: config.health_check.clone(),
    };

    let router_state = Arc::new(ArcSwap::from_pointee(initial_router_state));

    let https = HttpsConnector::new();
    let client = Client::builder(hyper_util::rt::TokioExecutor::new()).build(https);

    // Initialize Redis KeyStore
    let keystore = match RedisKeyStore::new(&config.redis_url).await {
        Ok(ks) => ks,
        Err(e) => {
            error!("Failed to initialize Redis KeyStore: {}", e);
            std::process::exit(1);
        }
    };

    let state = Arc::new(AppState {
        client: client.clone(),
        keystore: Arc::new(keystore),
        state: router_state.clone(),
    });

    // Spawn background health check task
    let health_check_client = client.clone();
    let health_check_state = router_state.clone();

    tokio::spawn(async move {
        info!("Starting health check loop");
        // Loop will read config from state each iteration
        health_check_loop(
            health_check_client,
            health_check_state,
        )
        .await;
    });

    // Spawn SIGHUP handler for hot reload
    let reload_state = router_state.clone();
    let config_path = args.config.clone();
    // We keep the original health_state to preserve history across reloads if backends match
    let persistent_health_state = health_state.clone(); 

    tokio::spawn(async move {
        let mut sighup = signal(SignalKind::hangup()).expect("Failed to register SIGHUP handler");
        
        loop {
            sighup.recv().await;
            info!("Received SIGHUP, reloading configuration from {}", config_path);

            match load_config(&config_path) {
                Ok(new_config) => {
                    info!("Configuration reloaded successfully");
                    info!("New backend count: {}", new_config.backends.len());
                    
                    // Re-initialize runtime backends
                    // We attempt to preserve health status if backend label matches
                    let new_runtime_backends: Vec<RuntimeBackend> = new_config
                        .backends
                        .iter()
                        .map(|b| {
                            // Check if we have existing status for this label
                            let is_healthy = if let Some(status) = persistent_health_state.get_status(&b.label) {
                                status.healthy
                            } else {
                                true // Default new backends to healthy
                            };

                            RuntimeBackend {
                                config: b.clone(),
                                healthy: Arc::new(AtomicBool::new(is_healthy)),
                            }
                        })
                        .collect();
                    
                    // Update method routes info
                    if !new_config.method_routes.is_empty() {
                         info!("Updated method routing overrides:");
                         for (method, label) in &new_config.method_routes {
                             info!("  - {} -> {}", method, label);
                         }
                    }

                    // Create new router state
                    let new_router_state = RouterState {
                        backends: new_runtime_backends,
                        method_routes: new_config.method_routes,
                        health_state: persistent_health_state.clone(), // Reuse the persistent health state container
                        proxy_timeout_secs: new_config.proxy.timeout_secs,
                        health_check_config: new_config.health_check,
                    };

                    // Atomically swap the state
                    reload_state.store(Arc::new(new_router_state));
                    info!("Router state atomically swapped");
                }
                Err(e) => {
                    error!("Failed to reload configuration: {}", e);
                }
            }
        }
    });

    // HTTP server (JSON-RPC over HTTP)
    let http_app = Router::new()
        .route("/", post(proxy))
        .route("/*path", post(proxy))
        .route("/health", get(health_endpoint))
        .with_state(state.clone())
        .layer(middleware::from_fn(track_metrics))
        .layer(middleware::from_fn(log_requests))
        .layer(middleware::from_fn(extract_rpc_method))
        .layer(CorsLayer::permissive());

    // WebSocket server (following Solana convention: WS port = HTTP port + 1)
    let ws_app = Router::new()
        .route("/", get(ws_proxy))
        .with_state(state)
        .layer(middleware::from_fn(log_requests))
        .layer(CorsLayer::permissive());

    // Metrics server (dedicated port)
    let metrics_app = Router::new()
        .route("/metrics", get(move || std::future::ready(handle.render())));

    let http_addr = SocketAddr::from(([0, 0, 0, 0], config.port));
    let ws_port = config
        .port
        .checked_add(1)
        .expect("WebSocket port overflow: HTTP port cannot be 65535");
    let ws_addr = SocketAddr::from(([0, 0, 0, 0], ws_port));
    let metrics_addr = SocketAddr::from(([0, 0, 0, 0], config.metrics_port));

    info!("HTTP server listening on http://{}", http_addr);
    info!("WebSocket server listening on ws://{}", ws_addr);
    info!("Metrics server listening on http://{}", metrics_addr);
    info!("Health monitoring endpoint: http://{}/health", http_addr);

    // Start all servers concurrently
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

    let metrics_server = async {
        axum::serve(
            tokio::net::TcpListener::bind(metrics_addr)
                .await
                .expect("Failed to bind Metrics server"),
            metrics_app.into_make_service_with_connect_info::<SocketAddr>(),
        )
        .await
        .expect("Metrics server error");
    };

    tokio::join!(http_server, ws_server, metrics_server);
}
