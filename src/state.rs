use std::{collections::HashMap, sync::Arc};

use axum::body::Body;
use hyper_tls::HttpsConnector;
use hyper_util::client::legacy::{connect::HttpConnector, Client};
use rand::Rng;
use tracing::info;

use crate::{config::Backend, health::HealthState};

#[derive(Clone)]
pub struct AppState {
    pub client: Client<HttpsConnector<HttpConnector>, Body>,
    pub backends: Vec<Backend>,
    pub api_keys: Vec<String>,
    pub method_routes: HashMap<String, String>,
    pub label_to_url: HashMap<String, String>,
    pub health_state: Arc<HealthState>,
    pub proxy_timeout_secs: u64,
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

    /// Select a healthy backend that has WebSocket support (ws_url configured)
    pub fn select_ws_backend(&self) -> Option<(&str, &str)> {
        // Filter to backends with ws_url configured and healthy
        let ws_backends: Vec<&Backend> = self
            .backends
            .iter()
            .filter(|b| {
                b.ws_url.is_some()
                    && self
                        .health_state
                        .get_status(&b.label)
                        .map(|s| s.healthy)
                        .unwrap_or(true)
            })
            .collect();

        if ws_backends.is_empty() {
            return None;
        }

        // Calculate total weight of WebSocket-capable backends
        let total_weight: u32 = ws_backends.iter().map(|b| b.weight).sum();

        // Weighted random selection
        let mut rng = rand::thread_rng();
        let mut random_weight = rng.gen_range(0..total_weight);

        for backend in &ws_backends {
            if random_weight < backend.weight {
                return Some((
                    backend.label.as_str(),
                    backend.ws_url.as_ref().unwrap().as_str(),
                ));
            }
            random_weight -= backend.weight;
        }

        // Fallback
        ws_backends.first().map(|b| {
            (
                b.label.as_str(),
                b.ws_url.as_ref().unwrap().as_str(),
            )
        })
    }
}
