use std::{
    collections::HashMap,
    sync::LazyLock,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use arc_swap::ArcSwap;
use async_trait::async_trait;
use moka::future::Cache;
use redis::{aio::ConnectionManager, Client, Script};

use crate::config::ApiKeyConfig;

/// How long key metadata is cached locally. Revocations and rate-limit
/// changes take up to this long to propagate to a running router.
pub const KEY_CACHE_TTL: Duration = Duration::from_secs(60);

/// Upper bound on cached key lookups (hits and misses). Bounds memory when a
/// client sprays random keys.
pub const KEY_CACHE_CAPACITY: u64 = 10_000;
/// Keys longer than this are rejected before any Redis round trip.
pub const MAX_KEY_LEN: usize = 128;

/// Redis hash prefix for API key metadata.
pub const KEY_PREFIX: &str = "api_key:";
/// Redis key prefix for the per-second rate limit counters.
pub const RATE_LIMIT_PREFIX: &str = "rate_limit:";
/// Redis set holding every key ever created, used by `rpc-admin list`.
pub const KEY_INDEX: &str = "api_keys_index";

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct KeyInfo {
    pub owner: String,
    /// Requests per second. `0` means unlimited.
    pub rate_limit: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum KeyStoreError {
    /// The key is valid but its per-second budget is spent.
    RateLimited,
    /// The keystore itself failed (Redis down, protocol error, ...).
    Backend(String),
}

impl std::fmt::Display for KeyStoreError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            KeyStoreError::RateLimited => write!(f, "rate limit exceeded"),
            KeyStoreError::Backend(msg) => write!(f, "keystore error: {}", msg),
        }
    }
}

impl std::error::Error for KeyStoreError {}

#[async_trait]
pub trait KeyStore: Send + Sync {
    /// `Ok(Some(info))` for a valid key, `Ok(None)` for unknown/inactive/
    /// expired keys, `Err(RateLimited)` when the per-second budget is spent,
    /// and `Err(Backend(_))` for infrastructure failures.
    async fn validate_key(&self, key: &str) -> Result<Option<KeyInfo>, KeyStoreError>;
}

/// INCR + EXPIRE in one atomic step; the SHA is computed once per process.
static RATE_LIMIT_SCRIPT: LazyLock<Script> = LazyLock::new(|| {
    Script::new(
        r#"
        local count = redis.call("INCR", KEYS[1])
        if count == 1 then
            redis.call("EXPIRE", KEYS[1], 1)
        end
        return count
        "#,
    )
});

pub struct RedisKeyStore {
    conn: ConnectionManager,
    cache: Cache<String, Option<KeyInfo>>,
}

impl RedisKeyStore {
    pub async fn new(redis_url: &str) -> Result<Self, KeyStoreError> {
        let client = Client::open(redis_url).map_err(|e| KeyStoreError::Backend(e.to_string()))?;
        let conn = client
            .get_connection_manager()
            .await
            .map_err(|e| KeyStoreError::Backend(e.to_string()))?;

        let cache = Cache::builder()
            .time_to_live(KEY_CACHE_TTL)
            .max_capacity(KEY_CACHE_CAPACITY)
            .build();

        Ok(Self { conn, cache })
    }

    async fn get_key_info(&self, key: &str) -> Result<Option<KeyInfo>, KeyStoreError> {
        if let Some(info) = self.cache.get(key).await {
            return Ok(info);
        }

        let mut conn = self.conn.clone();
        let redis_key = format!("{}{}", KEY_PREFIX, key);

        // One round trip for everything we need.
        let fields: HashMap<String, String> = redis::cmd("HGETALL")
            .arg(&redis_key)
            .query_async(&mut conn)
            .await
            .map_err(|e| KeyStoreError::Backend(e.to_string()))?;

        let info = parse_key_fields(&fields, now_unix());
        self.cache.insert(key.to_string(), info.clone()).await;
        Ok(info)
    }

    async fn check_rate_limit(&self, key: &str, limit: u64) -> Result<bool, KeyStoreError> {
        if limit == 0 {
            return Ok(true);
        }

        let mut conn = self.conn.clone();
        let redis_key = format!("{}{}", RATE_LIMIT_PREFIX, key);

        let count: u64 = RATE_LIMIT_SCRIPT
            .key(&redis_key)
            .invoke_async(&mut conn)
            .await
            .map_err(|e| KeyStoreError::Backend(e.to_string()))?;

        Ok(count <= limit)
    }
}

