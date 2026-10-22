//! `GET /v1/token` and `DELETE /v1/token`: the calling token's own key, name,
//! account, scopes and expiry, and revoking it. Neither needs a scope; a token
//! only ever reaches itself.
//!
//! Every mock token has a stored name (`Bonsai CLI on <hostname>` from a
//! browser login, or the name it was created with), so `name` is never null
//! here, though the contract allows it.

use axum::extract::State;
use axum::http::StatusCode;
use axum::routing::get;
use axum::{Json, Router};
use bonsai_api::{Ref, Token};
use rusqlite::params;

use crate::http::Auth;
use crate::problem::ProblemBuilder;
use crate::{AppState, db};

pub fn router() -> Router<AppState> {
    Router::new().route("/v1/token", get(show).delete(revoke))
}

async fn show(State(state): State<AppState>, auth: Auth) -> Result<Json<Token>, ProblemBuilder> {
    let token = state.db(|c| {
        c.query_row(
            "SELECT t.key_prefix, t.name, a.id, a.name, t.scopes, t.expires_at, t.created_at
             FROM tokens t JOIN accounts a ON a.id = t.account_id
             WHERE t.id = ?1",
            [auth.token_id],
            |r| {
                Ok(Token {
                    key: r.get(0)?,
                    name: r.get(1)?,
                    account: Ref {
                        id: r.get(2)?,
                        name: r.get(3)?,
                    },
                    scopes: db::json(r, 4)?,
                    expires_at: db::ts_opt(r, 5)?,
                    created_at: db::ts(r, 6)?,
                })
            },
        )
    })?;
    Ok(Json(token))
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
