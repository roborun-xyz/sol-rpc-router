use std::{
    collections::HashMap,
    sync::{atomic::Ordering, Arc, RwLock},
    time::SystemTime,
};

use axum::{body::Body, http::Request};
use hyper_tls::HttpsConnector;
use hyper_util::client::legacy::{connect::HttpConnector, Client};
use metrics::gauge;
use tokio::time::{sleep, timeout, Duration};

use crate::{
    config::{Backend, HealthCheckConfig},
    state::RuntimeBackend,
};

#[derive(Debug, Clone)]
pub struct BackendHealthStatus {
    pub healthy: bool,
    pub last_check_time: Option<SystemTime>,
    pub consecutive_failures: u32,
    pub consecutive_successes: u32,
    pub last_error: Option<String>,
}

impl Default for BackendHealthStatus {
    fn default() -> Self {
        Self {
            healthy: true, // Start optimistic - assume backends are healthy
            last_check_time: None,
            consecutive_failures: 0,
            consecutive_successes: 0,
            last_error: None,
        }
    }
}

pub struct HealthState {
    statuses: RwLock<HashMap<String, BackendHealthStatus>>,
}

impl HealthState {
    pub fn new(backend_labels: Vec<String>) -> Self {
        let mut statuses = HashMap::new();
        for label in backend_labels {
            statuses.insert(label, BackendHealthStatus::default());
        }
        Self {
            statuses: RwLock::new(statuses),
        }
    }

    pub fn get_status(&self, label: &str) -> Option<BackendHealthStatus> {
        self.statuses.read().unwrap().get(label).cloned()
    }

    pub fn update_status(&self, label: &str, status: BackendHealthStatus) {
        if let Some(s) = self.statuses.write().unwrap().get_mut(label) {
            *s = status;
        }
    }

    pub fn get_all_statuses(&self) -> HashMap<String, BackendHealthStatus> {
        self.statuses.read().unwrap().clone()
    }
}

async fn perform_health_check(
    client: &Client<HttpsConnector<HttpConnector>, Body>,
    backend: &Backend,
    health_config: &HealthCheckConfig,
) -> Result<(), String> {
    // Build health check request
    let health_request = serde_json::json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": health_config.method,
        "params": []
    });

    let body_bytes = serde_json::to_vec(&health_request)
        .map_err(|e| format!("Failed to serialize health check: {}", e))?;

    let req = Request::builder()
        .method("POST")
        .uri(&backend.url)
        .header("content-type", "application/json")
        .body(Body::from(body_bytes))
        .map_err(|e| format!("Failed to build request: {}", e))?;

    // Perform request with timeout
    let result = timeout(
        Duration::from_secs(health_config.timeout_secs),
        client.request(req),
    )
    .await;

    match result {
        Ok(Ok(response)) => {
            if response.status().is_success() {
                Ok(())
            } else {
                Err(format!(
                    "Health check returned status: {}",
                    response.status()
                ))
            }
        }
        Ok(Err(e)) => Err(format!("Health check request failed: {}", e)),
        Err(_) => Err(format!(
            "Health check timed out after {}s",
            health_config.timeout_secs
        )),
    }
}

pub async fn health_check_loop(
    client: Client<HttpsConnector<HttpConnector>, Body>,
    backends: Vec<RuntimeBackend>,
    health_state: Arc<HealthState>,
    health_config: HealthCheckConfig,
) {
    let check_interval = Duration::from_secs(health_config.interval_secs);

    loop {
        for backend in &backends {
            let check_result = perform_health_check(&client, &backend.config, &health_config).await;

            // Get current status from the detailed state
            let mut current_status = health_state
                .get_status(&backend.config.label)
                .unwrap_or_default();

            let previous_healthy = current_status.healthy;

            match check_result {
                Ok(_) => {
                    current_status.consecutive_successes += 1;
                    current_status.consecutive_failures = 0;
                    current_status.last_error = None;

                    // Mark healthy if threshold reached
                    if current_status.consecutive_successes
                        >= health_config.consecutive_successes_threshold
                    {
                        current_status.healthy = true;
                    }

                    tracing::debug!(
                        "Health check succeeded for backend {} (consecutive successes: {})",
                        backend.config.label,
                        current_status.consecutive_successes
                    );
                }
                Err(error) => {
                    current_status.consecutive_failures += 1;
                    current_status.consecutive_successes = 0;
                    current_status.last_error = Some(error.clone());

                    // Mark unhealthy if threshold reached
                    if current_status.consecutive_failures
                        >= health_config.consecutive_failures_threshold
                    {
                        current_status.healthy = false;
                    }

                    tracing::warn!(
                        "Health check failed for backend {} (consecutive failures: {}): {}",
                        backend.config.label,
                        current_status.consecutive_failures,
                        error
                    );
                }
            }

            current_status.last_check_time = Some(SystemTime::now());

            // Log state transitions
            if previous_healthy && !current_status.healthy {
                tracing::warn!(
                    "Backend {} marked as UNHEALTHY after {} consecutive failures",
                    backend.config.label,
                    current_status.consecutive_failures
                );
            } else if !previous_healthy && current_status.healthy {
                tracing::info!(
                    "Backend {} marked as HEALTHY after {} consecutive successes",
                    backend.config.label,
                    current_status.consecutive_successes
                );
            }

            // Update metrics
            gauge!("rpc_backend_health", "backend" => backend.config.label.clone())
                .set(if current_status.healthy { 1.0 } else { 0.0 });

            // Update detailed state (locked)
            health_state.update_status(&backend.config.label, current_status.clone());

            // Update atomic boolean (lock-free)
            backend
                .healthy
                .store(current_status.healthy, Ordering::Relaxed);
        }

        sleep(check_interval).await;
    }
}
