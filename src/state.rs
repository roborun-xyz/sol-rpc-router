use std::{
    collections::HashMap,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
};

use axum::body::Body;
use hyper_tls::HttpsConnector;
use hyper_util::client::legacy::{connect::HttpConnector, Client};
use rand::Rng;
use tracing::info;

use crate::{config::Backend, health::HealthState, keystore::KeyStore};

#[derive(Debug, Clone)]
pub struct RuntimeBackend {
    pub config: Backend,
    pub healthy: Arc<AtomicBool>,
}

#[derive(Clone)]
pub struct AppState {
    pub client: Client<HttpsConnector<HttpConnector>, Body>,
    pub backends: Vec<RuntimeBackend>,
    pub keystore: Arc<dyn KeyStore>,
    pub method_routes: HashMap<String, String>,
    pub health_state: Arc<HealthState>,
    pub proxy_timeout_secs: u64,
}

impl AppState {
    pub fn select_backend(&self, rpc_method: Option<&str>) -> Option<(&str, &str)> {
        // Check method-specific routing first
        if let Some(method) = rpc_method {
            if let Some(backend_label) = self.method_routes.get(method) {
                // Find the backend by label to check its atomic health
                if let Some(backend) = self
                    .backends
                    .iter()
                    .find(|b| b.config.label == *backend_label)
                {
                    if backend.healthy.load(Ordering::Relaxed) {
                        info!("Method {} routed to label={}", method, backend_label);
                        return Some((&backend.config.label, &backend.config.url));
                    } else {
                        info!(
                            "Method {} routed to label={} but backend is unhealthy, falling back to weighted selection",
                            method, backend_label
                        );
                    }
                }
            }
        }

        // Filter out unhealthy backends (lock-free)
        let healthy_backends: Vec<&RuntimeBackend> = self
            .backends
            .iter()
            .filter(|b| b.healthy.load(Ordering::Relaxed))
            .collect();

        if healthy_backends.is_empty() {
            return None; // No healthy backends available
        }

        // Calculate total weight of healthy backends
        let healthy_total_weight: u32 = healthy_backends.iter().map(|b| b.config.weight).sum();

        if healthy_total_weight == 0 {
            return healthy_backends
                .first()
                .map(|b| (b.config.label.as_str(), b.config.url.as_str()));
        }

        // Weighted random selection among healthy backends
        let mut rng = rand::thread_rng();
        let mut random_weight = rng.gen_range(0..healthy_total_weight);

        for backend in &healthy_backends {
            if random_weight < backend.config.weight {
                return Some((&backend.config.label, &backend.config.url));
            }
            random_weight -= backend.config.weight;
        }

        // Fallback (should never reach here if weights are valid)
        healthy_backends
            .first()
            .map(|b| (b.config.label.as_str(), b.config.url.as_str()))
    }

    /// Select a healthy backend that has WebSocket support (ws_url configured)
    pub fn select_ws_backend(&self) -> Option<(&str, &str)> {
        // Filter to backends with ws_url configured and healthy (lock-free)
        let ws_backends: Vec<&RuntimeBackend> = self
            .backends
            .iter()
            .filter(|b| b.config.ws_url.is_some() && b.healthy.load(Ordering::Relaxed))
            .collect();

        if ws_backends.is_empty() {
            return None;
        }

        // Calculate total weight of WebSocket-capable backends
        let total_weight: u32 = ws_backends.iter().map(|b| b.config.weight).sum();

        // Weighted random selection
        let mut rng = rand::thread_rng();
        let mut random_weight = rng.gen_range(0..total_weight);

        for backend in &ws_backends {
            if random_weight < backend.config.weight {
                return Some((
                    backend.config.label.as_str(),
                    backend.config.ws_url.as_ref().unwrap().as_str(),
                ));
            }
            random_weight -= backend.config.weight;
        }

        // Fallback
        ws_backends.first().map(|b| {
            (
                b.config.label.as_str(),
                b.config.ws_url.as_ref().unwrap().as_str(),
            )
        })
    }
}
