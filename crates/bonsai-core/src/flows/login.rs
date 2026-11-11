//! The browser login: RFC 8628's device flow, ending in an API token.

use std::time::Duration;

use bonsai_api::{ApiError, CliLoginGrant, CliLoginStart, Client, ProblemCode, Ref};
use jiff::Timestamp;
use secrecy::SecretString;
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;

use crate::CoreError;
use crate::wait::deadline_after;

/// RFC 8628 3.5: `slow_down` adds 5 s to the interval for this and every later poll.
const SLOW_DOWN: Duration = Duration::from_secs(5);
/// The least time between polls, whatever `interval` says.
const MIN_INTERVAL: Duration = Duration::from_secs(1);
/// The longest a login is polled for, whatever `expires_in` says.
const MAX_WAIT: Duration = Duration::from_secs(60 * 60);

pub struct LoginOutcome {
    pub token: SecretString,
    pub account: Ref,
    pub scopes: Vec<String>,
    pub expires_at: Option<Timestamp>,
}

/// Starts a login, hands the grant (user code and link) to `on_grant`, then polls
/// every `interval` until it is approved.
///
/// Stops with [`CoreError::LoginDenied`] on `access_denied`, [`CoreError::LoginExpired`]
/// on `expired_token` or once `expires_in` has passed, and
/// [`CoreError::LoginCancelled`] when `cancel` fires.
pub async fn device_login(
    api: &Client,
    start: CliLoginStart,
    on_grant: impl FnOnce(&CliLoginGrant),
    cancel: CancellationToken,
) -> Result<LoginOutcome, CoreError> {
    let grant = api.start_cli_login(&start).await?.value;
    on_grant(&grant);
    let deadline = deadline_after(Duration::from_secs(grant.expires_in).min(MAX_WAIT));
    let mut interval = Duration::from_secs(grant.interval).max(MIN_INTERVAL);
    loop {
        tokio::select! {
            biased;
            () = cancel.cancelled() => return Err(CoreError::LoginCancelled),
            // Never past the expiry: a long `interval` (or `slow_down` build-up)
            // would otherwise report it late.
            () = tokio::time::sleep(interval.min(deadline.saturating_duration_since(Instant::now()))) => {}
        }
        if Instant::now() >= deadline {
            return Err(CoreError::LoginExpired);
        }
        let polled = tokio::select! {
            biased;
            () = cancel.cancelled() => return Err(CoreError::LoginCancelled),
            res = api.poll_cli_login(&grant.device_code) => res,
        };
        match polled {
            Ok(res) => {
                let t = res.value;
                return Ok(LoginOutcome {
                    token: t.token,
                    account: t.account,
                    scopes: t.scopes,
                    expires_at: t.expires_at,
                });
            }
            Err(e) => match login_code(&e) {
                Some(ProblemCode::AuthorizationPending) => {}
                Some(ProblemCode::SlowDown) => interval = interval.saturating_add(SLOW_DOWN),
                Some(ProblemCode::AccessDenied) => return Err(CoreError::LoginDenied),
                Some(ProblemCode::ExpiredToken) => return Err(CoreError::LoginExpired),
                _ => return Err(e.into()),
            },
        }
    }
}

/// RFC 8628's error code, when `e` is a `400` Problem carrying one.
fn login_code(e: &ApiError) -> Option<&ProblemCode> {
    match e {
        ApiError::Problem {
            status: 400,
            problem,
            ..
        } => Some(&problem.code),
        _ => None,
    }
}