fn now_unix() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Interprets the `api_key:*` hash. Missing hash (empty map), `active=false`,
/// or a past `expires_at` all yield `None`.
pub fn parse_key_fields(fields: &HashMap<String, String>, now: u64) -> Option<KeyInfo> {
    let owner = fields.get("owner")?.clone();

    if fields.get("active").map(|a| a == "false").unwrap_or(false) {
        return None;
    }

    if let Some(exp) = fields.get("expires_at").and_then(|v| v.parse::<u64>().ok()) {
        if exp != 0 && exp <= now {
            return None;
        }
    }

    let rate_limit = fields
        .get("rate_limit")
        .and_then(|v| v.parse::<u64>().ok())
        .unwrap_or(0);

    Some(KeyInfo { owner, rate_limit })
}

#[async_trait]
impl KeyStore for RedisKeyStore {
    async fn validate_key(&self, key: &str) -> Result<Option<KeyInfo>, KeyStoreError> {
        if key.is_empty() || key.len() > MAX_KEY_LEN {
            return Ok(None);
        }
        let info = match self.get_key_info(key).await? {
            Some(info) => info,
            None => return Ok(None),
        };

        if !self.check_rate_limit(key, info.rate_limit).await? {
            return Err(KeyStoreError::RateLimited);
        }
        Ok(Some(info))
    }
}

// ---------------------------------------------------------------------------
// File keystore: keys from the config file, limits enforced in-process
// ---------------------------------------------------------------------------

#[derive(Clone, Debug)]
struct FileKey {
    info: KeyInfo,
    expires_at: u64,
    active: bool,
}

#[derive(Debug, Default)]
struct Window {
    second: u64,
    count: u64,
}

/// Keystore backed by `[[api_keys]]` in the config file. Rate limits are
/// fixed one-second windows held in memory, so they are per router instance.
/// `reload()` swaps the key set atomically (used by the SIGHUP handler).
pub struct FileKeyStore {
    keys: ArcSwap<HashMap<String, FileKey>>,
    windows: std::sync::Mutex<HashMap<String, Window>>,
}

impl FileKeyStore {
    pub fn new(entries: &[ApiKeyConfig]) -> Self {
        Self {
            keys: ArcSwap::from_pointee(Self::index(entries)),
            windows: std::sync::Mutex::new(HashMap::new()),
        }
    }

    fn index(entries: &[ApiKeyConfig]) -> HashMap<String, FileKey> {
        entries
            .iter()
            .map(|e| {
                (
                    e.key.clone(),
                    FileKey {
                        info: KeyInfo {
                            owner: e.owner.clone(),
                            rate_limit: e.rate_limit,
                        },
                        expires_at: e.expires_at,
                        active: e.active,
                    },
                )
            })
            .collect()
    }

    /// Replaces the key set. Counters for keys that disappeared are dropped.
    pub fn reload(&self, entries: &[ApiKeyConfig]) {
        let index = Self::index(entries);
        let mut windows = self.windows.lock().unwrap_or_else(|e| e.into_inner());
        windows.retain(|k, _| index.contains_key(k));
        self.keys.store(std::sync::Arc::new(index));
    }

    pub fn len(&self) -> usize {
        self.keys.load().len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    fn check_rate_limit(&self, key: &str, limit: u64, now: u64) -> bool {
        if limit == 0 {
            return true;
        }
        let mut windows = self.windows.lock().unwrap_or_else(|e| e.into_inner());
        let w = windows.entry(key.to_string()).or_default();
        if w.second != now {
            w.second = now;
            w.count = 0;
        }
        w.count += 1;
        w.count <= limit
    }

    fn validate_at(&self, key: &str, now: u64) -> Result<Option<KeyInfo>, KeyStoreError> {
        let keys = self.keys.load();
        let entry = match keys.get(key) {
            Some(e) => e,
            None => return Ok(None),
        };
        if !entry.active || (entry.expires_at != 0 && entry.expires_at <= now) {
            return Ok(None);
        }
        if !self.check_rate_limit(key, entry.info.rate_limit, now) {
            return Err(KeyStoreError::RateLimited);
        }
        Ok(Some(entry.info.clone()))
    }
}

#[async_trait]
impl KeyStore for FileKeyStore {
    async fn validate_key(&self, key: &str) -> Result<Option<KeyInfo>, KeyStoreError> {
        if key.is_empty() || key.len() > MAX_KEY_LEN {
            return Ok(None);
        }
        self.validate_at(key, now_unix())
    }
}

#[cfg(test)]
mod file_tests {
    use super::*;

