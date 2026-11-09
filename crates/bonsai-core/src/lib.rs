//! Config, auth, flows and events for the Bonsai CLI. No terminal I/O.

pub mod auth;
pub mod config;
pub mod events;
pub mod flows;
pub mod paths;
pub mod resolve;
pub mod urlcheck;
pub mod wait;

use std::path::PathBuf;
use std::time::Duration;

use bonsai_api::ActionErrorCode;

pub use events::{Event, EventSink};
pub use flows::{
    CleanupPolicy, CreateRequest, LoginOutcome, connection_url, create_cluster, destroy_cluster,
    device_login, resize_cluster,
};
pub use resolve::resolve_cluster;
pub use wait::{WaitOptions, wait_for_action};

/// Errors from the flows: cluster lookup, waiting on Actions, creating, destroying,
/// resizing, connecting and logging in.
#[derive(Debug, thiserror::Error)]
pub enum CoreError {
    #[error(transparent)]
    Api(#[from] bonsai_api::ApiError),
    #[error("no cluster named {0:?}")]
    ClusterNotFound(String),
    #[error("{name:?} matches several clusters: {}", .ids.join(", "))]
    Ambiguous { name: String, ids: Vec<String> },
    /// Boxed, so `CoreError` stays small enough for `Result` (`clippy::result_large_err`).
    #[error("action {} failed: {}", .0.action_id, .0.message)]
    ActionFailed(Box<ActionFailure>),
    #[error("still running after {}; resume with `bonsai wait {action_id}`", human_duration(*.timeout))]
    Timeout {
        action_id: String,
        timeout: Duration,
    },
    #[error("stopped waiting; resume with `bonsai wait {action_id}`")]
    Cancelled { action_id: String },
    /// Polling the Action failed after the client's retries; the Action may still run.
    /// The message leaves `source` out: it is the error's `source()`, so a chain
    /// printer (`{:#}`) shows it once.
    #[error("couldn't check on action {action_id}; resume with `bonsai wait {action_id}`")]
    WaitFailed {
        action_id: String,
        #[source]
        source: Box<bonsai_api::ApiError>,
    },
    #[error("{0}")]
    Usage(String),
    #[error("no rw credential on {0}")]
    NoCredential(String),
    #[error("the login was denied in the browser")]
    LoginDenied,
    #[error("the login expired before it was approved; run `bonsai login` again")]
    LoginExpired,
    #[error("stopped waiting for the login to be approved")]
    LoginCancelled,
}

/// `d` as people write it: `30m`, `1h 30m`, `45s`.
pub fn human_duration(d: Duration) -> String {
    match jiff::SignedDuration::try_from(d) {
        Ok(d) => format!("{:#}", jiff::SignedDuration::from_secs(d.as_secs())),
        Err(_) => format!("{}s", d.as_secs()),
    }
}

/// An Action that ended in `error`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ActionFailure {
    pub action_id: String,
    pub code: ActionErrorCode,
    pub message: String,
    pub action_url: Option<String>,
    /// The cluster the failed create left, whether or not it was removed. Only
    /// this run's own create is ever removed: see `replayed`.
    pub cluster_id: Option<String>,
    /// Set when Bonsai had already removed the cluster, or this run tried to.
    pub cleanup: Option<Cleanup>,
    /// The create carried a caller-supplied `Idempotency-Key` and was answered
    /// `Idempotent-Replayed: true`, so an earlier run that used the key may have
    /// created the cluster: nothing was removed, whatever the cleanup policy.
    pub replayed: bool,
}

/// What became of the cluster a failed create left behind.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Cleanup {
    pub cluster_id: String,
    pub outcome: CleanupOutcome,
}

/// How a cleanup ended, with its cause.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CleanupOutcome {
    /// The deprovision this run started finished with `success`.
    Removed { deprovision_action_id: String },
    /// Bonsai had already removed it: a failed create is terminated by Bonsai
    /// itself, so the re-read found it terminated (no `DELETE` was
    /// sent), or the `DELETE` was answered `422 cluster_terminated`.
    RemovedByBonsai,
    /// The deprovision started but wasn't seen to finish: the timeout ran out,
    /// the wait was cancelled, or a poll failed. `bonsai wait <id>` follows it.
    Pending { deprovision_action_id: String },
    /// The deprovision Action ended in `error`; the cluster is still there.
    DeprovisionFailed { deprovision_action_id: String },
    /// The `DELETE` failed; the cluster is still there.
    DeleteFailed,
    /// Ctrl-C or the timeout came while the cluster was being re-read, so this
    /// run doesn't know what became of it (Bonsai has probably removed it) and
    /// removed nothing.
    Unchecked { reason: StopReason },
}

/// What cut a step of a flow short.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StopReason {
    Cancelled,
    Timeout,
}

