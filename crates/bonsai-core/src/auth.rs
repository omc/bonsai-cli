use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use secrecy::{ExposeSecret, SecretString};
use serde::{Deserialize, Serialize};

use crate::{ConfigError, Result};

const SERVICE: &str = "bonsai-cli";
const CREDENTIALS_FILE: &str = "credentials.toml";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TokenSource {
    Env,
    Keychain,
    File,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StoreChoice {
    Keychain,
    File,
}

pub trait TokenStore {
    fn get(&self, profile: &str) -> Result<Option<SecretString>>;
    fn set(&self, profile: &str, token: &SecretString) -> Result<()>;
    fn delete(&self, profile: &str) -> Result<()>;
    fn name(&self) -> &'static str;
}

/// OS keychain: service `bonsai-cli`, account = profile. Only usable after
/// `keyring_available()` returned true (it installs the default store).
pub struct KeyringStore;

impl KeyringStore {
    fn entry(profile: &str) -> Result<keyring_core::Entry> {
        Ok(keyring_core::Entry::new(SERVICE, profile)?)
    }
}

impl TokenStore for KeyringStore {
    fn get(&self, profile: &str) -> Result<Option<SecretString>> {
        match Self::entry(profile)?.get_password() {
            Ok(p) => Ok(Some(SecretString::from(p))),
            Err(keyring_core::Error::NoEntry) => Ok(None),
            Err(e) => Err(e.into()),
        }
    }

    fn set(&self, profile: &str, token: &SecretString) -> Result<()> {
        Ok(Self::entry(profile)?.set_password(token.expose_secret())?)
    }

    fn delete(&self, profile: &str) -> Result<()> {
        match Self::entry(profile)?.delete_credential() {
            Ok(()) | Err(keyring_core::Error::NoEntry) => Ok(()),
            Err(e) => Err(e.into()),
        }
    }

    fn name(&self) -> &'static str {
        "keychain"
    }
}

#[derive(Default, Serialize, Deserialize)]
struct CredentialsFile {
    #[serde(default)]
    profiles: BTreeMap<String, CredentialsEntry>,
}

#[derive(Serialize, Deserialize)]
struct CredentialsEntry {
    token: String,
}

/// `credentials.toml` in `dir`, mode 0600 on unix.
pub struct FileStore {
    pub dir: PathBuf,
}

impl FileStore {
    pub fn new(dir: impl Into<PathBuf>) -> Self {
        Self { dir: dir.into() }
    }

    fn path(&self) -> PathBuf {
        self.dir.join(CREDENTIALS_FILE)
    }

    fn io(&self, source: std::io::Error) -> ConfigError {
        ConfigError::Io {
            path: self.path(),
            source,
        }
    }

    /// Refuse a symlinked `credentials.toml`.
    fn refuse_symlink(&self) -> Result<()> {
        match std::fs::symlink_metadata(self.path()) {
            Ok(m) if m.file_type().is_symlink() => Err(self.io(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "is a symlink; refusing to use it",
            ))),
            _ => Ok(()),
        }
    }

    /// Reads the file through one handle opened with `O_NOFOLLOW` on unix, so a
    /// symlink swapped in after [`FileStore::refuse_symlink`] is refused too, and
    /// checks and tightens that same handle.
    fn read(&self) -> Result<CredentialsFile> {
        use std::io::Read;
        let path = self.path();
        self.refuse_symlink()?;
        let mut opts = std::fs::OpenOptions::new();
        opts.read(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            opts.custom_flags(libc::O_NOFOLLOW);
        }
        let mut f = match opts.open(&path) {
            Ok(f) => f,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                return Ok(CredentialsFile::default());
            }
            Err(e) => return Err(self.io(e)),
        };
        let meta = f.metadata().map_err(|e| self.io(e))?;
        if !meta.is_file() {
            return Err(self.io(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "isn't a regular file; refusing to use it",
            )));
        }
        tighten_handle(&f, &meta, &path).map_err(|e| self.io(e))?;
        let mut s = String::new();
        f.read_to_string(&mut s).map_err(|e| self.io(e))?;
        toml::from_str(&s).map_err(|e| {
            let (line, column) = e.span().map(|r| line_col(&s, r.start)).unwrap_or((0, 0));
            ConfigError::SecretFileParse { path, line, column }
        })
    }

    /// Write a 0600 temp file in the same directory, fsync it, then rename it
    /// over `credentials.toml`, so a failed write never truncates the old file.
    fn write(&self, creds: &CredentialsFile) -> Result<()> {
        crate::create_private_dir(&self.dir)?;
        self.refuse_symlink()?;
        let path = self.path();
        let text = toml::to_string_pretty(creds)?;
        // Warn about (and fix) a loose existing file; the rename replaces it anyway.
        let _ = tighten_permissions(&path);
        crate::replace_file(&self.dir, CREDENTIALS_FILE, &text).map_err(|e| self.io(e))?;
        Ok(())
    }
}

