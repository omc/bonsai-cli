//! A cluster's credentials (`listClusterCredentials`,
//! `createClusterCredential`, `getClusterCredential`,
//! `revokeClusterCredential`), as the Bonsai API serves them. A terminated cluster has no active credentials, so its credentials
//! are 404 here.

use axum::extract::{Path, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::{Json, Router};
use bonsai_api::{
    AccessType, ClusterState, Credential, CredentialCreate, CredentialWithSecret, Page,
    ProblemCode, ProblemDetail,
};
use jiff::Timestamp;
use rusqlite::{Connection, OptionalExtension, Row, params};
use serde_json::Value;

use crate::engine::{self, ActionRow, NewCredential};
use crate::http::{
    self, AnyJsonBody, Auth, Idempotency, Op, PageParams, QueryParams, Recordable, body_required,
    not_an_object, paginate, replay_unavailable,
};
use crate::problem::{ProblemBuilder, invalid_parameter, not_found, status_problem};
use crate::routes::catalogue::PlanRow;
use crate::routes::clusters::ClusterRow;
use crate::{AppState, db};

pub const NAME_MAX_LENGTH: usize = 255;

const ACCESS_TYPES: &[&str] = &["rw", "ro", "rwm", "acl"];

/// `CredentialCreate`'s fields, in `openapi/v1.yml`
const CREATE_FIELDS: &[&str] = &[
    "name",
    "access_type",
    "allowed_actions",
    "allowed_index_globs",
    "allowed_cidrs",
];

const LAST_RW: &str = "The cluster's last active rw credential can't be revoked.";

pub fn router() -> Router<AppState> {
    Router::new()
        .route(
            "/v1/clusters/{cluster_id}/credentials",
            get(list).post(create),
        )
        .route(
            "/v1/clusters/{cluster_id}/credentials/{credential_id}",
            get(show).delete(revoke),
        )
}

/// A credential as stored.
#[derive(Debug, Clone)]
pub struct CredentialRow {
    pub id: String,
    pub cluster_id: String,
    pub name: Option<String>,
    pub access_type: String,
    pub allowed_actions: Vec<String>,
    pub allowed_index_globs: Vec<String>,
    pub allowed_cidrs: Vec<String>,
    pub username: String,
    pub password: String,
    pub created_at: Timestamp,
}

const CREDENTIAL_COLUMNS: &str = "id, cluster_id, name, access_type, allowed_actions,
    allowed_index_globs, allowed_cidrs, username, password, created_at";

fn credential_row(r: &Row) -> rusqlite::Result<CredentialRow> {
    Ok(CredentialRow {
        id: r.get(0)?,
        cluster_id: r.get(1)?,
        name: r.get(2)?,
        access_type: r.get(3)?,
        allowed_actions: db::json(r, 4)?,
        allowed_index_globs: db::json(r, 5)?,
        allowed_cidrs: db::json(r, 6)?,
        username: r.get(7)?,
        password: r.get(8)?,
        created_at: db::ts(r, 9)?,
    })
}

/// An `AccessType` from its wire string; an open set takes any string.
fn access_type(s: &str) -> AccessType {
    serde_json::from_value(Value::String(s.to_string())).expect("an open set takes any string")
}

impl Recordable for CredentialRow {
    const KIND: &'static str = "credential";
    fn id(&self) -> &str {
        &self.id
    }
}

impl CredentialRow {
    /// The cluster's active credentials, newest first; none once it's terminated.
    pub fn active(conn: &Connection, cluster: &ClusterRow) -> rusqlite::Result<Vec<CredentialRow>> {
        if cluster.terminated_at.is_some() {
            return Ok(Vec::new());
        }
        conn.prepare(&format!(
            "SELECT {CREDENTIAL_COLUMNS} FROM credentials
             WHERE cluster_id = ?1 AND revoked_at IS NULL ORDER BY rowid DESC"
        ))?
        .query_map([&cluster.id], credential_row)?
        .collect()
    }

    /// The cluster's active credential `id`; `None` once it's revoked or the
    /// cluster is terminated.
    pub fn find_active(
        conn: &Connection,
        cluster: &ClusterRow,
        id: &str,
    ) -> rusqlite::Result<Option<CredentialRow>> {
        if cluster.terminated_at.is_some() {
            return Ok(None);
        }
        conn.query_row(
            &format!(
                "SELECT {CREDENTIAL_COLUMNS} FROM credentials
                 WHERE id = ?1 AND cluster_id = ?2 AND revoked_at IS NULL"
            ),
            [id, &cluster.id],
            credential_row,
        )
        .optional()
    }

    /// The ID of the cluster's default credential: its oldest active `rw` one,
    /// as the Bonsai API picks it.
    pub fn default_id(conn: &Connection, cluster_id: &str) -> rusqlite::Result<Option<String>> {
        conn.query_row(
            "SELECT id FROM credentials
             WHERE cluster_id = ?1 AND revoked_at IS NULL AND access_type = 'rw'
             ORDER BY rowid LIMIT 1",
            [cluster_id],
            |r| r.get(0),
        )
        .optional()
    }

    /// `default_id` is the cluster's [`Self::default_id`].
    pub fn to_api(&self, default_id: Option<&str>) -> Credential {
        Credential {
            id: self.id.clone(),
            name: self.name.clone(),
            access_type: access_type(&self.access_type),
            default: default_id == Some(self.id.as_str()),
            allowed_actions: self.allowed_actions.clone(),
            allowed_index_globs: self.allowed_index_globs.clone(),
            allowed_cidrs: self.allowed_cidrs.clone(),
            created_at: self.created_at,
        }
    }

    /// With its secret and a complete `https://user:pass@host` URL.
    pub fn with_secret(&self, conn: &Connection) -> rusqlite::Result<CredentialWithSecret> {
        let host = engine::cluster_host(conn, &self.cluster_id)?;
        let default_id = Self::default_id(conn, &self.cluster_id)?;
        Ok(CredentialWithSecret {
            credential: self.to_api(default_id.as_deref()),
            username: self.username.clone(),
            password: self.password.clone(),
            url: format!("https://{}:{}@{host}", self.username, self.password),
        })
    }
}

/// `Cache-Control: no-store`, on every response that carries a secret.
fn no_store(resp: &mut Response) {
    resp.headers_mut()
        .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
}

// ---------------------------------------------------------------- list & show

/// Active credentials, newest first, without secrets.
async fn list(
    State(state): State<AppState>,
    auth: Auth,
    Path(cluster_id): Path<String>,
    q: QueryParams,
) -> Result<Json<Page<Credential>>, ProblemBuilder> {
    auth.require(Op::ReadClusterCredentials)?;
    let page = PageParams::from_query(&q)?;
    let creds = state.db(|c| -> Result<Vec<Credential>, ProblemBuilder> {
        let cluster = ClusterRow::find_current(c, &auth, &cluster_id, state.step_ms())?;
        let default_id = CredentialRow::default_id(c, &cluster.id)?;
        Ok(CredentialRow::active(c, &cluster)?
            .iter()
            .map(|row| row.to_api(default_id.as_deref()))
            .collect())
    })?;
    Ok(Json(paginate(creds, page)))
}

/// The credential, with its secret.
async fn show(
    State(state): State<AppState>,
    auth: Auth,
    Path((cluster_id, id)): Path<(String, String)>,
) -> Result<Response, ProblemBuilder> {
    auth.require(Op::ReadClusterCredentials)?;
    let cred = state.db(|c| -> Result<CredentialWithSecret, ProblemBuilder> {
        let cluster = ClusterRow::find_current(c, &auth, &cluster_id, state.step_ms())?;
        let row = CredentialRow::find_active(c, &cluster, &id)?.ok_or_else(not_found)?;
        Ok(row.with_secret(c)?)
    })?;
    let mut resp = Json(cred).into_response();
    no_store(&mut resp);
    Ok(resp)
}

// ---------------------------------------------------------------- create

/// Checks the body as the Bonsai API does, then
/// creates the credential idempotently: a replay returns it again, as it is
/// now, or 409 once it's revoked.
async fn create(
    State(state): State<AppState>,
    auth: Auth,
    Path(cluster_id): Path<String>,
    headers: HeaderMap,
    body: Result<AnyJsonBody, ProblemBuilder>,
) -> Result<Response, ProblemBuilder> {
    auth.require(Op::ManageClusterCredentials)?;
    state.db(|c| ClusterRow::find_current(c, &auth, &cluster_id, state.step_ms()))?;
    let body = create_body(body?.0)?;
    let path = format!("/v1/clusters/{cluster_id}/credentials");
    let key = Idempotency::from_request(&auth, &headers, "POST", &path, &body)?;
    let (made, secret) = state.db(|c| -> Result<_, ProblemBuilder> {
        let cluster = ClusterRow::find_current(c, &auth, &cluster_id, state.step_ms())?;
        let replay = |c: &Connection, id: Option<&str>| {
            let gone = || {
                replay_unavailable(
                    "The request with this Idempotency-Key succeeded, but its credential can no longer be returned.",
                )
            };
            let id = id.ok_or_else(gone)?;
            CredentialRow::find_active(c, &cluster, id)?.ok_or_else(gone)
        };
        let start = |c: &Connection| create_credential(c, &auth, &cluster, &body);
        let made = http::idempotently(c, key.as_ref(), StatusCode::CREATED, replay, start)?;
        let secret = made.value.with_secret(c)?;
        Ok((made, secret))
    })?;

    let mut resp = (StatusCode::CREATED, Json(secret)).into_response();
    no_store(&mut resp);
    let h = resp.headers_mut();
    let location = state.url(&format!("{path}/{}", made.value.id));
    h.insert(
        header::LOCATION,
        HeaderValue::from_str(&location).expect("ASCII URL"),
    );
    if made.replayed {
        h.insert("idempotent-replayed", HeaderValue::from_static("true"));
    }
    Ok(resp)
}

/// The body's `CredentialCreate` fields, checked one at a time in the Bonsai
/// API's order, each fault a 400 `invalid_parameter` naming its field: an object,
/// `access_type` a string, `name` a string of 1 to 255 characters, then each
/// `allowed_*` that was sent an array of strings. Fields the schema doesn't
/// name are refused first, each named, as the Bonsai API refuses them.
fn create_body(body: Value) -> Result<CredentialCreate, ProblemBuilder> {
    let obj = match body {
        Value::Null => return Err(body_required()),
        Value::Object(obj) => obj,
        _ => {
            return Err(status_problem(
                StatusCode::BAD_REQUEST,
                ProblemCode::InvalidParameter,
                "The body must be a JSON object.",
            )
            .push_detail(not_an_object()));
        }
    };
    let unknown: Vec<&String> = obj
        .keys()
        .filter(|k| !CREATE_FIELDS.contains(&k.as_str()))
        .collect();
    if !unknown.is_empty() {
        let problem = status_problem(
            StatusCode::BAD_REQUEST,
            ProblemCode::InvalidParameter,
            "The body doesn't match its schema. See details.",
        );
        return Err(unknown.into_iter().fold(problem, |p, name| {
            p.field(
                &format!("/{name}"),
                format!("{name} is not a field of this operation."),
            )
        }));
    }
    let Some(kind) = obj.get("access_type").and_then(Value::as_str) else {
        return Err(invalid_parameter(
            "/access_type",
            "access_type is required and must be a string.",
        ));
    };
    let name = obj
        .get("name")
        .and_then(Value::as_str)
        .filter(|n| (1..=NAME_MAX_LENGTH).contains(&n.chars().count()))
        .ok_or_else(|| {
            invalid_parameter(
                "/name",
                format!(
                    "name is required and must be a string of 1 to {NAME_MAX_LENGTH} characters."
                ),
            )
        })?;
    let strings = |field: &str| -> Result<Option<Vec<String>>, ProblemBuilder> {
        let Some(value) = obj.get(field) else {
            return Ok(None);
        };
        value
            .as_array()
            .and_then(|a| {
                a.iter()
                    .map(|v| v.as_str().map(str::to_string))
                    .collect::<Option<Vec<_>>>()
            })
            .map(Some)
            .ok_or_else(|| {
                invalid_parameter(
                    &format!("/{field}"),
                    format!("{field} must be an array of strings."),
                )
            })
    };
    Ok(CredentialCreate {
        name: name.to_string(),
        access_type: access_type(kind),
        allowed_actions: strings("allowed_actions")?,
        allowed_index_globs: strings("allowed_index_globs")?,
        allowed_cidrs: strings("allowed_cidrs")?,
    })
}

/// 422 `validation_failed` with a `details` entry per `(field, message)`.
fn validation_failed(detail: &str, details: Vec<(Option<&str>, String)>) -> ProblemBuilder {
    details.into_iter().fold(
        status_problem(
            StatusCode::UNPROCESSABLE_ENTITY,
            ProblemCode::ValidationFailed,
            detail,
        ),
        |p, (field, message)| {
            p.push_detail(ProblemDetail {
                field: field.map(str::to_string),
                message,
                scope: None,
            })
        },
    )
}

/// 422 `validation_failed` whose one detail, naming no field, repeats it.
fn refused(message: &str) -> ProblemBuilder {
    validation_failed(message, vec![(None, message.to_string())])
}

/// How many active credentials a cluster of each tier may hold, and which
/// access types it takes, as on the Bonsai API.
fn tier_rules(tier: &str) -> (usize, &'static [&'static str]) {
    match tier {
        "hobby" => (2, &["rw"]),
        "standard" => (10, &["rw", "ro"]),
        "business" => (100, &["rw", "ro", "rwm", "acl"]),
        _ => (1_000, &["rw", "ro", "rwm", "acl"]),
    }
}

