//! `login`, `logout` and `whoami`.

use std::cell::RefCell;
use std::io::IsTerminal;
use std::path::Path;

use anyhow::Result;
use bonsai_api::{CliLoginGrant, CliLoginStart, Client};
use bonsai_core::auth::{
    FileStore, KeyringStore, Replaced, StoreChoice, TokenSource, TokenStore, Undone,
    keyring_available, replace_token, undo_replace,
};
use bonsai_core::config::{Config, Profile, Settings};
use bonsai_core::urlcheck::may_open;
use bonsai_core::{CoreError, device_login};
use indicatif::ProgressBar;
use jiff::Timestamp;
use secrecy::SecretString;
use serde_json::{Value, json};
use url::Url;

use super::{Ctx, chain, quick_retry, read_stdin};
use crate::cli::{LoginArgs, LogoutArgs};
use crate::exit::Interrupted;
use crate::output::relative_time;
use crate::sanitize::sanitize;

enum Method {
    Browser,
    Stdin,
    Prompt,
}

const BROWSER: &str = "Log in with a browser";
const PASTE: &str = "Paste a token";

pub async fn login(ctx: &Ctx, args: LoginArgs) -> Result<()> {
    let settings = ctx.settings()?;
    let method = if args.with_token {
        Method::Stdin
    } else if args.web {
        Method::Browser
    } else if ctx.interactive {
        let choice = inquire::Select::new("How do you want to log in?", vec![BROWSER, PASTE])
            .prompt()
            .map_err(prompt_error)?;
        if choice == BROWSER {
            Method::Browser
        } else {
            Method::Prompt
        }
    } else {
        return Err(CoreError::Usage(
            "can't prompt here; pass --with-token to read a token from stdin, or --web to log in with a browser"
                .into(),
        )
        .into());
    };

    let (token, expires_at) = match method {
        Method::Browser => browser_login(ctx, &settings, &args).await?,
        Method::Stdin => {
            if std::io::stdin().is_terminal() {
                tokens_hint(ctx, &settings).await;
                ctx.out.notice("Paste the token, then press Ctrl-D");
            }
            (pasted(read_stdin(ctx).await?)?, None)
        }
        Method::Prompt => {
            tokens_hint(ctx, &settings).await;
            let text = inquire::Password::new("Token")
                .without_confirmation()
                .with_display_mode(inquire::PasswordDisplayMode::Masked)
                .prompt()
                .map_err(prompt_error)?;
            (pasted(text)?, None)
        }
    };

    // Stored only once the API has accepted it.
    let client = ctx.client(&settings, Some(token.clone()))?;
    let account = ctx
        .abortable(async { Ok(client.get_account().await?) })
        .await?;
    let expires_at = match expires_at {
        Some(t) => Some(t),
        None => token_expiry(&client).await,
    };

    // The token belongs to the host that accepted it, so the profile
    // records that URL, from --api-url or BONSAI_API_URL.
    let repoint = (settings.api_url != settings.profile_api_url).then(|| settings.api_url.clone());
    let (profile, dir, allow_file) = (
        settings.profile.clone(),
        ctx.config_dir.clone(),
        args.store_file,
    );
    let (stored, url) = (token.clone(), repoint.clone());
    let keep_previous = url.is_some();
    let choice = ctx
        .blocking(move || {
            persist_login(
                || {
                    Ok(replace_token(
                        &profile,
                        &dir,
                        &stored,
                        allow_file,
                        keep_previous,
                    )?)
                },
                url.map(|url| {
                    let (dir, profile) = (dir.clone(), profile.clone());
                    move || save_api_url(&dir, profile, url)
                }),
                |replaced| Ok(undo_replace(&profile, &dir, replaced)?),
            )
        })
        .await?;
    match choice {
        StoreChoice::Keychain => {}
        StoreChoice::File => ctx.out.warn(&format!(
            "No keychain here, so the token is in plain text in {} (mode 0600)",
            FileStore::new(&ctx.config_dir)
                .dir
                .join("credentials.toml")
                .display()
        )),
    }
    if let Some(url) = &repoint {
        ctx.out
            .notice(&format!("Profile {} now uses {url}", settings.profile));
    }

    if ctx.out.is_json() {
        ctx.out.json(&account.raw)?;
    } else {
        let expiry = match expires_at {
            Some(t) => format!("expires {}", t.strftime("%Y-%m-%d")),
            None => "no expiry".into(),
        };
        ctx.out.success(&format!(
            "Logged in to {} as {} ({expiry})",
            account.value.name, settings.profile
        ));
    }
    Ok(())
}

