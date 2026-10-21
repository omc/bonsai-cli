use std::path::{Path, PathBuf};

use etcetera::BaseStrategy;

use crate::{ConfigError, Result};

/// `$XDG_CONFIG_HOME/bonsai` if set to an absolute path, else the platform's
/// config dir (`~/.config/bonsai` on Linux and macOS).
pub fn config_dir() -> Result<PathBuf> {
    config_dir_with(std::env::var("XDG_CONFIG_HOME").ok().as_deref())
}

/// A relative `XDG_CONFIG_HOME` is ignored, as the XDG spec says, and there is no
/// fallback to the working directory: `credentials.toml` must never land in a
/// working copy that may be committed. No home directory is an error.
pub fn config_dir_with(xdg_config_home: Option<&str>) -> Result<PathBuf> {
    if let Some(x) = xdg_config_home.filter(|x| Path::new(x).is_absolute()) {
        return Ok(PathBuf::from(x).join("bonsai"));
    }
    match etcetera::choose_base_strategy() {
        Ok(s) => Ok(s.config_dir().join("bonsai")),
        Err(_) => Err(ConfigError::NoConfigDir),
    }
}
