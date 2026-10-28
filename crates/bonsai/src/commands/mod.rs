//! Command handlers, and what they share: settings, the client, the printer and the
//! cancellation token.

mod action;
mod api;
mod auth;
mod catalogue;
mod cluster;
mod misc;
mod observe;
mod skill;
mod url;

use std::path::PathBuf;
use std::time::Duration;

use anyhow::Result;
use bonsai_api::{Client, ClientConfig, RetryPolicy};
use bonsai_core::auth::TokenSource;
use bonsai_core::config::{Settings, resolve_settings};
use secrecy::SecretString;
use tokio_util::sync::CancellationToken;

use crate::cli::{ClusterCommand, Command, Global};
use crate::exit::{Interrupted, NotLoggedIn};
use crate::output::Printer;

/// What every command gets.
pub struct Ctx {
    pub out: Printer,
    pub global: Global,
    pub interactive: bool,
    /// Fired by the first Ctrl-C.
    pub cancel: CancellationToken,
    pub config_dir: PathBuf,
}

/// A client with a token, and where the token came from.
pub struct Authed {
    pub settings: Settings,
    pub client: Client,
    pub source: TokenSource,
}

impl Ctx {
    /// Profile and API URL: flag, then env, then `config.toml`, then the default.
    pub fn settings(&self) -> Result<Settings> {
        Ok(resolve_settings(
            &self.config_dir,
            self.global.profile.as_deref(),
            self.global.api_url.as_deref(),
        )?)
    }

    pub fn client(&self, settings: &Settings, token: Option<SecretString>) -> Result<Client> {
        self.client_with(settings, token, RetryPolicy::default())
    }

    pub fn client_with(
        &self,
        settings: &Settings,
        token: Option<SecretString>,
        retry: RetryPolicy,
    ) -> Result<Client> {
        Ok(Client::new(ClientConfig {
            base_url: settings.api_url.clone(),
            token,
            user_agent: user_agent(),
            retry,
            ca_bundle: std::env::var_os("BONSAI_CA_BUNDLE")
                .filter(|v| !v.is_empty())
                .map(PathBuf::from),
        })?)
    }

    /// The active profile's token, if there is one. The keychain (D-Bus, or an unlock
    /// prompt on macOS) and the file are read off the runtime thread. A stored token
    /// is refused when the API URL points at another host than its profile's
    /// before any request is made.
    pub async fn find_token(
        &self,
        settings: &Settings,
    ) -> Result<Option<(SecretString, TokenSource)>> {
        let profile = settings.profile.clone();
        let dir = self.config_dir.clone();
        let found = self
            .blocking(move || Ok(bonsai_core::auth::resolve_token(&profile, &dir)?))
            .await?;
        if let Some((_, source)) = &found {
            bonsai_core::auth::check_token_host(settings, *source)?;
        }
        Ok(found)
    }

    /// The token for the active profile, or [`NotLoggedIn`].
    pub async fn token(&self, settings: &Settings) -> Result<(SecretString, TokenSource)> {
        self.find_token(settings)
            .await?
            .ok_or_else(|| NotLoggedIn.into())
    }

    pub async fn authed(&self) -> Result<Authed> {
        let settings = self.settings()?;
        let (token, source) = self.token(&settings).await?;
        let client = self.client(&settings, Some(token))?;
        Ok(Authed {
            settings,
            client,
            source,
        })
    }

    /// Runs synchronous I/O (keychain, files, stdin) on tokio's blocking pool, so the
    /// runtime thread stays free; the first Ctrl-C stops waiting for it.
    pub async fn blocking<T: Send + 'static>(
        &self,
        f: impl FnOnce() -> Result<T> + Send + 'static,
    ) -> Result<T> {
        self.abortable(async { tokio::task::spawn_blocking(f).await? })
            .await
    }

    /// Runs `fut`, or stops with [`Interrupted`] at the first Ctrl-C. For work with
    /// nothing to clean up; flows that wait take `cancel` themselves.
    pub async fn abortable<T>(&self, fut: impl Future<Output = Result<T>>) -> Result<T> {
        tokio::select! {
            biased;
            () = self.cancel.cancelled() => Err(Interrupted.into()),
            res = fut => res,
        }
    }
}

/// `e` with its causes, `a: b: c`. A typed error's `{:#}` is its own message only
/// (thiserror doesn't walk `source()`), so convert before formatting.
pub fn chain(e: impl Into<anyhow::Error>) -> String {
    format!("{:#}", e.into())
}

/// `bonsai-cli/<version> (<os>; <arch>)`.
pub fn user_agent() -> String {
    format!(
        "bonsai-cli/{} ({}; {})",
        env!("CARGO_PKG_VERSION"),
        std::env::consts::OS,
        std::env::consts::ARCH
    )
}

/// Retries for `doctor` and `version`, which shouldn't hang for two minutes on an
/// API that's down.
pub fn quick_retry() -> RetryPolicy {
    RetryPolicy {
        max_retries: 1,
        deadline: Duration::from_secs(15),
        ..RetryPolicy::default()
    }
}

/// Reads all of stdin off the runtime thread.
pub async fn read_stdin(ctx: &Ctx) -> Result<String> {
    ctx.blocking(|| Ok(std::io::read_to_string(std::io::stdin())?))
        .await
}

pub async fn run(command: Command, ctx: &Ctx) -> Result<()> {
    match command {
        Command::Login(args) => auth::login(ctx, args).await,
        Command::Logout(args) => ctx.abortable(auth::logout(ctx, args)).await,
        Command::Whoami => ctx.abortable(auth::whoami(ctx)).await,
        Command::Doctor => ctx.abortable(misc::doctor(ctx)).await,
        Command::Version => ctx.abortable(misc::version(ctx)).await,
        Command::Completion(args) => misc::completion(ctx, args),
        Command::Skill(command) => skill::run(ctx, command),
        Command::Api(args) => ctx.abortable(api::api(ctx, args)).await,
        Command::Clusters(args) => cluster::list(ctx, args).await,
        Command::Cluster(command) => cluster::run(ctx, command).await,
        Command::History(args) => cluster::run(ctx, ClusterCommand::History(args)).await,
        Command::Url(args) => url::url(ctx, args).await,
        Command::Usage(args) => observe::usage(ctx, args).await,
        Command::Metrics(args) => observe::metrics(ctx, args).await,
        Command::Queries(args) => observe::queries(ctx, args).await,
        Command::Action(command) => action::run(ctx, command).await,
        Command::Wait(args) => action::wait(ctx, args).await,
        Command::Plans(args) => catalogue::plans(ctx, args).await,
        Command::Regions(args) => catalogue::regions(ctx, args).await,
        Command::Spaces(args) => catalogue::spaces(ctx, args).await,
        Command::Releases(args) => catalogue::releases(ctx, args).await,
    }
}
