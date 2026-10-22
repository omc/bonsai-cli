use std::collections::BTreeMap;
use std::path::Path;

use serde::{Deserialize, Serialize};
use url::Url;

use crate::{ConfigError, Result};

pub const DEFAULT_API_URL: &str = "https://api.bonsai.io/v1";
/// The dashboard for [`DEFAULT_API_URL`]'s host, which serves the browser login,
/// the API Tokens page and each cluster's page.
pub const DEFAULT_DASHBOARD_URL: &str = "https://app.bonsai.io";
const CONFIG_FILE: &str = "config.toml";

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct Config {
    pub active_profile: Option<String>,
    #[serde(default)]
    pub profiles: BTreeMap<String, Profile>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Profile {
    pub api_url: Url,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dashboard_url: Option<Url>,
}

impl Config {
    /// A missing file is an empty config.
    pub fn load(dir: &Path) -> Result<Self> {
        let path = dir.join(CONFIG_FILE);
        match std::fs::read_to_string(&path) {
            Ok(s) => toml::from_str(&s).map_err(|source| ConfigError::Toml { path, source }),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Self::default()),
            Err(source) => Err(ConfigError::Io { path, source }),
        }
    }

    pub fn save(&self, dir: &Path) -> Result<()> {
        crate::create_private_dir(dir)?;
        let text = toml::to_string_pretty(self)?;
        crate::replace_file(dir, CONFIG_FILE, &text).map_err(|source| ConfigError::Io {
            path: dir.join(CONFIG_FILE),
            source,
        })
    }
}

/// Resolved settings for one run.
#[derive(Debug, Clone)]
pub struct Settings {
    pub profile: String,
    /// The URL this run talks to: `--api-url`, `BONSAI_API_URL`, the profile's, or
    /// the default.
    pub api_url: Url,
    /// The profile's own URL (or the default when it has none), which a stored
    /// token belongs to (see [`crate::auth::check_token_host`]).
    pub profile_api_url: Url,
    /// The profile's `dashboard_url`, or [`DEFAULT_DASHBOARD_URL`] when this run
    /// talks to [`DEFAULT_API_URL`]'s host.
    pub dashboard_url: Option<Url>,
}

fn check_api_url(url: &Url) -> Result<()> {
    if !url.username().is_empty() || url.password().is_some() {
        return Err(ConfigError::ApiUrlCredentials);
    }
    match url.scheme() {
        "https" => Ok(()),
        "http" if bonsai_api::is_loopback_host(url) => Ok(()),
        _ => Err(ConfigError::InsecureApiUrl(url.to_string())),
    }
}

fn nonempty(s: Option<String>) -> Option<String> {
    s.filter(|s| !s.is_empty())
}

/// Precedence: flag > env (`BONSAI_PROFILE`, `BONSAI_API_URL`) > config > default.
pub fn resolve_settings(
    dir: &Path,
    profile_flag: Option<&str>,
    api_url_flag: Option<&str>,
) -> Result<Settings> {
    resolve_settings_with(&|k| std::env::var(k).ok(), dir, profile_flag, api_url_flag)
}

pub fn resolve_settings_with(
    env: &dyn Fn(&str) -> Option<String>,
    dir: &Path,
    profile_flag: Option<&str>,
    api_url_flag: Option<&str>,
) -> Result<Settings> {
    let cfg = Config::load(dir)?;
    let profile = nonempty(profile_flag.map(str::to_owned))
        .or_else(|| nonempty(env("BONSAI_PROFILE")))
        .or(cfg.active_profile.clone())
        .unwrap_or_else(|| "default".to_owned());
    let stored = cfg.profiles.get(&profile);
    let override_url =
        nonempty(api_url_flag.map(str::to_owned)).or_else(|| nonempty(env("BONSAI_API_URL")));
    let profile_api_url = match stored {
        Some(p) => p.api_url.clone(),
        None => Url::parse(DEFAULT_API_URL).expect("default API URL parses"),
    };
    let api_url = match override_url {
        Some(s) => Url::parse(&s).map_err(|_| ConfigError::InvalidUrl(s))?,
        None => profile_api_url.clone(),
    };
    check_api_url(&api_url)?;
    let default_host = Url::parse(DEFAULT_API_URL).expect("default API URL parses");
    let dashboard_url = stored.and_then(|p| p.dashboard_url.clone()).or_else(|| {
        (api_url.host_str() == default_host.host_str())
            .then(|| Url::parse(DEFAULT_DASHBOARD_URL).expect("default dashboard URL parses"))
    });
    Ok(Settings {
        profile,
        api_url,
        profile_api_url,
        dashboard_url,
    })
}
