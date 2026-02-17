use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
};

use async_trait::async_trait;

use crate::keystore::{KeyInfo, KeyStore};

#[derive(Clone)]
pub struct MockKeyStore {
    pub keys: Arc<Mutex<HashMap<String, KeyInfo>>>,
    pub call_counts: Arc<Mutex<HashMap<String, u64>>>,
    pub inactive_keys: Arc<Mutex<Vec<String>>>,
    pub rate_limited_keys: Arc<Mutex<Vec<String>>>,
    pub error_keys: Arc<Mutex<HashMap<String, String>>>,
}

impl Default for MockKeyStore {
    fn default() -> Self {
        Self::new()
    }
}

impl MockKeyStore {
    pub fn new() -> Self {
        Self {
            keys: Arc::new(Mutex::new(HashMap::new())),
            call_counts: Arc::new(Mutex::new(HashMap::new())),
            inactive_keys: Arc::new(Mutex::new(Vec::new())),
            rate_limited_keys: Arc::new(Mutex::new(Vec::new())),
            error_keys: Arc::new(Mutex::new(HashMap::new())),
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

    pub fn set_error(&self, key: &str, msg: &str) {
        self.error_keys
            .lock()
            .unwrap()
            .insert(key.to_string(), msg.to_string());
    }
}

#[async_trait]
impl KeyStore for MockKeyStore {
    async fn validate_key(&self, key: &str) -> Result<Option<KeyInfo>, String> {
        let mut counts = self.call_counts.lock().unwrap();
        *counts.entry(key.to_string()).or_insert(0) += 1;
        drop(counts);

        // Check for custom errors first
        if let Some(msg) = self.error_keys.lock().unwrap().get(key) {
            return Err(msg.clone());
        }

        if self
            .inactive_keys
            .lock()
            .unwrap()
            .contains(&key.to_string())
        {
            return Ok(None);
        }

        if let Some(info) = self.keys.lock().unwrap().get(key) {
            if self
                .rate_limited_keys
                .lock()
                .unwrap()
                .contains(&key.to_string())
            {
                return Err("Rate limit exceeded".to_string());
            }

            return Ok(Some(info.clone()));
        }

        Ok(None)
    }
}
