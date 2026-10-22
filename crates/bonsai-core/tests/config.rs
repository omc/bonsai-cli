//! No test mutates the process environment: env-dependent functions have `_with` variants.

use std::cell::RefCell;
use std::collections::HashMap;

use bonsai_core::ConfigError;
use bonsai_core::auth::*;
use bonsai_core::config::*;
use bonsai_core::paths::config_dir_with;
use bonsai_core::urlcheck::may_open;
use secrecy::{ExposeSecret, SecretString};
use url::Url;

fn env_of<'a>(pairs: &'a [(&'a str, &'a str)]) -> impl Fn(&str) -> Option<String> + 'a {
    move |k| {
        pairs
            .iter()
            .find(|(n, _)| *n == k)
            .map(|(_, v)| v.to_string())
    }
}

fn secret(s: &str) -> SecretString {
    SecretString::from(s.to_owned())
}

#[derive(Default)]
struct Fake(RefCell<HashMap<String, String>>);
impl TokenStore for Fake {
    fn get(&self, p: &str) -> bonsai_core::Result<Option<SecretString>> {
        Ok(self.0.borrow().get(p).map(|t| secret(t)))
    }
    fn set(&self, p: &str, t: &SecretString) -> bonsai_core::Result<()> {
        self.0
            .borrow_mut()
            .insert(p.into(), t.expose_secret().into());
        Ok(())
    }
    fn delete(&self, p: &str) -> bonsai_core::Result<()> {
        self.0.borrow_mut().remove(p);
        Ok(())
    }
    fn name(&self) -> &'static str {
        "fake"
    }
}

#[test]
fn config_dir_honours_xdg() {
    assert_eq!(
        config_dir_with(Some("/x/y")).unwrap(),
        std::path::PathBuf::from("/x/y/bonsai")
    );
    for xdg in [None, Some(""), Some("relative/dir"), Some("./x")] {
        let dir = config_dir_with(xdg).unwrap();
        assert!(dir.ends_with("bonsai"), "{xdg:?}: {}", dir.display());
        // A relative XDG_CONFIG_HOME is ignored, as the XDG spec says: the
        // credentials must never land in the working copy.
        assert!(dir.is_absolute(), "{xdg:?}: {}", dir.display());
    }
}

#[test]
fn config_round_trips_and_missing_is_empty() {
    let d = tempfile::tempdir().unwrap();
    assert_eq!(Config::load(d.path()).unwrap(), Config::default());
    let mut c = Config {
        active_profile: Some("staging".into()),
        ..Default::default()
    };
    c.profiles.insert(
        "staging".into(),
        Profile {
            api_url: Url::parse("https://api.staging.example/v1").unwrap(),
            dashboard_url: Some(Url::parse("https://app.staging.example").unwrap()),
        },
    );
    c.save(d.path()).unwrap();
    assert_eq!(Config::load(d.path()).unwrap(), c);
}

/// `Config::save` replaces the file by renaming a complete temp file over it, so a
/// failed write can't truncate it and lose every profile.
#[cfg(unix)]
#[test]
fn config_save_replaces_the_file_atomically() {
    use std::os::unix::fs::MetadataExt;
    let d = tempfile::tempdir().unwrap();
    let mut c = Config::default();
    c.save(d.path()).unwrap();
    let path = d.path().join("config.toml");
    let before = std::fs::metadata(&path).unwrap().ino();
    c.active_profile = Some("other".into());
    c.save(d.path()).unwrap();
    assert_ne!(
        std::fs::metadata(&path).unwrap().ino(),
        before,
        "written in place"
    );
    assert_eq!(Config::load(d.path()).unwrap(), c);
    let left: Vec<_> = std::fs::read_dir(d.path())
        .unwrap()
        .map(|e| e.unwrap().file_name())
        .collect();
    assert_eq!(left, ["config.toml"], "a temp file was left behind");
}

