//! The browser login (`startCliLogin`, `pollCliLogin`): an RFC 8628 device
//! authorization grant that ends in an ordinary API token.
//!
//! - `POST /v1/cli/logins` (no token) starts one: a device code for the CLI
//!   and a user code for the person. At most [`STARTS_PER_MINUTE`] per IP.
//! - `POST /v1/cli/logins/token` (no token) polls it with the device code:
//!   400 `authorization_pending`, `slow_down`, `access_denied` or
//!   `expired_token` (RFC 8628's codes, as Problems), or, once, the token.
//!   At most [`POLLS_PER_MINUTE`] per IP.
//! - `GET /_mock/cli/approve?user_code=` is the dashboard's approval page,
//!   and `POST /_mock/cli/approve` its form. The admin API approves or
//!   denies through [`decide`] too.
//!
//! Only the device code's SHA-256 is stored. A device code that doesn't
//! exist answers exactly as an expired one does, so polling never reveals
//! whether a code exists. `interval` is [`POLL_INTERVAL_SECS`] (1 s, where
//! the Bonsai API would say 5) so tests are fast.

use axum::body::Bytes;
use axum::extract::State;
use axum::http::StatusCode;
use axum::response::Response;
use axum::routing::{get, post};
use axum::{Json, Router};
use bonsai_api::{CliLoginGrant, ProblemCode};
use jiff::{SignedDuration, Timestamp};
use rusqlite::{Connection, OptionalExtension, params};
use serde_json::{Value, json};

use crate::http::{ClientIp, JsonBody, QueryParams, string_body};
use crate::problem::{ProblemBuilder, invalid_parameter, status_problem};
use crate::{AppState, db, html, ids, seed};

/// How long a login waits to be approved.
pub const LOGIN_TTL: SignedDuration = SignedDuration::from_secs(600);
/// The least time between two polls; RFC 8628's `interval`.
pub const POLL_INTERVAL_SECS: u64 = 1;
/// Logins one IP address may start per minute.
pub const STARTS_PER_MINUTE: u32 = 10;
/// Polls one IP address may make per minute.
pub const POLLS_PER_MINUTE: u32 = 60;
/// The scopes the approval page offers checked.
pub const DEFAULT_SCOPES: &[&str] = &[
    "account:read",
    "clusters:read",
    "clusters:write",
    "credentials:read",
];
/// The expiries a login's token may have, in days.
pub const EXPIRY_DAYS: &[u32] = &[30, 90, 365];
pub const DEFAULT_EXPIRY_DAYS: u32 = 90;
/// The account the admin API approves for when none is named.
pub const DEFAULT_ACCOUNT: &str = "acme";

/// RFC 8628's user-code alphabet: consonants only, no `Y`, so a code can't
/// spell a word and reads unambiguously.
const USER_CODE_LETTERS: &[u8] = b"BCDFGHJKLMNPQRSTVWXZ";
const DEVICE_CODE_CHARS: &[u8] = b"0123456789ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz";
const START_FIELDS: &[&str] = &["client_name", "hostname", "os"];
const POLL_FIELDS: &[&str] = &["device_code"];
const FIELD_MAX_CHARS: usize = 255;

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/v1/cli/logins", post(start))
        .route("/v1/cli/logins/token", post(poll))
}

/// The approval page, outside `/v1` and its rate limits.
pub fn page_router() -> Router<AppState> {
    Router::new().route("/_mock/cli/approve", get(approve_page).post(approve_form))
}

// ---------------------------------------------------------------- start

