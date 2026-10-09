use std::{
    collections::{HashMap, HashSet},
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
};

use arc_swap::ArcSwap;
use axum::body::Body;
use hyper_tls::HttpsConnector;
use hyper_util::client::legacy::{connect::HttpConnector, Client};
use rand::Rng;
use tracing::{debug, info};

use crate::{
    config::{Backend, Config, HealthCheckConfig, ProxyConfig},
    health::HealthState,
    keystore::KeyStore,
};

pub type HttpClient = Client<HttpsConnector<HttpConnector>, Body>;

#[derive(Debug, Clone)]
pub struct RuntimeBackend {
    pub config: Backend,
    pub healthy: Arc<AtomicBool>,
}

impl RuntimeBackend {
    pub fn new(config: Backend, healthy: bool) -> Self {
        Self {
            config,
            healthy: Arc::new(AtomicBool::new(healthy)),
        }
    }

    #[inline]
    pub fn is_healthy(&self) -> bool {
        self.healthy.load(Ordering::Relaxed)
    }
}

/// A backend chosen for one upstream attempt.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Selection {
    pub label: String,
    pub url: String,
}

#[derive(Debug, Clone)]
pub struct RouterState {
    pub backends: Vec<RuntimeBackend>,
    pub method_routes: HashMap<String, String>,
    pub health_state: Arc<HealthState>,
    pub proxy: ProxyConfig,
    pub health_check_config: HealthCheckConfig,
    /// Pre-computed for O(1) lookups on the hot path.
    pub fanout_methods: HashSet<String>,
    pub blocked_methods: HashSet<String>,
}

impl RouterState {
    /// Builds runtime state from a config. `initial_health` lets a hot reload
    /// carry over the last known health of backends that keep their label.
    pub fn from_config(
        config: &Config,
        health_state: Arc<HealthState>,
        initial_health: impl Fn(&str) -> bool,
    ) -> Self {
        let backends = config
            .backends
            .iter()
            .map(|b| RuntimeBackend::new(b.clone(), initial_health(&b.label)))
            .collect();

        Self {
            backends,
            method_routes: config.method_routes.clone(),
            health_state,
            fanout_methods: config.proxy.fanout_methods.iter().cloned().collect(),
            blocked_methods: config.proxy.blocked_methods.iter().cloned().collect(),
            proxy: config.proxy.clone(),
            health_check_config: config.health_check.clone(),
        }
    }

    /// Convenience constructor for tests and the in-process benchmark.
    pub fn simple(backends: Vec<RuntimeBackend>, health_state: Arc<HealthState>) -> Self {
        Self {
            backends,
            method_routes: HashMap::new(),
            health_state,
            proxy: ProxyConfig::default(),
            health_check_config: HealthCheckConfig::default(),
            fanout_methods: HashSet::new(),
            blocked_methods: HashSet::new(),
        }
    }

    pub fn is_blocked(&self, method: &str) -> bool {
        self.blocked_methods.contains(method)
    }

    pub fn is_fanout(&self, method: &str) -> bool {
        self.fanout_methods.contains(method)
    }

    /// All healthy backends, in config order.
    pub fn healthy_backends(&self) -> Vec<Selection> {
        self.backends
            .iter()
            .filter(|b| b.is_healthy())
            .map(|b| Selection {
                label: b.config.label.clone(),
                url: b.config.url.clone(),
            })
            .collect()
    }

    /// Picks a backend for `rpc_method`, honouring method routes first and
    /// falling back to weighted random among healthy backends. Backends whose
    /// label is in `exclude` are skipped, which is how retries avoid hitting
    /// the backend that just failed.
    pub fn select_backend(
        &self,
        rpc_method: Option<&str>,
        exclude: &[String],
    ) -> Option<Selection> {
        let allowed = |b: &RuntimeBackend| b.is_healthy() && !exclude.contains(&b.config.label);

        if let Some(method) = rpc_method {
            if let Some(target) = self.method_routes.get(method) {
                match self.backends.iter().find(|b| b.config.label == *target) {
                    Some(b) if allowed(b) => {
                        debug!("Method {} routed to label={}", method, target);
                        return Some(Selection {
                            label: b.config.label.clone(),
                            url: b.config.url.clone(),
                        });
                    }
                    Some(_) => info!(
                        "Method {} target label={} unavailable, falling back to weighted selection",
                        method, target
                    ),
                    None => {}
                }
            }
        }

        let candidates: Vec<&RuntimeBackend> =
            self.backends.iter().filter(|b| allowed(b)).collect();
        weighted_pick(&candidates).map(|b| Selection {
            label: b.config.label.clone(),
            url: b.config.url.clone(),
        })
    }

    /// Picks a healthy backend that has a `ws_url`, by weight.
    pub fn select_ws_backend(&self) -> Option<Selection> {
        let candidates: Vec<&RuntimeBackend> = self
            .backends
            .iter()
            .filter(|b| b.config.ws_url.is_some() && b.is_healthy())
            .collect();
        weighted_pick(&candidates).map(|b| Selection {
            label: b.config.label.clone(),
            url: b.config.ws_url.clone().expect("filtered on ws_url"),
        })
    }
}

/// Weighted random selection. Weights are validated > 0 at config load, but a
/// zero total (e.g. hand-built test state) degrades to "first candidate".
fn weighted_pick<'a>(candidates: &[&'a RuntimeBackend]) -> Option<&'a RuntimeBackend> {
    if candidates.is_empty() {
        return None;
    }
    let total: u32 = candidates.iter().map(|b| b.config.weight).sum();
    if total == 0 {
        return candidates.first().copied();
    }
    let mut roll = rand::thread_rng().gen_range(0..total);
    for b in candidates {
        if roll < b.config.weight {
            return Some(b);
        }
        roll -= b.config.weight;
    }
    candidates.first().copied()
}

#[derive(Clone)]
pub struct AppState {
    pub client: HttpClient,
    pub keystore: Arc<dyn KeyStore>,
    pub state: Arc<ArcSwap<RouterState>>,
}

impl AppState {
    pub fn select_backend(&self, rpc_method: Option<&str>) -> Option<(String, String)> {
        self.state
            .load()
            .select_backend(rpc_method, &[])
            .map(|s| (s.label, s.url))
    }

    pub fn select_ws_backend(&self) -> Option<(String, String)> {
        self.state
            .load()
            .select_ws_backend()
            .map(|s| (s.label, s.url))
    }
}
