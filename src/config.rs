use std::{collections::HashMap, fs, path::Path};

use serde::Deserialize;

#[derive(Debug, Deserialize, Clone)]
pub struct Config {
    pub port: u16,
    pub redis_url: String, // Added Redis URL
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
    pub timeout_secs: u64,
}

impl Default for ProxyConfig {
    fn default() -> Self {
        Self { timeout_secs: 30 }
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
}

impl Default for HealthCheckConfig {
    fn default() -> Self {
        Self {
            interval_secs: 30,
            timeout_secs: 5,
            method: "getSlot".to_string(),
            consecutive_failures_threshold: 3,
            consecutive_successes_threshold: 2,
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

    if config.redis_url.is_empty() {
        return Err("Redis URL must be configured".into());
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
        if backend.weight == 0 {
            return Err(format!("Backend '{}' has invalid weight 0", backend.label).into());
        }
        if backend.label.is_empty() {
            return Err(format!("Backend with URL '{}' has empty label", backend.url).into());
        }
    }

    if config.proxy.timeout_secs == 0 {
        return Err("Proxy timeout_secs must be > 0".into());
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

    Ok(config)
}
