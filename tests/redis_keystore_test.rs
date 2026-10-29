//! Exercises RedisKeyStore against a real Redis. Skipped unless TEST_REDIS_URL
//! is set (CI provides one; locally: `docker run -d -p 6379:6379 redis:7-alpine`
//! then `TEST_REDIS_URL=redis://127.0.0.1:6379/15 cargo test --test redis_keystore_test`).

use std::time::{Duration, SystemTime, UNIX_EPOCH};

use redis::AsyncCommands;
use sol_rpc_router::keystore::{
    KeyStore, KeyStoreError, RedisKeyStore, KEY_INDEX, KEY_PREFIX, NEGATIVE_KEY_CACHE_TTL,
    RATE_LIMIT_PREFIX,
};

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
        KeyStoreError::RateLimited
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

    // A cached key stops working when its expiry passes, not when the cache
    // entry would have expired.
    let expiring = unique("expiring");
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs();
    seed(
        &mut con,
        &expiring,
        &[
            ("owner", "frank".into()),
            ("expires_at", (now + 2).to_string()),
        ],
    )
    .await;
    assert!(store.validate_key(&expiring).await.unwrap().is_some());
    tokio::time::sleep(Duration::from_millis(2100)).await;
    assert_eq!(store.validate_key(&expiring).await.unwrap(), None);

    // A key created after a cached miss works once the short negative entry
    // lapses, well before the positive TTL.
    let late = unique("late");
    assert_eq!(store.validate_key(&late).await.unwrap(), None);
    seed(&mut con, &late, &[("owner", "gina".into())]).await;
    assert_eq!(store.validate_key(&late).await.unwrap(), None);
    tokio::time::sleep(NEGATIVE_KEY_CACHE_TTL + Duration::from_millis(200)).await;
    assert_eq!(
        store.validate_key(&late).await.unwrap().unwrap().owner,
        "gina"
    );

    // Cleanup.
    for k in [
        &limited, &unlimited, &revoked, &expired, &fresh, &expiring, &late,
    ] {
        let _: () = con.del(format!("{}{}", KEY_PREFIX, k)).await.unwrap();
        let _: () = con
            .del(format!("{}{}", RATE_LIMIT_PREFIX, k))
            .await
            .unwrap();
    }
}

fn rpc_admin(url: &str, args: &[&str]) -> String {
    let out = std::process::Command::new(env!("CARGO_BIN_EXE_rpc-admin"))
        .arg("--redis-url")
        .arg(url)
        .args(args)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "rpc-admin {:?} failed: {}",
        args,
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8(out.stdout).unwrap()
}

/// The `rpc-admin list` table, as printed before SSCAN paging was added.
fn list_table(total: usize, rows: &[(&str, &str, &str, &str, &str)]) -> String {
    let mut s = format!("Found {} key(s):\n", total);
    s += &format!(
        "{:<34} {:<20} {:<8} {:<12} EXPIRES\n",
        "KEY", "OWNER", "STATUS", "RATE LIMIT"
    );
    for (key, owner, status, rl, exp) in rows {
        s += &format!(
            "{:<34} {:<20} {:<8} {:<12} {}\n",
            key, owner, status, rl, exp
        );
    }
    s
}

#[tokio::test]
async fn rpc_admin_list_pages_through_index() {
    let Some(url) = redis_url() else {
        eprintln!("TEST_REDIS_URL not set, skipping");
        return;
    };

    let client = redis::Client::open(url.as_str()).unwrap();
    let mut con = client.get_multiplexed_async_connection().await.unwrap();

    // Seven keys for one owner plus an index entry with no hash, listed with
    // a batch size of 2 so the scan takes many round trips.
    let owner = unique("lister");
    let keys: Vec<String> = (0..7).map(|i| format!("{}-k{}", owner, i)).collect();
    let missing = format!("{}-gone", owner);
    let past = "1000000000".to_string();
    for (i, key) in keys.iter().enumerate() {
        let mut fields = vec![
            ("owner", owner.clone()),
            ("rate_limit", if i == 1 { "0" } else { "10" }.to_string()),
            ("active", if i == 3 { "false" } else { "true" }.to_string()),
        ];
        if i == 5 {
            fields.push(("expires_at", past.clone()));
        }
        seed(&mut con, key, &fields).await;
        let _: () = con.sadd(KEY_INDEX, key).await.unwrap();
    }
    let _: () = con.del(format!("{}{}", KEY_PREFIX, missing)).await.unwrap();
    let _: () = con.sadd(KEY_INDEX, &missing).await.unwrap();
    // Redis answers SSCAN on a small (listpack) set in one reply whatever the
    // COUNT, so add other owners' keys until the index is a real hash table.
    let fillers: Vec<String> = (0..150).map(|i| format!("{}-other{}", owner, i)).collect();
    let mut pipe = redis::pipe();
    for f in &fillers {
        pipe.hset(format!("{}{}", KEY_PREFIX, f), "owner", "someone-else")
            .sadd(KEY_INDEX, f);
    }
    let _: () = pipe.query_async(&mut con).await.unwrap();
    let encoding: String = redis::cmd("OBJECT")
        .arg("ENCODING")
        .arg(KEY_INDEX)
        .query_async(&mut con)
        .await
        .unwrap();
    assert_eq!(encoding, "hashtable");

    let expired = format!("{} (EXPIRED)", past);
    let row = |i: usize| {
        let (status, rl, exp) = match i {
            1 => ("active", "unlimited", "never"),
            3 => ("revoked", "10 rps", "never"),
            5 => ("expired", "10 rps", expired.as_str()),
            _ => ("active", "10 rps", "never"),
        };
        (keys[i].as_str(), owner.as_str(), status, rl, exp)
    };
    let mut all = vec![(missing.as_str(), "-", "missing", "", "")];
    all.extend((0..7).map(row));

    // Same output as the old SMEMBERS listing, whatever the batch size.
    let expected = list_table(8, &all);
    for batch in ["2", "500"] {
        assert_eq!(
            rpc_admin(&url, &["list", "--owner", &owner, "--batch-size", batch]),
            expected
        );
    }

    // --active-only drops revoked, expired and missing keys.
    let active: Vec<_> = [0, 1, 2, 4, 6].into_iter().map(row).collect();
    assert_eq!(
        rpc_admin(
            &url,
            &[
                "list",
                "--owner",
                &owner,
                "--active-only",
                "--batch-size",
                "2"
            ]
        ),
        list_table(5, &active)
    );

    // Pages of 3 cover every row once, in order.
    let page = |offset: &str| {
        rpc_admin(
            &url,
            &[
                "list",
                "--owner",
                &owner,
                "--batch-size",
                "2",
                "--limit",
                "3",
                "--offset",
                offset,
            ],
        )
    };
    assert_eq!(
        page("0"),
        list_table(8, &all[0..3]) + "Showing 1-3 of 8\nNext page: --offset 3\n"
    );
    assert_eq!(
        page("3"),
        list_table(8, &all[3..6]) + "Showing 4-6 of 8\nNext page: --offset 6\n"
    );
    assert_eq!(page("6"), list_table(8, &all[6..8]) + "Showing 7-8 of 8\n");
    assert_eq!(page("8"), list_table(8, &[]) + "No keys at offset 8\n");

    // Cleanup.
    for k in keys.iter().chain(&fillers).chain([&missing]) {
        let _: () = con.del(format!("{}{}", KEY_PREFIX, k)).await.unwrap();
        let _: () = con.srem(KEY_INDEX, k).await.unwrap();
    }
}
