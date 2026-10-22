//! HTTP conventions shared by every `/v1` route: the auth extractor and scopes,
//! rate-limit headers, pagination, query parameters and JSON bodies.

use std::collections::{BTreeMap, HashMap};
use std::net::SocketAddr;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use axum::body::Bytes;
use axum::extract::{ConnectInfo, FromRequest, FromRequestParts, Request, State};
use axum::http::request::Parts;
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use bonsai_api::{Page, ProblemCode, ProblemDetail};
use jiff::{SignedDuration, Timestamp};
use rusqlite::{Connection, OptionalExtension};
use serde::Serialize;
use serde_json::Value;

use crate::AppState;
use crate::problem::{ProblemBuilder, invalid_parameter, status_problem, unauthorized};
use crate::{db, scenario, seed};

// ---------------------------------------------------------------- auth

/// The `x-operation` of each `/v1` operation, which decides the scope it needs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Op {
    ReadAccount,
    ReadClusters,
    CreateCluster,
    UpdateCluster,
    DestroyCluster,
    ReadClusterCredentials,
    ManageClusterCredentials,
    ReadQueryLogs,
    ReadPlans,
    ReadSpaces,
    ReadReleases,
}

impl Op {
    pub fn scope(self) -> &'static str {
        match self {
            Op::ReadAccount | Op::ReadPlans | Op::ReadSpaces | Op::ReadReleases => "account:read",
            Op::ReadClusters => "clusters:read",
            Op::CreateCluster | Op::UpdateCluster | Op::DestroyCluster => "clusters:write",
            Op::ReadClusterCredentials => "credentials:read",
            Op::ManageClusterCredentials => "credentials:write",
            Op::ReadQueryLogs => "query_logs:read",
        }
    }
}

/// The caller's valid, unrevoked, unexpired API token. Extracting it rejects
/// with 401 `unauthorized` and `WWW-Authenticate: Bearer`.
#[derive(Debug, Clone)]
pub struct Auth {
    pub account_id: String,
    pub token_id: i64,
    pub scopes: Vec<String>,
}

impl Auth {
    /// 403 `missing_scope`, naming the scope, unless the token has the one `op` needs.
    pub fn require(&self, op: Op) -> Result<(), ProblemBuilder> {
        let scope = op.scope();
        if self.scopes.iter().any(|s| s == scope) {
            return Ok(());
        }
        Err(status_problem(
            StatusCode::FORBIDDEN,
            ProblemCode::MissingScope,
            format!("This API token does not have the {scope} scope, which this operation needs."),
        )
        .push_detail(ProblemDetail {
            field: None,
            message: format!("Missing scope {scope}."),
            scope: Some(scope.to_string()),
        }))
    }
}

/// The token from `Authorization: Bearer <token>`, if that's the scheme.
fn bearer(headers: &HeaderMap) -> Option<&str> {
    let value = headers.get(header::AUTHORIZATION)?.to_str().ok()?;
    let (scheme, token) = value.split_once(' ')?;
    scheme
        .eq_ignore_ascii_case("bearer")
        .then(|| token.trim())
        .filter(|t| !t.is_empty())
}

/// Splits `bonsai_<key>_<secret>` into its key and secret.
fn parse_token(token: &str) -> Option<(&str, &str)> {
    let (key, secret) = token.strip_prefix("bonsai_")?.split_once('_')?;
    let ok = |s: &str| !s.is_empty() && !s.contains('_') && !s.contains(char::is_whitespace);
    (ok(key) && ok(secret)).then_some((key, secret))
}

impl FromRequestParts<AppState> for Auth {
    type Rejection = ProblemBuilder;

    async fn from_request_parts(
        parts: &mut Parts,
        state: &AppState,
    ) -> Result<Self, Self::Rejection> {
        authenticate(state, &parts.headers)?.ok_or_else(unauthorized)
    }
}

/// The token in `headers`, when it is a valid, unrevoked, unexpired one.
fn authenticate(state: &AppState, headers: &HeaderMap) -> Result<Option<Auth>, ProblemBuilder> {
    let Some((key, secret)) = bearer(headers).and_then(parse_token) else {
        return Ok(None);
    };
    let row = state.db(|c| {
        c.query_row(
            "SELECT id, account_id, secret_sha256, scopes, expires_at, revoked_at
             FROM tokens WHERE key_prefix = ?1",
            [key],
            |r| {
                Ok((
                    r.get::<_, i64>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, String>(2)?,
                    db::json::<Vec<String>>(r, 3)?,
                    db::ts_opt(r, 4)?,
                    r.get::<_, Option<String>>(5)?,
                ))
            },
        )
        .optional()
    })?;
    let Some((token_id, account_id, hash, scopes, expires_at, revoked_at)) = row else {
        return Ok(None);
    };
    let expired = expires_at.is_some_and(|t| t <= Timestamp::now());
    if seed::sha256_hex(secret) != hash || revoked_at.is_some() || expired {
        return Ok(None);
    }
    Ok(Some(Auth {
        account_id,
        token_id,
        scopes,
    }))
}