/// A token read from stdin or a prompt, trimmed; empty is a usage error.
fn pasted(text: String) -> Result<SecretString> {
    let token = text.trim();
    if token.is_empty() {
        return Err(CoreError::Usage("no token given".into()).into());
    }
    Ok(SecretString::from(token.to_owned()))
}

/// The token's expiry from `GET /token`, when the API offers it.
async fn token_expiry(client: &Client) -> Option<Timestamp> {
    match client.get_token().await {
        Ok(Some(info)) => info.value.expires_at,
        Ok(None) => None,
        Err(e) => {
            tracing::debug!(error = %e, "couldn't read the token's details");
            None
        }
    }
}

/// The device flow: print the code and link, maybe open it, and wait for approval.
async fn browser_login(
    ctx: &Ctx,
    settings: &Settings,
    args: &LoginArgs,
) -> Result<(SecretString, Option<Timestamp>)> {
    let client = ctx.client(settings, None)?;
    let start = CliLoginStart {
        client_name: "bonsai-cli".into(),
        hostname: hostname(),
        os: std::env::consts::OS.into(),
    };
    let spinner: RefCell<Option<ProgressBar>> = RefCell::new(None);
    let on_grant = |grant: &CliLoginGrant| {
        show_grant(ctx, settings, args, grant);
        *spinner.borrow_mut() = ctx.out.spinner("Waiting for you to approve the login");
    };
    let outcome = device_login(&client, start, on_grant, ctx.cancel.clone()).await;
    if let Some(pb) = spinner.take() {
        pb.finish_and_clear();
    }
    let outcome = outcome?;
    Ok((outcome.token, outcome.expires_at))
}

/// Where a token is made by hand: `GET /v1`'s `tokens_url`. Nothing when the API
/// can't be reached; the token prompt still works without it.
async fn tokens_hint(ctx: &Ctx, settings: &Settings) {
    let Ok(anon) = ctx.client_with(settings, None, quick_retry()) else {
        return;
    };
    if let Ok(root) = anon.get_api_root().await {
        ctx.out.notice(&format!(
            "Make a token on the API Tokens page: {}",
            sanitize(&root.value.tokens_url, false)
        ));
    }
}

fn show_grant(ctx: &Ctx, settings: &Settings, args: &LoginArgs, grant: &CliLoginGrant) {
    ctx.out
        .notice(&format!("Your one-time code: {}", grant.user_code));
    ctx.out
        .notice("Check that the page shows it, then approve the login:");
    ctx.out
        .notice(&format!("  {}", grant.verification_uri_complete));
    if !tries_a_browser(args.no_browser, ctx.interactive) {
        return;
    }
    let allowed = Url::parse(&grant.verification_uri_complete)
        .is_ok_and(|u| may_open(&u, &settings.api_url, settings.dashboard_url.as_ref()));
    if !allowed {
        ctx.out
            .warn("Not opening that link: it isn't on Bonsai's host. Check it before you use it.");
    } else if let Err(e) = open::that_detached(&grant.verification_uri_complete) {
        ctx.out.warn(&format!(
            "Couldn't open a browser ({e}); open the link above"
        ));
    }
}