impl CleanupOutcome {
    /// The name `--json` reports: `removed`, `removed_by_bonsai`,
    /// `deprovisioning`, `remove_failed` (the deprovision or the `DELETE`
    /// failed) or `unchecked`.
    pub fn as_str(&self) -> &'static str {
        match self {
            CleanupOutcome::Removed { .. } => "removed",
            CleanupOutcome::RemovedByBonsai => "removed_by_bonsai",
            CleanupOutcome::Pending { .. } => "deprovisioning",
            CleanupOutcome::DeprovisionFailed { .. } | CleanupOutcome::DeleteFailed => {
                "remove_failed"
            }
            CleanupOutcome::Unchecked { .. } => "unchecked",
        }
    }

    /// The deprovision Action this run started, if any.
    pub fn deprovision_action_id(&self) -> Option<&str> {
        match self {
            CleanupOutcome::Removed {
                deprovision_action_id,
            }
            | CleanupOutcome::Pending {
                deprovision_action_id,
            }
            | CleanupOutcome::DeprovisionFailed {
                deprovision_action_id,
            } => Some(deprovision_action_id),
            CleanupOutcome::RemovedByBonsai
            | CleanupOutcome::DeleteFailed
            | CleanupOutcome::Unchecked { .. } => None,
        }
    }

    /// Whether the cluster is gone.
    pub fn removed(&self) -> bool {
        matches!(
            self,
            CleanupOutcome::Removed { .. } | CleanupOutcome::RemovedByBonsai
        )
    }
}

/// Errors from config and token handling.
#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error(
        "no OS keychain is available; set BONSAI_TOKEN instead, or pass --store-file to keep the token in a 0600 credentials file"
    )]
    NoKeychain,
    // The cause is `source()`, not part of the message, so a chain printer
    // (`{:#}`) shows it once.
    #[error("{path}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("{path}")]
    Toml {
        path: PathBuf,
        #[source]
        source: toml::de::Error,
    },
    /// A parse error in a file that holds secrets: location only, never the source line.
    #[error("{path}: invalid TOML at line {line}, column {column}")]
    SecretFileParse {
        path: PathBuf,
        line: usize,
        column: usize,
    },
    #[error("could not write TOML")]
    TomlWrite(#[from] toml::ser::Error),
    #[error("invalid URL {0:?}")]
    InvalidUrl(String),
    #[error("refusing {0}: an http API URL is only allowed for localhost, 127.0.0.1 and [::1]")]
    InsecureApiUrl(String),
    #[error("no home directory to keep the config in; set XDG_CONFIG_HOME to an absolute path")]
    NoConfigDir,
    /// The URL itself is left out: it holds the password.
    #[error("the API URL can't carry a username or password")]
    ApiUrlCredentials,
    #[error(
        "the token for profile {profile} belongs to {token_host}, not {api_host}; use --profile, log in to that URL, or set BONSAI_TOKEN"
    )]
    TokenHostMismatch {
        profile: String,
        token_host: String,
        api_host: String,
    },
    /// Display text of a `keyring_core::Error` only: `BadEncoding` carries the
    /// secret's bytes in its payload, so the error itself is never stored.
    #[error("keychain error: {0}")]
    Keychain(String),
}

impl From<keyring_core::Error> for ConfigError {
    fn from(e: keyring_core::Error) -> Self {
        // keyring-core's Display for BadEncoding is a fixed message with no bytes.
        ConfigError::Keychain(e.to_string())
    }
}

pub type Result<T> = std::result::Result<T, ConfigError>;

/// Create `dir` (and parents) with mode 0700 on unix.
pub(crate) fn create_private_dir(dir: &std::path::Path) -> Result<()> {
    let mut b = std::fs::DirBuilder::new();
    b.recursive(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        b.mode(0o700);
    }
    b.create(dir).map_err(|source| ConfigError::Io {
        path: dir.to_path_buf(),
        source,
    })
}

/// Replaces `dir/name` with `text`: writes a 0600 temp file in `dir`, fsyncs it, and
/// renames it over the old file, so a failed write never truncates it. The temp
/// file is removed on error.
pub fn replace_file(dir: &std::path::Path, name: &str, text: &str) -> std::io::Result<()> {
    use std::io::Write;
    use std::sync::atomic::{AtomicU32, Ordering};
    static SEQ: AtomicU32 = AtomicU32::new(0);
    let tmp = dir.join(format!(
        ".{name}.{}.{}.tmp",
        std::process::id(),
        SEQ.fetch_add(1, Ordering::Relaxed)
    ));
    let mut opts = std::fs::OpenOptions::new();
    opts.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    let result = (|| {
        let mut f = opts.open(&tmp)?;
        f.write_all(text.as_bytes())?;
        f.sync_all()?;
        drop(f);
        std::fs::rename(&tmp, dir.join(name))
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    result
}
