//! Exit codes.

use bonsai_api::{ActionErrorCode, ApiError, EngineError, ProblemCode};
use bonsai_core::{Cleanup, CleanupOutcome, ConfigError, CoreError, StopReason};

/// No token anywhere: not in `BONSAI_TOKEN`, the keychain or `credentials.toml`.
#[derive(Debug)]
pub struct NotLoggedIn;

impl std::fmt::Display for NotLoggedIn {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("not logged in; run `bonsai login`, or set BONSAI_TOKEN")
    }
}

impl std::error::Error for NotLoggedIn {}

/// A prompt was interrupted with Ctrl-C.
#[derive(Debug)]
pub struct Interrupted;

impl std::fmt::Display for Interrupted {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("interrupted")
    }
}

impl std::error::Error for Interrupted {}

/// What to do next, attached to an error as its outermost `anyhow` context. The
/// error report prints these lines after the error and what its Problem adds, and
/// leaves them out of the message and of `--json`. Never changes the exit code.
#[derive(Debug)]
pub struct Hint(pub Vec<String>);

impl std::fmt::Display for Hint {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0.join("\n"))
    }
}

/// A context that says, in the CLI's words, everything the errors below it in the
/// chain would: the error report prints it instead of them (their codes still
/// decide the exit code).
#[derive(Debug)]
pub struct Explained(pub String);

impl std::fmt::Display for Explained {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// Failed with no Action error to report, e.g. a `failed` cluster (exit 5).
#[derive(Debug)]
pub struct Failed(pub String);

impl std::fmt::Display for Failed {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for Failed {}

/// Still not there when `--timeout` ran out, with no Action to resume (exit 9).
#[derive(Debug)]
pub struct TimedOut(pub String);

impl std::fmt::Display for TimedOut {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for TimedOut {}

/// A cluster answered `bonsai cluster request` with a status of 400 or above. Its
/// body has already gone to stdout; this is the line on stderr, and the exit code:
/// 3 for a 401 or 403, 4 for a 404, 1 for anything else.
#[derive(Debug)]
pub struct EngineStatus {
    pub status: u16,
    /// What the body says went wrong, when it says so in one line.
    pub reason: Option<String>,
}

/// `404 Not Found`, or the bare number for a status with no standard phrase.
pub fn status_text(status: u16) -> String {
    let phrase = reqwest::StatusCode::from_u16(status)
        .ok()
        .and_then(|s| s.canonical_reason());
    match phrase {
        Some(phrase) => format!("{status} {phrase}"),
        None => status.to_string(),
    }
}

impl std::fmt::Display for EngineStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "the cluster answered {}", status_text(self.status))?;
        if let Some(reason) = &self.reason {
            write!(f, ": {reason}")?;
        }
        Ok(())
    }
}

impl std::error::Error for EngineStatus {}

/// The exit code for `e`, from the first error in its chain that has one.
///
/// | Code | For |
/// |---|---|
/// | 2 | usage: [`CoreError::Usage`], `Ambiguous`, clap, `invalid_parameter`, the config (1), a request path that doesn't start with / |
/// | 3 | `401`, `403`, [`NotLoggedIn`], a stored token pointed at another host, a cluster's 401 or 403 ([`EngineStatus`]) |
/// | 4 | `404`, `ClusterNotFound`, a cluster's 404 |
/// | 5 | an Action that failed with `timed_out`, `action_failed` or an unknown code; [`Failed`] |
/// | 6 | `payment_method_required`, `payment_failed` |
/// | 7 | `action_in_progress`, `idempotency_key_in_use` |
/// | 8 | a refusal (2) |
/// | 9 | `Timeout`, [`TimedOut`], an `ActionFailed` whose cleanup is `Unchecked` by the deadline, a cluster that didn't answer in time |
/// | 130 | [`Interrupted`], `Cancelled`, `LoginCancelled`, `Unchecked` by Ctrl-C |
/// | 1 | anything else: `metrics_unavailable`, `5xx`, transport |
///
/// (1) A bad or insecure API URL, one with a password, no keychain without
/// `--store-file`, no config directory.
/// (2) `validation_failed`, `name_mismatch`, `plan_change_refused`,
/// `cluster_terminated`, `idempotency_key_reused`.
pub fn exit_code(e: &anyhow::Error) -> u8 {
    e.chain().find_map(code_of).unwrap_or(1)
}