async fn start(
    State(state): State<AppState>,
    ClientIp(ip): ClientIp,
    body: Result<JsonBody, ProblemBuilder>,
) -> Result<Json<CliLoginGrant>, ProblemBuilder> {
    limit_per_ip(
        &state,
        &format!("cli_login:{ip}"),
        STARTS_PER_MINUTE,
        "logins started",
    )?;
    let body = string_body(body?.0, START_FIELDS, START_FIELDS)?;
    for field in START_FIELDS {
        let chars = body[*field].chars().count();
        if chars == 0 {
            return Err(invalid_parameter(
                &format!("/{field}"),
                format!("{field} can't be empty."),
            ));
        }
        if chars > FIELD_MAX_CHARS {
            return Err(invalid_parameter(
                &format!("/{field}"),
                format!("{field} can be at most {FIELD_MAX_CHARS} characters."),
            ));
        }
    }

    let device_code = ids::random_from(DEVICE_CODE_CHARS, 32);
    let now = Timestamp::now();
    let user_code = state.db(|c| -> rusqlite::Result<String> {
        loop {
            let user_code = new_user_code();
            let inserted = c.execute(
                "INSERT OR IGNORE INTO cli_logins
                    (device_code_sha256, user_code, client_name, hostname, os, ip, state,
                     created_at, expires_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, 'pending', ?7, ?8)",
                params![
                    seed::sha256_hex(&device_code),
                    user_code,
                    body["client_name"],
                    body["hostname"],
                    body["os"],
                    ip,
                    now.to_string(),
                    (now + LOGIN_TTL).to_string(),
                ],
            )?;
            if inserted == 1 {
                return Ok(user_code);
            }
        }
    })?;

    let verification_uri = state.url("/_mock/cli/approve");
    Ok(Json(CliLoginGrant {
        verification_uri_complete: format!("{verification_uri}?user_code={user_code}"),
        verification_uri,
        device_code,
        user_code,
        expires_in: LOGIN_TTL.as_secs() as u64,
        interval: POLL_INTERVAL_SECS,
    }))
}

/// 429 `rate_limited` once `key` has made more than `per_minute` requests
/// in the limiter's window.
fn limit_per_ip(
    state: &AppState,
    key: &str,
    per_minute: u32,
    what: &str,
) -> Result<(), ProblemBuilder> {
    state.limiter().check(key, per_minute).map_err(|reset| {
        status_problem(
            StatusCode::TOO_MANY_REQUESTS,
            ProblemCode::RateLimited,
            format!("Too many {what} from this address. At most {per_minute} a minute."),
        )
        .retry_after(reset)
    })
}

/// `XXXX-XXXX` from [`USER_CODE_LETTERS`].
fn new_user_code() -> String {
    let letters = ids::random_from(USER_CODE_LETTERS, 8);
    format!("{}-{}", &letters[..4], &letters[4..])
}

/// A user code as a person may type it (`wdjbmjht`, `WDJB MJHT`) in its
/// stored form, `WDJB-MJHT`. Anything else is returned as is, and matches
/// nothing.
pub fn normalize_user_code(typed: &str) -> String {
    let letters: String = typed
        .chars()
        .filter(char::is_ascii_alphabetic)
        .map(|c| c.to_ascii_uppercase())
        .collect();
    let stray = typed
        .chars()
        .any(|c| !c.is_ascii_alphabetic() && c != '-' && !c.is_whitespace());
    if letters.len() == 8 && !stray {
        format!("{}-{}", &letters[..4], &letters[4..])
    } else {
        typed.to_string()
    }
}

// ---------------------------------------------------------------- poll

fn login_problem(code: ProblemCode, detail: &str) -> ProblemBuilder {
    status_problem(StatusCode::BAD_REQUEST, code, detail)
}

/// The one answer for a device code that is unknown, expired or spent.
fn expired_token() -> ProblemBuilder {
    login_problem(
        ProblemCode::ExpiredToken,
        "This login has expired or was already used. Start a new one.",
    )
}

async fn poll(
    State(state): State<AppState>,
    ClientIp(ip): ClientIp,
    body: Result<JsonBody, ProblemBuilder>,
) -> Result<Json<Value>, ProblemBuilder> {
    limit_per_ip(
        &state,
        &format!("cli_login_poll:{ip}"),
        POLLS_PER_MINUTE,
        "polls",
    )?;
    let body = string_body(body?.0, POLL_FIELDS, POLL_FIELDS)?;
    let hash = seed::sha256_hex(&body["device_code"]);
    let now = Timestamp::now();
    // Not in a transaction: a refusal still records the poll, for `slow_down`.
    // The connection's lock keeps polls of one code from interleaving.
    state.db(|c| poll_login(c, &hash, now))
}

