use std::time::Duration;

use async_trait::async_trait;
use moka::future::Cache;
use redis::Client;

#[derive(Clone, Debug)]
pub struct KeyInfo {
    pub owner: String,
    pub rate_limit: u64,
}

#[async_trait]
pub trait KeyStore: Send + Sync {
    async fn validate_key(&self, key: &str) -> Result<Option<KeyInfo>, String>;
}

pub struct RedisKeyStore {
    client: Client,
    cache: Cache<String, Option<KeyInfo>>,
}

impl RedisKeyStore {
    pub fn new(redis_url: &str) -> Result<Self, String> {
        let client = Client::open(redis_url).map_err(|e| e.to_string())?;

        let cache = Cache::builder()
            .time_to_live(Duration::from_secs(60)) // Cache keys for 1 min
            .build();

        Ok(Self { client, cache })
    }

    async fn get_key_info(&self, key: &str) -> Result<Option<KeyInfo>, String> {
        // Check local cache
        if let Some(info) = self.cache.get(key).await {
            return Ok(info);
        }

        // Check Redis
        let mut conn = self
            .client
            .get_multiplexed_async_connection()
            .await
            .map_err(|e| e.to_string())?;

        let redis_key = format!("api_key:{}", key);
        // Check if exists first to avoid errors on empty keys
        let exists: bool = redis::cmd("EXISTS")
            .arg(&redis_key)
            .query_async(&mut conn)
            .await
            .map_err(|e| e.to_string())?;

        if !exists {
            self.cache.insert(key.to_string(), None).await;
            return Ok(None);
        }

        let owner: String = redis::cmd("HGET")
            .arg(&redis_key)
            .arg("owner")
            .query_async(&mut conn)
            .await
            .map_err(|e| e.to_string())?;
        let active: String = redis::cmd("HGET")
            .arg(&redis_key)
            .arg("active")
            .query_async(&mut conn)
            .await
            .unwrap_or("true".to_string());

        if active == "false" {
            self.cache.insert(key.to_string(), None).await;
            return Ok(None);
        }

        let rate_limit: u64 = redis::cmd("HGET")
            .arg(&redis_key)
            .arg("rate_limit")
            .query_async(&mut conn)
            .await
            .map_err(|e| e.to_string())?;

        let info = KeyInfo { owner, rate_limit };
        self.cache.insert(key.to_string(), Some(info.clone())).await;

        Ok(Some(info))
    }

    async fn check_rate_limit(&self, key: &str, limit: u64) -> Result<bool, String> {
        if limit == 0 {
            return Ok(true); // No limit
        }

        let mut conn = self
            .client
            .get_multiplexed_async_connection()
            .await
            .map_err(|e| e.to_string())?;
        let redis_key = format!("rate_limit:{}", key);

        // Atomic INCR and Expire if needed
        // Script to ensure atomicity: INCR key; IF == 1 THEN EXPIRE key 1; END; RETURN val
        let script = redis::Script::new(
            r#"
            local count = redis.call("INCR", KEYS[1])
            if count == 1 then
                redis.call("EXPIRE", KEYS[1], 1)
            end
            return count
        "#,
        );

        let count: u64 = script
            .key(&redis_key)
            .invoke_async(&mut conn)
            .await
            .map_err(|e| e.to_string())?;

        if count > limit {
            return Ok(false);
        }

        Ok(true)
    }
}

#[async_trait]
impl KeyStore for RedisKeyStore {
    async fn validate_key(&self, key: &str) -> Result<Option<KeyInfo>, String> {
        // 1. Get Key Info (Cache -> Redis)
        let info_opt = self.get_key_info(key).await?;

        if let Some(info) = info_opt {
            // 2. Check Rate Limit
            if !self.check_rate_limit(key, info.rate_limit).await? {
                return Err("Rate limit exceeded".to_string());
            }
            return Ok(Some(info));
        }

        Ok(None)
    }
}
