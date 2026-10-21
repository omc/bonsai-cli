//! The error report on stderr: `✗ <headline>`, what the Problem adds, then the
//! error's [`Hint`] lines; or, under `--json`, the Problem as it came (or
//! `{"error": ...}`, with `cleanup` after a failed create).
//!
//! A Problem's headline is the CLI's own for the codes it knows how to explain
//! (`payment_method_required`, `payment_failed`, `name_mismatch`); otherwise its
//! `detail`, falling back to its `title`, with the code. The contract says to
//! branch on `code`, not `detail`, whose text may name API fields.

use std::io::Write;

use bonsai_api::{ActionErrorCode, ApiError, Problem, ProblemCode};
use bonsai_core::CoreError;
use url::Url;

use crate::exit::{Explained, Hint};
use crate::output::{self, Printer};
use crate::render_cluster;

pub fn report(out: &Printer, json: bool, e: &anyhow::Error, api_url: Option<&Url>) {
    let (headline, lines) = human(e, api_url);
    if json {
        // The Problem as the server sent it, unknown fields included.
        let v = match find_problem(e) {
            Some((_, raw)) => raw.clone(),
            None => {
                let mut v = serde_json::json!({ "error": headline });
                if let Some(cleanup) = cleanup_json(e) {
                    v["cleanup"] = cleanup;
                }
                v
            }
        };
        let _ = writeln!(std::io::stderr(), "{}", output::json_string(&v));
        return;
    }
    out.error(&headline);
    for line in lines {
        out.notice(&line);
    }
}

/// The headline, and the lines printed under it.
pub fn human(e: &anyhow::Error, api_url: Option<&Url>) -> (String, Vec<String>) {
    let hint = e.downcast_ref::<Hint>();
    // The hint is the outermost context; it isn't part of the message.
    let skip = usize::from(hint.is_some());
    // A context isn't its own type in the chain, so the explanation is found by
    // its text.
    let explanation = e.downcast_ref::<Explained>().map(|x| x.0.as_str());
    let mut parts = Vec::new();
    let mut explained = false;
    for cause in e.chain().skip(skip) {
        let text = cause.to_string();
        if explanation == Some(text.as_str()) {
            // Says all the causes below it would.
            parts.push(text);
            explained = true;
            break;
        }
        if let Some(api) = api_of_problem(cause) {
            parts.push(api_headline(api));
            break;
        }
        match cause.downcast_ref::<CoreError>() {
            Some(CoreError::ActionFailed(f)) if f.code == ActionErrorCode::PaymentFailed => {
                parts.push(format!(
                    "action {} failed: the card on file was refused",
                    f.action_id
                ));
                break;
            }
            _ => parts.push(text),
        }
    }
    let mut headline = parts.join(": ");
    let problem = find_problem(e).map(|(p, _)| p);
    // Bonsai's /v1 answers every 404 with a Problem; a bare one is something else.
    if let Some(url) = api_url
        && problem.is_none()
        && bare_404(e)
    {
        headline = format!("{} ({headline})", no_api_at(url));
    }

    let mut lines = Vec::new();
    if let Some(p) = problem {
        if !explained && !owns_wording(&p.code) {
            for d in p.details.iter().filter(|d| d.message != p.detail) {
                lines.push(match &d.field {
                    Some(field) => format!("  {field}: {}", d.message),
                    None => format!("  {}", d.message),
                });
            }
        }
        if let Some(url) = &p.action_url {
            lines.push(format!("  {url}"));
        }
    }
    lines.extend(hint.map_or(&[][..], |h| &h.0).iter().cloned());
    (headline, lines)
}

/// What to say about an API that answered a bare 404 at `url`.
pub fn no_api_at(url: &Url) -> String {
    format!("no Bonsai /v1 API at {url}; check --api-url or BONSAI_API_URL")
}

/// Codes whose headline is the CLI's, with nothing of the Problem's under it.
fn owns_wording(code: &ProblemCode) -> bool {
    matches!(
        code,
        ProblemCode::PaymentMethodRequired
            | ProblemCode::PaymentFailed
            | ProblemCode::NameMismatch
            | ProblemCode::TooManyFailedAuthentications
    )
}

