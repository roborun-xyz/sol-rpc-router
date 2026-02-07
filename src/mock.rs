use async_trait::async_trait;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use crate::keystore::{KeyInfo, KeyStore};

#[derive(Clone)]
pub struct MockKeyStore {
    pub keys: Arc<Mutex<HashMap<String, KeyInfo>>>,
    pub call_counts: Arc<Mutex<HashMap<String, u64>>>,
    pub inactive_keys: Arc<Mutex<Vec<String>>>,
    pub rate_limited_keys: Arc<Mutex<Vec<String>>>,
}

impl MockKeyStore {
    pub fn new() -> Self {
        Self {
            keys: Arc::new(Mutex::new(HashMap::new())),
            call_counts: Arc::new(Mutex::new(HashMap::new())),
            inactive_keys: Arc::new(Mutex::new(Vec::new())),
            rate_limited_keys: Arc::new(Mutex::new(Vec::new())),
        }
    }

    pub fn add_key(&self, key: &str, owner: &str, rate_limit: u64) {
        self.keys.lock().unwrap().insert(
            key.to_string(),
            KeyInfo {
                owner: owner.to_string(),
                rate_limit,
            },
        );
    }

    pub fn set_inactive(&self, key: &str) {
        self.inactive_keys.lock().unwrap().push(key.to_string());
    }

    pub fn get_call_count(&self, key: &str) -> u64 {
        *self.call_counts.lock().unwrap().get(key).unwrap_or(&0)
    }
}

#[async_trait]
impl KeyStore for MockKeyStore {
    async fn validate_key(&self, key: &str) -> Result<Option<KeyInfo>, String> {
        let mut counts = self.call_counts.lock().unwrap();
        *counts.entry(key.to_string()).or_insert(0) += 1;

        if self.inactive_keys.lock().unwrap().contains(&key.to_string()) {
            return Ok(None);
        }

        if let Some(info) = self.keys.lock().unwrap().get(key) {
            // Check rate limit (simple check against strict list for testing)
            if self.rate_limited_keys.lock().unwrap().contains(&key.to_string()) {
                return Err("Rate limit exceeded".to_string());
            }
            
            // Also check implied rate limit if we tracked time, but for mock, we trust the explicit list
            // or we could implement a counter reset. For simplicity, let's assume we manually trigger limit.
            
            // Actually, let's implement the logic requested:
            // "Test validate_key scenarios: ... rate limit exceeded"
            // If the mock needs to simulate dynamic rate limiting, we need timestamps.
            // But usually mocks are configured.
            
            return Ok(Some(info.clone()));
        }

        Ok(None)
    }
}
