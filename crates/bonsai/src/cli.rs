//! The command tree. Every command takes the global flags.

use std::time::Duration;

use clap::{Args, Parser, Subcommand, ValueEnum};

#[derive(Debug, Parser)]
#[command(
    name = "bonsai",
    version,
    about = "Manage Bonsai search clusters from the command line",
    propagate_version = true,
    subcommand_required = true,
    arg_required_else_help = true
)]
pub struct Cli {
    #[command(flatten)]
    pub global: Global,
    #[command(subcommand)]
    pub command: Command,
}

#[derive(Debug, Args)]
pub struct Global {
    /// Print the API's JSON instead of tables and text
    #[arg(long, global = true)]
    pub json: bool,
    /// Never prompt; a missing value is an error (exit 2)
    #[arg(long, global = true)]
    pub no_input: bool,
    /// Print only data and errors, no progress or notes
    #[arg(long, short, global = true)]
    pub quiet: bool,
    /// Log requests to stderr; -vv logs more
    #[arg(short, long, action = clap::ArgAction::Count, global = true)]
    pub verbose: u8,
    /// The config profile to use
    #[arg(long, env = "BONSAI_PROFILE", global = true, value_name = "P")]
    pub profile: Option<String>,
    /// The API root, e.g. https://api.bonsai.io/v1
    #[arg(long, env = "BONSAI_API_URL", global = true, value_name = "URL")]
    pub api_url: Option<String>,
}

#[derive(Debug, Subcommand)]
pub enum Command {
    // ---- Getting started
    /// Log in with a browser or an API token
    Login(LoginArgs),
    /// Revoke the token and forget it on this machine
    Logout(LogoutArgs),
    /// Show the account, profile, API URL and where the token came from
    Whoami,
    /// Check the API, the token, the contract version and the clock
    Doctor,
    /// Show the CLI's, the contract's and the API's versions
    Version,

    // ---- Clusters
    /// List clusters (same as `cluster list`)
    Clusters(ClusterListArgs),
    /// Create, show, change and remove clusters
    #[command(subcommand)]
    Cluster(ClusterCommand),
    /// A cluster's Actions, newest first (same as `cluster history`)
    History(HistoryArgs),

    // ---- Connecting
    /// Print a cluster's connection URL, password included
    Url(UrlArgs),

    // ---- Watching a cluster
    /// Docs, shards and size against the plan's limits, over time
    Usage(UsageArgs),
    /// Request rates, errors, traffic and latency over time
    Metrics(MetricsArgs),
    /// The most frequent search terms
    Queries(QueriesArgs),

    // ---- Actions
    /// Look at Actions
    #[command(subcommand)]
    Action(ActionCommand),
    /// Wait for an Action again, after --no-wait, Ctrl-C or a timeout
    Wait(WaitArgs),

    // ---- Catalogue
    /// Plans this account may use
    Plans(PlansArgs),
    /// Regions open for new clusters
    Regions(WideArgs),
    /// The account's private spaces
    Spaces(WideArgs),
    /// Releases this account may use
    Releases(ReleasesArgs),

    // ---- Escape hatch and utilities
    /// Send an authenticated request to /v1 and print the JSON
    Api(ApiArgs),
    /// Print a shell completion script
    Completion(CompletionArgs),
}

#[derive(Debug, Args)]
pub struct LoginArgs {
    /// Log in through the browser
    #[arg(long, conflicts_with = "with_token")]
    pub web: bool,
    /// Read an API token from stdin
    #[arg(long)]
    pub with_token: bool,
    /// Print the login link instead of opening it
    #[arg(long)]
    pub no_browser: bool,
    /// Without a keychain, keep the token in credentials.toml (mode 0600)
    #[arg(long)]
    pub store_file: bool,
}

#[derive(Debug, Args)]
pub struct LogoutArgs {
    /// Leave the token valid on Bonsai; only forget it here
    #[arg(long)]
    pub keep_token: bool,
}

/// A cluster's ID or name.
#[derive(Debug, Args)]
pub struct ClusterArg {
    /// The cluster's ID or name
    #[arg(env = "BONSAI_CLUSTER", value_name = "CLUSTER")]
    pub cluster: String,
}