#[test]
fn settings_precedence() {
    let d = tempfile::tempdir().unwrap();
    let mut c = Config {
        active_profile: Some("p".into()),
        ..Default::default()
    };
    for (n, u) in [
        ("p", "https://profile.example/v1"),
        ("q", "https://q.example/v1"),
        ("e", "https://e.example/v1"),
    ] {
        c.profiles.insert(
            n.into(),
            Profile {
                api_url: Url::parse(u).unwrap(),
                dashboard_url: None,
            },
        );
    }
    c.save(d.path()).unwrap();
    let none = env_of(&[]);

    let s = resolve_settings_with(&none, d.path(), None, None).unwrap();
    assert_eq!(
        (s.profile.as_str(), s.api_url.as_str()),
        ("p", "https://profile.example/v1")
    );

    let env = env_of(&[
        ("BONSAI_PROFILE", "e"),
        ("BONSAI_API_URL", "https://env.example/v1"),
    ]);
    let s = resolve_settings_with(&env, d.path(), None, None).unwrap();
    assert_eq!(
        (s.profile.as_str(), s.api_url.as_str()),
        ("e", "https://env.example/v1")
    );

    let s =
        resolve_settings_with(&env, d.path(), Some("q"), Some("https://flag.example/v1")).unwrap();
    assert_eq!(
        (s.profile.as_str(), s.api_url.as_str()),
        ("q", "https://flag.example/v1")
    );

    // env profile selects its stored URL when no URL override is given
    let env = env_of(&[("BONSAI_PROFILE", "e")]);
    let s = resolve_settings_with(&env, d.path(), None, None).unwrap();
    assert_eq!(s.api_url.as_str(), "https://e.example/v1");
}

/// The default API's dashboard is known without a profile, so its links (the
/// browser login, `cluster open`) may be opened; another host's only when its
/// profile names one.
#[test]
fn the_default_api_knows_its_dashboard() {
    let d = tempfile::tempdir().unwrap();
    let none = env_of(&[]);

    let s = resolve_settings_with(&none, d.path(), None, None).unwrap();
    assert_eq!(
        s.dashboard_url.as_ref().map(Url::as_str),
        Some("https://app.bonsai.io/")
    );

    let s = resolve_settings_with(
        &none,
        d.path(),
        None,
        Some("https://api.staging.example/v1"),
    )
    .unwrap();
    assert_eq!(s.dashboard_url, None);
}

/// A stored token is bound to its profile's host: an `--api-url` or
/// `BONSAI_API_URL` on another host or port is refused before any request.
#[test]
fn a_stored_token_stays_on_its_profiles_host() {
    let d = tempfile::tempdir().unwrap();
    let mut c = Config::default();
    c.profiles.insert(
        "staging".into(),
        Profile {
            api_url: Url::parse("https://api.staging.example/v1").unwrap(),
            dashboard_url: None,
        },
    );
    c.save(d.path()).unwrap();
    let none = env_of(&[]);
    let settings =
        |profile, url| resolve_settings_with(&none, d.path(), Some(profile), url).unwrap();

    // The profile's own host, by default or spelled out (another path is fine).
    for url in [
        None,
        Some("https://api.staging.example/v1"),
        Some("https://api.staging.example:443/other"),
    ] {
        let s = settings("staging", url);
        assert_eq!(s.profile_api_url.as_str(), "https://api.staging.example/v1");
        for source in [TokenSource::Keychain, TokenSource::File, TokenSource::Env] {
            check_token_host(&s, source).unwrap();
        }
    }
    // A profile with no URL belongs to the default API.
    let s = settings("default", Some("https://api.bonsai.io/v1"));
    check_token_host(&s, TokenSource::File).unwrap();

    for (profile, url, owner, other) in [
        (
            "default",
            "https://api.staging.example/v1",
            "api.bonsai.io:443",
            "api.staging.example:443",
        ),
        (
            "staging",
            "https://api.bonsai.io/v1",
            "api.staging.example:443",
            "api.bonsai.io:443",
        ),
        (
            "staging",
            "https://api.staging.example:8443/v1",
            "api.staging.example:443",
            "api.staging.example:8443",
        ),
    ] {
        let s = settings(profile, Some(url));
        for source in [TokenSource::Keychain, TokenSource::File] {
            let err = check_token_host(&s, source).unwrap_err();
            assert!(
                matches!(err, ConfigError::TokenHostMismatch { .. }),
                "{err:?}"
            );
            let shown = err.to_string();
            assert!(
                shown.contains(&format!(
                    "the token for profile {profile} belongs to {owner}, not {other}"
                )),
                "{shown}"
            );
            assert!(shown.contains("BONSAI_TOKEN"), "{shown}");
        }
        // BONSAI_TOKEN goes wherever it's pointed.
        check_token_host(&s, TokenSource::Env).unwrap();
    }
}