/// Whether the login link may be opened at all (it also has to pass `may_open`):
/// never with `--no-browser`, and never when prompts are off, since no one may be
/// at the screen.
fn tries_a_browser(no_browser: bool, interactive: bool) -> bool {
    !no_browser && interactive
}

/// This machine's name, for the token's name ("Bonsai CLI on <hostname>").
fn hostname() -> String {
    let from_env = ["HOSTNAME", "COMPUTERNAME"]
        .iter()
        .find_map(|k| std::env::var(k).ok());
    let from_file = || {
        ["/proc/sys/kernel/hostname", "/etc/hostname"]
            .iter()
            .find_map(|p| std::fs::read_to_string(p).ok())
    };
    let from_command = || {
        std::process::Command::new("hostname")
            .output()
            .ok()
            .and_then(|o| String::from_utf8(o.stdout).ok())
    };
    from_env
        .or_else(from_file)
        .or_else(from_command)
        .map(|h| h.trim().chars().take(255).collect::<String>())
        .filter(|h| !h.is_empty())
        .unwrap_or_else(|| "unknown".into())
}

/// What storing a token left: where it went, and (for [`Replaced`]) what it
/// replaced, so the login can be undone.
trait Receipt {
    fn choice(&self) -> StoreChoice;
}

impl Receipt for StoreChoice {
    fn choice(&self) -> StoreChoice {
        *self
    }
}

impl Receipt for Replaced {
    fn choice(&self) -> StoreChoice {
        self.choice
    }
}

/// Stores the token, then saves the profile's new URL when there is one. A token
/// kept under the old URL would be sent to the wrong host, so when the save fails
/// the login is undone (`undo`): the token it replaced is put back, or, when there
/// was none, the new one removed. The error says which.
fn persist_login<R: Receipt, U: FnOnce() -> Result<()>>(
    store: impl FnOnce() -> Result<R>,
    save_url: Option<U>,
    undo: impl FnOnce(R) -> Result<Undone>,
) -> Result<StoreChoice> {
    let receipt = store()?;
    let choice = receipt.choice();
    let Some(save_url) = save_url else {
        return Ok(choice);
    };
    let Err(saving) = save_url() else {
        return Ok(choice);
    };
    let saving = format!("{saving:#}");
    match undo(receipt) {
        Ok(Undone::Restored) => anyhow::bail!(
            "couldn't save the API URL to config.toml, so the new token was not kept; kept your earlier login: {saving}"
        ),
        Ok(Undone::Removed { still_in: None }) => anyhow::bail!(
            "couldn't save the API URL to config.toml, so the new token was not kept; no login is stored: {saving}"
        ),
        Ok(Undone::Removed {
            still_in: Some(store),
        }) => anyhow::bail!(
            "couldn't save the API URL to config.toml, so the new token was not kept; an earlier login is still in {}: {saving}",
            match store {
                StoreChoice::Keychain => "the keychain",
                StoreChoice::File => "credentials.toml",
            }
        ),
        // The profile's URL is still the old one, so `logout` refuses the token
        // while --api-url or BONSAI_API_URL points elsewhere.
        Err(e) => anyhow::bail!(
            "couldn't save the API URL to config.toml ({saving}), and couldn't undo the login either ({e:#}); unset BONSAI_API_URL (and drop --api-url), then run `bonsai logout --keep-token`"
        ),
    }
}

fn save_api_url(dir: &Path, profile: String, api_url: Url) -> Result<()> {
    let mut config = Config::load(dir)?;
    config
        .profiles
        .entry(profile)
        .and_modify(|p| p.api_url = api_url.clone())
        .or_insert_with(|| Profile {
            api_url,
            dashboard_url: None,
        });
    config.save(dir)?;
    Ok(())
}

fn prompt_error(e: inquire::InquireError) -> anyhow::Error {
    match e {
        inquire::InquireError::OperationCanceled | inquire::InquireError::OperationInterrupted => {
            Interrupted.into()
        }
        e => anyhow::Error::new(e).context("couldn't prompt"),
    }
}

