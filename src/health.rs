use std::{
    collections::HashMap,
    sync::{atomic::Ordering, Arc, RwLock},
    time::SystemTime,
};

use arc_swap::ArcSwap;
use axum::{body::Body, http::Request};
use futures_util::future;
use metrics::{gauge, histogram};
use tokio::time::{sleep, timeout, Duration};

use crate::{
    config::{Backend, HealthCheckConfig},
    state::{HttpClient, RouterState},
};

#[derive(Debug, Clone)]
pub struct BackendHealthStatus {
    pub healthy: bool,
    pub last_check_time: Option<SystemTime>,
    pub consecutive_failures: u32,
    pub consecutive_successes: u32,
    pub last_error: Option<String>,
    /// Last slot (or block height) reported by the backend, if the probe
    /// method returns one.
    pub slot: Option<u64>,
    /// Round-trip time of the last probe.
    pub latency_ms: Option<u64>,
}

impl Default for BackendHealthStatus {
    fn default() -> Self {
        Self {
            healthy: true, // Start optimistic - assume backends are healthy
            last_check_time: None,
            consecutive_failures: 0,
            consecutive_successes: 0,
            last_error: None,
            slot: None,
            latency_ms: None,
        }
    }
}

#[derive(Debug)]
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
        self.statuses
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .get(label)
            .cloned()
    }

    pub fn update_status(&self, label: &str, status: BackendHealthStatus) {
        let mut statuses = self.statuses.write().unwrap_or_else(|e| e.into_inner());
        if let Some(s) = statuses.get_mut(label) {
            *s = status;
        } else {
            // If backend is new (hot reload), insert it
            statuses.insert(label.to_string(), status);
        }
    }

    pub fn get_all_statuses(&self) -> HashMap<String, BackendHealthStatus> {
        self.statuses
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }
}

/// Performs a health check against a backend.
/// Returns `Ok(Some(slot))` if the method is `getSlot` or `getBlockHeight` and the response
/// contains a numeric result. Returns `Ok(None)` for other methods. Returns `Err` on failure.
pub async fn perform_health_check(
    client: &HttpClient,
    backend: &Backend,
    health_config: &HealthCheckConfig,
) -> Result<Option<u64>, String> {
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
            if !response.status().is_success() {
                return Err(format!(
                    "Health check returned status: {}",
                    response.status()
                ));
            }

            // Parse the response body to extract slot/block height
            let method = health_config.method.as_str();
            if method == "getSlot" || method == "getBlockHeight" {
                let body_bytes = http_body_util::BodyExt::collect(response.into_body())
                    .await
                    .map_err(|e| format!("Failed to read response body: {}", e))?
                    .to_bytes();

                let json: serde_json::Value = serde_json::from_slice(&body_bytes)
                    .map_err(|e| format!("Failed to parse response JSON: {}", e))?;

                if let Some(slot) = json.get("result").and_then(|v| v.as_u64()) {
                    Ok(Some(slot))
                } else {
                    Err(format!(
                        "Health check response missing numeric 'result' field for method {}",
                        method
                    ))
                }
            } else {
                Ok(None)
            }
        }
        Ok(Err(e)) => Err(format!("Health check request failed: {}", e)),
        Err(_) => Err(format!(
            "Health check timed out after {}s",
            health_config.timeout_secs
        )),
    }
}

/// What one probe meant for a backend.
#[derive(Debug, PartialEq, Eq)]
pub enum ProbeOutcome {
    Healthy,
    Lagging { slot: u64, max: u64 },
    Failed(String),
}

/// Applies one probe result to a backend's status: counts consecutive
/// successes/failures, flips `healthy` at the configured thresholds, and
/// treats "more than `max_slot_lag` behind the best backend" as a failure.
pub fn apply_probe(
    status: &mut BackendHealthStatus,
    result: Result<Option<u64>, String>,
    max_slot: Option<u64>,
    cfg: &HealthCheckConfig,
) -> ProbeOutcome {
    let outcome = match result {
        Ok(slot_opt) => {
            if let Some(slot) = slot_opt {
                status.slot = Some(slot);
            }
            match (slot_opt, max_slot) {
                (Some(slot), Some(max)) if max > slot && (max - slot) > cfg.max_slot_lag => {
                    ProbeOutcome::Lagging { slot, max }
                }
                _ => ProbeOutcome::Healthy,
            }
        }
        Err(e) => ProbeOutcome::Failed(e),
    };

    match &outcome {
        ProbeOutcome::Healthy => {
            status.consecutive_successes += 1;
            status.consecutive_failures = 0;
            status.last_error = None;
            if status.consecutive_successes >= cfg.consecutive_successes_threshold {
                status.healthy = true;
            }
        }
        ProbeOutcome::Lagging { slot, max } => {
            status.consecutive_failures += 1;
            status.consecutive_successes = 0;
            status.last_error = Some(format!(
                "Backend lagging: slot {} is {} behind max {}",
                slot,
                max - slot,
                max
            ));
            if status.consecutive_failures >= cfg.consecutive_failures_threshold {
                status.healthy = false;
            }
        }
        ProbeOutcome::Failed(e) => {
            status.consecutive_failures += 1;
            status.consecutive_successes = 0;
            status.last_error = Some(e.clone());
            if status.consecutive_failures >= cfg.consecutive_failures_threshold {
                status.healthy = false;
            }
        }
    }
    outcome
}

