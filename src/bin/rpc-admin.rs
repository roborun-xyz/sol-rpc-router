use std::time::{SystemTime, UNIX_EPOCH};

use clap::{Parser, Subcommand};
use rand::{distributions::Alphanumeric, Rng};
use redis::AsyncCommands;

#[derive(Parser)]
#[command(name = "rpc-admin")]
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
        /// Rate limit (requests per second)
        #[arg(long, default_value_t = 10)]
        rate_limit: u64,
        /// Expiration timestamp (optional)
        #[arg(long)]
        expires_at: Option<u64>,
    },
    /// Revoke an API key
    Revoke { key: String },
    /// List all API keys
    List,
    /// Inspect an API key
    Inspect { key: String },
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
        } => {
            let key: String = rand::thread_rng()
                .sample_iter(&Alphanumeric)
                .take(32)
                .map(char::from)
                .collect();

            let redis_key = format!("api_key:{}", key);

            let created_at = SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs();

            let mut pipe = redis::pipe();
            pipe.atomic()
                .hset(&redis_key, "owner", &owner)
                .hset(&redis_key, "rate_limit", rate_limit)
                .hset(&redis_key, "created_at", created_at)
                .hset(&redis_key, "active", "true");

            if let Some(exp) = expires_at {
                pipe.hset(&redis_key, "expires_at", exp);
            }

            let _: () = pipe.query_async(&mut con).await?;

            // Also store in a set for listing
            let _: () = con.sadd("api_keys_index", &key).await?;

            println!("Created API key for {}:", owner);
            println!("{}", key);
        }
        Commands::Revoke { key } => {
            let redis_key = format!("api_key:{}", key);
            // Check existence first
            let exists: bool = redis::cmd("EXISTS")
                .arg(&redis_key)
                .query_async(&mut con)
                .await?;

            if exists {
                // Set active=false
                let _: () = con.hset(&redis_key, "active", "false").await?;
                // Optionally delete from index if you want to hide it
                // let _: () = con.srem("api_keys_index", &key).await?;
                println!("Revoked key: {}", key);
            } else {
                println!("Key not found: {}", key);
            }
        }
        Commands::List => {
            let keys: Vec<String> = con.smembers("api_keys_index").await?;
            println!("Found {} keys:", keys.len());
            for key in keys {
                let redis_key = format!("api_key:{}", key);
                let owner: Option<String> = con.hget(&redis_key, "owner").await?;
                let active: Option<String> = con.hget(&redis_key, "active").await?;

                if let Some(o) = owner {
                    println!(
                        "- {} [owner={}] [active={}]",
                        key,
                        o,
                        active.unwrap_or("true".to_string())
                    );
                } else {
                    println!("- {} [missing metadata]", key);
                }
            }
        }
        Commands::Inspect { key } => {
            let redis_key = format!("api_key:{}", key);
            let exists: bool = redis::cmd("EXISTS")
                .arg(&redis_key)
                .query_async(&mut con)
                .await?;

            if exists {
                let owner: String = con.hget(&redis_key, "owner").await.unwrap_or_default();
                let rate_limit: u64 = con.hget(&redis_key, "rate_limit").await.unwrap_or(0);
                let active: String = con
                    .hget(&redis_key, "active")
                    .await
                    .unwrap_or("true".to_string());
                let created_at: u64 = con.hget(&redis_key, "created_at").await.unwrap_or(0);

                println!("Key: {}", key);
                println!("Owner: {}", owner);
                println!("Active: {}", active);
                println!("Rate Limit: {} RPS", rate_limit);
                println!("Created At: {}", created_at);
            } else {
                println!("Key not found");
            }
        }
    }

    Ok(())
}