#[test]
fn settings_default_and_url_validation() {
    let d = tempfile::tempdir().unwrap();
    let s = resolve_settings_with(&env_of(&[]), d.path(), None, None).unwrap();
    assert_eq!(
        (s.profile.as_str(), s.api_url.as_str()),
        ("default", DEFAULT_API_URL)
    );
    let r = resolve_settings_with(&env_of(&[]), d.path(), None, Some("http://evil.example/v1"));
    assert!(matches!(r, Err(ConfigError::InsecureApiUrl(_))));
    assert!(
        resolve_settings_with(
            &env_of(&[]),
            d.path(),
            None,
            Some("http://127.0.0.1:4000/v1")
        )
        .is_ok()
    );
    assert!(
        resolve_settings_with(&env_of(&[]), d.path(), None, Some("http://[::1]:4000/v1")).is_ok()
    );
    assert!(matches!(
        resolve_settings_with(&env_of(&[]), d.path(), None, Some("not a url")),
        Err(ConfigError::InvalidUrl(_))
    ));
}

#[test]
fn file_store_round_trips() {
    let d = tempfile::tempdir().unwrap();
    let fs = FileStore::new(d.path());
    assert!(fs.get("default").unwrap().is_none());
    fs.set("default", &secret("t1")).unwrap();
    fs.set("other", &secret("t2")).unwrap();
    assert_eq!(fs.get("default").unwrap().unwrap().expose_secret(), "t1");
    fs.delete("default").unwrap();
    assert!(fs.get("default").unwrap().is_none());
    assert_eq!(fs.get("other").unwrap().unwrap().expose_secret(), "t2");
}

#[cfg(unix)]
#[test]
fn file_store_is_0600_and_tightens_existing() {
    use std::os::unix::fs::PermissionsExt;
    let mode = |p: &std::path::Path| std::fs::metadata(p).unwrap().permissions().mode() & 0o777;
    let d = tempfile::tempdir().unwrap();
    let fs = FileStore::new(d.path());
    fs.set("default", &secret("t")).unwrap();
    let path = d.path().join("credentials.toml");
    assert_eq!(mode(&path), 0o600);

    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
    fs.set("default", &secret("t2")).unwrap();
    assert_eq!(mode(&path), 0o600);

    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o666)).unwrap();
    assert!(fs.get("default").unwrap().is_some());
    assert_eq!(mode(&path), 0o600);
}

