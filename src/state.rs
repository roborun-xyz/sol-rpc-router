use crate::config::Backend;
use crate::health::HealthState;
use axum::body::Body;
use hyper_tls::HttpsConnector;
use hyper_util::client::legacy::{connect::HttpConnector, Client};
use rand::Rng;
use std::{collections::HashMap, sync::Arc};
use tracing::info;

#[derive(Clone)]
pub struct AppState {
    pub client: Client<HttpsConnector<HttpConnector>, Body>,
    pub backends: Vec<Backend>,
    pub api_keys: Vec<String>,
    pub method_routes: HashMap<String, String>,
    pub label_to_url: HashMap<String, String>,
    pub health_state: Arc<HealthState>,
}

impl AppState {
    pub fn select_backend(&self, rpc_method: Option<&str>) -> Option<(&str, &str)> {
        // Check method-specific routing first
        if let Some(method) = rpc_method {
            if let Some(backend_label) = self.method_routes.get(method) {
                if let Some(backend_url) = self.label_to_url.get(backend_label) {
                    // Check if method-routed backend is healthy
                    if let Some(status) = self.health_state.get_status(backend_label) {
                        if status.healthy {
                            info!("Method {} routed to label={}", method, backend_label);
                            return Some((backend_label, backend_url));
                        } else {
                            info!(
                                "Method {} routed to label={} but backend is unhealthy, falling back to weighted selection",
                                method, backend_label
                            );
                        }
                    }
                }
            }
        }

        // Filter out unhealthy backends
        let healthy_backends: Vec<&Backend> = self
            .backends
            .iter()
            .filter(|b| {
                self.health_state
                    .get_status(&b.label)
                    .map(|s| s.healthy)
                    .unwrap_or(true) // Default to healthy if status not found
            })
            .collect();

        if healthy_backends.is_empty() {
            return None; // No healthy backends available
        }

        // Calculate total weight of healthy backends
        let healthy_total_weight: u32 = healthy_backends.iter().map(|b| b.weight).sum();

        // Weighted random selection among healthy backends
        let mut rng = rand::thread_rng();
        let mut random_weight = rng.gen_range(0..healthy_total_weight);

        for backend in &healthy_backends {
            if random_weight < backend.weight {
                return Some((&backend.label, &backend.url));
            }
            random_weight -= backend.weight;
        }

        // Fallback (should never reach here if weights are valid)
        healthy_backends
            .first()
            .map(|b| (b.label.as_str(), b.url.as_str()))
    }
}
