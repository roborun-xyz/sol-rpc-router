use std::{
    collections::{BTreeMap, HashMap, HashSet},
    time::{SystemTime, UNIX_EPOCH},
};

use clap::{Parser, Subcommand};
use rand::{distributions::Alphanumeric, Rng};
use redis::AsyncCommands;
use sol_rpc_router::keystore::{
    KEY_CACHE_TTL, KEY_INDEX, KEY_PREFIX, NEGATIVE_KEY_CACHE_TTL, RATE_LIMIT_PREFIX,
};

#[derive(Parser)]
#[command(name = "rpc-admin", version)]
#[command(about = "Manage API keys for sol-rpc-router", long_about = None)]
struct Cli {
    /// Redis connection URL
    #[arg(long, env = "REDIS_URL", default_value = "redis://127.0.0.1:6379")]
    redis_url: String,

    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// Create a new API key
    Create {
        /// Owner identifier (e.g. client-name)
        owner: String,
        /// Rate limit in requests per second (0 = unlimited)
        #[arg(long, default_value_t = 10)]
        rate_limit: u64,
        /// Absolute expiry as a unix timestamp (seconds)
        #[arg(long, conflicts_with = "expires_in")]
        expires_at: Option<u64>,
        /// Relative expiry, e.g. 30d, 12h, 90m, 3600s
        #[arg(long, value_parser = parse_duration_secs)]
        expires_in: Option<u64>,
        /// Custom API key value (auto-generated if omitted)
        #[arg(long)]
        key: Option<String>,
    },
    /// Deactivate an API key (keeps its metadata)
    Revoke { key: String },
    /// Permanently delete an API key and its rate-limit counter
    Delete { key: String },
    /// Update an existing API key
    Update {
        /// API key to update
        key: String,
        /// New rate limit (requests per second, 0 = unlimited)
        #[arg(long)]
        rate_limit: Option<u64>,
        /// New owner name
        #[arg(long)]
        owner: Option<String>,
        /// Activate (true) or deactivate (false)
        #[arg(long)]
        active: Option<bool>,
        /// New absolute expiry (unix seconds); 0 clears the expiry
        #[arg(long, conflicts_with = "expires_in")]
        expires_at: Option<u64>,
        /// New relative expiry, e.g. 30d, 12h
        #[arg(long, value_parser = parse_duration_secs)]
        expires_in: Option<u64>,
    },
    /// List all API keys
    List {
        /// Only show keys for this owner
        #[arg(long)]
        owner: Option<String>,
        /// Only show active keys (hide revoked, expired and missing ones)
        #[arg(long)]
        active_only: bool,
        /// Show at most this many keys (after filtering, in OWNER/KEY order)
        #[arg(long)]
        limit: Option<usize>,
        /// Skip this many keys before the first one shown
        #[arg(long, default_value_t = 0)]
        offset: usize,
        /// Keys fetched per SSCAN round trip
        #[arg(long, default_value_t = 500, hide = true)]
        batch_size: usize,
    },
    /// Inspect an API key
    Inspect { key: String },
}

/// How long a change takes to reach routers that cached the key.
fn propagation_note(what: &str) -> String {
    format!(
        "(running routers {} within their key_cache_ttl_secs, {}s by default)",
        what,
        KEY_CACHE_TTL.as_secs()
    )
}

fn parse_duration_secs(s: &str) -> Result<u64, String> {
    let s = s.trim();
    let (num, unit) = s.split_at(s.find(|c: char| !c.is_ascii_digit()).unwrap_or(s.len()));
    let n: u64 = num.parse().map_err(|_| {
        format!(
            "invalid duration '{}': expected e.g. 30d, 12h, 90m, 3600s",
            s
        )
    })?;
    let mult = match unit {
        "" | "s" => 1,
        "m" => 60,
        "h" => 3600,
        "d" => 86_400,
        "w" => 604_800,
        _ => {
            return Err(format!(
                "unknown duration unit '{}' (use s, m, h, d, w)",
                unit
            ))
        }
    };
    Ok(n * mult)
}

fn now_unix() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn fmt_expiry(expires_at: Option<u64>, now: u64) -> String {
    match expires_at {
        None | Some(0) => "never".to_string(),
        Some(t) if t <= now => format!("{} (EXPIRED)", t),
        Some(t) => {
            let left = t - now;
            let human = if left >= 86_400 {
                format!("{}d", left / 86_400)
            } else if left >= 3600 {
                format!("{}h", left / 3600)
            } else {
                format!("{}m", left / 60)
            };
            format!("{} (in {})", t, human)
        }
    }
}