#[test]
fn resolve_token_order() {
    let d = tempfile::tempdir().unwrap();
    let file = FileStore::new(d.path());
    let kr = Fake::default();

    assert!(
        resolve_token_with(&env_of(&[]), &|| Some(&kr as &dyn TokenStore), &file, "p")
            .unwrap()
            .is_none()
    );

    file.set("p", &secret("from-file")).unwrap();
    let (t, src) = resolve_token_with(&env_of(&[]), &|| Some(&kr as &dyn TokenStore), &file, "p")
        .unwrap()
        .unwrap();
    assert_eq!((t.expose_secret(), src), ("from-file", TokenSource::File));

    kr.set("p", &secret("from-kr")).unwrap();
    let (t, src) = resolve_token_with(&env_of(&[]), &|| Some(&kr as &dyn TokenStore), &file, "p")
        .unwrap()
        .unwrap();
    assert_eq!((t.expose_secret(), src), ("from-kr", TokenSource::Keychain));

    // keychain unavailable: file
    let (t, src) = resolve_token_with(&env_of(&[]), &|| None, &file, "p")
        .unwrap()
        .unwrap();
    assert_eq!((t.expose_secret(), src), ("from-file", TokenSource::File));

    let env = env_of(&[("BONSAI_TOKEN", "from-env")]);
    let (t, src) = resolve_token_with(&env, &|| Some(&kr as &dyn TokenStore), &file, "p")
        .unwrap()
        .unwrap();
    assert_eq!((t.expose_secret(), src), ("from-env", TokenSource::Env));

    // empty BONSAI_TOKEN is ignored
    let env = env_of(&[("BONSAI_TOKEN", "")]);
    let (_, src) = resolve_token_with(&env, &|| Some(&kr as &dyn TokenStore), &file, "p")
        .unwrap()
        .unwrap();
    assert_eq!(src, TokenSource::Keychain);
}

#[test]
fn store_token_selection() {
    let d = tempfile::tempdir().unwrap();
    let file = FileStore::new(d.path());
    let kr = Fake::default();

    assert_eq!(
        store_token_with(Some(&kr), &file, "p", &secret("a"), false).unwrap(),
        StoreChoice::Keychain
    );
    assert_eq!(kr.get("p").unwrap().unwrap().expose_secret(), "a");
    assert!(file.get("p").unwrap().is_none());

    assert!(matches!(
        store_token_with(None, &file, "p", &secret("b"), false),
        Err(ConfigError::NoKeychain)
    ));
    assert!(file.get("p").unwrap().is_none());

    assert_eq!(
        store_token_with(None, &file, "p", &secret("b"), true).unwrap(),
        StoreChoice::File
    );
    assert_eq!(file.get("p").unwrap().unwrap().expose_secret(), "b");
}

/// A login whose URL can't be saved is undone: the token it replaced is put back,
/// or, when there was none, its own is removed. Never the earlier one lost.
#[test]
fn an_undone_store_puts_the_earlier_token_back() {
    let d = tempfile::tempdir().unwrap();
    let file = FileStore::new(d.path());
    let kr = Fake::default();

    // Keychain, with an earlier login.
    kr.set("p", &secret("old")).unwrap();
    let replaced = replace_token_with(Some(&kr), &file, "p", &secret("new"), false, true).unwrap();
    assert_eq!(replaced.choice, StoreChoice::Keychain);
    assert_eq!(kr.get("p").unwrap().unwrap().expose_secret(), "new");
    assert_eq!(
        undo_replace_with(Some(&kr), &file, "p", replaced).unwrap(),
        Undone::Restored
    );
    assert_eq!(kr.get("p").unwrap().unwrap().expose_secret(), "old");

    // Keychain, nothing before.
    let replaced = replace_token_with(Some(&kr), &file, "q", &secret("new"), false, true).unwrap();
    assert_eq!(
        undo_replace_with(Some(&kr), &file, "q", replaced).unwrap(),
        Undone::Removed { still_in: None }
    );
    assert!(kr.get("q").unwrap().is_none());

    // Keychain, nothing before there, but a login in the file: it's named.
    file.set("r", &secret("in-file")).unwrap();
    let replaced = replace_token_with(Some(&kr), &file, "r", &secret("new"), false, true).unwrap();
    assert_eq!(
        undo_replace_with(Some(&kr), &file, "r", replaced).unwrap(),
        Undone::Removed {
            still_in: Some(StoreChoice::File)
        }
    );

    // The file, with an earlier login and another profile beside it.
    file.set("p", &secret("old-file")).unwrap();
    file.set("other", &secret("o")).unwrap();
    let replaced = replace_token_with(None, &file, "p", &secret("new"), true, true).unwrap();
    assert_eq!(replaced.choice, StoreChoice::File);
    assert_eq!(
        undo_replace_with(None, &file, "p", replaced).unwrap(),
        Undone::Restored
    );
    assert_eq!(file.get("p").unwrap().unwrap().expose_secret(), "old-file");
    assert_eq!(file.get("other").unwrap().unwrap().expose_secret(), "o");
    // The keychain's earlier token was never touched by the file login.
    assert_eq!(kr.get("p").unwrap().unwrap().expose_secret(), "old");
}

