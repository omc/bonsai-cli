//! Helpers shared by the e2e test files: a fresh mock per test, and the `bonsai`
//! binary pointed at it with a clean environment.
#![allow(dead_code)]

use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::process::{Child, ExitStatus};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use assert_cmd::Command;
use bonsai_mock::{MockHandle, MockOptions};
use serde_json::Value;
use tempfile::TempDir;

pub struct Env {
    pub mock: MockHandle,
    pub cfg: TempDir,
    pub db: TempDir,
}

impl Env {
    pub async fn start() -> Self {
        Self::start_with(false).await
    }

    /// A mock that also answers its clusters' own API, for `cluster request`.
    pub async fn start_local() -> Self {
        Self::start_with(true).await
    }

    async fn start_with(local_clusters: bool) -> Self {
        let db = tempfile::tempdir().unwrap();
        let opts = MockOptions {
            db_path: db.path().join("mock.sqlite"),
            step_ms: 0,
            public_url: None,
            checkout_url: None,
            local_clusters,
        };
        let mock = bonsai_mock::spawn(opts, "127.0.0.1:0".parse().unwrap())
            .await
            .unwrap();
        Self {
            mock,
            cfg: tempfile::tempdir().unwrap(),
            db,
        }
    }

    /// The seeded `acme` token, with every scope.
    pub fn token(&self) -> String {
        self.mock
            .seeded_tokens
            .iter()
            .find(|t| t.account_id == "acme")
            .unwrap()
            .token
            .clone()
    }

    pub fn bonsai(&self) -> Command {
        bonsai(&self.mock, self.cfg.path())
    }

    /// `bonsai` with the `acme` token in `BONSAI_TOKEN`.
    pub fn authed(&self) -> Command {
        let mut c = self.bonsai();
        c.env("BONSAI_TOKEN", self.token());
        c
    }

    pub fn credentials_file(&self) -> PathBuf {
        self.cfg.path().join("bonsai").join("credentials.toml")
    }

    pub fn config_file(&self) -> PathBuf {
        self.cfg.path().join("bonsai").join("config.toml")
    }
}

/// The binary, configured for `mock` with its config in `cfg`: no keychain, no
/// colour, non-interactive, and nothing from the developer's own environment.
pub fn bonsai(mock: &MockHandle, cfg: &Path) -> Command {
    let mut c = Command::from_std(std_bonsai(mock, cfg));
    c.timeout(Duration::from_secs(60));
    c
}

/// [`bonsai`] as a plain `std::process::Command`, for a test that talks to the
/// process while it runs.
pub fn std_bonsai(mock: &MockHandle, cfg: &Path) -> std::process::Command {
    let mut c = std::process::Command::new(assert_cmd::cargo::cargo_bin!("bonsai"));
    for var in [
        "BONSAI_TOKEN",
        "BONSAI_PROFILE",
        "BONSAI_CLUSTER",
        "BONSAI_CA_BUNDLE",
        "CLICOLOR_FORCE",
        "RUST_LOG",
        "COLUMNS",
        "BONSAI_TEST_COLUMNS",
    ] {
        c.env_remove(var);
    }
    c.env("XDG_CONFIG_HOME", cfg)
        .env("BONSAI_KEYRING", "off")
        .env("BONSAI_API_URL", mock.base_url.as_str())
        .env("NO_COLOR", "1")
        .env("CI", "1");
    c
}

pub fn stdout_json(out: &std::process::Output) -> Value {
    serde_json::from_slice(&out.stdout).unwrap_or_else(|e| {
        panic!(
            "stdout isn't JSON ({e}): {}",
            String::from_utf8_lossy(&out.stdout)
        )
    })
}

/// Each line `r` gives, from a thread, so a test can wait with a timeout.
pub fn lines(r: impl std::io::Read + Send + 'static) -> mpsc::Receiver<String> {
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        for line in BufReader::new(r).lines() {
            let Ok(line) = line else { break };
            if tx.send(line).is_err() {
                break;
            }
        }
    });
    rx
}

/// The child's exit status, or `None` (and the child killed) after `limit`.
pub fn wait_for(child: &mut Child, limit: Duration) -> Option<ExitStatus> {
    let deadline = Instant::now() + limit;
    while Instant::now() < deadline {
        if let Some(status) = child.try_wait().unwrap() {
            // Let the reader threads drain the pipes.
            std::thread::sleep(Duration::from_millis(50));
            return Some(status);
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    let _ = child.kill();
    let _ = child.wait();
    None
}

/// Every line of `out` fits `width` display columns, measured with `str::width` as
/// the table layout does.
pub fn assert_fits(out: &str, width: usize) {
    use unicode_width::UnicodeWidthStr;
    for line in out.lines() {
        assert!(
            line.width() <= width,
            "{} columns wide: {line:?}\n{out}",
            line.width()
        );
    }
}
