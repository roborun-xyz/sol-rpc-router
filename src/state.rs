use std::{
    collections::{HashMap, HashSet},
    sync::{
        atomic::{AtomicBool, AtomicU64, Ordering},
        Arc, Mutex,
    },
    time::Duration,
};

use std::time::Instant;

use arc_swap::ArcSwap;
use axum::body::Body;
use hyper_tls::HttpsConnector;
use hyper_util::client::legacy::{connect::HttpConnector, Client};
use rand::Rng;
use tracing::debug;

use crate::{
    config::{Backend, Config, HealthCheckConfig, ProxyConfig, SelectionStrategy},
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

/// Smoothing factor for the request-latency EWMA (0..1, higher = more reactive).
const LATENCY_EWMA_ALPHA: f64 = 0.2;
/// A slow backend never drops below this share of its configured weight, so
/// it keeps receiving a trickle of traffic and can recover.
const LATENCY_WEIGHT_FLOOR: f64 = 0.05;

#[derive(Debug, Clone)]
pub struct RuntimeBackend {
    pub config: Backend,
    pub healthy: Arc<AtomicBool>,
    /// Present when `max_rps > 0`.
    pub budget: Option<Arc<TokenBucket>>,
    /// EWMA of observed upstream request latency in microseconds; 0 = no
    /// sample yet.
    pub latency_us: Arc<AtomicU64>,
}

impl RuntimeBackend {
    pub fn new(config: Backend, healthy: bool) -> Self {
        let budget = (config.max_rps > 0).then(|| Arc::new(TokenBucket::new(config.max_rps)));
        Self {
            config,
            healthy: Arc::new(AtomicBool::new(healthy)),
            budget,
            latency_us: Arc::new(AtomicU64::new(0)),
        }
    }

    /// Folds one observed request duration into the latency EWMA.
    pub fn record_latency(&self, d: Duration) {
        let sample = d.as_micros().min(u64::MAX as u128) as u64;
        let prev = self.latency_us.load(Ordering::Relaxed);
        let next = if prev == 0 {
            sample
        } else {
            (prev as f64 * (1.0 - LATENCY_EWMA_ALPHA) + sample as f64 * LATENCY_EWMA_ALPHA) as u64
        };
        self.latency_us.store(next.max(1), Ordering::Relaxed);
    }

    /// Observed request latency, if any sample has been recorded.
    pub fn latency(&self) -> Option<Duration> {
        match self.latency_us.load(Ordering::Relaxed) {
            0 => None,
            us => Some(Duration::from_micros(us)),
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

    /// Records an observed upstream latency for the backend with `label`.
    pub fn record_latency(&self, label: &str, d: Duration) {
        if let Some(b) = self.backends.iter().find(|b| b.config.label == label) {
            b.record_latency(d);
        }
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
        while let Some(b) = pick_with_strategy(&candidates, self.proxy.selection) {
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

fn pick_with_strategy<'a>(
    candidates: &[&'a RuntimeBackend],
    strategy: SelectionStrategy,
) -> Option<&'a RuntimeBackend> {
    match strategy {
        SelectionStrategy::Weighted => weighted_pick(candidates),
        SelectionStrategy::LatencyWeighted => {
            let weights = latency_scaled_weights(candidates);
            weighted_pick_f64(candidates, &weights)
        }
    }
}

/// Scales each candidate's configured weight by `fastest / own` latency, so
/// the fastest backend keeps its full weight and a backend twice as slow gets
/// half, down to [`LATENCY_WEIGHT_FLOOR`]. Backends with no sample yet count
/// as fastest so they get measured.
pub fn latency_scaled_weights(candidates: &[&RuntimeBackend]) -> Vec<f64> {
    let fastest = candidates
        .iter()
        .filter_map(|b| b.latency())
        .map(|d| d.as_secs_f64())
        .fold(f64::INFINITY, f64::min);
    candidates
        .iter()
        .map(|b| {
            let base = b.config.weight as f64;
            match b.latency() {
                Some(d) if fastest.is_finite() && d.as_secs_f64() > 0.0 => {
                    base * (fastest / d.as_secs_f64()).clamp(LATENCY_WEIGHT_FLOOR, 1.0)
                }
                _ => base,
            }
        })
        .collect()
}

fn weighted_pick_f64<'a>(
    candidates: &[&'a RuntimeBackend],
    weights: &[f64],
) -> Option<&'a RuntimeBackend> {
    let total: f64 = weights.iter().sum();
    if candidates.is_empty() {
        return None;
    }
    if total <= 0.0 {
        return candidates.first().copied();
    }
    let mut roll = rand::thread_rng().gen_range(0.0..total);
    for (b, w) in candidates.iter().zip(weights) {
        if roll < *w {
            return Some(b);
        }
        roll -= w;
    }
    candidates.last().copied()
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

    fn backend(label: &str, weight: u32) -> RuntimeBackend {
        RuntimeBackend::new(
            Backend {
                label: label.into(),
                url: format!("http://{}", label),
                weight,
                ws_url: None,
                max_rps: 0,
            },
            true,
        )
    }

    #[test]
    fn latency_ewma_smooths_samples() {
        let b = backend("a", 1);
        assert!(b.latency().is_none());
        b.record_latency(Duration::from_millis(100));
        assert_eq!(b.latency().unwrap(), Duration::from_millis(100));
        b.record_latency(Duration::from_millis(200));
        // 100 * 0.8 + 200 * 0.2 = 120
        assert_eq!(b.latency().unwrap(), Duration::from_millis(120));
    }

    #[test]
    fn latency_scaled_weights_favour_fast_backends_with_a_floor() {
        let fast = backend("fast", 10);
        let slow = backend("slow", 10);
        let glacial = backend("glacial", 10);
        let fresh = backend("fresh", 10);
        fast.record_latency(Duration::from_millis(10));
        slow.record_latency(Duration::from_millis(40));
        glacial.record_latency(Duration::from_secs(10));
        let w = latency_scaled_weights(&[&fast, &slow, &glacial, &fresh]);
        assert_eq!(w[0], 10.0, "fastest keeps full weight");
        assert_eq!(w[1], 2.5, "4x slower gets a quarter");
        assert_eq!(w[2], 0.5, "floor of 5%");
        assert_eq!(w[3], 10.0, "unmeasured counts as fastest");
    }

    #[test]
    fn latency_weighted_pick_skews_traffic() {
        let fast = backend("fast", 1);
        let slow = backend("slow", 1);
        fast.record_latency(Duration::from_millis(10));
        slow.record_latency(Duration::from_millis(1000));
        let candidates = [&fast, &slow];
        let mut fast_hits = 0;
        for _ in 0..2000 {
            if pick_with_strategy(&candidates, SelectionStrategy::LatencyWeighted)
                .unwrap()
                .config
                .label
                == "fast"
            {
                fast_hits += 1;
            }
        }
        // Expected share 1 / 1.05 ≈ 95%; allow slack for randomness.
        assert!(fast_hits > 1800, "fast got {} of 2000", fast_hits);
        assert!(fast_hits < 2000, "slow must still get a trickle");

        let mut fast_hits = 0;
        for _ in 0..2000 {
            if pick_with_strategy(&candidates, SelectionStrategy::Weighted)
                .unwrap()
                .config
                .label
                == "fast"
            {
                fast_hits += 1;
            }
        }
        assert!(
            (800..1200).contains(&fast_hits),
            "plain weighted stays ~50/50, got {}",
            fast_hits
        );
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
