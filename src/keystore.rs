use std::{
    collections::HashMap,
    sync::LazyLock,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use arc_swap::ArcSwap;
use async_trait::async_trait;
use moka::{future::Cache, Expiry};
use redis::{aio::ConnectionManager, Client, Script};

use crate::config::ApiKeyConfig;

/// Default for how long a valid key is cached locally (`key_cache_ttl_secs`).
/// Revocations and rate-limit changes take up to this long to propagate to a
/// running router. Entries for keys with an `expires_at` never outlive it.
pub const KEY_CACHE_TTL: Duration = Duration::from_secs(60);
/// How long an unknown, inactive or expired key is cached, so a key looked up
/// just before `rpc-admin create` starts working within this window. Capped
/// at the positive TTL.
pub const NEGATIVE_KEY_CACHE_TTL: Duration = Duration::from_secs(5);

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

/// A valid key as cached by `KeyCache`, with the expiry it was read with.
#[derive(Clone, Debug)]
struct CachedKey {
    info: KeyInfo,
    /// Unix seconds; `0` means never.
    expires_at: u64,
}

/// Per-entry TTL: `positive_ttl` for valid keys, cut short at their
/// `expires_at`; `NEGATIVE_KEY_CACHE_TTL` (at most `positive_ttl`) for misses.
fn entry_ttl(entry: Option<&CachedKey>, positive_ttl: Duration, now: u64) -> Duration {
    match entry {
        None => NEGATIVE_KEY_CACHE_TTL.min(positive_ttl),
        Some(k) if k.expires_at == 0 => positive_ttl,
        Some(k) => Duration::from_secs(k.expires_at.saturating_sub(now)).min(positive_ttl),
    }
}

struct KeyCacheExpiry {
    positive_ttl: Duration,
}

impl Expiry<String, Option<CachedKey>> for KeyCacheExpiry {
    fn expire_after_create(
        &self,
        _key: &String,
        value: &Option<CachedKey>,
        _created_at: Instant,
    ) -> Option<Duration> {
        Some(entry_ttl(value.as_ref(), self.positive_ttl, now_unix()))
    }
}

/// Bounded local cache of Redis key lookups (hits and misses).
struct KeyCache {
    cache: Cache<String, Option<CachedKey>>,
}

impl KeyCache {
    fn new(positive_ttl: Duration) -> Self {
        let cache = Cache::builder()
            .expire_after(KeyCacheExpiry { positive_ttl })
            .max_capacity(KEY_CACHE_CAPACITY)
            .build();
        Self { cache }
    }

    /// `None` on a cache miss. A cached key whose `expires_at` has passed is
    /// reported invalid even if its entry has not been evicted yet.
    async fn get(&self, key: &str, now: u64) -> Option<Option<KeyInfo>> {
        let entry = self.cache.get(key).await?;
        Some(entry.and_then(|k| {
            if k.expires_at != 0 && k.expires_at <= now {
                None
            } else {
                Some(k.info)
            }
        }))
    }

    async fn insert(&self, key: &str, entry: Option<CachedKey>) {
        self.cache.insert(key.to_string(), entry).await;
    }
}

pub struct RedisKeyStore {
    conn: ConnectionManager,
    cache: KeyCache,
}

impl RedisKeyStore {
    pub async fn new(redis_url: &str) -> Result<Self, KeyStoreError> {
        Self::with_cache_ttl(redis_url, KEY_CACHE_TTL).await
    }

    /// Like `new`, caching valid keys for `cache_ttl` (`key_cache_ttl_secs`).
    /// `Duration::ZERO` disables the cache.
    pub async fn with_cache_ttl(
        redis_url: &str,
        cache_ttl: Duration,
    ) -> Result<Self, KeyStoreError> {
        let client = Client::open(redis_url).map_err(|e| KeyStoreError::Backend(e.to_string()))?;
        let conn = client
            .get_connection_manager()
            .await
            .map_err(|e| KeyStoreError::Backend(e.to_string()))?;

        Ok(Self {
            conn,
            cache: KeyCache::new(cache_ttl),
        })
    }

    async fn get_key_info(&self, key: &str) -> Result<Option<KeyInfo>, KeyStoreError> {
        if let Some(info) = self.cache.get(key, now_unix()).await {
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
        let entry = info.clone().map(|info| CachedKey {
            info,
            expires_at: parse_expires_at(&fields),
        });
        self.cache.insert(key, entry).await;
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

    let exp = parse_expires_at(fields);
    if exp != 0 && exp <= now {
        return None;
    }

    let rate_limit = fields
        .get("rate_limit")
        .and_then(|v| v.parse::<u64>().ok())
        .unwrap_or(0);

    Some(KeyInfo { owner, rate_limit })
}

/// `expires_at` from an `api_key:*` hash; `0` (never) when absent or garbage.
fn parse_expires_at(fields: &HashMap<String, String>) -> u64 {
    fields
        .get("expires_at")
        .and_then(|v| v.parse::<u64>().ok())
        .unwrap_or(0)
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

    fn cached(owner: &str, expires_at: u64) -> CachedKey {
        CachedKey {
            info: KeyInfo {
                owner: owner.into(),
                rate_limit: 0,
            },
            expires_at,
        }
    }

    #[tokio::test]
    async fn cache_hit_rejects_key_once_expires_at_passes() {
        // Entry TTLs run on the real clock; `get` takes `now` explicitly.
        let now = now_unix();
        let cache = KeyCache::new(KEY_CACHE_TTL);
        cache.insert("k", Some(cached("alice", now + 30))).await;
        let hit = cache.get("k", now + 29).await;
        assert_eq!(hit.unwrap().unwrap().owner, "alice");
        // Still cached (its TTL is ~30 s away), but no longer valid.
        assert_eq!(cache.get("k", now + 30).await, Some(None));
        assert_eq!(cache.get("k", now + 31).await, Some(None));
    }

    #[tokio::test]
    async fn cache_hit_without_expiry_stays_valid() {
        let cache = KeyCache::new(KEY_CACHE_TTL);
        cache.insert("k", Some(cached("alice", 0))).await;
        assert!(cache.get("k", u64::MAX).await.unwrap().is_some());
        cache.insert("missing", None).await;
        assert_eq!(cache.get("missing", 1).await, Some(None));
        assert_eq!(cache.get("other", 1).await, None);
    }

    #[test]
    fn negative_entries_expire_sooner_than_positive_ones() {
        assert!(NEGATIVE_KEY_CACHE_TTL < KEY_CACHE_TTL);
        assert_eq!(entry_ttl(None, KEY_CACHE_TTL, 0), NEGATIVE_KEY_CACHE_TTL);
        assert_eq!(
            entry_ttl(Some(&cached("a", 0)), KEY_CACHE_TTL, 0),
            KEY_CACHE_TTL
        );
        // A shorter configured TTL also caps negative entries.
        let short = Duration::from_secs(2);
        assert_eq!(entry_ttl(None, short, 0), short);
        assert_eq!(entry_ttl(None, Duration::ZERO, 0), Duration::ZERO);
    }

    #[test]
    fn positive_ttl_is_capped_at_expiry() {
        let ttl = KEY_CACHE_TTL;
        assert_eq!(
            entry_ttl(Some(&cached("a", 110)), ttl, 100),
            Duration::from_secs(10)
        );
        assert_eq!(entry_ttl(Some(&cached("a", 10_000)), ttl, 100), ttl);
        assert_eq!(entry_ttl(Some(&cached("a", 50)), ttl, 100), Duration::ZERO);
    }
}