pub async fn health_check_loop(client: HttpClient, router_state: Arc<ArcSwap<RouterState>>) {
    loop {
        // Load the current state for this iteration
        let current_state = router_state.load();

        let health_config = &current_state.health_check_config;
        let health_state = &current_state.health_state;
        let check_interval = Duration::from_secs(health_config.interval_secs);

        // Run all health checks concurrently so one slow backend doesn't block others
        let check_futures: Vec<_> = current_state
            .backends
            .iter()
            .map(|backend| {
                let client = client.clone();
                let config = backend.config.clone();
                let hc = health_config.clone();
                async move {
                    let started = std::time::Instant::now();
                    let result = perform_health_check(&client, &config, &hc).await;
                    (config.label.clone(), result, started.elapsed())
                }
            })
            .collect();

        let results = future::join_all(check_futures).await;

        // Collect slot numbers from successful checks to determine the max (consensus tip)
        let max_slot: Option<u64> = results
            .iter()
            .filter_map(|(_, result, _)| match result {
                Ok(Some(slot)) => Some(*slot),
                _ => None,
            })
            .max();

        for (i, (label, check_result, elapsed)) in results.into_iter().enumerate() {
            let backend = &current_state.backends[i];
            let latency_ms = elapsed.as_millis() as u64;
            histogram!("rpc_backend_health_check_duration_seconds", "backend" => label.clone())
                .record(elapsed.as_secs_f64());

            // Get current status from the detailed state
            let mut current_status = health_state.get_status(&label).unwrap_or_default();

            let previous_healthy = current_status.healthy;

            current_status.latency_ms = Some(latency_ms);
            if let Ok(Some(slot)) = &check_result {
                gauge!("rpc_backend_slot", "backend" => label.clone()).set(*slot as f64);
                if let Some(max) = max_slot {
                    gauge!("rpc_backend_slot_lag", "backend" => label.clone())
                        .set(max.saturating_sub(*slot) as f64);
                }
            }

            match apply_probe(&mut current_status, check_result, max_slot, health_config) {
                ProbeOutcome::Healthy => tracing::debug!(
                    "Health check succeeded for backend {} (consecutive successes: {})",
                    label,
                    current_status.consecutive_successes
                ),
                ProbeOutcome::Lagging { slot, max } => tracing::warn!(
                    "Backend {} is lagging: slot {} is {} behind consensus max {} (threshold: {})",
                    label,
                    slot,
                    max - slot,
                    max,
                    health_config.max_slot_lag
                ),
                ProbeOutcome::Failed(error) => tracing::warn!(
                    "Health check failed for backend {} (consecutive failures: {}): {}",
                    label,
                    current_status.consecutive_failures,
                    error
                ),
            }

            current_status.last_check_time = Some(SystemTime::now());

            // Log state transitions
            if previous_healthy && !current_status.healthy {
                tracing::warn!(
                    "Backend {} marked as UNHEALTHY after {} consecutive failures",
                    label,
                    current_status.consecutive_failures
                );
            } else if !previous_healthy && current_status.healthy {
                tracing::info!(
                    "Backend {} marked as HEALTHY after {} consecutive successes",
                    label,
                    current_status.consecutive_successes
                );
            }

            // Update metrics
            gauge!("rpc_backend_health", "backend" => label.clone())
                .set(if current_status.healthy { 1.0 } else { 0.0 });

            // Update detailed state (locked)
            health_state.update_status(&label, current_status.clone());

            // Update atomic boolean (lock-free)
            backend
                .healthy
                .store(current_status.healthy, Ordering::Relaxed);
        }

        // Release the guard before sleeping so we don't hold old state in memory if it gets swapped
        drop(current_state);

        sleep(check_interval).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg() -> HealthCheckConfig {
        HealthCheckConfig {
            consecutive_failures_threshold: 2,
            consecutive_successes_threshold: 2,
            max_slot_lag: 50,
            ..Default::default()
        }
    }

    #[test]
    fn failures_flip_unhealthy_at_threshold_and_successes_recover() {
        let mut st = BackendHealthStatus::default();
        assert!(st.healthy);
        apply_probe(&mut st, Err("boom".into()), None, &cfg());
        assert!(st.healthy, "one failure is below threshold");
        apply_probe(&mut st, Err("boom".into()), None, &cfg());
        assert!(!st.healthy);
        assert_eq!(st.last_error.as_deref(), Some("boom"));

        apply_probe(&mut st, Ok(Some(100)), Some(100), &cfg());
        assert!(!st.healthy, "one success is below threshold");
        apply_probe(&mut st, Ok(Some(101)), Some(101), &cfg());
        assert!(st.healthy);
        assert_eq!(st.slot, Some(101));
        assert!(st.last_error.is_none());
    }

    #[test]
    fn lag_beyond_threshold_counts_as_failure() {
        let mut st = BackendHealthStatus::default();
        let out = apply_probe(&mut st, Ok(Some(1000)), Some(1060), &cfg());
        assert_eq!(
            out,
            ProbeOutcome::Lagging {
                slot: 1000,
                max: 1060
            }
        );
        assert_eq!(st.consecutive_failures, 1);
        assert!(st.last_error.unwrap().contains("60 behind"));

        // Exactly at the threshold is fine.
        let mut st = BackendHealthStatus::default();
        assert_eq!(
            apply_probe(&mut st, Ok(Some(1000)), Some(1050), &cfg()),
            ProbeOutcome::Healthy
        );
        // The best backend itself is never lagging.
        assert_eq!(
            apply_probe(&mut st, Ok(Some(1050)), Some(1050), &cfg()),
            ProbeOutcome::Healthy
        );
        // Methods that return no slot can't lag.
        assert_eq!(
            apply_probe(&mut st, Ok(None), Some(1050), &cfg()),
            ProbeOutcome::Healthy
        );
    }
}