fn line_col(s: &str, offset: usize) -> (usize, usize) {
    let before = &s[..offset.min(s.len())];
    let line = before.matches('\n').count() + 1;
    let column = before.rsplit('\n').next().map_or(0, |l| l.chars().count()) + 1;
    (line, column)
}

/// [`tighten_permissions`] on an open file.
fn tighten_handle(f: &std::fs::File, meta: &std::fs::Metadata, path: &Path) -> std::io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = meta.permissions().mode();
        if mode & 0o077 != 0 {
            tracing::warn!(
                "{} had mode {:o}; tightening to 600",
                path.display(),
                mode & 0o777
            );
            f.set_permissions(std::fs::Permissions::from_mode(0o600))?;
        }
    }
    #[cfg(not(unix))]
    let _ = (f, meta, path);
    Ok(())
}

/// If `path` exists with group/other permissions on unix, chmod it to 0600 and
/// warn. An error means the file is loose and could not be fixed.
fn tighten_permissions(path: &Path) -> std::io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if let Ok(meta) = std::fs::metadata(path) {
            let mode = meta.permissions().mode();
            if mode & 0o077 != 0 {
                tracing::warn!(
                    "{} had mode {:o}; tightening to 600",
                    path.display(),
                    mode & 0o777
                );
                std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
            }
        }
    }
    #[cfg(not(unix))]
    let _ = path;
    Ok(())
}

impl TokenStore for FileStore {
    fn get(&self, profile: &str) -> Result<Option<SecretString>> {
        Ok(self
            .read()?
            .profiles
            .remove(profile)
            .map(|e| SecretString::from(e.token)))
    }

    fn set(&self, profile: &str, token: &SecretString) -> Result<()> {
        let mut creds = self.read()?;
        creds.profiles.insert(
            profile.to_owned(),
            CredentialsEntry {
                token: token.expose_secret().to_owned(),
            },
        );
        self.write(&creds)
    }

    fn delete(&self, profile: &str) -> Result<()> {
        let mut creds = self.read()?;
        if creds.profiles.remove(profile).is_some() {
            self.write(&creds)?;
        }
        Ok(())
    }

    fn name(&self) -> &'static str {
        "file"
    }
}