fn poll_login(c: &Connection, hash: &str, now: Timestamp) -> Result<Json<Value>, ProblemBuilder> {
    let row = c
        .query_row(
            "SELECT l.state, l.expires_at, l.last_poll_at, l.token_plain, l.scopes,
                    a.id, a.name
             FROM cli_logins l LEFT JOIN accounts a ON a.id = l.account_id
             WHERE l.device_code_sha256 = ?1",
            [hash],
            |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    db::ts(r, 1)?,
                    db::ts_opt(r, 2)?,
                    r.get::<_, Option<String>>(3)?,
                    r.get::<_, Option<String>>(4)?,
                    r.get::<_, Option<String>>(5)?,
                    r.get::<_, Option<String>>(6)?,
                ))
            },
        )
        .optional()?;
    let Some((login_state, expires_at, last_poll, token, scopes, account_id, account_name)) = row
    else {
        return Err(expired_token());
    };
    if now >= expires_at || login_state == "spent" {
        return Err(expired_token());
    }
    c.execute(
        "UPDATE cli_logins SET last_poll_at = ?2 WHERE device_code_sha256 = ?1",
        params![hash, now.to_string()],
    )?;
    match login_state.as_str() {
        "denied" => Err(login_problem(
            ProblemCode::AccessDenied,
            "The login was denied.",
        )),
        "approved" => {
            let (Some(token), Some(scopes), Some(id), Some(name)) =
                (token, scopes, account_id, account_name)
            else {
                return Err(expired_token());
            };
            c.execute(
                "UPDATE cli_logins SET state = 'spent', token_plain = NULL
                 WHERE device_code_sha256 = ?1",
                [hash],
            )?;
            let key = token
                .strip_prefix("bonsai_")
                .and_then(|t| t.split_once('_'))
                .map(|(k, _)| k)
                .unwrap_or_default();
            let token_expires: Option<String> = c
                .query_row(
                    "SELECT expires_at FROM tokens WHERE key_prefix = ?1",
                    [key],
                    |r| r.get(0),
                )
                .optional()?
                .flatten();
            let scopes: Value = serde_json::from_str(&scopes).unwrap_or_else(|_| json!([]));
            Ok(Json(json!({
                "token": token,
                "account": {"id": id, "name": name},
                "scopes": scopes,
                "expires_at": token_expires,
            })))
        }
        _ => {
            let too_soon = last_poll.is_some_and(|t| {
                now.duration_since(t) < SignedDuration::from_secs(POLL_INTERVAL_SECS as i64)
            });
            if too_soon {
                Err(login_problem(
                    ProblemCode::SlowDown,
                    "Polled sooner than `interval` after the last poll. Wait longer between polls.",
                ))
            } else {
                Err(login_problem(
                    ProblemCode::AuthorizationPending,
                    "The login is waiting to be approved in the browser.",
                ))
            }
        }
    }
}

// ---------------------------------------------------------------- approve and deny

/// What a person (or the admin API) decides about a pending login.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Decision {
    Approve {
        account: String,
        scopes: Vec<String>,
        expires_in_days: u32,
    },
    Deny,
}

/// A login still waiting, by its user code.
struct Pending {
    user_code: String,
    client_name: String,
    hostname: String,
    os: String,
    ip: String,
    created_at: Timestamp,
    state: String,
}

/// The login with this user code, unless there's none or it expired (404).
fn find_live(c: &Connection, typed: &str) -> Result<Pending, ProblemBuilder> {
    let user_code = normalize_user_code(typed);
    let row = c
        .query_row(
            "SELECT user_code, client_name, hostname, os, ip, created_at, state, expires_at
             FROM cli_logins WHERE user_code = ?1",
            [&user_code],
            |r| {
                Ok((
                    Pending {
                        user_code: r.get(0)?,
                        client_name: r.get(1)?,
                        hostname: r.get(2)?,
                        os: r.get(3)?,
                        ip: r.get(4)?,
                        created_at: db::ts(r, 5)?,
                        state: r.get(6)?,
                    },
                    db::ts(r, 7)?,
                ))
            },
        )
        .optional()?;
    match row {
        Some((login, expires_at)) if Timestamp::now() < expires_at => Ok(login),
        _ => Err(status_problem(
            StatusCode::NOT_FOUND,
            ProblemCode::NotFound,
            "No login has this code, or it expired. Run `bonsai login` again.",
        )),
    }
}

