//! `/_mock`: test knobs that aren't part of any API, outside `/v1`'s rate
//! limits.
//!
//! The admin API needs `Authorization: Bearer <admin token>` (printed at
//! start, new each time):
//!
//! - `GET /_mock/scenario`: every scenario set, as an object;
//! - `PUT /_mock/scenario/{key}` with a JSON value, `DELETE /_mock/scenario/{key}`
//!   (see [`crate::scenario`] for the keys);
//! - `PUT /_mock/accounts/{id}/payment_method` with `true` or `false`;
//! - `POST /_mock/cli/logins/{user_code}/approve` with
//!   `{account, scopes, expires_in_days}` (each optional: `acme`, the
//!   approval page's default scopes, 90), and `.../deny`;
//! - `POST /_mock/clusters/{id}/stats` with `{docs, shards, bytes}`;
//! - `DELETE /_mock/rate_limits`: forgets every rate-limit counter, failed
//!   authentications included.
//!
//! Each answers 204, or a Problem. A few pages stand in for the dashboard's, so
//! a browser reaches them without a token:
//!
//! - `GET /_mock/checkout/{account}` shows an "Add card" button, which POSTs
//!   back and gives the account a payment method;
//! - `GET /_mock/clusters/{id}`, a cluster's page (its `dashboard_url`);
//! - `GET /_mock/api_tokens`, the API Tokens page (`GET /v1`'s `tokens_url`).

use axum::body::Bytes;
use axum::extract::{FromRequestParts, Path, State};
use axum::http::request::Parts;
use axum::http::{StatusCode, header};
use axum::response::Response;
use axum::routing::{get, post, put};
use axum::{Json, Router};
use bonsai_api::ProblemCode;
use rusqlite::params;
use serde_json::Value;

use crate::http::{AnyJsonBody, not_an_object};
use crate::problem::{ProblemBuilder, invalid_parameter, not_found, status_problem, unauthorized};
use crate::routes::login::{self, Decision};
use crate::{AppState, db, html, scenario, seed};

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/_mock/scenario", get(show_scenario))
        .route(
            "/_mock/scenario/{key}",
            put(set_scenario).delete(clear_scenario),
        )
        .route(
            "/_mock/accounts/{account}/payment_method",
            put(set_payment_method),
        )
        .route("/_mock/cli/logins/{user_code}/approve", post(approve))
        .route("/_mock/cli/logins/{user_code}/deny", post(deny))
        .route("/_mock/clusters/{cluster}", get(cluster_page))
        .route("/_mock/clusters/{cluster}/stats", post(set_stats))
        .route("/_mock/api_tokens", get(api_tokens_page))
        .route(
            "/_mock/rate_limits",
            axum::routing::delete(reset_rate_limits),
        )
        .route("/_mock/checkout/{account}", get(checkout).post(add_card))
}

/// A request carrying the admin token; 401 otherwise.
pub struct Admin;

impl FromRequestParts<AppState> for Admin {
    type Rejection = ProblemBuilder;

    async fn from_request_parts(
        parts: &mut Parts,
        state: &AppState,
    ) -> Result<Self, Self::Rejection> {
        let token = parts
            .headers
            .get(header::AUTHORIZATION)
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.split_once(' '))
            .filter(|(scheme, _)| scheme.eq_ignore_ascii_case("bearer"))
            .map(|(_, t)| t.trim());
        // Compared by hash, so the time taken doesn't depend on how much of
        // the token matched.
        match token {
            Some(t) if seed::sha256_hex(t) == seed::sha256_hex(state.admin_token()) => Ok(Admin),
            _ => Err(unauthorized()),
        }
    }
}

// ---------------------------------------------------------------- scenario

async fn show_scenario(
    State(state): State<AppState>,
    _: Admin,
) -> Result<Json<Value>, ProblemBuilder> {
    let all = state.db(scenario::all)?;
    Ok(Json(Value::Object(all.into_iter().collect())))
}

fn scenario_problem(key: &str, message: String) -> ProblemBuilder {
    invalid_parameter(key, message)
}

async fn set_scenario(
    State(state): State<AppState>,
    _: Admin,
    Path(key): Path<String>,
    body: Result<AnyJsonBody, ProblemBuilder>,
) -> Result<StatusCode, ProblemBuilder> {
    let value = body?.0;
    scenario::validate(&key, &value).map_err(|m| scenario_problem(&key, m))?;
    state.db(|c| scenario::set(c, &key, &value))?;
    Ok(StatusCode::NO_CONTENT)
}