// ---------------------------------------------------------------- rate limits

pub const RATE_LIMIT_QUOTA: u32 = 600;
pub const RATE_LIMIT_WINDOW: Duration = Duration::from_secs(60);

/// A quota as `RateLimit` and `RateLimit-Policy` name it, and what its 429 says.
#[derive(Debug, Clone, Copy)]
pub struct Quota {
    pub name: &'static str,
    pub per_window: u32,
    pub refusal: &'static str,
}

/// Every route's quota but the login endpoints'.
pub const TOKEN_QUOTA: Quota = Quota {
    name: "token",
    per_window: RATE_LIMIT_QUOTA,
    refusal: "Too many requests for this token. Wait for the Retry-After seconds, then try again.",
};

/// The most counters [`RateLimiter`] keeps.
pub const MAX_LIMITER_KEYS: usize = 10_000;

/// Fixed-window request counters, keyed per token (or per IP).
#[derive(Debug, Default)]
pub struct RateLimiter {
    windows: Mutex<HashMap<String, (Instant, u32)>>,
}

struct Hit {
    remaining: u32,
    reset_secs: u64,
    exceeded: bool,
}

impl RateLimiter {
    fn hit(&self, key: &str) -> Hit {
        self.hit_quota(key, RATE_LIMIT_QUOTA)
    }

    /// Forgets every counter.
    pub(crate) fn reset(&self) {
        self.windows
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clear();
    }

    /// How many requests `key` has made in its current window, without counting one.
    fn count(&self, key: &str) -> u32 {
        let windows = self.windows.lock().unwrap_or_else(|e| e.into_inner());
        match windows.get(key) {
            Some((start, n)) if start.elapsed() < RATE_LIMIT_WINDOW => *n,
            _ => 0,
        }
    }

    fn hit_quota(&self, key: &str, quota: u32) -> Hit {
        let mut windows = self.windows.lock().unwrap_or_else(|e| e.into_inner());
        let now = Instant::now();
        // Made-up keys (IPs, failed tokens) mustn't grow the map without bound:
        // forget finished windows, and everything if that isn't enough.
        if windows.len() >= MAX_LIMITER_KEYS && !windows.contains_key(key) {
            windows.retain(|_, (start, _)| now.duration_since(*start) < RATE_LIMIT_WINDOW);
            if windows.len() >= MAX_LIMITER_KEYS {
                windows.clear();
            }
        }
        let w = windows.entry(key.to_string()).or_insert((now, 0));
        if now.duration_since(w.0) >= RATE_LIMIT_WINDOW {
            *w = (now, 0);
        }
        w.1 = w.1.saturating_add(1);
        let left = RATE_LIMIT_WINDOW.saturating_sub(now.duration_since(w.0));
        Hit {
            remaining: quota.saturating_sub(w.1),
            reset_secs: left.as_secs().clamp(1, RATE_LIMIT_WINDOW.as_secs()),
            exceeded: w.1 > quota,
        }
    }
}

/// The client's IP address, as text (`"unknown"` when the server wasn't
/// started with connection info, as in a router test).
#[derive(Debug, Clone)]
pub struct ClientIp(pub String);

impl ClientIp {
    fn of(extensions: &axum::http::Extensions) -> Self {
        ClientIp(
            extensions
                .get::<ConnectInfo<SocketAddr>>()
                .map(|ci| ci.0.ip().to_string())
                .unwrap_or_else(|| "unknown".into()),
        )
    }
}

impl<S: Send + Sync> FromRequestParts<S> for ClientIp {
    type Rejection = std::convert::Infallible;

    async fn from_request_parts(parts: &mut Parts, _: &S) -> Result<Self, Self::Rejection> {
        Ok(ClientIp::of(&parts.extensions))
    }
}

fn ip_key(req: &Request) -> String {
    format!("ip:{}", ClientIp::of(req.extensions()).0)
}

/// Failed authentications an IP may make per [`RATE_LIMIT_WINDOW`] before it
/// is refused outright (the Bonsai API's limit).
pub const FAILED_AUTHENTICATION_LIMIT: u32 = 20;