/// Installs the platform's default keyring store, if the platform has one.
fn init_platform_store() -> keyring_core::Result<()> {
    #[cfg(target_os = "linux")]
    {
        keyring_core::set_default_store(zbus_secret_service_keyring_store::Store::new()?);
        Ok(())
    }
    #[cfg(target_os = "macos")]
    {
        keyring_core::set_default_store(apple_native_keyring_store::keychain::Store::new()?);
        Ok(())
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    {
        Err(keyring_core::Error::NotSupportedByStore(
            "no keychain on this platform".into(),
        ))
    }
}

/// Initialise the store and read a non-existent entry; only `Ok` or `NoEntry`
/// counts as available. Any platform error means unavailable.
fn probe() -> bool {
    if init_platform_store().is_err() {
        return false;
    }
    let result =
        keyring_core::Entry::new(SERVICE, "__bonsai_probe__").and_then(|e| e.get_password());
    matches!(result, Ok(_) | Err(keyring_core::Error::NoEntry))
}

/// False if `BONSAI_KEYRING` is `off` (or `0`, `false`, `no`) or the store can't be
/// initialised (e.g. no Secret Service). The probe runs once per process.
///
/// The first call without `BONSAI_KEYRING=off` touches the real keychain: a
/// D-Bus connection on Linux, possibly an unlock prompt on macOS.
pub fn keyring_available() -> bool {
    keyring_available_with(&|k| std::env::var(k).ok())
}

/// Whether `BONSAI_KEYRING` turns the keychain off: `off`, `0`, `false` or `no`, in
/// any case.
pub fn keyring_disabled(env: &dyn Fn(&str) -> Option<String>) -> bool {
    env("BONSAI_KEYRING").is_some_and(|v| {
        let v = v.trim();
        ["off", "0", "false", "no"]
            .iter()
            .any(|off| v.eq_ignore_ascii_case(off))
    })
}

pub fn keyring_available_with(env: &dyn Fn(&str) -> Option<String>) -> bool {
    if keyring_disabled(env) {
        return false;
    }
    static PROBE: OnceLock<bool> = OnceLock::new();
    *PROBE.get_or_init(probe)
}

/// `BONSAI_TOKEN` > keychain (if available) > `credentials.toml`.
///
/// With `BONSAI_TOKEN` set, no keychain is constructed or probed.
pub fn resolve_token(profile: &str, dir: &Path) -> Result<Option<(SecretString, TokenSource)>> {
    let env = |k: &str| std::env::var(k).ok();
    resolve_token_with(
        &env,
        &|| keyring_available_with(&env).then_some(&KeyringStore as &dyn TokenStore),
        &FileStore::new(dir),
        profile,
    )
}

/// `keyring` is called only when `BONSAI_TOKEN` is unset. A keychain read error
/// propagates: it does not silently fall back to `credentials.toml`.
pub fn resolve_token_with<'a>(
    env: &dyn Fn(&str) -> Option<String>,
    keyring: &dyn Fn() -> Option<&'a dyn TokenStore>,
    file: &dyn TokenStore,
    profile: &str,
) -> Result<Option<(SecretString, TokenSource)>> {
    if let Some(t) = env("BONSAI_TOKEN").filter(|t| !t.is_empty()) {
        return Ok(Some((SecretString::from(t), TokenSource::Env)));
    }
    if let Some(k) = keyring()
        && let Some(t) = k.get(profile)?
    {
        return Ok(Some((t, TokenSource::Keychain)));
    }
    Ok(file.get(profile)?.map(|t| (t, TokenSource::File)))
}

/// `host:port` of `url`, with the scheme's default port when none is given.
fn host_port(url: &url::Url) -> String {
    let host = url.host_str().unwrap_or_default();
    match url.port_or_known_default() {
        Some(port) => format!("{host}:{port}"),
        None => host.to_owned(),
    }
}

/// Refuses to send a token from the keychain or `credentials.toml` to a host other
/// than its profile's: an `--api-url` or `BONSAI_API_URL` on another
/// host or port would otherwise get the profile's token. `BONSAI_TOKEN` is the
/// caller's own choice, so it goes wherever it's pointed.
pub fn check_token_host(settings: &crate::config::Settings, source: TokenSource) -> Result<()> {
    if source == TokenSource::Env {
        return Ok(());
    }
    let (owner, target) = (
        host_port(&settings.profile_api_url),
        host_port(&settings.api_url),
    );
    if owner.eq_ignore_ascii_case(&target) {
        return Ok(());
    }
    Err(ConfigError::TokenHostMismatch {
        profile: settings.profile.clone(),
        token_host: owner,
        api_host: target,
    })
}

/// Keychain if available; else the file only when `allow_file`, else `NoKeychain`.
pub fn store_token(
    profile: &str,
    dir: &Path,
    token: &SecretString,
    allow_file: bool,
) -> Result<StoreChoice> {
    let keyring = keyring_available().then_some(KeyringStore);
    store_token_with(
        keyring.as_ref().map(|k| k as &dyn TokenStore),
        &FileStore::new(dir),
        profile,
        token,
        allow_file,
    )
}