/// `hobby` -> `Hobby`, the tier's name as the Bonsai API shows it.
fn tier_name(tier: &str) -> String {
    let mut chars = tier.chars();
    chars
        .next()
        .map(|c| c.to_uppercase().chain(chars).collect())
        .unwrap_or_default()
}

/// Ruby's `/\A\s*\z/`: empty, or only ASCII whitespace.
fn is_blank(s: &str) -> bool {
    s.bytes().all(|b| b" \t\r\n\x0b\x0c".contains(&b))
}

/// The list without `"*"` and blank entries, as the Bonsai API stores it.
fn clean(list: Option<&Vec<String>>) -> Vec<String> {
    list.into_iter()
        .flatten()
        .filter(|s| *s != "*" && !is_blank(s))
        .cloned()
        .collect()
}

/// Creates the credential, or refuses as the Bonsai API does: its request checks,
/// then its credential validations, in that order. `allowed_*` are stored for
/// any access type, as the Bonsai API stores them.
fn create_credential(
    conn: &Connection,
    auth: &Auth,
    cluster: &ClusterRow,
    body: &CredentialCreate,
) -> Result<CredentialRow, ProblemBuilder> {
    let kind = body.access_type.as_str();
    if cluster.terminated_at.is_some() {
        return Err(status_problem(
            StatusCode::UNPROCESSABLE_ENTITY,
            ProblemCode::ClusterTerminated,
            "The cluster is terminated.",
        ));
    }
    if !ACCESS_TYPES.contains(&kind) {
        let message = format!("access_type must be one of {}.", ACCESS_TYPES.join(", "));
        return Err(validation_failed(
            &message,
            vec![(Some("/access_type"), message.clone())],
        ));
    }
    if matches!(kind, "rwm" | "acl") {
        let enabled: bool = conn.query_row(
            "SELECT advanced_credentials FROM accounts WHERE id = ?1",
            [&auth.account_id],
            |r| r.get(0),
        )?;
        if !enabled {
            return Err(status_problem(
                StatusCode::FORBIDDEN,
                ProblemCode::FeatureNotEnabled,
                format!("{kind} credentials are not enabled for this token's owner."),
            ));
        }
    }
    if cluster.state(&ActionRow::for_cluster(conn, &cluster.id)?) != ClusterState::Ready {
        return Err(refused(
            "The cluster must be ready before it can get new credentials.",
        ));
    }
    let plan = match &cluster.plan_id {
        Some(id) => PlanRow::find(conn, id)?,
        None => None,
    };
    let tier = plan.as_ref().map_or("standard", |p| p.tier.as_str());
    let active = CredentialRow::active(conn, cluster)?.len();
    if tier == "hobby" && active > 0 {
        return Err(refused("A Hobby cluster may have one active credential."));
    }

    // The record's validations: all of them, in the Bonsai API's order.
    let mut errors: Vec<(Option<&str>, String)> = Vec::new();
    if body.name.trim().is_empty() {
        errors.push((Some("/name"), "Name can't be blank".into()));
    }
    let (limit, types) = tier_rules(tier);
    if active == limit {
        errors.push((
            None,
            format!(
                "{} clusters are limited to {limit} active credentials",
                tier_name(tier)
            ),
        ));
    }
    if kind != "rw" {
        // A paid Hobby plan may have ro; a free one may not.
        let hobby_ro = tier == "hobby" && kind == "ro";
        let free = plan.as_ref().is_some_and(|p| p.plan.price_in_cents == 0);
        if hobby_ro && free {
            errors.push((
                Some("/access_type"),
                "Access type Read-only access is not allowed for Hobby clusters".into(),
            ));
        }
        let paid_hobby_ro = hobby_ro && !free;
        if !paid_hobby_ro && !types.contains(&kind) {
            errors.push((
                Some("/access_type"),
                format!(
                    "Access type {} clusters are limited to {} access types",
                    tier_name(tier),
                    types.join(", ")
                ),
            ));
        }
    }
    if !errors.is_empty() {
        return Err(validation_failed("The credential is not valid.", errors));
    }

    let (actions, globs, cidrs) = (
        clean(body.allowed_actions.as_ref()),
        clean(body.allowed_index_globs.as_ref()),
        clean(body.allowed_cidrs.as_ref()),
    );
    let id = engine::insert_credential(
        conn,
        &cluster.id,
        &NewCredential {
            name: &body.name,
            access_type: kind,
            allowed_actions: &actions,
            allowed_index_globs: &globs,
            allowed_cidrs: &cidrs,
        },
        Timestamp::now(),
    )?;
    Ok(CredentialRow::find_active(conn, cluster, &id)?.expect("just inserted"))
}