/// A store whose reads fail (a corrupt entry, a declined unlock prompt) but
/// whose writes work.
struct GetFails(Fake);
impl TokenStore for GetFails {
    fn get(&self, _: &str) -> bonsai_core::Result<Option<SecretString>> {
        Err(ConfigError::Keychain("can't read that entry".into()))
    }
    fn set(&self, p: &str, t: &SecretString) -> bonsai_core::Result<()> {
        self.0.set(p, t)
    }
    fn delete(&self, p: &str) -> bonsai_core::Result<()> {
        self.0.delete(p)
    }
    fn name(&self) -> &'static str {
        "get-fails"
    }
}

/// The earlier token is read only when the login may need undoing (a new URL to
/// save): a plain re-login stores without reading, so an unreadable entry
/// doesn't block it.
#[test]
fn the_earlier_token_is_read_only_when_needed() {
    let d = tempfile::tempdir().unwrap();
    let file = FileStore::new(d.path());
    let kr = GetFails(Fake::default());
    let stored = replace_token_with(Some(&kr), &file, "p", &secret("new"), false, false).unwrap();
    assert_eq!(stored.choice, StoreChoice::Keychain);
    assert_eq!(kr.0.get("p").unwrap().unwrap().expose_secret(), "new");
    assert!(replace_token_with(Some(&kr), &file, "p", &secret("new"), false, true).is_err());
}

/// Touches the real OS keychain. Run by hand: `cargo test -p bonsai-core -- --ignored`.
#[test]
#[ignore = "writes to the real OS keychain"]
fn real_keychain_round_trip() {
    if !keyring_available() {
        return;
    }
    let s = KeyringStore;
    s.set("bonsai-test-profile", &secret("t")).unwrap();
    assert_eq!(
        s.get("bonsai-test-profile")
            .unwrap()
            .unwrap()
            .expose_secret(),
        "t"
    );
    s.delete("bonsai-test-profile").unwrap();
}

#[test]
fn may_open_rules() {
    let u = |s: &str| Url::parse(s).unwrap();
    let api = u("https://api.bonsai.io/v1");
    let dash = u("https://app.bonsai.io");
    let ok = |s: &str| may_open(&u(s), &api, Some(&dash));

    assert!(ok("https://checkout.stripe.com/c/pay/cs_1"));
    assert!(ok("https://CHECKOUT.Stripe.com/c/pay"));
    assert!(ok("https://app.bonsai.io/billing"));
    assert!(ok("https://api.bonsai.io/x"));
    assert!(!ok("http://evil.com"));
    assert!(!ok("https://evil.com"));
    assert!(!ok("javascript:alert(1)"));
    assert!(!ok("http://checkout.stripe.com/x"));
    assert!(!ok("https://user@checkout.stripe.com/x"));
    assert!(!ok("https://user:pw@checkout.stripe.com/x"));
    assert!(!ok("https://checkout.stripe.com.evil.com/x"));
    assert!(!ok("file:///etc/passwd"));

    let mock = u("http://127.0.0.1:4000/v1");
    assert!(may_open(&u("http://127.0.0.1:4000/pay"), &mock, None));
    assert!(!may_open(&u("http://127.0.0.1:4000/pay"), &api, None));
    assert!(!may_open(&u("http://localhost:4000/pay"), &mock, None));
    assert!(!may_open(&u("https://app.bonsai.io"), &api, None));
}

