//! `bonsai-mock`: parses arguments and calls into the library.

use std::net::{Ipv4Addr, SocketAddr};
use std::path::{Path, PathBuf};

use anyhow::{Context, bail};
use bonsai_mock::{MockOptions, db, scenario, seed};
use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(
    name = "bonsai-mock",
    version,
    about = "A stateful mock of the Bonsai /v1 API"
)]
struct Cli {
    /// The SQLite database file.
    #[arg(long, global = true, default_value = "./bonsai-mock.sqlite")]
    db: PathBuf,
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Serve the mock on 127.0.0.1.
    Serve {
        #[arg(long, default_value_t = 4010)]
        port: u16,
        /// How long each Action step takes, in milliseconds.
        #[arg(long, default_value_t = 1500)]
        step_ms: u64,
        /// Delete the database first and start from the seed.
        #[arg(long)]
        reset: bool,
        /// The origin clients reach the mock at, for links and `Location`
        /// headers, e.g. https://api.bonsai.io behind a TLS proxy. Its host is
        /// accepted as a `Host`.
        #[arg(long, value_name = "URL")]
        public_url: Option<url::Url>,
        /// Make payment `action_url`s PREFIX plus a fake Stripe session ID,
        /// e.g. https://checkout.stripe.com/c/pay/, not the mock's checkout page.
        #[arg(long, value_name = "PREFIX")]
        checkout_url: Option<String>,
        /// Answer a small part of each cluster's own API (Elasticsearch's) on this
        /// port too, and point credential URLs here, so `bonsai cluster request`
        /// has a cluster to talk to.
        #[arg(long)]
        local_clusters: bool,
    },
    /// Manage API tokens.
    Token {
        #[command(subcommand)]
        command: TokenCommand,
    },
    /// Set, clear or show scenarios in the database, which a running server
    /// sees at once (the same table as `/_mock/scenario`).
    Scenario {
        #[command(subcommand)]
        command: ScenarioCommand,
    },
}

#[derive(Subcommand)]
enum ScenarioCommand {
    /// Set KEY to VALUE: fail_next (payment_failed, action_failed,
    /// timed_out), metrics_unavailable (true, false), rate_limit_next (a
    /// count) or cluster_status_next (a status from 400 to 599).
    Set {
        key: String,
        /// Read as JSON, or as a string when it isn't JSON.
        value: String,
    },
    /// Clear KEY, or every scenario when no key is given.
    Clear { key: Option<String> },
    /// Print every scenario that is set, as a JSON object.
    Show,
}

#[derive(Subcommand)]
enum TokenCommand {
    /// Create a token and print it.
    Create {
        #[arg(long)]
        account: String,
        /// Space-separated scopes, e.g. "account:read clusters:read".
        #[arg(long)]
        scopes: String,
        #[arg(long)]
        expires_days: Option<u32>,
        #[arg(long, default_value = "bonsai-mock token")]
        name: String,
    },
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "bonsai_mock=info".into()),
        )
        .with_writer(std::io::stderr)
        .init();
    let cli = Cli::parse();
    match cli.command {
        Command::Serve {
            port,
            step_ms,
            reset,
            public_url,
            checkout_url,
            local_clusters,
        } => {
            serve(
                cli.db,
                port,
                step_ms,
                reset,
                public_url,
                checkout_url,
                local_clusters,
            )
            .await
        }
        Command::Token {
            command:
                TokenCommand::Create {
                    account,
                    scopes,
                    expires_days,
                    name,
                },
        } => create_token(&cli.db, &account, &scopes, expires_days, &name),
        Command::Scenario { command } => run_scenario(&cli.db, command),
    }
}

fn run_scenario(db_path: &Path, command: ScenarioCommand) -> anyhow::Result<()> {
    let conn = db::open(db_path).with_context(|| format!("opening {}", db_path.display()))?;
    match command {
        ScenarioCommand::Set { key, value } => {
            let value = serde_json::from_str(&value).unwrap_or(serde_json::Value::String(value));
            scenario::validate(&key, &value).map_err(anyhow::Error::msg)?;
            scenario::set(&conn, &key, &value)?;
        }
        ScenarioCommand::Clear { key: Some(key) } => {
            if !scenario::KEYS.contains(&key.as_str()) {
                bail!(
                    "unknown scenario {key}; scenarios are: {}",
                    scenario::KEYS.join(", ")
                );
            }
            scenario::clear(&conn, &key)?;
        }
        ScenarioCommand::Clear { key: None } => {
            for key in scenario::KEYS {
                scenario::clear(&conn, key)?;
            }
        }
        ScenarioCommand::Show => {
            let all: serde_json::Map<_, _> = scenario::all(&conn)?.into_iter().collect();
            println!("{}", serde_json::Value::Object(all));
        }
    }
    Ok(())
}

async fn serve(
    db_path: PathBuf,
    port: u16,
    step_ms: u64,
    reset: bool,
    public_url: Option<url::Url>,
    checkout_url: Option<String>,
    local_clusters: bool,
) -> anyhow::Result<()> {
    if reset {
        db::remove(&db_path).with_context(|| format!("removing {}", db_path.display()))?;
    }
    let handle = bonsai_mock::spawn(
        MockOptions {
            db_path: db_path.clone(),
            step_ms,
            public_url,
            checkout_url,
            local_clusters,
        },
        SocketAddr::from((Ipv4Addr::LOCALHOST, port)),
    )
    .await?;

    println!("bonsai-mock serving {}", handle.base_url);
    println!("database: {}", db_path.display());
    if local_clusters {
        println!("local clusters: credential URLs point at this server");
    }
    if handle.seeded_tokens.is_empty() {
        println!(
            "Existing database: its tokens were printed when it was created. \
             Make another with `bonsai-mock token create`, or start over with --reset."
        );
    } else {
        for t in &handle.seeded_tokens {
            println!(
                "token {} ({}): {}",
                t.account_id,
                t.scopes.join(" "),
                t.token
            );
        }
    }
    println!("admin token (/_mock): {}", handle.admin_token);

    tokio::signal::ctrl_c().await?;
    let _ = handle.shutdown.send(());
    Ok(())
}

fn create_token(
    db_path: &Path,
    account: &str,
    scopes: &str,
    expires_days: Option<u32>,
    name: &str,
) -> anyhow::Result<()> {
    let scopes: Vec<&str> = scopes.split_whitespace().collect();
    if let Some(bad) = scopes.iter().find(|s| !seed::ALL_SCOPES.contains(s)) {
        bail!(
            "unknown scope {bad}; scopes are: {}",
            seed::ALL_SCOPES.join(" ")
        );
    }
    let conn = db::open(db_path).with_context(|| format!("opening {}", db_path.display()))?;
    seed::seed_if_empty(&conn)?;
    let exists: bool = conn.query_row(
        "SELECT EXISTS (SELECT 1 FROM accounts WHERE id = ?1)",
        [account],
        |r| r.get(0),
    )?;
    if !exists {
        bail!("no account {account}");
    }
    let expires_at = expires_days
        .map(|d| {
            jiff::Timestamp::now().checked_add(jiff::SignedDuration::from_hours(24 * i64::from(d)))
        })
        .transpose()?;
    println!(
        "{}",
        seed::create_token(&conn, account, name, &scopes, expires_at)?
    );
    Ok(())
}