fn redis_key(key: &str) -> String {
    format!("{}{}", KEY_PREFIX, key)
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let cli = Cli::parse();
    let client = redis::Client::open(cli.redis_url)?;
    let mut con = client.get_multiplexed_async_connection().await?;

    match cli.command {
        Commands::Create {
            owner,
            rate_limit,
            expires_at,
            expires_in,
            key: custom_key,
        } => {
            let key: String = custom_key.unwrap_or_else(|| {
                rand::thread_rng()
                    .sample_iter(&Alphanumeric)
                    .take(32)
                    .map(char::from)
                    .collect()
            });
            let rk = redis_key(&key);

            let exists: bool = con.exists(&rk).await?;
            if exists {
                eprintln!("Key already exists: {}", key);
                std::process::exit(1);
            }

            let now = now_unix();
            let expires_at = expires_at.or(expires_in.map(|d| now + d));

            let mut pipe = redis::pipe();
            pipe.atomic()
                .hset(&rk, "owner", &owner)
                .hset(&rk, "rate_limit", rate_limit)
                .hset(&rk, "created_at", now)
                .hset(&rk, "active", "true")
                .sadd(KEY_INDEX, &key);
            if let Some(exp) = expires_at {
                pipe.hset(&rk, "expires_at", exp);
            }
            let _: () = pipe.query_async(&mut con).await?;

            println!("Created API key for {}:", owner);
            println!("{}", key);
            println!();
            println!("  rate limit: {}", fmt_rate_limit(rate_limit));
            println!("  expires:    {}", fmt_expiry(expires_at, now));
            println!(
                "(usable right away; a router that rejected this key just before may keep doing so for up to {}s)",
                NEGATIVE_KEY_CACHE_TTL.as_secs()
            );
        }
        Commands::Revoke { key } => {
            let rk = redis_key(&key);
            let exists: bool = con.exists(&rk).await?;
            if exists {
                let _: () = con.hset(&rk, "active", "false").await?;
                println!("Revoked key: {}", key);
                println!("{}", propagation_note("stop accepting it"));
            } else {
                println!("Key not found: {}", key);
            }
        }
        Commands::Delete { key } => {
            let rk = redis_key(&key);
            let mut pipe = redis::pipe();
            pipe.atomic()
                .del(&rk)
                .del(format!("{}{}", RATE_LIMIT_PREFIX, key))
                .srem(KEY_INDEX, &key);
            let (deleted, _, _): (u64, u64, u64) = pipe.query_async(&mut con).await?;
            if deleted > 0 {
                println!("Deleted key: {}", key);
            } else {
                println!("Key not found: {}", key);
            }
        }
        Commands::Update {
            key,
            rate_limit,
            owner,
            active,
            expires_at,
            expires_in,
        } => {
            let rk = redis_key(&key);
            let exists: bool = con.exists(&rk).await?;
            if !exists {
                println!("Key not found: {}", key);
                return Ok(());
            }

            let now = now_unix();
            let expires_at = expires_at.or(expires_in.map(|d| now + d));

            let mut pipe = redis::pipe();
            let mut changes = Vec::new();

            if let Some(rl) = rate_limit {
                pipe.hset(&rk, "rate_limit", rl);
                changes.push(format!("rate_limit -> {}", fmt_rate_limit(rl)));
            }
            if let Some(o) = owner {
                pipe.hset(&rk, "owner", &o);
                changes.push(format!("owner -> {}", o));
            }
            if let Some(a) = active {
                let status = if a { "true" } else { "false" };
                pipe.hset(&rk, "active", status);
                changes.push(format!("active -> {}", status));
            }
            match expires_at {
                Some(0) => {
                    pipe.hdel(&rk, "expires_at");
                    changes.push("expires_at -> never".to_string());
                }
                Some(exp) => {
                    pipe.hset(&rk, "expires_at", exp);
                    changes.push(format!("expires_at -> {}", fmt_expiry(Some(exp), now)));
                }
                None => {}
            }

            if changes.is_empty() {
                println!("No changes requested for key: {}", key);
            } else {
                let _: () = pipe.query_async(&mut con).await?;
                println!("Updated key: {}", key);
                for change in changes {
                    println!("  {}", change);
                }
                println!("{}", propagation_note("pick this up"));
            }
        }
        Commands::List {
            owner,
            active_only,
            limit,
            offset,
            batch_size,
        } => {
            let filter = ListFilter {
                owner,
                active_only,
                keep: limit.map(|l| offset.saturating_add(l)),
            };
            let (total, rows) = list_keys(&mut con, &filter, batch_size.max(1)).await?;
            let shown: Vec<_> = rows
                .into_iter()
                .skip(offset)
                .take(limit.unwrap_or(usize::MAX))
                .collect();

            println!("Found {} key(s):", total);
            println!(
                "{:<34} {:<20} {:<8} {:<12} EXPIRES",
                "KEY", "OWNER", "STATUS", "RATE LIMIT"
            );
            for ((owner, key), (status, rl, exp)) in &shown {
                println!("{:<34} {:<20} {:<8} {:<12} {}", key, owner, status, rl, exp);
            }
            if limit.is_some() || offset > 0 {
                let end = offset + shown.len();
                if shown.is_empty() {
                    println!("No keys at offset {}", offset);
                } else {
                    println!("Showing {}-{} of {}", offset + 1, end, total);
                }
                if end < total {
                    println!("Next page: --offset {}", end);
                }
            }
        }
        Commands::Inspect { key } => {
            let fields: HashMap<String, String> = con.hgetall(redis_key(&key)).await?;
            if fields.is_empty() {
                println!("Key not found");
                return Ok(());
            }
            let now = now_unix();
            let expires_at = fields.get("expires_at").and_then(|v| v.parse().ok());
            let current_rps: Option<u64> = con.get(format!("{}{}", RATE_LIMIT_PREFIX, key)).await?;

            println!("Key:         {}", key);
            println!(
                "Owner:       {}",
                fields.get("owner").map(String::as_str).unwrap_or("-")
            );
            println!(
                "Active:      {}",
                fields.get("active").map(String::as_str).unwrap_or("true")
            );
            println!(
                "Rate limit:  {}",
                fields
                    .get("rate_limit")
                    .and_then(|v| v.parse().ok())
                    .map(fmt_rate_limit)
                    .unwrap_or_else(|| "-".into())
            );
            println!("This second: {} request(s)", current_rps.unwrap_or(0));
            println!(
                "Created at:  {}",
                fields.get("created_at").map(String::as_str).unwrap_or("-")
            );
            println!("Expires:     {}", fmt_expiry(expires_at, now));
        }
    }

    Ok(())
}

