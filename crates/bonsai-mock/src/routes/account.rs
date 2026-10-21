//! `GET /v1/account` (`getAccount`).

use axum::extract::State;
use axum::routing::get;
use axum::{Json, Router};
use bonsai_api::Account;

use crate::http::{Auth, Op};
use crate::problem::ProblemBuilder;
use crate::{AppState, db};

pub fn router() -> Router<AppState> {
    Router::new().route("/v1/account", get(show))
}

async fn show(State(state): State<AppState>, auth: Auth) -> Result<Json<Account>, ProblemBuilder> {
    auth.require(Op::ReadAccount)?;
    let account = state.db(|c| {
        c.query_row(
            "SELECT id, name, contact_email_override, created_at FROM accounts WHERE id = ?1",
            [&auth.account_id],
            |r| {
                Ok(Account {
                    id: r.get(0)?,
                    name: r.get(1)?,
                    contact_email_override: r.get(2)?,
                    created_at: db::ts(r, 3)?,
                })
            },
        )
    })?;
    Ok(Json(account))
}