fn failed_auth_key(req: &Request) -> String {
    format!("failed-auth:{}", ClientIp::of(req.extensions()).0)
}

/// Middleware for authenticated routes, in the Bonsai API's order:
///
/// 1. an IP with [`FAILED_AUTHENTICATION_LIMIT`] failed authentications in the
///    window gets `429 too_many_failed_authentications`, before its
///    credentials are checked;
/// 2. a request is counted per token only once the token has authenticated, so
///    a wrong secret under someone's (public) key prefix can't drain their
///    quota; a request without a valid token is counted per IP;
/// 3. a `401` counts as a failed authentication for the IP.
pub async fn limit_by_token(State(state): State<AppState>, req: Request, next: Next) -> Response {
    let failed = failed_auth_key(&req);
    if state.limiter().count(&failed) >= FAILED_AUTHENTICATION_LIMIT {
        let window = RATE_LIMIT_WINDOW.as_secs();
        let mut resp = status_problem(
            StatusCode::TOO_MANY_REQUESTS,
            ProblemCode::TooManyFailedAuthentications,
            "Too many failed authentication attempts. Try again later.",
        )
        .retry_after(window)
        .into_response();
        let headers = resp.headers_mut();
        headers.insert(
            "ratelimit",
            HeaderValue::from_str(&format!("\"failed_authentication\";r=0;t={window}"))
                .expect("ASCII"),
        );
        headers.insert(
            "ratelimit-policy",
            HeaderValue::from_str(&format!(
                "\"failed_authentication\";q={FAILED_AUTHENTICATION_LIMIT};w={window}"
            ))
            .expect("ASCII"),
        );
        log_request(&state, req.method().as_str(), req.uri().path(), 429);
        return resp;
    }
    let key = match authenticate(&state, req.headers()) {
        Ok(Some(auth)) => format!("token:{}", auth.token_id),
        _ => ip_key(&req),
    };
    let resp = limit(state.clone(), key, TOKEN_QUOTA, req, next).await;
    if resp.status() == StatusCode::UNAUTHORIZED {
        state.limiter().hit(&failed);
    }
    resp
}

/// Middleware for `GET /v1`: rate-limited per IP.
pub async fn limit_by_ip(State(state): State<AppState>, req: Request, next: Next) -> Response {
    let key = ip_key(&req);
    limit(state, key, TOKEN_QUOTA, req, next).await
}

/// Middleware for a route with a per-IP quota of its own, such as the login
/// endpoints: counted under `scope` and the client's IP, apart from every other
/// counter, and reported under the quota's own name.
pub async fn limit_ip_quota(
    state: AppState,
    scope: &str,
    quota: Quota,
    req: Request,
    next: Next,
) -> Response {
    let key = format!("{scope}:{}", ClientIp::of(req.extensions()).0);
    limit(state, key, quota, req, next).await
}

/// Counts the request, answers 429 when over quota or when the
/// `rate_limit_next` scenario says so, and puts `RateLimit` and
/// `RateLimit-Policy` on every response.
async fn limit(state: AppState, key: String, quota: Quota, req: Request, next: Next) -> Response {
    let method = req.method().to_string();
    let path = req.uri().path().to_string();
    let hit = state.limiter().hit_quota(&key, quota.per_window);
    let forced = state.db(|c| scenario::take_counter(c, "rate_limit_next"));

    let (mut resp, remaining) = match forced {
        Err(e) => (ProblemBuilder::from(e).into_response(), hit.remaining),
        Ok(true) => (rate_limited(quota, 1).into_response(), 0),
        Ok(false) if hit.exceeded => (rate_limited(quota, hit.reset_secs).into_response(), 0),
        Ok(false) => (next.run(req).await, hit.remaining),
    };
    let headers = resp.headers_mut();
    headers.insert(
        "ratelimit",
        HeaderValue::from_str(&format!(
            "\"{}\";r={remaining};t={}",
            quota.name, hit.reset_secs
        ))
        .expect("ASCII"),
    );
    headers.insert(
        "ratelimit-policy",
        HeaderValue::from_str(&format!(
            "\"{}\";q={};w={}",
            quota.name,
            quota.per_window,
            RATE_LIMIT_WINDOW.as_secs()
        ))
        .expect("ASCII"),
    );

    log_request(&state, &method, &path, resp.status().as_u16());
    resp
}