pub fn store_token_with(
    keyring: Option<&dyn TokenStore>,
    file: &dyn TokenStore,
    profile: &str,
    token: &SecretString,
    allow_file: bool,
) -> Result<StoreChoice> {
    if let Some(k) = keyring {
        k.set(profile, token)?;
        return Ok(StoreChoice::Keychain);
    }
    if allow_file {
        // Debug, not warn: the caller gets `StoreChoice::File` and tells the user,
        // with the file's path.
        tracing::debug!("storing the token in plain text in credentials.toml (mode 0600)");
        file.set(profile, token)?;
        return Ok(StoreChoice::File);
    }
    Err(ConfigError::NoKeychain)
}

/// A token stored by [`replace_token`], with what it replaced in that store, so
/// the login can be undone. `Debug` doesn't show the token.
pub struct Replaced {
    pub choice: StoreChoice,
    previous: Option<SecretString>,
}

/// What [`undo_replace`] did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Undone {
    /// The earlier token is back.
    Restored,
    /// There was none: the new one was removed. `still_in` names the other store
    /// when it holds a login for the profile, which is then the one in use.
    Removed { still_in: Option<StoreChoice> },
}

/// [`store_token`]. With `keep_previous` (the login may need undoing), it first
/// reads what the chosen store held for `profile`; without, it reads nothing, so
/// an unreadable entry can't block a plain re-login.
pub fn replace_token(
    profile: &str,
    dir: &Path,
    token: &SecretString,
    allow_file: bool,
    keep_previous: bool,
) -> Result<Replaced> {
    let keyring = keyring_available().then_some(KeyringStore);
    replace_token_with(
        keyring.as_ref().map(|k| k as &dyn TokenStore),
        &FileStore::new(dir),
        profile,
        token,
        allow_file,
        keep_previous,
    )
}

pub fn replace_token_with(
    keyring: Option<&dyn TokenStore>,
    file: &dyn TokenStore,
    profile: &str,
    token: &SecretString,
    allow_file: bool,
    keep_previous: bool,
) -> Result<Replaced> {
    // The store `store_token_with` will pick, read before it writes.
    let previous = match (keep_previous, keyring, allow_file) {
        (false, _, _) => None,
        (true, Some(k), _) => k.get(profile)?,
        (true, None, true) => file.get(profile)?,
        (true, None, false) => None,
    };
    let choice = store_token_with(keyring, file, profile, token, allow_file)?;
    Ok(Replaced { choice, previous })
}

/// Undoes [`replace_token`] (made with `keep_previous`): puts the earlier token
/// back, or removes the new one when there was none, and then says whether the
/// other store still holds a login for the profile.
pub fn undo_replace(profile: &str, dir: &Path, replaced: Replaced) -> Result<Undone> {
    let keyring = keyring_available().then_some(KeyringStore);
    undo_replace_with(
        keyring.as_ref().map(|k| k as &dyn TokenStore),
        &FileStore::new(dir),
        profile,
        replaced,
    )
}

pub fn undo_replace_with(
    keyring: Option<&dyn TokenStore>,
    file: &dyn TokenStore,
    profile: &str,
    replaced: Replaced,
) -> Result<Undone> {
    let store = match (replaced.choice, keyring) {
        (StoreChoice::Keychain, Some(k)) => k,
        (StoreChoice::Keychain, None) => return Err(ConfigError::NoKeychain),
        (StoreChoice::File, _) => file,
    };
    match &replaced.previous {
        Some(old) => {
            store.set(profile, old)?;
            Ok(Undone::Restored)
        }
        None => {
            store.delete(profile)?;
            // A read error there counts as nothing found: the message is advice.
            let (other, name) = match replaced.choice {
                StoreChoice::Keychain => (Some(file), StoreChoice::File),
                StoreChoice::File => (keyring, StoreChoice::Keychain),
            };
            let still_in = other
                .and_then(|o| o.get(profile).ok().flatten())
                .map(|_| name);
            Ok(Undone::Removed { still_in })
        }
    }
}