// ---- edge cases ----

#[test]
fn credentials_parse_error_does_not_leak_the_token() {
    let d = tempfile::tempdir().unwrap();
    std::fs::write(
        d.path().join("credentials.toml"),
        "[profiles.default]\ntoken = abc123secret\n",
    )
    .unwrap();
    let err = FileStore::new(d.path()).get("default").unwrap_err();
    for s in [err.to_string(), format!("{err:?}")] {
        assert!(!s.contains("abc123secret"), "leaked: {s}");
    }
    assert!(err.to_string().contains("credentials.toml"));
}

#[test]
fn keychain_errors_are_redacted() {
    let err = ConfigError::from(keyring_core::Error::BadEncoding(b"hunter2-token".to_vec()));
    for s in [err.to_string(), format!("{err:?}")] {
        assert!(!s.contains("[104") && !s.contains("hunter2"), "leaked: {s}");
    }
}

#[cfg(unix)]
#[test]
fn credentials_symlink_is_refused() {
    let d = tempfile::tempdir().unwrap();
    let target = d.path().join("elsewhere");
    std::fs::write(&target, "").unwrap();
    std::os::unix::fs::symlink(&target, d.path().join("credentials.toml")).unwrap();
    let fs = FileStore::new(d.path());
    assert!(fs.get("p").is_err());
    assert!(fs.set("p", &secret("t")).is_err());
    assert_eq!(std::fs::read_to_string(&target).unwrap(), "");
}

#[cfg(unix)]
#[test]
fn private_dirs_are_0700_and_no_temp_files_remain() {
    use std::os::unix::fs::PermissionsExt;
    let d = tempfile::tempdir().unwrap();
    let dir = d.path().join("a").join("bonsai");
    FileStore::new(&dir).set("p", &secret("t")).unwrap();
    assert_eq!(
        std::fs::metadata(&dir).unwrap().permissions().mode() & 0o777,
        0o700
    );
    let names: Vec<_> = std::fs::read_dir(&dir)
        .unwrap()
        .map(|e| e.unwrap().file_name())
        .collect();
    assert_eq!(names, vec![std::ffi::OsString::from("credentials.toml")]);

    let dir2 = d.path().join("b").join("bonsai");
    Config::default().save(&dir2).unwrap();
    assert_eq!(
        std::fs::metadata(&dir2).unwrap().permissions().mode() & 0o777,
        0o700
    );
}

#[test]
fn empty_flags_are_unset() {
    let d = tempfile::tempdir().unwrap();
    let s = resolve_settings_with(&env_of(&[]), d.path(), Some(""), Some("")).unwrap();
    assert_eq!(
        (s.profile.as_str(), s.api_url.as_str()),
        ("default", DEFAULT_API_URL)
    );
}

#[test]
fn api_url_loopback_edge_cases() {
    let d = tempfile::tempdir().unwrap();
    let r = |u: &str| resolve_settings_with(&env_of(&[]), d.path(), None, Some(u));
    assert!(matches!(
        r("http://localhost.evil.com/v1"),
        Err(ConfigError::InsecureApiUrl(_))
    ));
    assert!(matches!(
        r("http://127.0.0.2/v1"),
        Err(ConfigError::InsecureApiUrl(_))
    ));
    assert!(r("http://localhost:4010/v1").is_ok());
}

#[test]
fn may_open_more_cases() {
    let u = |s: &str| Url::parse(s).unwrap();
    let api = u("https://api.bonsai.io/v1");
    let ok = |s: &str| may_open(&u(s), &api, None);
    assert!(!ok("data:text/html,<script>1</script>"));
    // A port on an allowed https host stays allowed: the host is what we trust.
    assert!(ok("https://checkout.stripe.com:8443/c/pay"));
    // IDN / punycode lookalikes do not match.
    assert!(!ok("https://checkout.str\u{131}pe.com/x"));
    assert!(!ok("https://xn--checkout.stripe.com/x"));
    assert!(!ok("https://checkout.stripe.com\u{ff0e}evil.com/x"));
}