pub async fn logout(ctx: &Ctx, args: LogoutArgs) -> Result<()> {
    let settings = ctx.settings()?;
    let Some((token, source)) = ctx.find_token(&settings).await? else {
        ctx.out.warn(&format!(
            "Not logged in as {}; nothing to do",
            settings.profile
        ));
        return Ok(());
    };

    let mut revoked = false;
    if !args.keep_token {
        let client = ctx.client(&settings, Some(token))?;
        match client.revoke_token().await {
            Ok(Some(())) => {
                revoked = true;
                ctx.out.success("Revoked the token on Bonsai");
            }
            Ok(None) => ctx
                .out
                .warn("This API can't revoke tokens yet; the token stays valid until it expires or is revoked in the dashboard"),
            Err(e) if e.status() == Some(401) => {
                ctx.out.warn("Bonsai no longer accepts the token");
            }
            Err(e) => {
                return Err(anyhow::Error::new(e).context(
                    "couldn't revoke the token; nothing was removed (--keep-token only forgets it here)",
                ));
            }
        }
    }

    if source == TokenSource::Env {
        ctx.out
            .warn("The token came from BONSAI_TOKEN, which bonsai never stores; unset it yourself");
        return Ok(());
    }
    // Both stores, so an older copy in the other can't take over. One failing
    // doesn't stop the other.
    let (profile, dir) = (settings.profile.clone(), ctx.config_dir.clone());
    let (checked, keychain, file) = ctx
        .blocking(move || {
            let checked = keyring_available();
            let keychain = if checked {
                KeyringStore.delete(&profile)
            } else {
                Ok(())
            };
            let file = FileStore::new(&dir).delete(&profile);
            Ok((checked, keychain, file))
        })
        .await?;
    let failed: Vec<String> = [("keychain", keychain), ("credentials.toml", file)]
        .into_iter()
        .filter_map(|(store, res)| res.err().map(|e| format!("{store}: {}", chain(e))))
        .collect();
    if failed.is_empty() {
        if checked {
            ctx.out.success(&format!(
                "Removed the {} token from this machine",
                settings.profile
            ));
        } else {
            // A copy an earlier login left in the keychain may still be there.
            ctx.out.success(&format!(
                "Removed the {} token from credentials.toml; the keychain wasn't checked (BONSAI_KEYRING is off, or there's no keychain here)",
                settings.profile
            ));
        }
        return Ok(());
    }
    let what = if revoked {
        "the token was revoked on Bonsai, but couldn't be removed from this machine"
    } else {
        "couldn't remove the token from this machine"
    };
    anyhow::bail!("{what}: {}", failed.join("; "))
}