async fn reset_rate_limits(State(state): State<AppState>, _: Admin) -> StatusCode {
    state.limiter().reset();
    StatusCode::NO_CONTENT
}

async fn clear_scenario(
    State(state): State<AppState>,
    _: Admin,
    Path(key): Path<String>,
) -> Result<StatusCode, ProblemBuilder> {
    if !scenario::KEYS.contains(&key.as_str()) {
        return Err(scenario_problem(
            &key,
            format!(
                "unknown scenario {key}; scenarios are: {}",
                scenario::KEYS.join(", ")
            ),
        ));
    }
    state.db(|c| scenario::clear(c, &key))?;
    Ok(StatusCode::NO_CONTENT)
}

// ---------------------------------------------------------------- accounts and clusters

async fn set_payment_method(
    State(state): State<AppState>,
    _: Admin,
    Path(account): Path<String>,
    body: Result<AnyJsonBody, ProblemBuilder>,
) -> Result<StatusCode, ProblemBuilder> {
    let Some(has) = body?.0.as_bool() else {
        return Err(invalid_parameter("/", "The body must be true or false."));
    };
    let updated = state.db(|c| {
        c.execute(
            "UPDATE accounts SET has_payment_method = ?2 WHERE id = ?1",
            params![account, has],
        )
    })?;
    if updated == 0 {
        return Err(not_found());
    }
    Ok(StatusCode::NO_CONTENT)
}

async fn set_stats(
    State(state): State<AppState>,
    _: Admin,
    Path(cluster): Path<String>,
    body: Result<AnyJsonBody, ProblemBuilder>,
) -> Result<StatusCode, ProblemBuilder> {
    let body = body?.0;
    let mut values = [0u64; 3];
    for (slot, field) in values.iter_mut().zip(["docs", "shards", "bytes"]) {
        // SQLite stores i64, so that's the largest value.
        *slot = body
            .get(field)
            .and_then(Value::as_u64)
            .filter(|v| i64::try_from(*v).is_ok())
            .ok_or_else(|| {
                invalid_parameter(
                    &format!("/{field}"),
                    format!(
                        "{field} is required, as a whole number from 0 to {}.",
                        i64::MAX
                    ),
                )
            })?;
    }
    let [docs, shards, bytes] = values;
    let updated = state.db(|c| {
        c.execute(
            "UPDATE clusters SET docs = ?2, shards = ?3, bytes = ?4, polled_at = ?5 WHERE id = ?1",
            params![cluster, docs, shards, bytes, db::now()],
        )
    })?;
    if updated == 0 {
        return Err(not_found());
    }
    Ok(StatusCode::NO_CONTENT)
}

// ---------------------------------------------------------------- logins

/// `{account, scopes, expires_in_days}`, each optional; no body is `{}`.
fn approval(body: &Bytes) -> Result<Decision, ProblemBuilder> {
    let value: Value = if body.iter().all(u8::is_ascii_whitespace) {
        Value::Object(Default::default())
    } else {
        serde_json::from_slice(body).map_err(|_| {
            status_problem(
                StatusCode::BAD_REQUEST,
                ProblemCode::InvalidJson,
                "The request body is not valid JSON.",
            )
        })?
    };
    let Value::Object(obj) = value else {
        return Err(status_problem(
            StatusCode::BAD_REQUEST,
            ProblemCode::InvalidParameter,
            "The body must be a JSON object.",
        )
        .push_detail(not_an_object()));
    };
    let account = match obj.get("account") {
        None => login::DEFAULT_ACCOUNT.to_string(),
        Some(Value::String(s)) => s.clone(),
        Some(_) => return Err(invalid_parameter("/account", "account must be a string.")),
    };
    let scopes = match obj.get("scopes") {
        None => login::DEFAULT_SCOPES
            .iter()
            .map(|s| s.to_string())
            .collect(),
        Some(Value::Array(a)) if a.iter().all(Value::is_string) => a
            .iter()
            .filter_map(|v| v.as_str().map(str::to_string))
            .collect(),
        Some(_) => {
            return Err(invalid_parameter(
                "/scopes",
                "scopes must be an array of strings.",
            ));
        }
    };
    let expires_in_days = match obj.get("expires_in_days") {
        None => login::DEFAULT_EXPIRY_DAYS,
        Some(v) => v
            .as_u64()
            .and_then(|d| u32::try_from(d).ok())
            .ok_or_else(|| {
                invalid_parameter("/expires_in_days", "expires_in_days must be 30, 90 or 365.")
            })?,
    };
    Ok(Decision::Approve {
        account,
        scopes,
        expires_in_days,
    })
}