struct ListFilter {
    owner: Option<String>,
    active_only: bool,
    /// Keep only the first N rows in sort order (offset + limit); None keeps all.
    keep: Option<usize>,
}

/// ((owner, key), (status, rate limit, expiry)); the map key gives the
/// OWNER-then-KEY output order.
type ListRow = ((String, String), (String, String, String));

/// Walks the key index with SSCAN and fetches each batch's hashes in one
/// pipeline. When paginating, only the first `keep` rows are held in memory.
/// Returns the number of matching keys and the kept rows in display order.
async fn list_keys(
    con: &mut redis::aio::MultiplexedConnection,
    filter: &ListFilter,
    batch_size: usize,
) -> redis::RedisResult<(usize, Vec<ListRow>)> {
    let now = now_unix();
    let mut rows = BTreeMap::new();
    // SSCAN permits duplicates, including keys trimmed from the retained page.
    // Keep identities separately; each key's first observed hash wins.
    let mut seen = HashSet::new();
    let mut total = 0;
    let mut cursor: u64 = 0;

    loop {
        let (next, mut keys): (u64, Vec<String>) = redis::cmd("SSCAN")
            .arg(KEY_INDEX)
            .arg(cursor)
            .arg("COUNT")
            .arg(batch_size)
            .query_async(con)
            .await?;

        keys.retain(|key| seen.insert(key.clone()));

        let mut pipe = redis::pipe();
        for key in &keys {
            pipe.hgetall(redis_key(key));
        }
        let hashes: Vec<HashMap<String, String>> = if keys.is_empty() {
            Vec::new()
        } else {
            pipe.query_async(con).await?
        };

        for (key, fields) in keys.into_iter().zip(hashes) {
            let Some((owner, info)) = list_row(&fields, filter, now) else {
                continue;
            };
            rows.insert((owner, key), info);
            total += 1;
            if let Some(keep) = filter.keep {
                if rows.len() > keep {
                    rows.pop_last();
                }
            }
        }

        cursor = next;
        if cursor == 0 {
            break;
        }
    }

    Ok((total, rows.into_iter().collect()))
}

/// Builds one list row, or None if the filter excludes it. Index entries with
/// no hash are shown as "missing" regardless of `--owner`.
fn list_row(
    fields: &HashMap<String, String>,
    filter: &ListFilter,
    now: u64,
) -> Option<(String, (String, String, String))> {
    if fields.is_empty() {
        if filter.active_only {
            return None;
        }
        return Some((
            "-".to_string(),
            ("missing".to_string(), String::new(), String::new()),
        ));
    }
    let owner = fields.get("owner").cloned().unwrap_or_else(|| "-".into());
    if filter.owner.as_ref().is_some_and(|f| &owner != f) {
        return None;
    }
    let active = fields.get("active").map(|a| a != "false").unwrap_or(true);
    let expires_at = fields.get("expires_at").and_then(|v| v.parse().ok());
    let expired = matches!(expires_at, Some(t) if t != 0 && t <= now);
    let status = match (active, expired) {
        (false, _) => "revoked",
        (true, true) => "expired",
        (true, false) => "active",
    };
    if filter.active_only && status != "active" {
        return None;
    }
    let rl = fields
        .get("rate_limit")
        .and_then(|v| v.parse().ok())
        .map(fmt_rate_limit)
        .unwrap_or_else(|| "-".into());
    Some((owner, (status.to_string(), rl, fmt_expiry(expires_at, now))))
}

fn fmt_rate_limit(rl: u64) -> String {
    if rl == 0 {
        "unlimited".to_string()
    } else {
        format!("{} rps", rl)
    }
}
