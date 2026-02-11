use std::{
    collections::HashMap,
    net::SocketAddr,
    sync::{
        atomic::{AtomicBool, AtomicUsize, Ordering},
        Arc,
    },
    time::Instant,
};

use arc_swap::ArcSwap;
use axum::{
    extract::Json,
    middleware,
    routing::{get, post},
    Router,
};
use bytes::Bytes;
use clap::Parser;
use http_body_util::Full;
use hyper_tls::HttpsConnector;
use hyper_util::{client::legacy::Client, rt::TokioExecutor};
use serde_json::{json, Value};
use sol_rpc_router::{
    config::Backend,
    handlers::{extract_rpc_method, health_endpoint, proxy, track_metrics},
    health::HealthState,
    mock::MockKeyStore,
    state::{AppState, RouterState, RuntimeBackend},
};
use tokio::sync::Barrier;

#[derive(Parser, Debug)]
#[command(author, version, about = "Benchmark for sol-rpc-router")]
struct Args {
    /// Number of concurrent clients
    #[arg(short, long, default_value_t = 50)]
    concurrency: usize,

    /// Duration of the benchmark in seconds
    #[arg(short, long, default_value_t = 10)]
    duration: u64,
}

/// Spawn a mock upstream that returns a fixed JSON-RPC response.
/// Returns the address it's listening on.
async fn start_mock_upstream() -> SocketAddr {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();

    tokio::spawn(async move {
        let app = Router::new().route(
            "/",
            post(|Json(_payload): Json<Value>| async {
                Json(json!({
                    "jsonrpc": "2.0",
                    "result": "0x1234567890abcdef",
                    "id": 1
                }))
            }),
        );
        axum::serve(listener, app).await.unwrap();
    });

    addr
}

/// Build and start the router in-process, returning the address it's listening on.
async fn start_router(upstream_addr: SocketAddr) -> SocketAddr {
    let https = HttpsConnector::new();
    let client = Client::builder(TokioExecutor::new()).build(https);

    let keystore = Arc::new(MockKeyStore::new());
    keystore.add_key("bench-key", "benchmark", 999_999_999);

    let backend = Backend {
        label: "mock-upstream".to_string(),
        url: format!("http://{}", upstream_addr),
        ws_url: None,
        weight: 1,
    };

    let runtime_backend = RuntimeBackend {
        config: backend,
        healthy: Arc::new(AtomicBool::new(true)),
    };

    let health_state = Arc::new(HealthState::new(vec!["mock-upstream".to_string()]));

    let router_state = RouterState {
        backends: vec![runtime_backend],
        method_routes: HashMap::new(),
        health_state: health_state.clone(),
        proxy_timeout_secs: 30,
        health_check_config: sol_rpc_router::config::HealthCheckConfig::default(),
    };

    let state = Arc::new(AppState {
        client,
        keystore,
        state: Arc::new(ArcSwap::from_pointee(router_state)),
    });

    let app = Router::new()
        .route("/", post(proxy))
        .route("/health", get(health_endpoint))
        .with_state(state)
        .layer(middleware::from_fn(track_metrics))
        .layer(middleware::from_fn(extract_rpc_method));

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();

    tokio::spawn(async move {
        axum::serve(
            listener,
            app.into_make_service_with_connect_info::<SocketAddr>(),
        )
        .await
        .unwrap();
    });

    addr
}

#[tokio::main]
async fn main() {
    let args = Args::parse();

    // 1. Start mock upstream
    let upstream_addr = start_mock_upstream().await;
    println!("Mock upstream listening on {}", upstream_addr);

    // 2. Start router in-process (no Redis, no config file)
    let router_addr = start_router(upstream_addr).await;
    println!("Router listening on {}", router_addr);

    // Give servers a moment to be fully ready
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;

    // 3. Flood the router
    let target_url = format!("http://{}/?api-key=bench-key", router_addr);
    let client: Client<hyper_util::client::legacy::connect::HttpConnector, Full<Bytes>> =
        Client::builder(TokioExecutor::new()).build_http();

    let start_time = Instant::now();
    let duration = std::time::Duration::from_secs(args.duration);
    let success_count = Arc::new(AtomicUsize::new(0));
    let error_count = Arc::new(AtomicUsize::new(0));
    let latencies = Arc::new(tokio::sync::Mutex::new(Vec::new()));

    let barrier = Arc::new(Barrier::new(args.concurrency));
    let mut handles = Vec::new();

    println!(
        "Starting benchmark: {} clients, {} seconds...",
        args.concurrency, args.duration
    );

    for _ in 0..args.concurrency {
        let client = client.clone();
        let success_count = success_count.clone();
        let error_count = error_count.clone();
        let latencies = latencies.clone();
        let barrier = barrier.clone();
        let target_url = target_url.clone();

        handles.push(tokio::spawn(async move {
            barrier.wait().await;
            let mut local_latencies = Vec::new();

            while start_time.elapsed() < duration {
                let req_start = Instant::now();
                let body = json!({
                    "jsonrpc": "2.0",
                    "method": "getSlot",
                    "params": [],
                    "id": 1
                });

                let req = hyper::Request::builder()
                    .method("POST")
                    .uri(&target_url)
                    .header("content-type", "application/json")
                    .body(Full::new(Bytes::from(serde_json::to_vec(&body).unwrap())))
                    .unwrap();

                match client.request(req).await {
                    Ok(_) => {
                        local_latencies.push(req_start.elapsed().as_micros() as u64);
                        success_count.fetch_add(1, Ordering::Relaxed);
                    }
                    Err(e) => {
                        eprintln!("Request failed: {}", e);
                        error_count.fetch_add(1, Ordering::Relaxed);
                    }
                }
            }

            let mut l = latencies.lock().await;
            l.extend(local_latencies);
        }));
    }

    for handle in handles {
        handle.await.unwrap();
    }

    // 4. Report results
    let total_success = success_count.load(Ordering::Relaxed);
    let total_errors = error_count.load(Ordering::Relaxed);
    let elapsed = start_time.elapsed().as_secs_f64();
    let rps = total_success as f64 / elapsed;

    let mut latencies = latencies.lock().await;
    latencies.sort();

    let avg = if latencies.is_empty() {
        0.0
    } else {
        latencies.iter().sum::<u64>() as f64 / latencies.len() as f64 / 1000.0
    };

    let p50 = latencies
        .get(latencies.len() / 2)
        .copied()
        .unwrap_or(0) as f64
        / 1000.0;

    let p99_idx = ((latencies.len() as f64) * 0.99) as usize;
    let p99 = latencies.get(p99_idx).copied().unwrap_or(0) as f64 / 1000.0;

    let p999_idx = ((latencies.len() as f64) * 0.999) as usize;
    let p999 = latencies.get(p999_idx).copied().unwrap_or(0) as f64 / 1000.0;

    println!("\n--- Benchmark Results ---");
    println!("Duration:        {:.2}s", elapsed);
    println!("Concurrency:     {}", args.concurrency);
    println!("Total Requests:  {}", total_success + total_errors);
    println!("Successful:      {}", total_success);
    println!("Errors:          {}", total_errors);
    println!("RPS:             {:.2}", rps);
    println!("Avg Latency:     {:.2}ms", avg);
    println!("P50 Latency:     {:.2}ms", p50);
    println!("P99 Latency:     {:.2}ms", p99);
    println!("P99.9 Latency:   {:.2}ms", p999);
}