async fn approve(
    State(state): State<AppState>,
    _: Admin,
    Path(user_code): Path<String>,
    body: Bytes,
) -> Result<StatusCode, ProblemBuilder> {
    let decision = approval(&body)?;
    state.db(|c| login::decide(c, &user_code, &decision))?;
    Ok(StatusCode::NO_CONTENT)
}

async fn deny(
    State(state): State<AppState>,
    _: Admin,
    Path(user_code): Path<String>,
) -> Result<StatusCode, ProblemBuilder> {
    state.db(|c| login::decide(c, &user_code, &Decision::Deny))?;
    Ok(StatusCode::NO_CONTENT)
}

// ---------------------------------------------------------------- checkout

/// The account's name, if it exists.
fn account_name(state: &AppState, account: &str) -> Result<Option<String>, ProblemBuilder> {
    use rusqlite::OptionalExtension;
    Ok(state.db(|c| {
        c.query_row("SELECT name FROM accounts WHERE id = ?1", [account], |r| {
            r.get(0)
        })
        .optional()
    })?)
}

fn no_account() -> Response {
    html::message(
        StatusCode::NOT_FOUND,
        "No such account",
        "There's no such account.",
    )
}

async fn cluster_page(State(state): State<AppState>, Path(cluster): Path<String>) -> Response {
    use rusqlite::OptionalExtension;
    let found = state.db(|c| {
        c.query_row(
            "SELECT name, account_id FROM clusters WHERE id = ?1",
            [&cluster],
            |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)),
        )
        .optional()
    });
    match found {
        Ok(Some((name, account))) => html::page(
            StatusCode::OK,
            &name,
            &format!(
                "<p>The dashboard's page for cluster <code>{}</code> on account <code>{}</code>. \
                 This is the mock: there's nothing more here.</p>",
                html::escape(&cluster),
                html::escape(&account),
            ),
        ),
        Ok(None) => html::message(
            StatusCode::NOT_FOUND,
            "No such cluster",
            "There's no such cluster.",
        ),
        Err(_) => html::message(
            StatusCode::INTERNAL_SERVER_ERROR,
            "Error",
            "Something went wrong in the mock.",
        ),
    }
}

async fn api_tokens_page() -> Response {
    html::message(
        StatusCode::OK,
        "API Tokens",
        "The dashboard's API Tokens page, where a token is made by hand. This is the mock: its tokens are seeded.",
    )
}

async fn checkout(State(state): State<AppState>, Path(account): Path<String>) -> Response {
    match account_name(&state, &account) {
        Ok(Some(name)) => html::page(
            StatusCode::OK,
            "Add a payment method",
            &format!(
                "<p>Add a card to <strong>{}</strong> ({}). This is the mock: no card details are asked for.</p>\n\
                 <form method=\"post\" action=\"/_mock/checkout/{}\">\n\
                 <p><button type=\"submit\">Add card</button></p>\n\
                 </form>",
                html::escape(&name),
                html::escape(&account),
                html::escape(&account),
            ),
        ),
        Ok(None) => no_account(),
        Err(_) => html::message(
            StatusCode::INTERNAL_SERVER_ERROR,
            "Error",
            "Something went wrong in the mock.",
        ),
    }
}

async fn add_card(State(state): State<AppState>, Path(account): Path<String>) -> Response {
    let updated = state.db(|c| {
        c.execute(
            "UPDATE accounts SET has_payment_method = 1 WHERE id = ?1",
            [&account],
        )
    });
    match updated {
        Ok(0) => no_account(),
        Ok(_) => html::message(
            StatusCode::OK,
            "Card added",
            "The account has a payment method now. Go back to the terminal and try again.",
        ),
        Err(_) => html::message(
            StatusCode::INTERNAL_SERVER_ERROR,
            "Error",
            "Something went wrong in the mock.",
        ),
    }
}
