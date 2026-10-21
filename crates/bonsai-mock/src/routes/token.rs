//! `GET /v1/token` and `DELETE /v1/token`, proposed and not in `v1.yml`
//! (a proposed addition to the contract): the calling token's own name, scopes, expiry and
//! key, and revoking it. Neither needs a scope; a token only ever reaches
//! itself.

use axum::extract::State;
use axum::http::StatusCode;
use axum::routing::get;
use axum::{Json, Router};
use bonsai_api::TokenInfo;
use rusqlite::params;

use crate::http::Auth;
use crate::problem::ProblemBuilder;
use crate::{AppState, db};

pub fn router() -> Router<AppState> {
    Router::new().route("/v1/token", get(show).delete(revoke))
}

async fn show(
    State(state): State<AppState>,
    auth: Auth,
) -> Result<Json<TokenInfo>, ProblemBuilder> {
    let info = state.db(|c| {
        c.query_row(
            "SELECT name, scopes, expires_at, key_prefix FROM tokens WHERE id = ?1",
            [auth.token_id],
            |r| {
                Ok(TokenInfo {
                    name: r.get(0)?,
                    scopes: db::json(r, 1)?,
                    expires_at: db::ts_opt(r, 2)?,
                    key_prefix: r.get(3)?,
                })
            },
        )
    })?;
    Ok(Json(info))
}

/// 204; the token answers 401 from then on.
async fn revoke(State(state): State<AppState>, auth: Auth) -> Result<StatusCode, ProblemBuilder> {
    state.db(|c| {
        c.execute(
            "UPDATE tokens SET revoked_at = ?2 WHERE id = ?1",
            params![auth.token_id, db::now()],
        )
    })?;
    Ok(StatusCode::NO_CONTENT)
}