/// The code one error in a chain decides, or `None` to look at its source.
fn code_of(e: &(dyn std::error::Error + 'static)) -> Option<u8> {
    if let Some(e) = e.downcast_ref::<CoreError>() {
        return Some(core_code(e));
    }
    if let Some(e) = e.downcast_ref::<ApiError>() {
        return Some(api_code(e));
    }
    if let Some(e) = e.downcast_ref::<ConfigError>() {
        return Some(config_code(e));
    }
    if let Some(e) = e.downcast_ref::<EngineStatus>() {
        return Some(match e.status {
            401 | 403 => 3,
            404 => 4,
            _ => 1,
        });
    }
    if let Some(e) = e.downcast_ref::<EngineError>() {
        return Some(match e {
            EngineError::InvalidPath(_) => 2,
            EngineError::Timeout => 9,
            _ => 1,
        });
    }
    if e.is::<clap::Error>() {
        return Some(2);
    }
    if e.is::<NotLoggedIn>() {
        return Some(3);
    }
    if e.is::<Interrupted>() {
        return Some(130);
    }
    if e.is::<TimedOut>() {
        return Some(9);
    }
    if e.is::<Failed>() {
        return Some(5);
    }
    None
}

fn core_code(e: &CoreError) -> u8 {
    match e {
        CoreError::Api(api) => api_code(api),
        CoreError::Usage(_) | CoreError::Ambiguous { .. } => 2,
        CoreError::ClusterNotFound(_) => 4,
        CoreError::ActionFailed(f) => match (&f.cleanup, &f.code) {
            // Ctrl-C or the timeout cut the check of the cluster short: that
            // decides, as it would anywhere else.
            (
                Some(Cleanup {
                    outcome: CleanupOutcome::Unchecked { reason },
                    ..
                }),
                _,
            ) => match reason {
                StopReason::Cancelled => 130,
                StopReason::Timeout => 9,
            },
            (_, ActionErrorCode::PaymentFailed) => 6,
            _ => 5,
        },
        CoreError::Timeout { .. } => 9,
        // Ctrl-C: the hint still says how to resume.
        CoreError::Cancelled { .. } | CoreError::LoginCancelled => 130,
        // The poll's own error decides: a 401 is still "not allowed".
        CoreError::WaitFailed { source, .. } => api_code(source),
        _ => 1,
    }
}

fn api_code(e: &ApiError) -> u8 {
    match e.code() {
        Some(ProblemCode::PaymentMethodRequired | ProblemCode::PaymentFailed) => return 6,
        Some(ProblemCode::ActionInProgress | ProblemCode::IdempotencyKeyInUse) => return 7,
        Some(
            ProblemCode::ValidationFailed
            | ProblemCode::NameMismatch
            | ProblemCode::PlanChangeRefused
            | ProblemCode::ClusterTerminated
            | ProblemCode::IdempotencyKeyReused,
        ) => return 8,
        // A flag's value the API checks, not the CLI: `--resolution 5m`, `--window 3d`.
        Some(ProblemCode::InvalidParameter) => return 2,
        _ => {}
    }
    match e {
        ApiError::InsecureUrl(_) | ApiError::InvalidPath(_) => return 2,
        _ => {}
    }
    match e.status() {
        Some(401 | 403) => 3,
        Some(404) => 4,
        _ => 1,
    }
}

fn config_code(e: &ConfigError) -> u8 {
    match e {
        ConfigError::NoKeychain
        | ConfigError::InvalidUrl(_)
        | ConfigError::InsecureApiUrl(_)
        | ConfigError::ApiUrlCredentials
        | ConfigError::NoConfigDir => 2,
        ConfigError::TokenHostMismatch { .. } => 3,
        _ => 1,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bonsai_api::{ActionErrorCode, ApiError, Problem, ProblemCode};
    use bonsai_core::{ActionFailure, ConfigError, CoreError};
    use std::time::Duration;

    fn problem(status: u16, code: &str) -> ApiError {
        ApiError::Problem {
            status,
            problem: Box::new(Problem {
                r#type: "about:blank".into(),
                title: "t".into(),
                status,
                code: serde_json::from_value(serde_json::json!(code)).unwrap(),
                detail: "d".into(),
                instance: None,
                action_url: None,
                details: vec![],
            }),
            raw: Box::new(serde_json::json!({ "code": code })),
            retry_after: None,
        }
    }

    fn api(status: u16, code: &str) -> u8 {
        exit_code(&anyhow::Error::new(problem(status, code)))
    }

    fn core(e: CoreError) -> u8 {
        exit_code(&anyhow::Error::new(e))
    }

    fn failed(code: ActionErrorCode) -> CoreError {
        CoreError::ActionFailed(Box::new(ActionFailure {
            action_id: "act_1".into(),
            code,
            message: "m".into(),
            action_url: None,
            cluster_id: None,
            cleanup: None,
            replayed: false,
        }))
    }

    #[test]
    fn usage_is_2() {
        assert_eq!(core(CoreError::Usage("x".into())), 2);
        assert_eq!(
            core(CoreError::Ambiguous {
                name: "a".into(),
                ids: vec!["1".into(), "2".into()]
            }),
            2
        );
        let clap = clap::Error::new(clap::error::ErrorKind::UnknownArgument);
        assert_eq!(exit_code(&anyhow::Error::new(clap)), 2);
        assert_eq!(exit_code(&anyhow::Error::new(ConfigError::NoKeychain)), 2);
        // A value the API refused is a bad flag too.
        assert_eq!(api(400, "invalid_parameter"), 2);
        assert_eq!(core(CoreError::Api(problem(400, "invalid_parameter"))), 2);
    }

    #[test]
    fn not_allowed_is_3() {
        assert_eq!(api(401, "unauthorized"), 3);
        assert_eq!(api(403, "missing_scope"), 3);
        assert_eq!(api(403, "feature_not_enabled"), 3);
        assert_eq!(exit_code(&anyhow::Error::new(NotLoggedIn)), 3);
    }

    #[test]
    fn not_found_is_4() {
        assert_eq!(api(404, "not_found"), 4);
        assert_eq!(core(CoreError::ClusterNotFound("x".into())), 4);
        // Through CoreError::Api too.
        assert_eq!(core(CoreError::Api(problem(404, "not_found"))), 4);
    }

    #[test]
    fn action_failed_is_5() {
        assert_eq!(core(failed(ActionErrorCode::TimedOut)), 5);
        assert_eq!(core(failed(ActionErrorCode::ActionFailed)), 5);
        assert_eq!(core(failed(ActionErrorCode::Unknown("new_code".into()))), 5);
    }

    #[test]
    fn payment_is_6() {
        assert_eq!(api(422, "payment_method_required"), 6);
        assert_eq!(api(422, "payment_failed"), 6);
        assert_eq!(core(failed(ActionErrorCode::PaymentFailed)), 6);
    }

    #[test]
    fn busy_is_7() {
        assert_eq!(api(409, "action_in_progress"), 7);
        assert_eq!(api(409, "idempotency_key_in_use"), 7);
    }

    #[test]
    fn refused_is_8() {
        for code in [
            "validation_failed",
            "name_mismatch",
            "plan_change_refused",
            "cluster_terminated",
            "idempotency_key_reused",
        ] {
            assert_eq!(api(422, code), 8, "{code}");
        }
    }

    #[test]
    fn timeout_is_9() {
        let e = CoreError::Timeout {
            action_id: "a".into(),
            timeout: Duration::from_secs(1),
        };
        assert_eq!(core(e), 9);
    }

    /// A failed create whose cluster wasn't re-read: Ctrl-C is 130, the deadline 9,
    /// as anywhere else.
    #[test]
    fn an_unchecked_cleanup_takes_the_code_of_what_stopped_it() {
        use bonsai_core::{Cleanup, CleanupOutcome, StopReason};
        let unchecked = |reason| {
            let CoreError::ActionFailed(mut f) = failed(ActionErrorCode::ActionFailed) else {
                unreachable!()
            };
            f.cleanup = Some(Cleanup {
                cluster_id: "c1".into(),
                outcome: CleanupOutcome::Unchecked { reason },
            });
            core(CoreError::ActionFailed(f))
        };
        assert_eq!(unchecked(StopReason::Cancelled), 130);
        assert_eq!(unchecked(StopReason::Timeout), 9);
    }

    #[test]
    fn ctrl_c_during_a_wait_is_130() {
        assert_eq!(
            core(CoreError::Cancelled {
                action_id: "a".into()
            }),
            130
        );
        assert_eq!(core(CoreError::LoginCancelled), 130);
        assert_eq!(exit_code(&anyhow::Error::new(Interrupted)), 130);
    }

    #[test]
    fn everything_else_is_1() {
        assert_eq!(api(503, "metrics_unavailable"), 1);
        assert_eq!(api(500, "internal_error"), 1);
        assert_eq!(api(422, "a_code_from_the_future"), 1);
        assert_eq!(
            exit_code(&anyhow::Error::new(ApiError::Decode("x".into()))),
            1
        );
        assert_eq!(exit_code(&anyhow::anyhow!("plain")), 1);
    }

    #[test]
    fn wait_failed_takes_the_code_of_its_cause() {
        let e = CoreError::WaitFailed {
            action_id: "a".into(),
            source: Box::new(problem(401, "unauthorized")),
        };
        assert_eq!(core(e), 3);
        let e = CoreError::WaitFailed {
            action_id: "a".into(),
            source: Box::new(ApiError::Http {
                status: 502,
                body: String::new(),
            }),
        };
        assert_eq!(core(e), 1);
    }

    #[test]
    fn context_doesnt_hide_the_cause() {
        let e = anyhow::Error::new(problem(404, "not_found")).context("looking up x");
        assert_eq!(exit_code(&e), 4);
    }

    #[test]
    fn a_hint_doesnt_change_the_code() {
        let e = anyhow::Error::new(problem(422, "payment_failed"))
            .context(Hint(vec!["Then run: x".into()]));
        assert_eq!(exit_code(&e), 6);
        assert!(e.downcast_ref::<Hint>().is_some());
    }

    #[test]
    fn timed_out_is_9() {
        assert_eq!(exit_code(&anyhow::Error::new(TimedOut("x".into()))), 9);
    }

    #[test]
    fn interrupted_is_130() {
        assert_eq!(exit_code(&anyhow::Error::new(Interrupted)), 130);
    }

    #[test]
    fn problem_code_is_read_as_known() {
        // Guards the helper above: a known code deserializes to its variant.
        let ApiError::Problem { problem, .. } = problem(409, "action_in_progress") else {
            unreachable!()
        };
        assert_eq!(problem.code, ProblemCode::ActionInProgress);
    }

    #[test]
    fn a_clusters_status_decides_the_code() {
        let code = |status| {
            exit_code(&anyhow::Error::new(EngineStatus {
                status,
                reason: None,
            }))
        };
        assert_eq!(code(401), 3);
        assert_eq!(code(403), 3);
        assert_eq!(code(404), 4);
        for other in [400, 409, 413, 429, 500, 501, 503] {
            assert_eq!(code(other), 1, "{other}");
        }
        // A hint doesn't change it.
        let hinted = anyhow::Error::new(EngineStatus {
            status: 401,
            reason: None,
        })
        .context(Hint(vec!["x".into()]));
        assert_eq!(exit_code(&hinted), 3);
    }

    #[test]
    fn a_clusters_status_reads_as_one_line() {
        let e = EngineStatus {
            status: 404,
            reason: Some("no such index [products]".into()),
        };
        assert_eq!(
            e.to_string(),
            "the cluster answered 404 Not Found: no such index [products]"
        );
        let bare = EngineStatus {
            status: 599,
            reason: None,
        };
        assert_eq!(bare.to_string(), "the cluster answered 599");
    }

    #[test]
    fn a_status_reads_with_its_phrase() {
        assert_eq!(status_text(200), "200 OK");
        assert_eq!(status_text(204), "204 No Content");
        assert_eq!(status_text(299), "299");
    }

    #[test]
    fn engine_errors_have_codes() {
        let code = |e: EngineError| exit_code(&anyhow::Error::new(e));
        assert_eq!(code(EngineError::InvalidPath("x".into())), 2);
        assert_eq!(code(EngineError::Timeout), 9);
        assert_eq!(code(EngineError::TooLarge { max: 1 }), 1);
        assert_eq!(code(EngineError::InsecureUrl), 1);
        // Not `exit::Interrupted` (Ctrl-C, 130): a connection that broke.
        let cause = || {
            bonsai_api::http_client()
                .build()
                .unwrap()
                .get("not a url")
                .build()
                .unwrap_err()
        };
        assert_eq!(code(EngineError::Unreachable(cause())), 1);
        assert_eq!(code(EngineError::Interrupted(cause())), 1);
    }
}