fn log_request(state: &AppState, method: &str, path: &str, status: u16) {
    let logged = state.db(|c| {
        c.execute(
            "INSERT INTO request_log (at, method, path, status) VALUES (?1, ?2, ?3, ?4)",
            (db::now(), method, path, status),
        )
    });
    if let Err(e) = logged {
        tracing::warn!(error = %e, "could not write request_log");
    }
}

fn rate_limited(quota: Quota, retry_after: u64) -> ProblemBuilder {
    status_problem(
        StatusCode::TOO_MANY_REQUESTS,
        ProblemCode::RateLimited,
        quota.refusal,
    )
    .retry_after(retry_after)
}

// ---------------------------------------------------------------- query & pagination

/// The query string's parameters; the last value wins for a repeated name.
/// Never rejects, so a bad value becomes a problem in the handler.
#[derive(Debug, Clone, Default)]
pub struct QueryParams(pub HashMap<String, String>);

impl QueryParams {
    /// The value of `name`, treating an empty value as absent.
    pub fn get(&self, name: &str) -> Option<&str> {
        self.0
            .get(name)
            .map(String::as_str)
            .filter(|v| !v.is_empty())
    }
}

impl<S: Send + Sync> FromRequestParts<S> for QueryParams {
    type Rejection = std::convert::Infallible;

    async fn from_request_parts(parts: &mut Parts, _: &S) -> Result<Self, Self::Rejection> {
        let q = parts.uri.query().unwrap_or("");
        Ok(QueryParams(
            url::form_urlencoded::parse(q.as_bytes())
                .into_owned()
                .collect(),
        ))
    }
}

pub const DEFAULT_LIMIT: usize = 25;
pub const MAX_LIMIT: usize = 100;

/// `limit` and `cursor`, validated.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PageParams {
    pub limit: usize,
    pub offset: usize,
}

impl PageParams {
    /// `limit` must be 1..=100 (default 25) and `cursor` one this API returned;
    /// otherwise 400 `invalid_parameter` naming the field.
    pub fn from_query(q: &QueryParams) -> Result<Self, ProblemBuilder> {
        let limit = match q.get("limit") {
            None => DEFAULT_LIMIT,
            Some(s) => s
                .parse::<usize>()
                .ok()
                .filter(|n| (1..=MAX_LIMIT).contains(n))
                .ok_or_else(|| {
                    invalid_parameter(
                        "limit",
                        format!("limit must be an integer from 1 to {MAX_LIMIT}."),
                    )
                })?,
        };
        let offset = match q.get("cursor") {
            None => 0,
            Some(c) => decode_cursor(c).ok_or_else(|| {
                invalid_parameter("cursor", "cursor is not one this API returned.")
            })?,
        };
        Ok(PageParams { limit, offset })
    }
}

/// The opaque cursor for the page starting at `offset`: `base64url("o:<offset>")`.
pub fn encode_cursor(offset: usize) -> String {
    URL_SAFE_NO_PAD.encode(format!("o:{offset}"))
}

fn decode_cursor(cursor: &str) -> Option<usize> {
    let raw = URL_SAFE_NO_PAD.decode(cursor).ok()?;
    let s = String::from_utf8(raw).ok()?;
    let digits = s.strip_prefix("o:")?;
    if digits.is_empty() || !digits.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    digits.parse().ok()
}

/// One page of `items`, which must already be in a stable order.
pub fn paginate<T>(items: Vec<T>, p: PageParams) -> Page<T> {
    let total = items.len();
    let end = p.offset.saturating_add(p.limit);
    let data = items.into_iter().skip(p.offset).take(p.limit).collect();
    Page {
        data,
        next_cursor: (end < total).then(|| encode_cursor(end)),
    }
}

// ---------------------------------------------------------------- JSON bodies

/// A required JSON request body. An empty body, or `{}`, is 400
/// `body_required`; one that doesn't parse is 400 `invalid_json`.
#[derive(Debug, Clone)]
pub struct JsonBody(pub Value);

impl<S: Send + Sync> FromRequest<S> for JsonBody {
    type Rejection = ProblemBuilder;

    async fn from_request(req: Request, state: &S) -> Result<Self, Self::Rejection> {
        let AnyJsonBody(value) = AnyJsonBody::from_request(req, state).await?;
        if value.as_object().is_some_and(|o| o.is_empty()) {
            return Err(body_required());
        }
        Ok(JsonBody(value))
    }
}

/// A required JSON request body, which may be any JSON value, `{}` and `null`
/// included, for an operation that judges those itself. An empty body is 400
/// `body_required`; one that doesn't parse is 400 `invalid_json`.
#[derive(Debug, Clone)]
pub struct AnyJsonBody(pub Value);