pub async fn whoami(ctx: &Ctx) -> Result<()> {
    let a = ctx.authed().await?;
    let account = a.client.get_account().await?;
    let token = match a.client.get_token().await {
        Ok(t) => t,
        Err(e) => {
            ctx.out.warn(&format!(
                "Couldn't read the token's scopes and expiry: {}",
                chain(e)
            ));
            None
        }
    };
    let source = match a.source {
        TokenSource::Env => "env",
        TokenSource::Keychain => "keychain",
        TokenSource::File => "file",
    };

    if ctx.out.is_json() {
        ctx.out.json(&json!({
            "account": account.raw,
            "profile": a.settings.profile,
            "api_url": a.settings.api_url.as_str(),
            "token_source": source,
            "token": token.as_ref().map_or(Value::Null, |t| t.raw.clone()),
        }))?;
        return Ok(());
    }

    let where_from = match a.source {
        TokenSource::Env => "env (BONSAI_TOKEN)".to_owned(),
        TokenSource::Keychain => "keychain".to_owned(),
        TokenSource::File => format!(
            "file ({})",
            ctx.config_dir.join("credentials.toml").display()
        ),
    };
    let mut rows = vec![
        (
            "Account",
            format!("{} ({})", account.value.name, account.value.id),
        ),
        ("Profile", a.settings.profile.clone()),
        ("API", a.settings.api_url.to_string()),
        ("Token", where_from),
    ];
    if let Some(t) = token {
        let t = t.value;
        rows.push(("Token key", t.key));
        if let Some(name) = t.name {
            rows.push(("Token name", name));
        }
        rows.push(("Scopes", t.scopes.join(", ")));
        rows.push((
            "Expires",
            match t.expires_at {
                Some(at) => format!(
                    "{} ({})",
                    at.strftime("%Y-%m-%d"),
                    relative_time(at, Timestamp::now())
                ),
                None => "never".into(),
            },
        ));
    }
    ctx.out.kv(&rows)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_pasted_token_is_trimmed() {
        use secrecy::ExposeSecret;
        let t = pasted("  bonsai_a_b \n".into()).unwrap();
        assert_eq!(t.expose_secret(), "bonsai_a_b");
        assert!(pasted(" \n".into()).is_err());
    }

    #[test]
    fn the_browser_opens_only_when_interactive_and_allowed() {
        assert!(tries_a_browser(false, true));
        assert!(!tries_a_browser(true, true), "--no-browser alone");
        assert!(!tries_a_browser(false, false), "non-interactive alone");
        assert!(!tries_a_browser(true, false));
    }

    #[test]
    fn a_failed_url_save_undoes_the_login() {
        use std::cell::Cell;
        let undone = Cell::new(false);
        let fail = || anyhow::bail!("disk full");

        // An earlier login is put back, and the message says so.
        let err = persist_login(
            || Ok(StoreChoice::File),
            Some(fail),
            |_| {
                undone.set(true);
                Ok(Undone::Restored)
            },
        )
        .unwrap_err();
        assert!(undone.get());
        let shown = format!("{err:#}");
        assert!(shown.contains("the new token was not kept"), "{shown}");
        assert!(shown.contains("kept your earlier login"), "{shown}");
        assert!(shown.contains("disk full"), "{shown}");

        // None before: no login is stored now.
        let err = persist_login(
            || Ok(StoreChoice::File),
            Some(fail),
            |_| Ok(Undone::Removed { still_in: None }),
        )
        .unwrap_err();
        let shown = format!("{err:#}");
        assert!(shown.contains("no login is stored"), "{shown}");

        // None before in that store, but one in the other: named, not "none".
        let err = persist_login(
            || Ok(StoreChoice::Keychain),
            Some(fail),
            |_| {
                Ok(Undone::Removed {
                    still_in: Some(StoreChoice::File),
                })
            },
        )
        .unwrap_err();
        let shown = format!("{err:#}");
        assert!(
            shown.contains("an earlier login is still in credentials.toml"),
            "{shown}"
        );
        assert!(!shown.contains("no login is stored"), "{shown}");

        // Saved: kept, nothing undone.
        let undone = Cell::new(false);
        let choice = persist_login(
            || Ok(StoreChoice::Keychain),
            Some(|| Ok(())),
            |_| {
                undone.set(true);
                Ok(Undone::Removed { still_in: None })
            },
        )
        .unwrap();
        assert_eq!(choice, StoreChoice::Keychain);
        assert!(!undone.get());

        // Undoing fails too: both are said, and the advice works with the URL
        // override still set.
        let err = persist_login(
            || Ok(StoreChoice::File),
            Some(fail),
            |_| anyhow::bail!("locked"),
        )
        .unwrap_err();
        let shown = format!("{err:#}");
        assert!(
            shown.contains("disk full") && shown.contains("locked"),
            "{shown}"
        );
        assert!(shown.contains("unset BONSAI_API_URL"), "{shown}");
    }

    #[test]
    fn hostname_is_never_empty() {
        assert!(!hostname().is_empty());
    }
}