#[derive(Debug, Args)]
pub struct ClusterListArgs {
    /// Only clusters in this state
    #[arg(long, value_name = "S")]
    pub state: Option<String>,
    /// Only clusters in this region
    #[arg(long, value_name = "R")]
    pub region: Option<String>,
    /// Only clusters with this name
    #[arg(long, value_name = "N")]
    pub name: Option<String>,
    /// Show every column
    #[arg(long, short)]
    pub wide: bool,
}

/// How long a command that starts an Action waits for it.
#[derive(Debug, Args)]
pub struct WaitFlags {
    /// Print the Action and return without waiting
    #[arg(long)]
    pub no_wait: bool,
    /// Stop waiting after this long (exit 9); the Action keeps running
    #[arg(long, value_name = "D", default_value = "30m", value_parser = parse_duration)]
    pub timeout: Duration,
}

#[derive(Debug, Subcommand)]
pub enum ClusterCommand {
    /// List clusters
    List(ClusterListArgs),
    /// Create a cluster and wait until it's ready
    Create(CreateArgs),
    /// A cluster's state, plan, host, usage and latest Action
    Show(ClusterArg),
    /// Rename a cluster
    Rename(RenameArgs),
    /// Change a cluster's plan
    Resize(ResizeArgs),
    /// Deprovision a cluster
    Destroy(DestroyArgs),
    /// A cluster's Actions, newest first
    History(HistoryArgs),
    /// Block until a cluster reaches a state
    Wait(ClusterWaitArgs),
}

#[derive(Debug, Args)]
pub struct CreateArgs {
    /// The new cluster's name
    pub name: String,
    /// The plan's ID
    #[arg(long, value_name = "P")]
    pub plan: Option<String>,
    /// The region's ID
    #[arg(long, value_name = "R", conflicts_with = "space")]
    pub region: Option<String>,
    /// A private space's ID, instead of a region
    #[arg(long, value_name = "S")]
    pub space: Option<String>,
    /// The release's ID (default: the first the plan offers in the region)
    #[arg(long, value_name = "V")]
    pub release: Option<String>,
    /// Leave a cluster whose create failed instead of removing it
    #[arg(long)]
    pub keep_failed: bool,
    /// Send this Idempotency-Key instead of a fresh one, for scripts that retry on
    /// their own
    #[arg(long, value_name = "KEY")]
    pub idempotency_key: Option<String>,
    #[command(flatten)]
    pub wait: WaitFlags,
}

#[derive(Debug, Args)]
pub struct RenameArgs {
    /// The cluster's ID or name
    pub cluster: String,
    /// The new name
    pub new_name: String,
}

#[derive(Debug, Args)]
pub struct ResizeArgs {
    /// The cluster's ID or name
    pub cluster: String,
    /// The new plan's ID
    pub plan: String,
    /// Send this Idempotency-Key instead of a fresh one, for scripts that retry on
    /// their own
    #[arg(long, value_name = "KEY")]
    pub idempotency_key: Option<String>,
    #[command(flatten)]
    pub wait: WaitFlags,
}

#[derive(Debug, Args)]
pub struct DestroyArgs {
    #[command(flatten)]
    pub cluster: ClusterArg,
    /// The cluster's current name; needed when prompts are off
    #[arg(long, value_name = "NAME")]
    pub confirm: Option<String>,
    /// Send this Idempotency-Key instead of a fresh one, for scripts that retry on
    /// their own
    #[arg(long, value_name = "KEY")]
    pub idempotency_key: Option<String>,
    #[command(flatten)]
    pub wait: WaitFlags,
}