impl<S: Send + Sync> FromRequest<S> for AnyJsonBody {
    type Rejection = ProblemBuilder;

    async fn from_request(req: Request, state: &S) -> Result<Self, Self::Rejection> {
        let invalid_json = || {
            status_problem(
                StatusCode::BAD_REQUEST,
                ProblemCode::InvalidJson,
                "The request body is not valid JSON.",
            )
        };
        let bytes = Bytes::from_request(req, state)
            .await
            .map_err(|_| invalid_json())?;
        if bytes.iter().all(u8::is_ascii_whitespace) {
            return Err(body_required());
        }
        let value: Value = serde_json::from_slice(&bytes).map_err(|_| invalid_json())?;
        Ok(AnyJsonBody(value))
    }
}

pub fn body_required() -> ProblemBuilder {
    status_problem(
        StatusCode::BAD_REQUEST,
        ProblemCode::BodyRequired,
        "This operation needs a JSON request body.",
    )
}

/// The fields of a write whose body fields are all strings, as the Bonsai
/// API checks them: an object (otherwise 400
/// `invalid_parameter` with `field: null`), not empty (400 `body_required`),
/// and then one problem naming every field at fault, each by its JSON pointer:
/// unknown, missing while required, or not a string. Whether a value is
/// acceptable is the operation's to judge.
pub fn string_body(
    body: Value,
    allowed: &[&str],
    required: &[&str],
) -> Result<BTreeMap<String, String>, ProblemBuilder> {
    let schema_mismatch = || {
        status_problem(
            StatusCode::BAD_REQUEST,
            ProblemCode::InvalidParameter,
            "The body doesn't match its schema. See details.",
        )
    };
    let Value::Object(obj) = body else {
        return Err(schema_mismatch().push_detail(not_an_object()));
    };
    if obj.is_empty() {
        return Err(body_required());
    }

    let mut faults: Vec<(String, String)> = Vec::new();
    for name in obj.keys().filter(|k| !allowed.contains(&k.as_str())) {
        faults.push((
            format!("/{name}"),
            format!("{name} is not a field of this operation."),
        ));
    }
    for name in required.iter().filter(|r| !obj.contains_key(**r)) {
        faults.push((format!("/{name}"), format!("{name} is required.")));
    }
    for (name, value) in &obj {
        if allowed.contains(&name.as_str()) && !value.is_string() {
            faults.push((format!("/{name}"), format!("{name} must be a string.")));
        }
    }
    if !faults.is_empty() {
        return Err(faults
            .into_iter()
            .fold(schema_mismatch(), |p, (field, message)| {
                p.field(&field, message)
            }));
    }

    Ok(obj
        .into_iter()
        .filter_map(|(k, v)| match v {
            Value::String(s) => Some((k, s)),
            _ => None,
        })
        .collect())
}

/// The `details` entry for a body that isn't a JSON object.
pub fn not_an_object() -> ProblemDetail {
    ProblemDetail {
        field: None,
        message: "The body must be a JSON object.".into(),
        scope: None,
    }
}

// ---------------------------------------------------------------- idempotency

pub const IDEMPOTENCY_KEY: &str = "Idempotency-Key";

/// How long a key is remembered.
pub const IDEMPOTENCY_TTL: SignedDuration = SignedDuration::from_hours(24);

/// A write's `Idempotency-Key`, and what identifies the request it came with.
#[derive(Debug, Clone)]
pub struct Idempotency {
    token_id: i64,
    key: String,
    fingerprint: String,
    method: String,
    path: String,
}

/// What an earlier request with the same key made.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Recorded {
    pub status: u16,
    pub resource_kind: String,
    pub resource_id: String,
}

impl Idempotency {
    /// The request's key, if it sent one. A key must be 1-255 printable
    /// ASCII characters, otherwise 400 `invalid_parameter` naming the header.
    /// `body` is the request's fields as the operation accepted them.
    pub fn from_request(
        auth: &Auth,
        headers: &HeaderMap,
        method: &str,
        path: &str,
        body: &impl Serialize,
    ) -> Result<Option<Self>, ProblemBuilder> {
        let Some(raw) = headers.get(IDEMPOTENCY_KEY) else {
            return Ok(None);
        };
        let bytes = raw.as_bytes();
        let printable = bytes.iter().all(|b| (0x20..=0x7e).contains(b));
        if bytes.is_empty() || bytes.len() > 255 || !printable {
            return Err(invalid_parameter(
                IDEMPOTENCY_KEY,
                "Idempotency-Key must be 1-255 printable ASCII characters.",
            ));
        }
        let key = String::from_utf8(bytes.to_vec()).expect("printable ASCII");
        Ok(Some(Idempotency {
            token_id: auth.token_id,
            fingerprint: fingerprint(method, path, body),
            key,
            method: method.to_string(),
            path: path.to_string(),
        }))
    }

