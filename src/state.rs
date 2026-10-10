use std::{
    collections::{HashMap, HashSet},
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Mutex,
    },
};

use std::time::Instant;

use arc_swap::ArcSwap;
use axum::body::Body;
use hyper_tls::HttpsConnector;
use hyper_util::client::legacy::{connect::HttpConnector, Client};
use rand::Rng;
use tracing::debug;

use crate::{
    config::{Backend, Config, HealthCheckConfig, ProxyConfig},
    health::HealthState,
    keystore::KeyStore,
};

pub type HttpClient = Client<HttpsConnector<HttpConnector>, Body>;

/// Token bucket with a one-second burst, refilled continuously.
#[derive(Debug)]
pub struct TokenBucket {
    rate: f64,
    inner: Mutex<BucketState>,
}

#[derive(Debug)]
struct BucketState {
    tokens: f64,
    last: Instant,
}

impl TokenBucket {
    pub fn new(rate_per_sec: u32) -> Self {
        Self {
            rate: rate_per_sec as f64,
            inner: Mutex::new(BucketState {
                tokens: rate_per_sec as f64,
                last: Instant::now(),
            }),
        }
    }

    fn refill(&self, st: &mut BucketState, now: Instant) {
        let elapsed = now.duration_since(st.last).as_secs_f64();
        if elapsed > 0.0 {
            st.tokens = (st.tokens + elapsed * self.rate).min(self.rate);
            st.last = now;
        }
    }

    /// Takes one token if available.
    pub fn try_acquire(&self) -> bool {
        self.try_acquire_at(Instant::now())
    }

    pub fn try_acquire_at(&self, now: Instant) -> bool {
        let mut st = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        self.refill(&mut st, now);
        if st.tokens >= 1.0 {
            st.tokens -= 1.0;
            true
        } else {
            false
        }
    }

    /// Whether a token is available right now, without taking it.
    pub fn has_capacity(&self) -> bool {
        self.has_capacity_at(Instant::now())
    }

    pub fn has_capacity_at(&self, now: Instant) -> bool {
        let mut st = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        self.refill(&mut st, now);
        st.tokens >= 1.0
    }
}

#[derive(Debug, Clone)]
pub struct RuntimeBackend {
    pub config: Backend,
    pub healthy: Arc<AtomicBool>,
    /// Present when `max_rps > 0`.
    pub budget: Option<Arc<TokenBucket>>,
}

impl RuntimeBackend {
    pub fn new(config: Backend, healthy: bool) -> Self {
        let budget = (config.max_rps > 0).then(|| Arc::new(TokenBucket::new(config.max_rps)));
        Self {
            config,
            healthy: Arc::new(AtomicBool::new(healthy)),
            budget,
        }
    }

    #[inline]
    pub fn is_healthy(&self) -> bool {
        self.healthy.load(Ordering::Relaxed)
    }

    /// True if the backend has upstream budget left (or no limit).
    #[inline]
    pub fn has_capacity(&self) -> bool {
        self.budget
            .as_ref()
            .map(|b| b.has_capacity())
            .unwrap_or(true)
    }

