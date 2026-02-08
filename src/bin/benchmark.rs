use std::{
    net::SocketAddr,
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    },
    time::{Duration, Instant},
};

use axum::{
    extract::Json,
    routing::post,
    Router,
};
use clap::Parser;
use hyper_util::{client::legacy::Client, rt::TokioExecutor};
use serde_json::{json, Value};
use tokio::sync::Barrier;
use bytes::Bytes;
use http_body_util::Full;

#[derive(Parser, Debug)]
#[command(author, version, about, long_about = None)]
struct Args {
    /// Number of concurrent clients
    #[arg(short, long, default_value_t = 50)]
    concurrency: usize,

    /// Duration of the benchmark in seconds
    #[arg(short, long, default_value_t = 10)]
    duration: u64,

    /// Target RPS (0 for unlimited)
    #[arg(short, long, default_value_t = 0)]
    target_rps: usize,
}

// Mock upstream server that returns a fixed response
async fn mock_upstream(addr: SocketAddr) {
    let app = Router::new().route("/", post(|Json(_payload): Json<Value>| async {
        Json(json!({
            "jsonrpc": "2.0",
            "result": "0x1234567890abcdef",
            "id": 1
        }))
    }));

    let listener = tokio::net::TcpListener::bind(addr).await.unwrap();
    println!("Mock upstream listening on {}", addr);
    axum::serve(listener, app).await.unwrap();
}

#[tokio::main]
async fn main() {
    let args = Args::parse();

    // 1. Spawn Mock Upstream
    let mock_port = 9090;
    let mock_addr = SocketAddr::from(([127, 0, 0, 1], mock_port));
    tokio::spawn(mock_upstream(mock_addr));

    // Wait for mock server to be ready
    tokio::time::sleep(Duration::from_secs(1)).await;

    // 2. Configure Router (We'll assume the router is running separately for now or spawn it here if possible)
    // For this benchmark, we'll assume the user runs the router separately or we can try to import the router logic.
    // However, importing the router logic might be tricky if it's in `main.rs`.
    // Let's assume the router is running on port 8080.
    // Wait, the prompt says "Runs the router pointing to this mock".
    // I should probably spawn the router process or integrate its logic.
    // Integrating logic is better if `main.rs` allows it, but usually it doesn't expose `main`.
    // Let's spawn the router process.

    // Write a temporary config file
    let config_content = format!(
        r#"
bind_addr = "127.0.0.1:8080"
rpc_path = "/"
ws_path = "/ws"
health_check_interval_ms = 1000

[[upstreams]]
url = "http://127.0.0.1:{}"
weight = 1
"#,
        mock_port
    );
    let config_path = "bench_config.toml";
    std::fs::write(config_path, config_content).expect("Failed to write config");

    // Spawn the router process
    println!("Spawning router...");
    let mut router_process = std::process::Command::new("cargo")
        .args(&["run", "--release", "--", "--config", config_path])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .expect("Failed to spawn router");

    // Wait for router to start
    tokio::time::sleep(Duration::from_secs(5)).await;

    // 3. Flood the router
    let target_url = "http://127.0.0.1:8080/";
    let client: Client<hyper_util::client::legacy::connect::HttpConnector, Full<Bytes>> =
        Client::builder(TokioExecutor::new()).build_http();

    let start_time = Instant::now();
    let duration = Duration::from_secs(args.duration);
    let request_count = Arc::new(AtomicUsize::new(0));
    let latencies = Arc::new(tokio::sync::Mutex::new(Vec::new()));

    let barrier = Arc::new(Barrier::new(args.concurrency));
    let mut handles = Vec::new();

    println!("Starting benchmark with {} concurrent clients for {} seconds...", args.concurrency, args.duration);

    for _ in 0..args.concurrency {
        let client = client.clone();
        let request_count = request_count.clone();
        let latencies = latencies.clone();
        let barrier = barrier.clone();
        let target_url = target_url.to_string();

        handles.push(tokio::spawn(async move {
            barrier.wait().await;
            let mut local_latencies = Vec::new();
            while start_time.elapsed() < duration {
                let req_start = Instant::now();
                let body = json!({
                    "jsonrpc": "2.0",
                    "method": "eth_blockNumber",
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
                        request_count.fetch_add(1, Ordering::Relaxed);
                    }
                    Err(e) => {
                        eprintln!("Request failed: {}", e);
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

    // Stop router
    router_process.kill().expect("Failed to kill router");
    std::fs::remove_file(config_path).unwrap_or(());

    // 4. Report Results
    let total_requests = request_count.load(Ordering::Relaxed);
    let elapsed = start_time.elapsed().as_secs_f64();
    let rps = total_requests as f64 / elapsed;

    let mut latencies = latencies.lock().await;
    latencies.sort();
    let p99_idx = (latencies.len() as f64 * 0.99) as usize;
    let p99 = latencies.get(p99_idx).unwrap_or(&0);

    println!("\n--- Benchmark Results ---");
    println!("Total Requests: {}", total_requests);
    println!("Duration: {:.2}s", elapsed);
    println!("RPS: {:.2}", rps);
    println!("P99 Latency: {:.2}ms", *p99 as f64 / 1000.0);
}
