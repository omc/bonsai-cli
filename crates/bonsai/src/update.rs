//! The update notice: at most once a day, ask GitHub for the latest release and,
//! when it's newer, say how to update for the way `bonsai` was installed.

use std::io::IsTerminal;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::Result;
use jiff::{SignedDuration, Timestamp};

/// The latest published release.
pub const LATEST_URL: &str = "https://api.github.com/repos/omc/bonsai-cli/releases/latest";
const RELEASES_URL: &str = "https://github.com/omc/bonsai-cli/releases/latest";
/// What the state file is called, in the config dir.
const STATE_FILE: &str = "update-check.json";
/// What dist's shell installer leaves in the config dir.
const RECEIPT_FILE: &str = "bonsai-receipt.json";
const EVERY: SignedDuration = SignedDuration::from_hours(24);
/// The most a command waits, after it's done, for a check still running.
pub const WAIT: Duration = Duration::from_secs(2);

/// Whether to check at all: only when the notice can be seen, so not under
/// `--quiet`, in CI, with stderr redirected, or when `BONSAI_NO_UPDATE_NOTIFIER`
/// is set.
pub fn wanted(quiet: bool) -> bool {
    !quiet
        && std::io::stderr().is_terminal()
        && !crate::interactive::is_ci(std::env::var("CI").ok().as_deref())
        && !opted_out(std::env::var("BONSAI_NO_UPDATE_NOTIFIER").ok().as_deref())
}

/// The notice to print, if a check was due and found a newer release. The check
/// is recorded before the request, so a failing one isn't retried until tomorrow.
pub async fn check(config_dir: PathBuf) -> Option<String> {
    let now = Timestamp::now();
    if !due(last_check(&config_dir), now) {
        return None;
    }
    if let Err(e) = record_check(&config_dir, now) {
        tracing::debug!(error = %e, "can't record the update check");
    }
    let client = bonsai_api::http_client()
        .user_agent(crate::commands::user_agent())
        .timeout(WAIT)
        .build()
        .ok()?;
    let latest = latest(&client, LATEST_URL)
        .await
        .inspect_err(|e| tracing::debug!(error = %e, "update check failed"))
        .ok()?;
    let exe = std::env::current_exe()
        .and_then(std::fs::canonicalize)
        .ok()?;
    let receipt = config_dir.join(RECEIPT_FILE).exists();
    notice(env!("CARGO_PKG_VERSION"), &latest, &channel(&exe, receipt))
}

/// How this copy of `bonsai` was installed, from where the binary is.
#[derive(Debug, PartialEq, Eq)]
pub enum Channel {
    Homebrew,
    Npm,
    Nix,
    /// dist's shell installer, which leaves a receipt and `bonsai-update`.
    Installer,
    Unknown,
}

pub fn channel(exe: &Path, receipt: bool) -> Channel {
    let has = |part: &str| exe.components().any(|c| c.as_os_str() == part);
    if has("Cellar") {
        Channel::Homebrew
    } else if has("node_modules") {
        Channel::Npm
    } else if exe.starts_with("/nix/store") {
        Channel::Nix
    } else if receipt {
        Channel::Installer
    } else {
        Channel::Unknown
    }
}

/// The line to print when `latest` is newer than `current`. Nix installs get
/// none: their version is pinned by the flake.
pub fn notice(current: &str, latest: &str, channel: &Channel) -> Option<String> {
    if semver::Version::parse(latest).ok()? <= semver::Version::parse(current).ok()? {
        return None;
    }
    let how = match channel {
        Channel::Nix => return None,
        Channel::Homebrew => "brew upgrade bonsai",
        Channel::Npm => "npm install -g @omc/bonsai",
        Channel::Installer => "bonsai-update",
        Channel::Unknown => RELEASES_URL,
    };
    Some(format!(
        "bonsai {latest} is out (you have {current}). Update with: {how}"
    ))
}

/// A check is due a day after the last one, or when the clock has gone back.
pub fn due(last: Option<Timestamp>, now: Timestamp) -> bool {
    last.is_none_or(|last| last > now || now.duration_since(last) >= EVERY)
}

/// `BONSAI_NO_UPDATE_NOTIFIER` set to anything but empty turns the check off.
pub fn opted_out(v: Option<&str>) -> bool {
    v.is_some_and(|v| !v.is_empty())
}

/// When the last check ran, from the state file; `None` if there's no readable one.
pub fn last_check(config_dir: &Path) -> Option<Timestamp> {
    let text = std::fs::read_to_string(config_dir.join(STATE_FILE)).ok()?;
    let state: State = serde_json::from_str(&text).ok()?;
    Some(state.checked_at)
}

/// The check often runs before anything else has made the config folder, so it
/// makes it the way the credentials file needs it: private to the user.
pub fn record_check(config_dir: &Path, at: Timestamp) -> std::io::Result<()> {
    bonsai_core::create_private_dir(config_dir).map_err(std::io::Error::other)?;
    let text = serde_json::to_string(&State { checked_at: at })?;
    bonsai_core::replace_file(config_dir, STATE_FILE, &text)
}

#[derive(serde::Serialize, serde::Deserialize)]
struct State {
    checked_at: Timestamp,
}