    /// The earlier request with this token and key, if one succeeded in the
    /// last [`IDEMPOTENCY_TTL`]. A different request under the same key is 422
    /// `idempotency_key_reused`. An expired record is forgotten.
    pub fn lookup(
        &self,
        conn: &Connection,
        now: Timestamp,
    ) -> Result<Option<Recorded>, ProblemBuilder> {
        let row = conn
            .query_row(
                "SELECT fingerprint, status, resource_kind, resource_id, created_at
                 FROM idempotency WHERE token_id = ?1 AND key = ?2",
                (self.token_id, &self.key),
                |r| {
                    Ok((
                        r.get::<_, String>(0)?,
                        Recorded {
                            status: r.get(1)?,
                            resource_kind: r.get::<_, Option<String>>(2)?.unwrap_or_default(),
                            resource_id: r.get::<_, Option<String>>(3)?.unwrap_or_default(),
                        },
                        db::ts(r, 4)?,
                    ))
                },
            )
            .optional()?;
        let Some((fingerprint, recorded, created_at)) = row else {
            return Ok(None);
        };
        if now.duration_since(created_at) >= IDEMPOTENCY_TTL {
            conn.execute(
                "DELETE FROM idempotency WHERE token_id = ?1 AND key = ?2",
                (self.token_id, &self.key),
            )?;
            return Ok(None);
        }
        if fingerprint != self.fingerprint {
            return Err(status_problem(
                StatusCode::UNPROCESSABLE_ENTITY,
                ProblemCode::IdempotencyKeyReused,
                "This Idempotency-Key was already sent with a different request.",
            ));
        }
        Ok(Some(recorded))
    }

