use std::{collections::HashMap, fs, path::Path};

use serde::Deserialize;

/// Environment variable that overrides `redis_url` from the config file.
pub const REDIS_URL_ENV: &str = "REDIS_URL";

#[derive(Debug, Deserialize, Clone)]
pub struct Config {
    pub port: u16,
    pub metrics_port: u16,
    /// Redis connection URL for the Redis keystore. Can be overridden with the
    /// `REDIS_URL` env var. Leave empty to use `[[api_keys]]` from this file.
    #[serde(default)]
    pub redis_url: String,
    /// Inline API keys. Used when `redis_url` is empty; keys are reloaded on
    /// SIGHUP. Rate limits are enforced in-process, so this mode suits a
    /// single router instance.
    #[serde(default)]
    pub api_keys: Vec<ApiKeyConfig>,
    pub backends: Vec<Backend>,
    #[serde(default)]
    pub method_routes: HashMap<String, String>,
    #[serde(default)]
    pub health_check: HealthCheckConfig,
    #[serde(default)]
    pub proxy: ProxyConfig,
}

#[derive(Debug, Deserialize, Clone)]
#[serde(default)]
pub struct ProxyConfig {
    /// Per-attempt upstream timeout.
    pub timeout_secs: u64,
    /// How many additional backends to try when an attempt fails with a
    /// connection error, timeout, HTTP 408/429 or 5xx. `0` disables failover.
    pub max_retries: u32,
    /// Methods that are broadcast to every healthy backend at once. The first
    /// successful JSON-RPC response wins. Typically `["sendTransaction"]` to
    /// improve landing rates.
    pub fanout_methods: Vec<String>,
    /// Methods rejected before they reach any backend (HTTP 403 + JSON-RPC
    /// error). Useful for keeping expensive calls like `getProgramAccounts`
    /// away from a shared endpoint.
    pub blocked_methods: Vec<String>,
    /// Seconds to wait for in-flight requests after SIGTERM/SIGINT before
    /// exiting anyway.
    pub shutdown_grace_secs: u64,
    /// Maximum concurrent WebSocket sessions per API key. `0` disables the cap.
    pub max_ws_connections_per_key: u32,
}

impl Default for ProxyConfig {
    fn default() -> Self {
        Self {
            timeout_secs: 30,
            max_retries: 2,
            fanout_methods: Vec::new(),
            blocked_methods: Vec::new(),
            shutdown_grace_secs: 10,
            max_ws_connections_per_key: 100,
        }
    }
}

#[derive(Debug, Deserialize, Clone)]
#[serde(default)]
pub struct HealthCheckConfig {
    pub interval_secs: u64,
    pub timeout_secs: u64,
    pub method: String,
    pub consecutive_failures_threshold: u32,
    pub consecutive_successes_threshold: u32,
    pub max_slot_lag: u64,
}

impl Default for HealthCheckConfig {
    fn default() -> Self {
        Self {
            interval_secs: 30,
            timeout_secs: 5,
            method: "getSlot".to_string(),
            consecutive_failures_threshold: 3,
            consecutive_successes_threshold: 2,
            max_slot_lag: 50,
        }
    }
}

#[derive(Debug, Deserialize, Clone)]
pub struct ApiKeyConfig {
    pub key: String,
    pub owner: String,
    /// Requests per second; `0` means unlimited.
    #[serde(default)]
    pub rate_limit: u64,
    /// Unix seconds; `0` or absent means never.
    #[serde(default)]
    pub expires_at: u64,
    #[serde(default = "default_true")]
    pub active: bool,
}

fn default_true() -> bool {
    true
}

/// Which keystore a config selects.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeyStoreKind {
    Redis,
    File,
}

impl Config {
    pub fn keystore_kind(&self) -> KeyStoreKind {
        if self.redis_url.is_empty() {
            KeyStoreKind::File
        } else {
            KeyStoreKind::Redis
        }
    }
}

#[derive(Debug, Deserialize, Clone)]
pub struct Backend {
    pub label: String,
    pub url: String,
    pub weight: u32,
    pub ws_url: Option<String>,
}

