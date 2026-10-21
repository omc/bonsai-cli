//! Actions (`listClusterActions`, `getAction`), and the `202` with which the
//! operations that start one answer, idempotently.

use std::collections::BTreeMap;

use axum::extract::{Path, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::{Json, Router};
use bonsai_api::{Action, Page};
use jiff::Timestamp;
use rusqlite::Connection;

use crate::AppState;
use crate::engine::{self, ActionRow};
use crate::http::{
    self, Auth, Idempotency, Op, PageParams, QueryParams, Recordable, paginate, replay_unavailable,
};
use crate::problem::{ProblemBuilder, not_found};
use crate::routes::clusters::ClusterRow;

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/v1/actions/{action_id}", get(show))
        .route("/v1/clusters/{cluster_id}/actions", get(list))
}

/// An Action the operation started (or replayed), to answer with [`accept`].
pub struct Accepted {
    pub action: ActionRow,
    pub replayed: bool,
}

impl Recordable for ActionRow {
    const KIND: &'static str = "action";
    fn id(&self) -> &str {
        &self.id
    }
}

/// Runs `start` (which starts an Action, or refuses) unless the request's
/// `Idempotency-Key` says otherwise, with the database locked throughout:
///
/// - a key seen with this token in the last 24 h, on the same request, replays
///   the Action it started, as it is now (409 `idempotency_replay_unavailable`
///   if it's gone);
/// - on another request, it's 422 `idempotency_key_reused`;
/// - otherwise `start` runs, and the key is recorded only if it succeeded.
///
/// `body` is the request's fields as the operation accepted them.
pub fn start_idempotently(
    state: &AppState,
    auth: &Auth,
    headers: &HeaderMap,
    method: &str,
    path: &str,
    body: &BTreeMap<String, String>,
    start: impl FnOnce(&Connection) -> Result<ActionRow, ProblemBuilder>,
) -> Result<Accepted, ProblemBuilder> {
    let key = Idempotency::from_request(auth, headers, method, path, body)?;
    let made = state.db(|c| {
        let replay = |c: &Connection, id: Option<&str>| {
            let gone = || {
                replay_unavailable(
                    "The request with this Idempotency-Key succeeded, but what it started no longer exists.",
                )
            };
            let id = id.ok_or_else(gone)?;
            engine::advance(c, id, Timestamp::now(), state.step_ms())?;
            ActionRow::find(c, id)?.ok_or_else(gone)
        };
        http::idempotently(c, key.as_ref(), StatusCode::ACCEPTED, replay, start)
    })?;
    Ok(Accepted {
        action: made.value,
        replayed: made.replayed,
    })
}

/// `202` with the Action, its URL as `Location`, `Retry-After` while it runs,
/// and `Idempotent-Replayed: true` on a replay.
pub fn accept(state: &AppState, accepted: Accepted) -> Response {
    let Accepted { action, replayed } = accepted;
    let mut resp = action_response(state, &action, StatusCode::ACCEPTED);
    let headers = resp.headers_mut();
    let location = state.url(&format!("/v1/actions/{}", action.id));
    headers.insert(
        header::LOCATION,
        HeaderValue::from_str(&location).expect("ASCII URL"),
    );
    if replayed {
        headers.insert("idempotent-replayed", HeaderValue::from_static("true"));
    }
    resp
}

/// The Action as JSON, with `Retry-After` while it's `new` or `running`.
fn action_response(state: &AppState, action: &ActionRow, status: StatusCode) -> Response {
    let mut resp = (status, Json::<Action>(action.to_api())).into_response();
    if let Some(secs) = action.retry_after(Timestamp::now(), state.step_ms()) {
        resp.headers_mut()
            .insert(header::RETRY_AFTER, HeaderValue::from(secs));
    }
    resp
}

/// `^act_[0-9a-z]{16}$`, the only Action IDs there are.
fn is_action_id(id: &str) -> bool {
    id.strip_prefix("act_").is_some_and(|rest| {
        rest.len() == 16
            && rest
                .bytes()
                .all(|b| b.is_ascii_digit() || b.is_ascii_lowercase())
    })
}

async fn show(
    State(state): State<AppState>,
    auth: Auth,
    Path(id): Path<String>,
) -> Result<Response, ProblemBuilder> {
    auth.require(Op::ReadClusters)?;
    if !is_action_id(&id) {
        return Err(not_found());
    }
    let action = state.db(|c| -> Result<ActionRow, ProblemBuilder> {
        match ActionRow::find(c, &id)? {
            Some(a) if a.account_id == auth.account_id => {}
            _ => return Err(not_found()),
        }
        engine::advance(c, &id, Timestamp::now(), state.step_ms())?;
        ActionRow::find(c, &id)?.ok_or_else(not_found)
    })?;
    Ok(action_response(&state, &action, StatusCode::OK))
}

async fn list(
    State(state): State<AppState>,
    auth: Auth,
    Path(cluster_id): Path<String>,
    q: QueryParams,
) -> Result<Json<Page<Action>>, ProblemBuilder> {
    auth.require(Op::ReadClusters)?;
    let page = PageParams::from_query(&q)?;
    let actions = state.db(|c| -> Result<Vec<Action>, ProblemBuilder> {
        ClusterRow::find_current(c, &auth, &cluster_id, state.step_ms())?;
        Ok(ActionRow::for_cluster(c, &cluster_id)?
            .iter()
            .map(ActionRow::to_api)
            .collect())
    })?;
    Ok(Json(paginate(actions, page)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn action_id_pattern() {
        assert!(is_action_id("act_0123456789abcdef"));
        assert!(!is_action_id("act_0123456789abcde"));
        assert!(!is_action_id("act_0123456789ABCDEF"));
        assert!(!is_action_id("123"));
    }
}