    fn entry(key: &str, owner: &str, rate_limit: u64) -> ApiKeyConfig {
        ApiKeyConfig {
            key: key.into(),
            owner: owner.into(),
            rate_limit,
            expires_at: 0,
            active: true,
        }
    }

    #[test]
    fn unknown_inactive_and_expired_keys_are_none() {
        let mut expired = entry("old", "x", 0);
        expired.expires_at = 100;
        let mut off = entry("off", "y", 0);
        off.active = false;
        let store = FileKeyStore::new(&[entry("ok", "alice", 0), expired, off]);
        assert_eq!(store.validate_at("nope", 50).unwrap(), None);
        assert_eq!(store.validate_at("off", 50).unwrap(), None);
        assert_eq!(store.validate_at("old", 100).unwrap(), None);
        assert!(store.validate_at("old", 99).unwrap().is_some());
        assert_eq!(store.validate_at("ok", 50).unwrap().unwrap().owner, "alice");
    }

    #[test]
    fn rate_limit_is_per_second_window() {
        let store = FileKeyStore::new(&[entry("k", "alice", 2)]);
        assert!(store.validate_at("k", 10).unwrap().is_some());
        assert!(store.validate_at("k", 10).unwrap().is_some());
        assert_eq!(
            store.validate_at("k", 10).unwrap_err(),
            KeyStoreError::RateLimited
        );
        // Next second resets.
        assert!(store.validate_at("k", 11).unwrap().is_some());
    }

    #[test]
    fn unlimited_key_never_limits() {
        let store = FileKeyStore::new(&[entry("k", "alice", 0)]);
        for _ in 0..1000 {
            assert!(store.validate_at("k", 1).unwrap().is_some());
        }
    }

    #[test]
    fn reload_swaps_keys_and_drops_stale_counters() {
        let store = FileKeyStore::new(&[entry("a", "alice", 1)]);
        assert!(store.validate_at("a", 1).unwrap().is_some());
        assert!(store.validate_at("a", 1).is_err());
        store.reload(&[entry("b", "bob", 5)]);
        assert_eq!(store.validate_at("a", 1).unwrap(), None);
        assert_eq!(store.validate_at("b", 1).unwrap().unwrap().owner, "bob");
        assert_eq!(store.len(), 1);
        assert!(store.windows.lock().unwrap().get("a").is_none());
    }

    #[tokio::test]
    async fn trait_impl_rejects_overlong_keys() {
        let store = FileKeyStore::new(&[entry("k", "alice", 0)]);
        assert_eq!(store.validate_key(&"k".repeat(200)).await.unwrap(), None);
        assert!(store.validate_key("k").await.unwrap().is_some());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fields(pairs: &[(&str, &str)]) -> HashMap<String, String> {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    #[test]
    fn missing_hash_is_none() {
        assert_eq!(parse_key_fields(&HashMap::new(), 100), None);
    }

    #[test]
    fn active_key_parses() {
        let f = fields(&[("owner", "alice"), ("rate_limit", "25"), ("active", "true")]);
        assert_eq!(
            parse_key_fields(&f, 100),
            Some(KeyInfo {
                owner: "alice".into(),
                rate_limit: 25
            })
        );
    }

    #[test]
    fn missing_active_defaults_to_active() {
        let f = fields(&[("owner", "alice"), ("rate_limit", "25")]);
        assert!(parse_key_fields(&f, 100).is_some());
    }

    #[test]
    fn inactive_key_is_none() {
        let f = fields(&[
            ("owner", "alice"),
            ("rate_limit", "25"),
            ("active", "false"),
        ]);
        assert_eq!(parse_key_fields(&f, 100), None);
    }

    #[test]
    fn expired_key_is_none() {
        let f = fields(&[
            ("owner", "alice"),
            ("rate_limit", "25"),
            ("expires_at", "99"),
        ]);
        assert_eq!(parse_key_fields(&f, 100), None);
        assert_eq!(parse_key_fields(&f, 99), None);
        assert!(parse_key_fields(&f, 98).is_some());
    }

    #[test]
    fn zero_expiry_means_never() {
        let f = fields(&[("owner", "alice"), ("expires_at", "0")]);
        assert!(parse_key_fields(&f, u64::MAX).is_some());
    }

    #[test]
    fn garbage_rate_limit_means_unlimited() {
        let f = fields(&[("owner", "alice"), ("rate_limit", "lots")]);
        assert_eq!(parse_key_fields(&f, 1).unwrap().rate_limit, 0);
    }
}