/// Approves or denies the pending login with `user_code`. Approving creates
/// the token, named `Bonsai CLI on <hostname>`, which the next poll hands out.
///
/// 404 for an unknown or expired code, 409 once decided, 400 for an unknown
/// account or scope or an expiry other than 30, 90 or 365 days.
pub fn decide(c: &Connection, user_code: &str, decision: &Decision) -> Result<(), ProblemBuilder> {
    db::atomically(c, |c| {
        let login = find_live(c, user_code)?;
        if login.state != "pending" {
            return Err(status_problem(
                StatusCode::CONFLICT,
                ProblemCode::Unknown("login_already_decided".into()),
                format!("This login was already {}.", login.state),
            ));
        }
        match decision {
            Decision::Deny => {
                c.execute(
                    "UPDATE cli_logins SET state = 'denied' WHERE user_code = ?1",
                    [&login.user_code],
                )?;
            }
            Decision::Approve {
                account,
                scopes,
                expires_in_days,
            } => {
                let exists: bool = c.query_row(
                    "SELECT EXISTS (SELECT 1 FROM accounts WHERE id = ?1)",
                    [account],
                    |r| r.get(0),
                )?;
                if !exists {
                    return Err(invalid_parameter("/account", "No such account."));
                }
                if scopes.is_empty() {
                    return Err(invalid_parameter("/scopes", "Pick at least one scope."));
                }
                if let Some(bad) = scopes
                    .iter()
                    .find(|s| !seed::ALL_SCOPES.contains(&s.as_str()))
                {
                    return Err(invalid_parameter(
                        "/scopes",
                        format!(
                            "Unknown scope {bad}; scopes are {}.",
                            seed::ALL_SCOPES.join(", ")
                        ),
                    ));
                }
                if !EXPIRY_DAYS.contains(expires_in_days) {
                    return Err(invalid_parameter(
                        "/expires_in_days",
                        "expires_in_days must be 30, 90 or 365.",
                    ));
                }
                let scope_refs: Vec<&str> = scopes.iter().map(String::as_str).collect();
                let expires_at =
                    Timestamp::now() + SignedDuration::from_hours(24 * i64::from(*expires_in_days));
                let token = seed::create_token(
                    c,
                    account,
                    &format!("Bonsai CLI on {}", login.hostname),
                    &scope_refs,
                    Some(expires_at),
                )?;
                c.execute(
                    "UPDATE cli_logins SET state = 'approved', account_id = ?2, scopes = ?3,
                        expires_in_days = ?4, token_plain = ?5
                     WHERE user_code = ?1",
                    params![
                        login.user_code,
                        account,
                        serde_json::to_string(&scope_refs).expect("strings serialize"),
                        expires_in_days,
                        token,
                    ],
                )?;
            }
        }
        Ok(())
    })
}

// ---------------------------------------------------------------- the page

const WARNING: &str = "<p class=\"warn\"><strong>Only approve this if you just ran \
    <code>bonsai login</code> yourself.</strong> Whoever started this login gets an API \
    token for the account you pick. If someone sent you this link, deny it.</p>";

/// An HTML page for a refusal.
fn html_error(p: ProblemBuilder) -> Response {
    let problem = p.problem();
    let status = StatusCode::from_u16(problem.status).unwrap_or(StatusCode::BAD_REQUEST);
    html::message(status, "Can't do that", &problem.detail)
}

async fn approve_page(State(state): State<AppState>, q: QueryParams) -> Response {
    let Some(typed) = q.get("user_code") else {
        return html::message(
            StatusCode::NOT_FOUND,
            "No such login",
            "This page needs the code from `bonsai login`.",
        );
    };
    let page = state.db(|c| -> Result<String, ProblemBuilder> {
        let login = find_live(c, typed)?;
        if login.state != "pending" {
            return Err(status_problem(
                StatusCode::CONFLICT,
                ProblemCode::Unknown("login_already_decided".into()),
                format!("This login was already {}.", login.state),
            ));
        }
        let accounts: Vec<(String, String)> = c
            .prepare("SELECT id, name FROM accounts ORDER BY id")?
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?
            .collect::<rusqlite::Result<_>>()?;
        Ok(approval_form(&login, &accounts))
    });
    match page {
        Ok(body) => html::page(StatusCode::OK, "Approve a CLI login", &body),
        Err(p) => html_error(p),
    }
}