/// The headline for a Problem, with what the response around it adds
/// (`Retry-After`).
fn api_headline(api: &ApiError) -> String {
    match api {
        ApiError::Problem {
            problem,
            retry_after,
            ..
        } if problem.code == ProblemCode::TooManyFailedAuthentications => {
            let when = retry_after.map_or_else(
                || "try again later".to_owned(),
                |d| format!("try again in {} s", d.as_secs().max(1)),
            );
            format!("too many failed logins from this machine; {when}")
        }
        ApiError::Problem { problem, .. } => problem_headline(problem),
        other => other.to_string(),
    }
}

fn problem_headline(p: &Problem) -> String {
    match p.code {
        // A create and a plan change both answer it: the words fit both, and the
        // command's hint says what to run once the card is on file.
        ProblemCode::PaymentMethodRequired => "This plan needs a card on file.".into(),
        // Neutral too: `v1.yml` documents it for creates, but its shared 422 lists
        // it for a plan change as well.
        ProblemCode::PaymentFailed => "The card on file was refused; nothing was done.".into(),
        ProblemCode::NameMismatch => format!(
            "--confirm doesn't match the cluster's current name; pass it exactly ({})",
            p.code
        ),
        _ => {
            let text = if p.detail.trim().is_empty() {
                p.title.as_str()
            } else {
                p.detail.as_str()
            };
            let text = text.trim();
            format!("{} ({})", text.strip_suffix('.').unwrap_or(text), p.code)
        }
    }
}

/// What became of the cluster a failed create left, for `--json` errors:
/// `{cluster_id, outcome, deprovision_action_id}` (see
/// [`render_cluster::cleanup_outcome`]).
fn cleanup_json(e: &anyhow::Error) -> Option<serde_json::Value> {
    e.chain()
        .find_map(|cause| match cause.downcast_ref::<CoreError>() {
            Some(CoreError::ActionFailed(f)) => {
                let (outcome, cluster_id, dep) = render_cluster::cleanup_outcome(f)?;
                Some(serde_json::json!({
                    "cluster_id": cluster_id,
                    "outcome": outcome,
                    "deprovision_action_id": dep,
                }))
            }
            _ => None,
        })
}