#[test]
fn env_token_never_constructs_or_probes_the_keychain() {
    let d = tempfile::tempdir().unwrap();
    let file = FileStore::new(d.path());
    let env = env_of(&[("BONSAI_TOKEN", "from-env")]);
    let (t, src) = resolve_token_with(&env, &|| panic!("keychain probed"), &file, "p")
        .unwrap()
        .unwrap();
    assert_eq!((t.expose_secret(), src), ("from-env", TokenSource::Env));
}

struct Errs;
impl TokenStore for Errs {
    fn get(&self, _: &str) -> bonsai_core::Result<Option<SecretString>> {
        Err(ConfigError::NoKeychain)
    }
    fn set(&self, _: &str, _: &SecretString) -> bonsai_core::Result<()> {
        Ok(())
    }
    fn delete(&self, _: &str) -> bonsai_core::Result<()> {
        Ok(())
    }
    fn name(&self) -> &'static str {
        "errs"
    }
}

#[test]
fn keychain_read_error_propagates_without_file_fallback() {
    let d = tempfile::tempdir().unwrap();
    let file = FileStore::new(d.path());
    file.set("p", &secret("from-file")).unwrap();
    let r = resolve_token_with(&env_of(&[]), &|| Some(&Errs as &dyn TokenStore), &file, "p");
    assert!(r.is_err());
}

#[test]
fn keyring_off_means_unavailable() {
    assert!(!keyring_available_with(&env_of(&[(
        "BONSAI_KEYRING",
        "off"
    )])));
    assert!(!keyring_available_with(&env_of(&[(
        "BONSAI_KEYRING",
        "OFF"
    )])));
    // Checked without probing: any other value would touch the real keychain.
    for off in ["off", "0", "false", "no", "FALSE", " No "] {
        assert!(
            keyring_disabled(&env_of(&[("BONSAI_KEYRING", off)])),
            "{off:?}"
        );
    }
    for on in ["", "on", "1", "true", "yes"] {
        assert!(
            !keyring_disabled(&env_of(&[("BONSAI_KEYRING", on)])),
            "{on:?}"
        );
    }
    assert!(!keyring_disabled(&env_of(&[])));
}

#[test]
fn no_keychain_error_names_the_escape_hatches() {
    let d = tempfile::tempdir().unwrap();
    let msg = store_token_with(None, &FileStore::new(d.path()), "p", &secret("x"), false)
        .unwrap_err()
        .to_string();
    assert!(msg.contains("--store-file") && msg.contains("BONSAI_TOKEN"));
}

#[test]
fn loopback_host_helper() {
    let l = |s: &str| bonsai_api::is_loopback_host(&Url::parse(s).unwrap());
    assert!(l("http://localhost:1/"));
    assert!(l("http://LOCALHOST/"));
    assert!(l("http://127.0.0.1/"));
    assert!(l("http://[::1]:9/"));
    assert!(!l("http://localhost.evil.com/"));
    assert!(!l("http://127.0.0.2/"));
    assert!(!l("http://10.0.0.1/"));
    assert!(!l("mailto:a@b"));
}

#[test]
fn file_errors_name_their_cause_once() {
    // The cause is the error's `source()`; a chain printer adds it, so the
    // message itself must not repeat it.
    let d = tempfile::tempdir().unwrap();
    std::fs::write(d.path().join("config.toml"), "active_profile = [\n").unwrap();
    let err = Config::load(d.path()).unwrap_err();
    let source = std::error::Error::source(&err)
        .expect("a source")
        .to_string();
    assert!(!err.to_string().contains(&source), "{err}");
    assert!(err.to_string().contains("config.toml"), "{err}");

    let err = Config::load(&d.path().join("config.toml")).unwrap_err();
    let source = std::error::Error::source(&err)
        .expect("a source")
        .to_string();
    assert!(!err.to_string().contains(&source), "{err}");
}