fn approval_form(login: &Pending, accounts: &[(String, String)]) -> String {
    use html::escape as e;
    let mut out = String::new();
    out.push_str(WARNING);
    out.push_str("\n<p>Check that this code matches the one in your terminal:</p>\n");
    out.push_str(&format!("<p class=\"code\">{}</p>\n", e(&login.user_code)));
    out.push_str("<table>\n");
    for (label, value) in [
        ("Client", login.client_name.as_str()),
        ("Hostname", login.hostname.as_str()),
        ("OS", login.os.as_str()),
        ("IP address", login.ip.as_str()),
        ("Requested at", &login.created_at.to_string()),
    ] {
        out.push_str(&format!("<tr><th>{label}</th><td>{}</td></tr>\n", e(value)));
    }
    out.push_str("</table>\n");
    out.push_str("<form method=\"post\" action=\"/_mock/cli/approve\">\n");
    out.push_str(&format!(
        "<input type=\"hidden\" name=\"user_code\" value=\"{}\">\n",
        e(&login.user_code)
    ));
    out.push_str("<p><label>Account <select name=\"account\">\n");
    for (id, name) in accounts {
        out.push_str(&format!(
            "<option value=\"{}\">{} ({})</option>\n",
            e(id),
            e(name),
            e(id)
        ));
    }
    out.push_str("</select></label></p>\n<fieldset><legend>Scopes</legend>\n");
    for scope in seed::ALL_SCOPES {
        let checked = if DEFAULT_SCOPES.contains(scope) {
            " checked"
        } else {
            ""
        };
        out.push_str(&format!(
            "<label><input type=\"checkbox\" name=\"scope\" value=\"{scope}\"{checked}> {scope}</label><br>\n"
        ));
    }
    out.push_str("</fieldset>\n<p><label>Expires after <select name=\"expires_in_days\">\n");
    for days in EXPIRY_DAYS {
        let selected = if *days == DEFAULT_EXPIRY_DAYS {
            " selected"
        } else {
            ""
        };
        out.push_str(&format!(
            "<option value=\"{days}\"{selected}>{days} days</option>\n"
        ));
    }
    out.push_str(
        "</select></label></p>\n\
         <p><button type=\"submit\" name=\"decision\" value=\"approve\">Approve</button>\n\
         <button type=\"submit\" name=\"decision\" value=\"deny\">Deny</button></p>\n\
         </form>",
    );
    out
}

/// The form's `application/x-www-form-urlencoded` body: `user_code`,
/// `decision` (`approve` or `deny`), and to approve, `account`, `scope`
/// (repeated) and `expires_in_days`.
async fn approve_form(State(state): State<AppState>, body: Bytes) -> Response {
    let fields: Vec<(String, String)> = url::form_urlencoded::parse(&body).into_owned().collect();
    let one = |name: &str| {
        fields
            .iter()
            .rev()
            .find(|(k, _)| k == name)
            .map(|(_, v)| v.as_str())
    };
    let Some(user_code) = one("user_code") else {
        return html::message(
            StatusCode::BAD_REQUEST,
            "Can't do that",
            "No login code was sent.",
        );
    };
    let decision = match one("decision") {
        Some("deny") => Decision::Deny,
        Some("approve") => {
            let Ok(expires_in_days) = one("expires_in_days").unwrap_or("90").parse::<u32>() else {
                return html::message(
                    StatusCode::BAD_REQUEST,
                    "Can't do that",
                    "The expiry must be a number of days.",
                );
            };
            Decision::Approve {
                account: one("account").unwrap_or(DEFAULT_ACCOUNT).to_string(),
                scopes: fields
                    .iter()
                    .filter(|(k, _)| k == "scope")
                    .map(|(_, v)| v.clone())
                    .collect(),
                expires_in_days,
            }
        }
        _ => {
            return html::message(
                StatusCode::BAD_REQUEST,
                "Can't do that",
                "Choose Approve or Deny.",
            );
        }
    };
    match state.db(|c| decide(c, user_code, &decision)) {
        Ok(()) if decision == Decision::Deny => html::message(
            StatusCode::OK,
            "Denied",
            "Denied. The terminal that asked will be told so.",
        ),
        Ok(()) => html::message(
            StatusCode::OK,
            "Approved",
            "Approved. You can close this tab and go back to the terminal.",
        ),
        Err(p) => html_error(p),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn user_codes() {
        let code = new_user_code();
        assert_eq!(code.len(), 9);
        assert_eq!(&code[4..5], "-");
        assert!(
            code.bytes()
                .filter(|b| *b != b'-')
                .all(|b| USER_CODE_LETTERS.contains(&b))
        );
        assert_eq!(normalize_user_code("wdjb-mjht"), "WDJB-MJHT");
        assert_eq!(normalize_user_code("WDJBMJHT"), "WDJB-MJHT");
        assert_eq!(normalize_user_code(" wdjb mjht "), "WDJB-MJHT");
        assert_eq!(normalize_user_code("WDJB-MJH"), "WDJB-MJH");
        assert_eq!(normalize_user_code("WDJB<MJHT"), "WDJB<MJHT");
    }
}