    /// Remembers a successful (2xx) response to this request.
    pub fn record(
        &self,
        conn: &Connection,
        status: StatusCode,
        resource_kind: &str,
        resource_id: &str,
        now: Timestamp,
    ) -> rusqlite::Result<()> {
        if !status.is_success() {
            return Ok(());
        }
        conn.execute(
            "INSERT OR REPLACE INTO idempotency
                (token_id, key, fingerprint, method, path, status, resource_kind, resource_id, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
            rusqlite::params![
                self.token_id,
                self.key,
                self.fingerprint,
                self.method,
                self.path,
                status.as_u16(),
                resource_kind,
                resource_id,
                now.to_string(),
            ],
        )?;
        Ok(())
    }
}

/// SHA-256 of the method, the path and the body as the operation accepted it,
/// serialized in a fixed order (a map's in key order).
pub fn fingerprint(method: &str, path: &str, body: &impl Serialize) -> String {
    let body = serde_json::to_string(body).expect("bodies serialize");
    seed::sha256_hex(&format!("{method}\n{path}\n{body}"))
}

/// What a write makes, which an `Idempotency-Key` records and a replay finds again.
pub trait Recordable {
    /// The `resource_kind` recorded with the key (`"action"`, `"credential"`).
    const KIND: &'static str;
    fn id(&self) -> &str;
}

/// What [`idempotently`] answers with: what `start` made, or a replay of it.
#[derive(Debug)]
pub struct Made<T> {
    pub value: T,
    pub replayed: bool,
}

/// Runs `start` (which makes a `T`, or refuses) unless `key` says otherwise:
///
/// - a key seen with this token in the last 24 h, on the same request, calls
///   `replay` with the ID it recorded, to re-read what it made as that is now
///   (`replay` answers 409 `idempotency_replay_unavailable` if it's gone);
/// - on another request, it's 422 `idempotency_key_reused`;
/// - otherwise `start` runs, and the key is recorded, with `status`, only if
///   it succeeded.
///
/// Call it with the database locked, so a key is looked up and recorded at once.
/// It runs in one transaction (joining one already open), so the write and its
/// key are kept or lost together. A refusal is committed too, with whatever it
/// did (such as using up a `fail_next`); only an internal error rolls back.
pub fn idempotently<T: Recordable>(
    conn: &Connection,
    key: Option<&Idempotency>,
    status: StatusCode,
    replay: impl FnOnce(&Connection, Option<&str>) -> Result<T, ProblemBuilder>,
    start: impl FnOnce(&Connection) -> Result<T, ProblemBuilder>,
) -> Result<Made<T>, ProblemBuilder> {
    if !conn.is_autocommit() {
        return idempotently_in(conn, key, status, replay, start);
    }
    let tx = conn.unchecked_transaction()?;
    let out = idempotently_in(&tx, key, status, replay, start);
    match &out {
        Err(p) if p.problem().status >= 500 => {} // dropped: rolled back
        _ => tx.commit()?,
    }
    out
}

fn idempotently_in<T: Recordable>(
    conn: &Connection,
    key: Option<&Idempotency>,
    status: StatusCode,
    replay: impl FnOnce(&Connection, Option<&str>) -> Result<T, ProblemBuilder>,
    start: impl FnOnce(&Connection) -> Result<T, ProblemBuilder>,
) -> Result<Made<T>, ProblemBuilder> {
    let now = Timestamp::now();
    if let Some(key) = key
        && let Some(recorded) = key.lookup(conn, now)?
    {
        let id = (recorded.resource_kind == T::KIND).then_some(recorded.resource_id.as_str());
        return Ok(Made {
            value: replay(conn, id)?,
            replayed: true,
        });
    }
    let value = start(conn)?;
    if let Some(key) = key {
        key.record(conn, status, T::KIND, value.id(), now)?;
    }
    Ok(Made {
        value,
        replayed: false,
    })
}

/// 409 `idempotency_replay_unavailable`: the keyed request succeeded, but what
/// it made can't be returned again.
pub fn replay_unavailable(detail: &str) -> ProblemBuilder {
    status_problem(
        StatusCode::CONFLICT,
        ProblemCode::IdempotencyReplayUnavailable,
        detail,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;

    #[test]
    fn scopes_per_operation() {
        assert_eq!(Op::ReadAccount.scope(), "account:read");
        assert_eq!(Op::ReadSpaces.scope(), "account:read");
        assert_eq!(Op::DestroyCluster.scope(), "clusters:write");
        assert_eq!(Op::ManageClusterCredentials.scope(), "credentials:write");
        assert_eq!(Op::ReadQueryLogs.scope(), "query_logs:read");
    }

    #[test]
    fn token_parsing() {
        assert_eq!(parse_token("bonsai_ab12_cd34"), Some(("ab12", "cd34")));
        assert_eq!(parse_token("bonsai_ab12_cd_34"), None);
        assert_eq!(parse_token("bonsai__cd34"), None);
        assert_eq!(parse_token("other_ab12_cd34"), None);
        let mut h = HeaderMap::new();
        h.insert(header::AUTHORIZATION, "bearer  tok ".parse().unwrap());
        assert_eq!(bearer(&h), Some("tok"));
        h.insert(header::AUTHORIZATION, "Basic tok".parse().unwrap());
        assert_eq!(bearer(&h), None);
    }

    struct Made1;
    impl Recordable for Made1 {
        const KIND: &'static str = "thing";
        fn id(&self) -> &str {
            "t-1"
        }
    }

    fn key() -> Idempotency {
        Idempotency {
            token_id: 1,
            key: "k".into(),
            fingerprint: "f".into(),
            method: "POST".into(),
            path: "/v1/x".into(),
        }
    }

    /// The write and its key are one transaction: a failure to record the key
    /// rolls the write back, and a refusal's side effects (a used-up scenario)
    /// are kept.
    #[test]
    fn idempotently_is_one_transaction() {
        let dir = tempfile::tempdir().unwrap();
        let conn = db::open(&dir.path().join("i.sqlite")).unwrap();
        let fail_next = || scenario::get(&conn, "fail_next").unwrap();

        // Recording the key fails (its table is gone by then): the write is undone.
        let res = idempotently(
            &conn,
            Some(&key()),
            StatusCode::ACCEPTED,
            |_, _| unreachable!(),
            |c| {
                scenario::set(c, "fail_next", &serde_json::json!("timed_out"))?;
                c.execute_batch("ALTER TABLE idempotency RENAME TO gone")?;
                Ok(Made1)
            },
        );
        assert!(res.is_err());
        let gone: i64 = conn
            .query_row(
                "SELECT count(*) FROM sqlite_master WHERE name = 'gone'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        if gone == 1 {
            conn.execute_batch("ALTER TABLE gone RENAME TO idempotency")
                .unwrap();
        }
        assert_eq!(fail_next(), None, "the write outlived the failed record");

        // A refusal commits what it did.
        scenario::set(&conn, "fail_next", &serde_json::json!("payment_failed")).unwrap();
        let res = idempotently::<Made1>(
            &conn,
            Some(&key()),
            StatusCode::ACCEPTED,
            |_, _| unreachable!(),
            |c| {
                scenario::clear(c, "fail_next")?;
                Err(status_problem(
                    StatusCode::UNPROCESSABLE_ENTITY,
                    ProblemCode::PaymentFailed,
                    "refused",
                ))
            },
        );
        assert!(res.is_err());
        assert_eq!(fail_next(), None, "the refusal's clear was rolled back");
    }

    #[test]
    fn the_limiter_map_stays_bounded() {
        let l = RateLimiter::default();
        for i in 0..(MAX_LIMITER_KEYS + 50) {
            l.hit(&format!("token:{i}"));
        }
        let len = l.windows.lock().unwrap().len();
        assert!(len <= MAX_LIMITER_KEYS, "{len} keys");
    }

    #[test]
    fn cursors_round_trip() {
        assert_eq!(decode_cursor(&encode_cursor(0)), Some(0));
        assert_eq!(decode_cursor(&encode_cursor(42)), Some(42));
        assert_eq!(decode_cursor("bzp4"), None);
        assert_eq!(decode_cursor("!!"), None);
        let p = PageParams {
            limit: 2,
            offset: 2,
        };
        let page = paginate(vec![1, 2, 3, 4, 5], p);
        assert_eq!(page.data, [3, 4]);
        assert_eq!(page.next_cursor, Some(encode_cursor(4)));
        let last = paginate(
            vec![1, 2, 3, 4, 5],
            PageParams {
                limit: 2,
                offset: 4,
            },
        );
        assert_eq!(last.data, [5]);
        assert_eq!(last.next_cursor, None);
    }

    async fn body(raw: &str) -> Result<Value, ProblemCode> {
        let req = Request::builder()
            .method("POST")
            .uri("/")
            .body(Body::from(raw.to_string()))
            .unwrap();
        JsonBody::from_request(req, &())
            .await
            .map(|b| b.0)
            .map_err(|p| p.problem().code.clone())
    }

    fn fields(p: &ProblemBuilder) -> Vec<Option<String>> {
        p.problem()
            .details
            .iter()
            .map(|d| d.field.clone())
            .collect()
    }

    #[test]
    fn string_bodies() {
        use serde_json::json;
        let allowed = ["name", "plan", "region"];
        let required = ["name", "plan"];
        let ok = string_body(json!({"name": "a", "plan": "b"}), &allowed, &required).unwrap();
        assert_eq!(ok.len(), 2);
        assert_eq!(ok["name"], "a");

        let p = string_body(json!([1]), &allowed, &required).unwrap_err();
        assert_eq!(p.problem().code, ProblemCode::InvalidParameter);
        assert_eq!(fields(&p), [None]);
        assert_eq!(
            p.problem().details[0].message,
            "The body must be a JSON object."
        );

        let p = string_body(json!({}), &allowed, &required).unwrap_err();
        assert_eq!(p.problem().code, ProblemCode::BodyRequired);

        let p = string_body(json!({"name": 1, "foo": "x"}), &allowed, &required).unwrap_err();
        assert_eq!(p.problem().status, 400);
        assert_eq!(
            fields(&p),
            [
                Some("/foo".into()),
                Some("/plan".into()),
                Some("/name".into())
            ]
        );
    }

    #[test]
    fn fingerprints_cover_method_path_and_body() {
        let body = |v: &str| BTreeMap::from([("name".to_string(), v.to_string())]);
        let a = fingerprint("POST", "/v1/clusters", &body("x"));
        assert_eq!(a, fingerprint("POST", "/v1/clusters", &body("x")));
        assert_ne!(a, fingerprint("POST", "/v1/clusters", &body("y")));
        assert_ne!(a, fingerprint("DELETE", "/v1/clusters", &body("x")));
        assert_ne!(a, fingerprint("POST", "/v1/clusters/c", &body("x")));
    }

    #[tokio::test]
    async fn json_bodies() {
        assert_eq!(body("").await, Err(ProblemCode::BodyRequired));
        assert_eq!(body("  ").await, Err(ProblemCode::BodyRequired));
        assert_eq!(body("{}").await, Err(ProblemCode::BodyRequired));
        assert_eq!(body("{nope").await, Err(ProblemCode::InvalidJson));
        assert_eq!(
            body(r#"{"name":"x"}"#).await,
            Ok(serde_json::json!({"name": "x"}))
        );
    }
}