/// The latest release's version, without the tag's `v`.
pub async fn latest(client: &reqwest::Client, url: &str) -> Result<String> {
    #[derive(serde::Deserialize)]
    struct Release {
        tag_name: String,
    }
    let release: Release = client
        .get(url)
        .header("accept", "application/vnd.github+json")
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;
    Ok(release
        .tag_name
        .strip_prefix('v')
        .unwrap_or(&release.tag_name)
        .to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn homebrew_is_found_by_the_cellar() {
        for p in [
            "/opt/homebrew/Cellar/bonsai/0.1.0/bin/bonsai",
            "/usr/local/Cellar/bonsai/0.1.0/bin/bonsai",
            "/home/linuxbrew/.linuxbrew/Cellar/bonsai/0.1.0/bin/bonsai",
        ] {
            assert_eq!(channel(Path::new(p), true), Channel::Homebrew, "{p}");
        }
    }

    #[test]
    fn npm_and_nix_are_found_by_their_paths() {
        let npm = "/usr/lib/node_modules/@omc/bonsai/node_modules/.bin_real/bonsai";
        assert_eq!(channel(Path::new(npm), false), Channel::Npm);
        let nix = "/nix/store/v4ncjdw42rv5hjkzq339bnkbffil6svl-bonsai-0.1.0/bin/bonsai";
        assert_eq!(channel(Path::new(nix), true), Channel::Nix);
    }

    #[test]
    fn the_receipt_means_the_shell_installer() {
        let p = Path::new("/home/me/.cargo/bin/bonsai");
        assert_eq!(channel(p, true), Channel::Installer);
        assert_eq!(channel(p, false), Channel::Unknown);
    }

    #[test]
    fn a_newer_release_names_the_command_for_the_channel() {
        let line = notice("0.1.0", "0.2.0", &Channel::Homebrew).unwrap();
        assert_eq!(
            line,
            "bonsai 0.2.0 is out (you have 0.1.0). Update with: brew upgrade bonsai"
        );
        let npm = notice("0.1.0", "0.1.1", &Channel::Npm).unwrap();
        assert!(npm.ends_with("npm install -g @omc/bonsai"), "{npm}");
        let installer = notice("0.1.0", "0.1.1", &Channel::Installer).unwrap();
        assert!(installer.ends_with("bonsai-update"), "{installer}");
        let unknown = notice("0.1.0", "0.1.1", &Channel::Unknown).unwrap();
        assert!(unknown.ends_with(RELEASES_URL), "{unknown}");
    }

    #[test]
    fn no_notice_unless_newer() {
        assert_eq!(notice("0.2.0", "0.2.0", &Channel::Homebrew), None);
        assert_eq!(notice("0.2.0", "0.1.9", &Channel::Homebrew), None);
        assert_eq!(notice("0.2.0", "0.10.0", &Channel::Nix), None);
        assert_eq!(notice("0.2.0", "not a version", &Channel::Homebrew), None);
    }

    #[test]
    fn a_check_is_due_after_a_day() {
        let now: Timestamp = "2026-10-20T12:00:00Z".parse().unwrap();
        assert!(due(None, now));
        assert!(!due(Some("2026-10-19T13:00:00Z".parse().unwrap()), now));
        assert!(due(Some("2026-10-19T12:00:00Z".parse().unwrap()), now));
        // The clock went back.
        assert!(due(Some("2026-10-21T12:00:00Z".parse().unwrap()), now));
    }

    #[test]
    fn opting_out_is_any_value_but_empty() {
        assert!(opted_out(Some("1")));
        assert!(!opted_out(Some("")));
        assert!(!opted_out(None));
    }

    #[cfg(unix)]
    #[test]
    fn recording_a_check_leaves_the_config_folder_private() {
        use std::os::unix::fs::PermissionsExt;
        let mode = |p: &Path| std::fs::metadata(p).unwrap().permissions().mode() & 0o777;
        let dir = tempfile::tempdir().unwrap();
        let at: Timestamp = "2026-10-20T12:00:00Z".parse().unwrap();

        let fresh = dir.path().join("fresh/bonsai");
        record_check(&fresh, at).unwrap();
        assert_eq!(mode(&fresh), 0o700);
        assert_eq!(mode(&fresh.join(STATE_FILE)), 0o600);

        // One an earlier release made with the umask's mode.
        let loose = dir.path().join("loose");
        std::fs::create_dir(&loose).unwrap();
        std::fs::set_permissions(&loose, std::fs::Permissions::from_mode(0o755)).unwrap();
        record_check(&loose, at).unwrap();
        assert_eq!(mode(&loose), 0o700);
    }

    #[test]
    fn the_last_check_round_trips_through_the_state_file() {
        let dir = tempfile::tempdir().unwrap();
        let config = dir.path().join("bonsai");
        assert_eq!(last_check(&config), None);
        let at: Timestamp = "2026-10-20T12:00:00Z".parse().unwrap();
        record_check(&config, at).unwrap();
        assert_eq!(last_check(&config), Some(at));
        std::fs::write(config.join(STATE_FILE), "not json").unwrap();
        assert_eq!(last_check(&config), None);
    }

    #[tokio::test]
    async fn latest_reads_the_tag_without_its_v() {
        use wiremock::matchers::{header_exists, method};
        use wiremock::{Mock, MockServer, ResponseTemplate};
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(header_exists("user-agent"))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(serde_json::json!({"tag_name": "v0.2.0"})),
            )
            .mount(&server)
            .await;
        let client = bonsai_api::http_client()
            .user_agent("test")
            .build()
            .unwrap();
        assert_eq!(latest(&client, &server.uri()).await.unwrap(), "0.2.0");
    }

    #[tokio::test]
    async fn latest_fails_on_an_error_status() {
        use wiremock::{Mock, MockServer, ResponseTemplate, matchers::any};
        let server = MockServer::start().await;
        Mock::given(any())
            .respond_with(ResponseTemplate::new(404))
            .mount(&server)
            .await;
        let client = bonsai_api::http_client().build().unwrap();
        assert!(latest(&client, &server.uri()).await.is_err());
    }
}
