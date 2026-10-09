use std::{net::SocketAddr, sync::Arc, time::Duration};

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
    handlers::{
        extract_rpc_method, health_endpoint, log_requests, proxy, request_id, track_metrics,
        ws_proxy,
    },
    health::{health_check_loop, HealthState},
    keystore::RedisKeyStore,
    state::{AppState, RouterState},
};
use tokio::{
    net::TcpListener,
    signal::unix::{signal, SignalKind},
    sync::watch,
};
use tower_http::cors::CorsLayer;
use tracing::{error, info, warn};

#[derive(Parser, Debug)]
#[command(name = "sol-rpc-router", version)]
#[command(about = "Solana JSON-RPC reverse proxy with auth, rate limiting, failover and metrics", long_about = None)]
struct Args {
    /// Path to configuration file
    #[arg(short, long, default_value = "config.toml", env = "RPC_ROUTER_CONFIG")]
    config: String,
}

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt::init();

    // Explicit buckets make the exporter emit true Prometheus histograms
    // (_bucket/_sum/_count), which histogram_quantile() in Grafana needs.
    let builder = PrometheusBuilder::new()
        .set_buckets(&[
            0.001, 0.005, 0.01, 0.025, 0.05, 0.1, 0.25, 0.5, 1.0, 2.5, 5.0, 10.0,
        ])
        .expect("failed to set histogram buckets");
    let handle = builder
        .install_recorder()
        .expect("failed to install Prometheus recorder");

    let args = Args::parse();
    let config = load_config(&args.config).expect("Failed to load router configuration");

    info!(
        "sol-rpc-router v{} loaded configuration from {}",
        env!("CARGO_PKG_VERSION"),
        args.config
    );
    info!("Loaded {} backends", config.backends.len());
    for backend in &config.backends {
        info!(
            "  - [{}] {} (weight: {}, ws: {})",
            backend.label,
            backend.url,
            backend.weight,
            backend.ws_url.is_some()
        );
    }
    if !config.method_routes.is_empty() {
        info!("Method routing overrides:");
        for (method, label) in &config.method_routes {
            info!("  - {} -> {}", method, label);
        }
    }
    info!(
        "Proxy: timeout={}s max_retries={} fanout={:?} blocked={:?}",
        config.proxy.timeout_secs,
        config.proxy.max_retries,
        config.proxy.fanout_methods,
        config.proxy.blocked_methods
    );

    let backend_labels: Vec<String> = config.backends.iter().map(|b| b.label.clone()).collect();
    let health_state = Arc::new(HealthState::new(backend_labels));

    let router_state = Arc::new(ArcSwap::from_pointee(RouterState::from_config(
        &config,
        health_state.clone(),
        |_| true,
    )));

    let https = HttpsConnector::new();
    let client = Client::builder(hyper_util::rt::TokioExecutor::new()).build(https);

    let keystore = match RedisKeyStore::new(&config.redis_url).await {
        Ok(ks) => ks,
        Err(e) => {
            error!("Failed to initialize Redis KeyStore: {}", e);
            std::process::exit(1);
        }
    };
    info!("Connected to Redis");

    let state = Arc::new(AppState {
        client: client.clone(),
        keystore: Arc::new(keystore),
        state: router_state.clone(),
    });

    // Background health checks.
    {
        let client = client.clone();
        let router_state = router_state.clone();
        tokio::spawn(async move {
            info!("Starting health check loop");
            health_check_loop(client, router_state).await;
        });
    }

    // SIGHUP hot reload. Health history is kept for backends whose label is
    // unchanged; new backends start healthy.
    {
        let router_state = router_state.clone();
        let health_state = health_state.clone();
        let config_path = args.config.clone();
        tokio::spawn(async move {
            let mut sighup =
                signal(SignalKind::hangup()).expect("Failed to register SIGHUP handler");
            loop {
                sighup.recv().await;
                info!(
                    "Received SIGHUP, reloading configuration from {}",
                    config_path
                );
                match load_config(&config_path) {
                    Ok(new_config) => {
                        let new_state =
                            RouterState::from_config(&new_config, health_state.clone(), |label| {
                                health_state
                                    .get_status(label)
                                    .map(|s| s.healthy)
                                    .unwrap_or(true)
                            });
                        info!(
                            "Configuration reloaded: {} backends, {} method routes, fanout={:?}, blocked={:?}",
                            new_state.backends.len(),
                            new_state.method_routes.len(),
                            new_config.proxy.fanout_methods,
                            new_config.proxy.blocked_methods
                        );
                        router_state.store(Arc::new(new_state));
                    }
                    Err(e) => error!("Failed to reload configuration: {}", e),
                }
            }
        });
    }

    // HTTP server (JSON-RPC over HTTP + WebSocket upgrade on the same port).
    let http_app = Router::new()
        .route("/", get(ws_proxy).post(proxy))
        .route("/*path", post(proxy))
        .route("/health", get(health_endpoint))
        .with_state(state.clone())
        .layer(middleware::from_fn(track_metrics))
        .layer(middleware::from_fn(log_requests))
        .layer(middleware::from_fn(extract_rpc_method))
        .layer(middleware::from_fn(request_id))
        .layer(CorsLayer::permissive());

    // Dedicated WebSocket server (Solana convention: WS port = HTTP port + 1).
    let ws_app = Router::new()
        .route("/", get(ws_proxy))
        .with_state(state)
        .layer(middleware::from_fn(log_requests))
        .layer(middleware::from_fn(request_id))
        .layer(CorsLayer::permissive());

    let metrics_app =
        Router::new().route("/metrics", get(move || std::future::ready(handle.render())));

    let http_addr = SocketAddr::from(([0, 0, 0, 0], config.port));
    let ws_port = config
        .port
        .checked_add(1)
        .expect("WebSocket port overflow: HTTP port cannot be 65535");
    let ws_addr = SocketAddr::from(([0, 0, 0, 0], ws_port));
    let metrics_addr = SocketAddr::from(([0, 0, 0, 0], config.metrics_port));

    let http_listener = TcpListener::bind(http_addr)
        .await
        .expect("Failed to bind HTTP server");
    let ws_listener = TcpListener::bind(ws_addr)
        .await
        .expect("Failed to bind WebSocket server");
    let metrics_listener = TcpListener::bind(metrics_addr)
        .await
        .expect("Failed to bind Metrics server");

    info!("HTTP server listening on http://{}", http_addr);
    info!("WebSocket server listening on ws://{}", ws_addr);
    info!(
        "Metrics server listening on http://{}/metrics",
        metrics_addr
    );
    info!("Health endpoint: http://{}/health", http_addr);

    let (shutdown_tx, shutdown_rx) = watch::channel(false);
    tokio::spawn(async move {
        shutdown_signal().await;
        let _ = shutdown_tx.send(true);
    });

    let wait_for_shutdown = |mut rx: watch::Receiver<bool>| async move {
        let _ = rx.wait_for(|v| *v).await;
    };

    let http_server = axum::serve(
        http_listener,
        http_app.into_make_service_with_connect_info::<SocketAddr>(),
    )
    .with_graceful_shutdown(wait_for_shutdown(shutdown_rx.clone()));

    let ws_server = axum::serve(
        ws_listener,
        ws_app.into_make_service_with_connect_info::<SocketAddr>(),
    )
    .with_graceful_shutdown(wait_for_shutdown(shutdown_rx.clone()));

    let metrics_server = axum::serve(
        metrics_listener,
        metrics_app.into_make_service_with_connect_info::<SocketAddr>(),
    )
    .with_graceful_shutdown(wait_for_shutdown(shutdown_rx.clone()));

    let servers = async {
        let (h, w, m) = tokio::join!(http_server, ws_server, metrics_server);
        if let Err(e) = h {
            error!("HTTP server error: {}", e);
        }
        if let Err(e) = w {
            error!("WebSocket server error: {}", e);
        }
        if let Err(e) = m {
            error!("Metrics server error: {}", e);
        }
    };

    // Long-lived WebSocket sessions would otherwise keep the process alive
    // forever after SIGTERM; cap the drain at the configured grace period.
    let grace = Duration::from_secs(config.proxy.shutdown_grace_secs);
    let forced_exit = async {
        wait_for_shutdown(shutdown_rx.clone()).await;
        info!(
            "Shutdown requested, draining connections for up to {:?}",
            grace
        );
        tokio::time::sleep(grace).await;
        warn!("Grace period elapsed, exiting with open connections");
    };

    tokio::select! {
        _ = servers => info!("All servers stopped cleanly"),
        _ = forced_exit => {}
    }
}

async fn shutdown_signal() {
    let ctrl_c = async {
        tokio::signal::ctrl_c()
            .await
            .expect("Failed to register Ctrl+C handler");
    };
    let terminate = async {
        signal(SignalKind::terminate())
            .expect("Failed to register SIGTERM handler")
            .recv()
            .await;
    };
    tokio::select! {
        _ = ctrl_c => info!("Received SIGINT"),
        _ = terminate => info!("Received SIGTERM"),
    }
}
