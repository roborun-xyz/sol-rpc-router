use std::{
    collections::HashMap,
    sync::LazyLock,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use async_trait::async_trait;
use moka::future::Cache;
use redis::{aio::ConnectionManager, Client, Script};

/// How long key metadata is cached locally. Revocations and rate-limit
/// changes take up to this long to propagate to a running router.
pub const KEY_CACHE_TTL: Duration = Duration::from_secs(60);

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

#[async_trait]
pub trait KeyStore: Send + Sync {
    /// `Ok(Some(info))` for a valid key, `Ok(None)` for unknown/inactive/
    /// expired keys, `Err("Rate limit exceeded")` when the per-second budget
    /// is spent, and `Err(other)` for infrastructure failures.
    async fn validate_key(&self, key: &str) -> Result<Option<KeyInfo>, String>;
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
    pub async fn new(redis_url: &str) -> Result<Self, String> {
        let client = Client::open(redis_url).map_err(|e| e.to_string())?;
        let conn = client
            .get_connection_manager()
            .await
            .map_err(|e| e.to_string())?;

        let cache = Cache::builder().time_to_live(KEY_CACHE_TTL).build();

        Ok(Self { conn, cache })
    }

    async fn get_key_info(&self, key: &str) -> Result<Option<KeyInfo>, String> {
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
            .map_err(|e| e.to_string())?;

        let info = parse_key_fields(&fields, now_unix());
        self.cache.insert(key.to_string(), info.clone()).await;
        Ok(info)
    }

    async fn check_rate_limit(&self, key: &str, limit: u64) -> Result<bool, String> {
        if limit == 0 {
            return Ok(true);
        }

        let mut conn = self.conn.clone();
        let redis_key = format!("{}{}", RATE_LIMIT_PREFIX, key);

        let count: u64 = RATE_LIMIT_SCRIPT
            .key(&redis_key)
            .invoke_async(&mut conn)
            .await
            .map_err(|e| e.to_string())?;

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
    async fn validate_key(&self, key: &str) -> Result<Option<KeyInfo>, String> {
        let info = match self.get_key_info(key).await? {
            Some(info) => info,
            None => return Ok(None),
        };

        if !self.check_rate_limit(key, info.rate_limit).await? {
            return Err("Rate limit exceeded".to_string());
        }
        Ok(Some(info))
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