#[derive(Debug, Args)]
pub struct HistoryArgs {
    #[command(flatten)]
    pub cluster: ClusterArg,
    /// At most this many Actions (fetched in pages of up to 100)
    #[arg(long, value_name = "N", value_parser = clap::value_parser!(u32).range(1..))]
    pub limit: Option<u32>,
    /// Show each Action's steps
    #[arg(long)]
    pub steps: bool,
    /// Show every column
    #[arg(long, short)]
    pub wide: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum WaitFor {
    Ready,
}

#[derive(Debug, Args)]
pub struct ClusterWaitArgs {
    #[command(flatten)]
    pub cluster: ClusterArg,
    /// The state to wait for
    #[arg(long = "for", value_enum, default_value = "ready")]
    pub for_state: WaitFor,
    /// Give up after this long (exit 9)
    #[arg(long, value_name = "D", default_value = "30m", value_parser = parse_duration)]
    pub timeout: Duration,
}

#[derive(Debug, Args)]
pub struct UrlArgs {
    #[command(flatten)]
    pub cluster: ClusterArg,
    /// The credential's ID (default: the cluster's only rw credential)
    #[arg(long, value_name = "ID")]
    pub credential: Option<String>,
    /// Print NAME=<url>, for .env files or eval
    #[arg(long, value_name = "NAME")]
    pub env: Option<String>,
}

#[derive(Debug, Args)]
pub struct UsageArgs {
    #[command(flatten)]
    pub cluster: ClusterArg,
    /// How far back to look, at most 90 days
    #[arg(long, value_name = "D", default_value = "7d", value_parser = parse_duration)]
    pub since: Duration,
}

#[derive(Debug, Args)]
pub struct MetricsArgs {
    #[command(flatten)]
    pub cluster: ClusterArg,
    /// How far back to look, at most 30 days
    #[arg(long, value_name = "D", default_value = "1h", value_parser = parse_duration)]
    pub since: Duration,
    /// The bucket size, e.g. 1m, 1h
    #[arg(long, value_name = "R")]
    pub resolution: Option<String>,
}

#[derive(Debug, Args)]
pub struct QueriesArgs {
    #[command(flatten)]
    pub cluster: ClusterArg,
    /// The window the API counts over
    #[arg(long, value_name = "W", default_value = "7d")]
    pub window: String,
    /// Show this many terms (default 20; under --json, every term unless given)
    #[arg(long, value_name = "N")]
    pub top: Option<usize>,
    /// Show every column
    #[arg(long, short)]
    pub wide: bool,
}

#[derive(Debug, Subcommand)]
pub enum ActionCommand {
    /// An Action's type, state, subject, steps, error and result
    Show(ActionArg),
}

#[derive(Debug, Args)]
pub struct ActionArg {
    /// The Action's ID
    pub action_id: String,
}

#[derive(Debug, Args)]
pub struct WaitArgs {
    /// The Action's ID
    pub action_id: String,
    /// Give up after this long (exit 9); the Action keeps running
    #[arg(long, value_name = "D", default_value = "30m", value_parser = parse_duration)]
    pub timeout: Duration,
}

#[derive(Debug, Args)]
pub struct PlansArgs {
    /// Only plans offered in this region
    #[arg(long, value_name = "R")]
    pub region: Option<String>,
    /// Only plans offering this release (in --region, with both)
    #[arg(long, value_name = "V")]
    pub release: Option<String>,
    /// Show every column
    #[arg(long, short)]
    pub wide: bool,
}

/// A list's flags when it has no filters.
#[derive(Debug, Args)]
pub struct WideArgs {
    /// Show every column
    #[arg(long, short)]
    pub wide: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum EngineArg {
    Elasticsearch,
    Opensearch,
}

#[derive(Debug, Args)]
pub struct ReleasesArgs {
    /// Only this engine's releases
    #[arg(long, value_enum)]
    pub engine: Option<EngineArg>,
    /// Show every column
    #[arg(long, short)]
    pub wide: bool,
}

#[derive(Debug, Args)]
pub struct ApiArgs {
    /// The HTTP method, e.g. GET
    pub method: String,
    /// The path under /v1, e.g. /clusters?limit=5
    pub path: String,
    /// The JSON body; @FILE reads a file, - reads stdin
    #[arg(short = 'd', long = "data", value_name = "JSON")]
    pub data: Option<String>,
    /// An extra header, 'Name: value'; repeatable
    #[arg(short = 'H', long = "header", value_name = "NAME: VALUE")]
    pub headers: Vec<String>,
    /// Follow next_cursor and print every page's data as one array
    #[arg(long)]
    pub paginate: bool,
}

#[derive(Debug, Args)]
pub struct CompletionArgs {
    /// The shell
    #[arg(value_enum)]
    pub shell: clap_complete::Shell,
}

/// A duration such as `30m`, `7d`, `1h30m` or ISO 8601's `PT90S`. Days are 24 hours.
pub fn parse_duration(s: &str) -> Result<Duration, String> {
    let span: jiff::Span = s
        .parse()
        .map_err(|e| format!("{s:?} isn't a duration such as 30s, 10m or 7d: {e}"))?;
    let d = span
        .to_duration(jiff::SpanRelativeTo::days_are_24_hours())
        .map_err(|e| format!("{s:?}: {e}"))?;
    Duration::try_from(d).map_err(|_| format!("{s:?} is negative"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::CommandFactory;

    #[test]
    fn the_tree_is_valid() {
        Cli::command().debug_assert();
    }

    #[test]
    fn durations() {
        assert_eq!(parse_duration("30m"), Ok(Duration::from_secs(1800)));
        assert_eq!(parse_duration("7d"), Ok(Duration::from_secs(7 * 86_400)));
        assert_eq!(parse_duration("1h30m"), Ok(Duration::from_secs(5400)));
        assert!(parse_duration("soon").is_err());
        assert!(parse_duration("-5m").is_err());
    }

    #[test]
    fn globals_go_anywhere() {
        let cli = Cli::try_parse_from(["bonsai", "whoami", "--json", "-vv"]).unwrap();
        assert!(cli.global.json);
        assert_eq!(cli.global.verbose, 2);
    }

    #[test]
    fn clusters_takes_list_flags() {
        let cli = Cli::try_parse_from(["bonsai", "clusters", "--state", "ready"]).unwrap();
        assert!(matches!(cli.command, Command::Clusters(a) if a.state.as_deref() == Some("ready")));
    }

    #[test]
    fn history_is_also_top_level() {
        let cli =
            Cli::try_parse_from(["bonsai", "history", "c", "--limit", "5", "--steps"]).unwrap();
        assert!(matches!(cli.command, Command::History(a) if a.limit == Some(5) && a.steps));
    }

    #[test]
    fn queries_top_is_none_unless_given() {
        let top = |args: &[&str]| match Cli::try_parse_from(args).unwrap().command {
            Command::Queries(q) => q.top,
            _ => unreachable!(),
        };
        assert_eq!(top(&["bonsai", "queries", "c"]), None);
        assert_eq!(top(&["bonsai", "queries", "c", "--top", "5"]), Some(5));
    }

    #[test]
    fn list_commands_take_wide() {
        let wide = |args: &[&str]| match Cli::try_parse_from(args).unwrap().command {
            Command::Clusters(a) | Command::Cluster(ClusterCommand::List(a)) => a.wide,
            Command::History(a) | Command::Cluster(ClusterCommand::History(a)) => a.wide,
            Command::Queries(a) => a.wide,
            Command::Plans(a) => a.wide,
            Command::Releases(a) => a.wide,
            Command::Regions(a) | Command::Spaces(a) => a.wide,
            _ => unreachable!(),
        };
        for cmd in [
            &["clusters"][..],
            &["cluster", "list"],
            &["history", "c"],
            &["cluster", "history", "c"],
            &["queries", "c"],
            &["plans"],
            &["releases"],
            &["regions"],
            &["spaces"],
        ] {
            let mut args = vec!["bonsai"];
            args.extend(cmd);
            assert!(!wide(&args), "{cmd:?}");
            for flag in ["--wide", "-w"] {
                let mut a = args.clone();
                a.push(flag);
                assert!(wide(&a), "{cmd:?} {flag}");
            }
        }
    }

    #[test]
    fn web_and_with_token_conflict() {
        assert!(Cli::try_parse_from(["bonsai", "login", "--web", "--with-token"]).is_err());
    }
}
