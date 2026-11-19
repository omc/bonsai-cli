//! `POST /v1/support_requests`: a question, a problem or a feature request for
//! the support team. It needs no scope: any token may ask for help.
//!
//! The mock sends nothing anywhere. It checks the body as the Bonsai API does
//! and answers with an ID. The `support_unavailable` scenario, while `true`,
//! makes it answer `503 support_unavailable`, as when the support desk doesn't
//! take the request.

use axum::extract::State;
use axum::http::StatusCode;
use axum::routing::post;
use axum::{Json, Router};
use bonsai_api::{ProblemCode, SupportRequest};

use crate::http::{Auth, JsonBody, string_body};
use crate::problem::{ProblemBuilder, status_problem};
use crate::routes::clusters::ClusterRow;
use crate::{AppState, ids, scenario};

const FIELDS: &[&str] = &[
    "subject", "message", "category", "kind", "cluster", "context",
];
const REQUIRED: &[&str] = &["subject", "message", "category"];
pub const CATEGORIES: &[&str] = &[
    "cluster_availability",
    "search_advice",
    "billing",
    "compliance",
    "sales",
];
pub const KINDS: &[&str] = &["question", "problem", "feature_request"];
const DEFAULT_KIND: &str = "question";
const SUBJECT_MAX_LENGTH: usize = 200;
const MESSAGE_MAX_LENGTH: usize = 10_000;
const CONTEXT_MAX_LENGTH: usize = 5_000;
/// `Retry-After` on 503 `support_unavailable`, as the Bonsai API sends it.
const UNAVAILABLE_RETRY_AFTER: u64 = 60;

pub fn router() -> Router<AppState> {
    Router::new().route("/v1/support_requests", post(create))
}

async fn create(
    State(state): State<AppState>,
    auth: Auth,
    body: Result<JsonBody, ProblemBuilder>,
) -> Result<(StatusCode, Json<SupportRequest>), ProblemBuilder> {
    let body = string_body(body?.0, FIELDS, REQUIRED)?;
    let subject = body["subject"].trim().to_owned();
    let message = &body["message"];
    let category = &body["category"];
    let kind = body.get("kind").map_or(DEFAULT_KIND, String::as_str);
    let length = |text: &str| text.encode_utf16().count();

    let mut faults: Vec<(&str, String)> = Vec::new();
    if subject.is_empty() {
        faults.push(("/subject", "subject can't be blank.".into()));
    }
    if subject.chars().count() > SUBJECT_MAX_LENGTH {
        faults.push((
            "/subject",
            format!("subject can be at most {SUBJECT_MAX_LENGTH} characters."),
        ));
    }
    if message.trim().is_empty() {
        faults.push(("/message", "message can't be blank.".into()));
    }
    if length(message) > MESSAGE_MAX_LENGTH {
        faults.push((
            "/message",
            "message can be at most 10,000 characters.".into(),
        ));
    }
    if !CATEGORIES.contains(&category.as_str()) {
        faults.push((
            "/category",
            format!("category must be one of: {}.", CATEGORIES.join(", ")),
        ));
    }
    if !KINDS.contains(&kind) {
        faults.push((
            "/kind",
            format!("kind must be one of: {}.", KINDS.join(", ")),
        ));
    }
    if let Some(id) = body.get("cluster") {
        let active = state
            .db(|c| ClusterRow::find(c, &auth.account_id, id))?
            .is_some_and(|row| row.terminated_at.is_none());
        if !active {
            faults.push((
                "/cluster",
                "No active cluster of this account has that ID.".into(),
            ));
        }
    }
    if body
        .get("context")
        .is_some_and(|c| length(c) > CONTEXT_MAX_LENGTH)
    {
        faults.push((
            "/context",
            "context can be at most 5,000 characters.".into(),
        ));
    }
    if !faults.is_empty() {
        return Err(faults.into_iter().fold(
            status_problem(
                StatusCode::UNPROCESSABLE_ENTITY,
                ProblemCode::ValidationFailed,
                "The request was refused. See details.",
            ),
            |problem, (field, message)| problem.field(field, message),
        ));
    }

    let down = state
        .db(|c| scenario::get(c, "support_unavailable"))?
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    if down {
        return Err(status_problem(
            StatusCode::SERVICE_UNAVAILABLE,
            ProblemCode::SupportUnavailable,
            "The request couldn't be delivered to the support team. Try again shortly, or write to support@bonsai.io.",
        )
        .retry_after(UNAVAILABLE_RETRY_AFTER));
    }

    Ok((
        StatusCode::CREATED,
        Json(SupportRequest {
            id: format!("th_{}", ids::lower_alnum(26)),
            subject,
            category: category.clone(),
            kind: kind.to_owned(),
        }),
    ))
}