/// The API error one cause in a chain is, or carries.
fn api_of<'a>(cause: &'a (dyn std::error::Error + 'static)) -> Option<&'a ApiError> {
    match cause.downcast_ref::<CoreError>() {
        Some(CoreError::Api(api)) => Some(api),
        Some(CoreError::WaitFailed { source, .. }) => Some(source),
        Some(_) => None,
        None => cause.downcast_ref::<ApiError>(),
    }
}

/// The Problem-carrying API error one cause is, not one it merely carries as
/// its source.
fn api_of_problem<'a>(cause: &'a (dyn std::error::Error + 'static)) -> Option<&'a ApiError> {
    let api = match cause.downcast_ref::<CoreError>() {
        Some(CoreError::Api(api)) => api,
        Some(_) => return None,
        None => cause.downcast_ref::<ApiError>()?,
    };
    matches!(api, ApiError::Problem { .. }).then_some(api)
}

/// Whether `e` is a 404 that came without a Problem body.
pub fn bare_404(e: &anyhow::Error) -> bool {
    e.chain()
        .any(|cause| api_of(cause).is_some_and(is_bare_404))
}

/// Whether this API error is a 404 that came without a Problem body.
pub fn is_bare_404(e: &ApiError) -> bool {
    matches!(e, ApiError::Http { status: 404, .. })
}

/// The Problem anywhere in `e`'s chain, parsed and as it came.
fn find_problem(e: &anyhow::Error) -> Option<(&Problem, &serde_json::Value)> {
    e.chain().find_map(|cause| match api_of(cause)? {
        ApiError::Problem { problem, raw, .. } => Some((&**problem, &**raw)),
        _ => None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn problem(status: u16, code: &str, detail: &str, details: serde_json::Value) -> ApiError {
        let raw = json!({
            "type": "about:blank", "title": "Unprocessable Entity", "status": status,
            "code": code, "detail": detail, "details": details,
            "action_url": if code.starts_with("payment") { json!("https://checkout.stripe.com/c/x") } else { json!(null) },
        });
        ApiError::Problem {
            status,
            problem: Box::new(serde_json::from_value(raw.clone()).unwrap()),
            raw: Box::new(raw),
            retry_after: None,
        }
    }

    fn shown(e: ApiError) -> (String, Vec<String>) {
        human(&anyhow::Error::new(CoreError::Api(e)), None)
    }

    #[test]
    fn a_problem_leads_with_its_detail_and_says_it_once() {
        let (headline, lines) = shown(problem(
            400,
            "invalid_parameter",
            "limit must be an integer from 1 to 100.",
            json!([{"field": "limit", "message": "limit must be an integer from 1 to 100."}]),
        ));
        assert_eq!(
            headline,
            "limit must be an integer from 1 to 100 (invalid_parameter)"
        );
        assert!(lines.is_empty(), "{lines:?}");

        let (headline, lines) = shown(problem(
            400,
            "invalid_parameter",
            "The body doesn't match its schema. See details.",
            json!([{"field": "/name", "message": "name is required."}]),
        ));
        assert!(headline.starts_with("The body doesn't match"), "{headline}");
        assert_eq!(lines, ["  /name: name is required."]);
    }

    #[test]
    fn a_problem_without_detail_falls_back_to_its_title() {
        let (headline, _) = shown(problem(422, "validation_failed", "", json!([])));
        assert_eq!(headline, "Unprocessable Entity (validation_failed)");
    }

    #[test]
    fn payment_refusals_use_the_clis_words() {
        let (headline, lines) = shown(problem(
            422,
            "payment_method_required",
            "This plan needs a payment method. Add a card at action_url.",
            json!([]),
        ));
        assert_eq!(headline, "This plan needs a card on file.");
        assert_eq!(lines, ["  https://checkout.stripe.com/c/x"]);

        let (headline, lines) = shown(problem(
            422,
            "payment_failed",
            "Bonsai couldn't charge the account for this cluster. Add a card at action_url.",
            json!([]),
        ));
        assert_eq!(headline, "The card on file was refused; nothing was done.");
        assert_eq!(lines, ["  https://checkout.stripe.com/c/x"]);
        assert!(!headline.contains("action_url") && !headline.contains("Unprocessable"));
    }

    #[test]
    fn name_mismatch_names_the_flag() {
        let (headline, lines) = shown(problem(
            422,
            "name_mismatch",
            "`name` doesn't match the cluster's current name.",
            json!([{"field": "name", "message": "name must equal the cluster's current name exactly."}]),
        ));
        assert!(
            headline.starts_with("--confirm doesn't match"),
            "{headline}"
        );
        assert!(headline.ends_with("(name_mismatch)"), "{headline}");
        assert!(lines.is_empty(), "{lines:?}");
    }

    #[test]
    fn an_explanation_replaces_the_problem_below_it() {
        let e = anyhow::Error::new(problem(
            503,
            "metrics_unavailable",
            "Metrics are unavailable right now.",
            json!([]),
        ))
        .context(Explained(
            "metrics are unavailable right now, try again in 30 s".into(),
        ));
        let (headline, lines) = human(&e, None);
        assert_eq!(
            headline,
            "metrics are unavailable right now, try again in 30 s"
        );
        assert!(lines.is_empty(), "{lines:?}");
    }

    #[test]
    fn a_failed_login_lockout_says_when_to_try_again() {
        let mut e = problem(
            429,
            "too_many_failed_authentications",
            "Too many failed authentication attempts. Try again later.",
            json!([]),
        );
        if let ApiError::Problem { retry_after, .. } = &mut e {
            *retry_after = Some(std::time::Duration::from_secs(60));
        }
        let (headline, lines) = shown(e);
        assert_eq!(
            headline,
            "too many failed logins from this machine; try again in 60 s"
        );
        assert!(lines.is_empty(), "{lines:?}");
    }

    #[test]
    fn an_action_level_payment_failure_uses_the_clis_words() {
        let f = bonsai_core::ActionFailure {
            action_id: "act_1".into(),
            code: ActionErrorCode::PaymentFailed,
            message: "Bonsai couldn't charge the account. Add a card at action_url.".into(),
            action_url: Some("https://checkout.stripe.com/c/x".into()),
            cluster_id: None,
            cleanup: None,
            replayed: false,
        };
        let e = anyhow::Error::new(CoreError::ActionFailed(Box::new(f)));
        let (headline, _) = human(&e, None);
        assert_eq!(
            headline,
            "action act_1 failed: the card on file was refused"
        );
    }
}