pub fn load_config(config_path: &str) -> Result<Config, Box<dyn std::error::Error>> {
    if !Path::new(config_path).exists() {
        return Err(format!("Configuration file not found: {}", config_path).into());
    }

    let contents = fs::read_to_string(config_path)?;
    let config: Config = toml::from_str(&contents)?;
    let redis_override = std::env::var(REDIS_URL_ENV).ok().filter(|v| !v.is_empty());
    validate_config(config, redis_override)
}

/// Validates a parsed config. `redis_url_override` (normally the `REDIS_URL`
/// env var) replaces the file's `redis_url` when present.
pub fn validate_config(
    mut config: Config,
    redis_url_override: Option<String>,
) -> Result<Config, Box<dyn std::error::Error>> {
    if let Some(url) = redis_url_override {
        config.redis_url = url;
    }

    if config.redis_url.is_empty() && config.api_keys.is_empty() {
        return Err(format!(
            "No keystore configured: set `redis_url` (or the {} env var) for Redis-backed keys, or add [[api_keys]] entries to the config file",
            REDIS_URL_ENV
        )
        .into());
    }
    if !config.redis_url.is_empty() && !config.api_keys.is_empty() {
        return Err(
            "Both `redis_url` and [[api_keys]] are set; pick one keystore (Redis manages keys with rpc-admin, the file keystore reads them from this config)"
                .into(),
        );
    }

    let mut seen_keys = std::collections::HashSet::new();
    for k in &config.api_keys {
        if k.key.is_empty() {
            return Err(format!("api_keys entry for owner '{}' has an empty key", k.owner).into());
        }
        if k.key.len() > crate::keystore::MAX_KEY_LEN {
            return Err(format!(
                "api_keys entry for owner '{}' is longer than {} bytes",
                k.owner,
                crate::keystore::MAX_KEY_LEN
            )
            .into());
        }
        if k.owner.is_empty() {
            return Err("api_keys entry has an empty owner".into());
        }
        if !seen_keys.insert(k.key.as_str()) {
            return Err(format!("Duplicate api_keys entry for owner '{}'", k.owner).into());
        }
    }
    if config.backends.is_empty() {
        return Err("At least one backend must be configured".into());
    }

    let backend_labels: HashMap<String, String> = config
        .backends
        .iter()
        .map(|b| (b.label.clone(), b.url.clone()))
        .collect();

    if backend_labels.len() != config.backends.len() {
        return Err("Duplicate backend labels found in configuration".into());
    }

    for backend in &config.backends {
        if backend.label.is_empty() {
            return Err(format!(
                "Backend with URL '{}' has empty label",
                crate::upstream::redact_url(&backend.url)
            )
            .into());
        }
        if backend.weight == 0 {
            return Err(format!("Backend '{}' has invalid weight 0", backend.label).into());
        }
        if !(backend.url.starts_with("http://") || backend.url.starts_with("https://")) {
            return Err(format!(
                "Backend '{}' url must start with http:// or https://",
                backend.label
            )
            .into());
        }
        if let Some(ws) = &backend.ws_url {
            if !(ws.starts_with("ws://") || ws.starts_with("wss://")) {
                return Err(format!(
                    "Backend '{}' ws_url must start with ws:// or wss://",
                    backend.label
                )
                .into());
            }
        }
    }

    if config.proxy.timeout_secs == 0 {
        return Err("Proxy timeout_secs must be > 0".into());
    }

    for method in &config.proxy.fanout_methods {
        if config.proxy.blocked_methods.contains(method) {
            return Err(
                format!("Method '{}' cannot be both fanned out and blocked", method).into(),
            );
        }
    }

    for (method, label) in &config.method_routes {
        if !backend_labels.contains_key(label) {
            return Err(format!(
                "Method route '{}' references unknown backend label '{}'",
                method, label
            )
            .into());
        }
    }

    if config.port == config.metrics_port {
        return Err("HTTP port and Metrics port must be different".into());
    }

    // Check for WebSocket port conflict (port + 1)
    let ws_port = config.port.checked_add(1).ok_or("Port overflow")?;
    if ws_port == config.metrics_port {
        return Err(format!(
            "Metrics port {} conflicts with WebSocket port (HTTP port + 1)",
            config.metrics_port
        )
        .into());
    }

    Ok(config)
}