    /// Consumes one unit of upstream budget. Always true for unlimited backends.
    #[inline]
    pub fn take_capacity(&self) -> bool {
        self.budget
            .as_ref()
            .map(|b| b.try_acquire())
            .unwrap_or(true)
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

    /// Healthy backends with upstream budget, in config order. One unit of
    /// budget is consumed per backend returned (this is what fan-out sends to).
    pub fn healthy_backends(&self) -> Vec<Selection> {
        self.backends
            .iter()
            .filter(|b| b.is_healthy() && b.take_capacity())
            .map(|b| Selection {
                label: b.config.label.clone(),
                url: b.config.url.clone(),
            })
            .collect()
    }

    pub fn any_healthy(&self) -> bool {
        self.backends.iter().any(|b| b.is_healthy())
    }

    /// Picks a backend for `rpc_method`, honouring method routes first and
    /// falling back to weighted random among healthy backends that still have
    /// upstream budget. Backends whose label is in `exclude` are skipped,
    /// which is how retries avoid hitting the backend that just failed. One
    /// unit of the chosen backend's budget is consumed.
    ///
    /// Returns `None` when nothing is healthy, or everything healthy is out of
    /// budget (`any_healthy()` tells the two apart).
    pub fn select_backend(
        &self,
        rpc_method: Option<&str>,
        exclude: &[String],
    ) -> Option<Selection> {
        let allowed = |b: &RuntimeBackend| {
            b.is_healthy() && !exclude.contains(&b.config.label) && b.has_capacity()
        };

        if let Some(method) = rpc_method {
            if let Some(target) = self.method_routes.get(method) {
                match self.backends.iter().find(|b| b.config.label == *target) {
                    Some(b) if allowed(b) && b.take_capacity() => {
                        debug!("Method {} routed to label={}", method, target);
                        return Some(Selection {
                            label: b.config.label.clone(),
                            url: b.config.url.clone(),
                        });
                    }
                    Some(_) => debug!(
                        "Method {} target label={} unavailable, falling back to weighted selection",
                        method, target
                    ),
                    None => {}
                }
            }
        }

        // Weighted pick, then claim budget; on a lost race (bucket drained
        // between peek and take) drop that candidate and pick again.
        let mut candidates: Vec<&RuntimeBackend> =
            self.backends.iter().filter(|b| allowed(b)).collect();
        while let Some(b) = weighted_pick(&candidates) {
            if b.take_capacity() {
                return Some(Selection {
                    label: b.config.label.clone(),
                    url: b.config.url.clone(),
                });
            }
            candidates.retain(|c| c.config.label != b.config.label);
        }
        None
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
    /// Open WebSocket sessions per owner, for the per-key cap.
    pub ws_sessions: Arc<Mutex<HashMap<String, u32>>>,
}

impl AppState {
    pub fn new(
        client: HttpClient,
        keystore: Arc<dyn KeyStore>,
        state: Arc<ArcSwap<RouterState>>,
    ) -> Self {
        Self {
            client,
            keystore,
            state,
            ws_sessions: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    /// Reserves a WebSocket slot for `owner` if under `limit` (0 = unlimited).
    /// Returns a guard that releases the slot when dropped.
    pub fn try_open_ws_session(&self, owner: &str, limit: u32) -> Option<WsSessionGuard> {
        let mut sessions = self.ws_sessions.lock().unwrap_or_else(|e| e.into_inner());
        let count = sessions.entry(owner.to_string()).or_insert(0);
        if limit != 0 && *count >= limit {
            return None;
        }
        *count += 1;
        Some(WsSessionGuard {
            owner: owner.to_string(),
            sessions: self.ws_sessions.clone(),
        })
    }

    pub fn ws_session_count(&self, owner: &str) -> u32 {
        self.ws_sessions
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(owner)
            .copied()
            .unwrap_or(0)
    }
}

/// Releases a per-owner WebSocket slot on drop.
pub struct WsSessionGuard {
    owner: String,
    sessions: Arc<Mutex<HashMap<String, u32>>>,
}

impl Drop for WsSessionGuard {
    fn drop(&mut self) {
        let mut sessions = self.sessions.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(count) = sessions.get_mut(&self.owner) {
            *count = count.saturating_sub(1);
            if *count == 0 {
                sessions.remove(&self.owner);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn token_bucket_bursts_then_refills() {
        let b = TokenBucket::new(2);
        let t0 = Instant::now();
        assert!(b.try_acquire_at(t0));
        assert!(b.try_acquire_at(t0));
        assert!(!b.try_acquire_at(t0), "burst of one second exhausted");
        assert!(!b.has_capacity_at(t0));
        // Half a second later one token is back.
        let t1 = t0 + Duration::from_millis(500);
        assert!(b.has_capacity_at(t1));
        assert!(b.try_acquire_at(t1));
        assert!(!b.try_acquire_at(t1));
        // Never accumulates beyond one second of burst.
        let t2 = t0 + Duration::from_secs(60);
        assert!(b.try_acquire_at(t2));
        assert!(b.try_acquire_at(t2));
        assert!(!b.try_acquire_at(t2));
    }

    #[test]
    fn unlimited_backend_always_has_capacity() {
        let b = RuntimeBackend::new(
            Backend {
                label: "x".into(),
                url: "http://x".into(),
                weight: 1,
                ws_url: None,
                max_rps: 0,
            },
            true,
        );
        for _ in 0..10_000 {
            assert!(b.take_capacity());
        }
    }
}