// ---------------------------------------------------------------- revoke

/// 204, or 422 for the cluster's last active `rw` credential.
async fn revoke(
    State(state): State<AppState>,
    auth: Auth,
    Path((cluster_id, id)): Path<(String, String)>,
) -> Result<StatusCode, ProblemBuilder> {
    auth.require(Op::ManageClusterCredentials)?;
    state.db(|c| -> Result<(), ProblemBuilder> {
        let cluster = ClusterRow::find_current(c, &auth, &cluster_id, state.step_ms())?;
        let cred = CredentialRow::find_active(c, &cluster, &id)?.ok_or_else(not_found)?;
        if cred.access_type == "rw" {
            let rw = CredentialRow::active(c, &cluster)?
                .iter()
                .filter(|r| r.access_type == "rw")
                .count();
            if rw <= 1 {
                return Err(refused(LAST_RW));
            }
        }
        // As the Bonsai API revokes a credential: the password is forgotten.
        c.execute(
            "UPDATE credentials SET revoked_at = ?2, password = '' WHERE id = ?1",
            params![cred.id, db::now()],
        )?;
        Ok(())
    })?;
    Ok(StatusCode::NO_CONTENT)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tier_names_blanks_and_cleaning() {
        assert_eq!(tier_name("hobby"), "Hobby");
        assert_eq!(tier_name(""), "");
        assert!(is_blank(""));
        assert!(is_blank(" \t"));
        assert!(!is_blank(" a "));
        let list = vec!["*".to_string(), " ".into(), "a".into()];
        assert_eq!(clean(Some(&list)), ["a"]);
        assert!(clean(None).is_empty());
    }
}
