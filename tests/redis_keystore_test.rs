//! Exercises RedisKeyStore against a real Redis. Skipped unless TEST_REDIS_URL
//! is set (CI provides one; locally: `docker run -d -p 6379:6379 redis:7-alpine`
//! then `TEST_REDIS_URL=redis://127.0.0.1:6379/15 cargo test --test redis_keystore_test`).

use std::time::{Duration, SystemTime, UNIX_EPOCH};

use redis::AsyncCommands;
use sol_rpc_router::keystore::{KeyStore, RedisKeyStore, KEY_PREFIX, RATE_LIMIT_PREFIX};

fn redis_url() -> Option<String> {
    std::env::var("TEST_REDIS_URL")
        .ok()
        .filter(|s| !s.is_empty())
}

async fn seed(con: &mut redis::aio::MultiplexedConnection, key: &str, fields: &[(&str, String)]) {
    let rk = format!("{}{}", KEY_PREFIX, key);
    let _: () = con.del(&rk).await.unwrap();
    let _: () = con
        .del(format!("{}{}", RATE_LIMIT_PREFIX, key))
        .await
        .unwrap();
    for (f, v) in fields {
        let _: () = con.hset(&rk, *f, v).await.unwrap();
    }
}

fn unique(prefix: &str) -> String {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    format!("{}-{}-{}", prefix, std::process::id(), nanos)
}

#[tokio::test]
async fn redis_keystore_end_to_end() {
    let Some(url) = redis_url() else {
        eprintln!("TEST_REDIS_URL not set, skipping");
        return;
    };

    let client = redis::Client::open(url.as_str()).unwrap();
    let mut con = client.get_multiplexed_async_connection().await.unwrap();
    let store = RedisKeyStore::new(&url).await.unwrap();

    // Unknown key.
    assert_eq!(store.validate_key(&unique("nope")).await.unwrap(), None);

    // Active key with a rate limit of 3 rps: 3 pass, 4th is limited.
    let limited = unique("limited");
    seed(
        &mut con,
        &limited,
        &[
            ("owner", "alice".into()),
            ("rate_limit", "3".into()),
            ("active", "true".into()),
        ],
    )
    .await;
    for _ in 0..3 {
        let info = store.validate_key(&limited).await.unwrap().unwrap();
        assert_eq!(info.owner, "alice");
        assert_eq!(info.rate_limit, 3);
    }
    assert_eq!(
        store.validate_key(&limited).await.unwrap_err(),
        "Rate limit exceeded"
    );
    // The counter window is one second.
    tokio::time::sleep(Duration::from_millis(1100)).await;
    assert!(store.validate_key(&limited).await.unwrap().is_some());

    // Unlimited key never trips the limiter.
    let unlimited = unique("unlimited");
    seed(
        &mut con,
        &unlimited,
        &[("owner", "bob".into()), ("rate_limit", "0".into())],
    )
    .await;
    for _ in 0..50 {
        assert!(store.validate_key(&unlimited).await.unwrap().is_some());
    }

    // Revoked key.
    let revoked = unique("revoked");
    seed(
        &mut con,
        &revoked,
        &[
            ("owner", "carol".into()),
            ("rate_limit", "10".into()),
            ("active", "false".into()),
        ],
    )
    .await;
    assert_eq!(store.validate_key(&revoked).await.unwrap(), None);

    // Expired key.
    let expired = unique("expired");
    let past = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs()
        - 10;
    seed(
        &mut con,
        &expired,
        &[
            ("owner", "dave".into()),
            ("rate_limit", "10".into()),
            ("expires_at", past.to_string()),
        ],
    )
    .await;
    assert_eq!(store.validate_key(&expired).await.unwrap(), None);

    // Future expiry is fine.
    let fresh = unique("fresh");
    seed(
        &mut con,
        &fresh,
        &[
            ("owner", "erin".into()),
            ("rate_limit", "10".into()),
            ("expires_at", (past + 3600).to_string()),
        ],
    )
    .await;
    assert!(store.validate_key(&fresh).await.unwrap().is_some());

    // Cleanup.
    for k in [&limited, &unlimited, &revoked, &expired, &fresh] {
        let _: () = con.del(format!("{}{}", KEY_PREFIX, k)).await.unwrap();
        let _: () = con
            .del(format!("{}{}", RATE_LIMIT_PREFIX, k))
            .await
            .unwrap();
    }
}
