//! Clusters (`listClusters`, `createCluster`, `getCluster`, `updateCluster`,
//! `deleteCluster`, `changeClusterPlan`). A cluster's state is derived from its
//! Actions, as the Bonsai API reports a cluster's state; every handler advances the
//! Actions it reads first.

use std::collections::BTreeMap;

use axum::extract::{Path, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::Response;
use axum::routing::{get, post};
use axum::{Json, Router};
use bonsai_api::{
    ActionRef, ActionState, ActionType, Cluster, ClusterState, ClusterStats, Page, ProblemCode, Ref,
};
use jiff::Timestamp;
use rusqlite::{Connection, OptionalExtension, Row, params};

use crate::engine::{self, ActionRow, Outcome};
use crate::http::{Auth, JsonBody, Op, PageParams, QueryParams, paginate, string_body};
use crate::problem::{ProblemBuilder, invalid_parameter, not_found, status_problem};
use crate::routes::actions::{accept, start_idempotently};
use crate::routes::catalogue::PlanRow;
use crate::{AppState, db, ids, scenario};

pub const NAME_MAX_LENGTH: usize = 512;

const CREATE_FIELDS: &[&str] = &["name", "plan", "region", "space", "release"];
const CREATE_REQUIRED: &[&str] = &["name", "plan", "region"];
const NAME_FIELD: &[&str] = &["name"];
const PLAN_FIELD: &[&str] = &["plan"];

/// Every `ClusterState` the `state` filter takes.
const STATES: &[&str] = &[
    "draft",
    "provisioning",
    "ready",
    "failed",
    "read_only",
    "disabled",
    "deprovisioning",
    "deprovisioned",
];

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/v1/clusters", get(list).post(create))
        .route(
            "/v1/clusters/{cluster_id}",
            get(show).patch(update).delete(destroy),
        )
        .route("/v1/clusters/{cluster_id}/plan_changes", post(change_plan))
}

/// A cluster as stored.
#[derive(Debug, Clone)]
pub struct ClusterRow {
    pub id: String,
    pub account_id: String,
    pub name: String,
    pub plan_id: Option<String>,
    pub region: String,
    pub space: Option<String>,
    pub release: String,
    pub single_tenant: bool,
    pub host: Option<String>,
    pub created_at: Timestamp,
    pub terminated_at: Option<Timestamp>,
    pub docs: Option<u64>,
    pub shards: Option<u64>,
    pub bytes: Option<u64>,
    pub polled_at: Option<Timestamp>,
}

const CLUSTER_COLUMNS: &str = "id, account_id, name, plan_id, region, space, release,
    single_tenant, host, created_at, terminated_at, docs, shards, bytes, polled_at";

fn cluster_row(r: &Row) -> rusqlite::Result<ClusterRow> {
    Ok(ClusterRow {
        id: r.get(0)?,
        account_id: r.get(1)?,
        name: r.get(2)?,
        plan_id: r.get(3)?,
        region: r.get(4)?,
        space: r.get(5)?,
        release: r.get(6)?,
        single_tenant: r.get(7)?,
        host: r.get(8)?,
        created_at: db::ts(r, 9)?,
        terminated_at: db::ts_opt(r, 10)?,
        docs: r.get(11)?,
        shards: r.get(12)?,
        bytes: r.get(13)?,
        polled_at: db::ts_opt(r, 14)?,
    })
}

impl ClusterRow {
    /// The account's cluster `id`, terminated or not.
    pub fn find(
        conn: &Connection,
        account_id: &str,
        id: &str,
    ) -> rusqlite::Result<Option<ClusterRow>> {
        conn.query_row(
            &format!("SELECT {CLUSTER_COLUMNS} FROM clusters WHERE id = ?1 AND account_id = ?2"),
            [id, account_id],
            cluster_row,
        )
        .optional()
    }

    /// [`ClusterRow::find`] after advancing the cluster's Actions to now; 404
    /// `not_found` when the account has no such cluster.
    pub fn find_current(
        conn: &Connection,
        auth: &Auth,
        id: &str,
        step_ms: u64,
    ) -> Result<ClusterRow, ProblemBuilder> {
        if ClusterRow::find(conn, &auth.account_id, id)?.is_none() {
            return Err(not_found());
        }
        engine::advance_cluster(conn, id, Timestamp::now(), step_ms)?;
        ClusterRow::find(conn, &auth.account_id, id)?.ok_or_else(not_found)
    }

    /// Derived from the cluster's Actions, as the Bonsai API reports it.
    pub fn state(&self, actions: &[ActionRow]) -> ClusterState {
        let newest = |kind: ActionType| actions.iter().find(|a| a.kind == kind);
        if self.terminated_at.is_some() {
            return match newest(ActionType::Deprovision) {
                Some(a) if !a.is_finished() => ClusterState::Deprovisioning,
                _ => ClusterState::Deprovisioned,
            };
        }
        match newest(ActionType::Provision) {
            None => ClusterState::Draft,
            Some(a) if !a.is_finished() => ClusterState::Provisioning,
            Some(a) if a.state == ActionState::Error => ClusterState::Failed,
            Some(_) => ClusterState::Ready,
        }
    }

    pub fn to_api(&self, conn: &Connection) -> rusqlite::Result<Cluster> {
        let actions = ActionRow::for_cluster(conn, &self.id)?;
        let state = self.state(&actions);
        let draft = state == ClusterState::Draft;
        let plan = match (&self.plan_id, draft) {
            (Some(id), false) => PlanRow::find(conn, id)?,
            _ => None,
        };
        let named = |table: &str, id: &str| -> rusqlite::Result<Ref> {
            conn.query_row(
                &format!("SELECT id, name FROM {table} WHERE id = ?1"),
                [id],
                |r| {
                    Ok(Ref {
                        id: r.get(0)?,
                        name: r.get(1)?,
                    })
                },
            )
        };
        let no_host = matches!(
            state,
            ClusterState::Draft | ClusterState::Provisioning | ClusterState::Failed
        );
        let polled = self.polled_at.is_some();
        Ok(Cluster {
            id: self.id.clone(),
            name: self.name.clone(),
            state,
            plan: plan.as_ref().map(|p| Ref {
                id: p.plan.id.clone(),
                name: p.plan.name.clone(),
            }),
            region: named("regions", &self.region)?,
            space: self
                .space
                .as_deref()
                .map(|s| named("spaces", s))
                .transpose()?,
            release: named("releases", &self.release)?,
            single_tenant: self.single_tenant,
            host: if no_host { None } else { self.host.clone() },
            port: 443,
            stats: ClusterStats {
                docs: self.docs.filter(|_| polled),
                shards: self.shards.filter(|_| polled),
                bytes: self.bytes.filter(|_| polled),
                polled_at: self.polled_at,
                limits: plan.and_then(|p| p.limits),
            },
            latest_action: actions.first().map(|a| ActionRef {
                id: a.id.clone(),
                r#type: a.kind.clone(),
                state: a.state.clone(),
            }),
            created_at: self.created_at,
            terminated_at: self.terminated_at,
        })
    }

    /// The plan change still running on this cluster, if any. Only another
    /// plan change blocks one, as on the Bonsai API.
    fn running_plan_change(&self, conn: &Connection) -> rusqlite::Result<Option<ActionRow>> {
        Ok(ActionRow::for_cluster(conn, &self.id)?
            .into_iter()
            .find(|a| a.kind == ActionType::ChangePlan && !a.is_finished()))
    }
}

/// 409 `action_in_progress` for a plan change while another runs, retried
/// when the running one's next step is due.
fn action_in_progress(running: &ActionRow, step_ms: u64) -> ProblemBuilder {
    status_problem(
        StatusCode::CONFLICT,
        ProblemCode::ActionInProgress,
        format!(
            "A plan change ({}) is already in progress for this cluster. Wait for it to finish.",
            running.id
        ),
    )
    .retry_after(running.retry_after(Timestamp::now(), step_ms).unwrap_or(1))
}

fn cluster_terminated(id: &str) -> ProblemBuilder {
    status_problem(
        StatusCode::UNPROCESSABLE_ENTITY,
        ProblemCode::ClusterTerminated,
        format!("Cluster {id} is deprovisioned."),
    )
}

fn validation_failed(details: Vec<(&str, String)>) -> ProblemBuilder {
    validation_failed_with("The request was refused. See details.", details)
}

fn validation_failed_with(detail: &str, details: Vec<(&str, String)>) -> ProblemBuilder {
    details.into_iter().fold(
        status_problem(
            StatusCode::UNPROCESSABLE_ENTITY,
            ProblemCode::ValidationFailed,
            detail,
        ),
        |p, (field, message)| p.field(field, message),
    )
}

/// 422 `validation_failed` on `/name` unless it's 1..=512 characters.
fn check_name(name: &str) -> Result<(), ProblemBuilder> {
    let len = name.chars().count();
    if len == 0 {
        return Err(validation_failed(vec![(
            "/name",
            "Name can't be blank.".into(),
        )]));
    }
    if len > NAME_MAX_LENGTH {
        return Err(validation_failed(vec![(
            "/name",
            format!("Name is too long (maximum is {NAME_MAX_LENGTH} characters)."),
        )]));
    }
    Ok(())
}

/// Whether the account must add a card before using a paid plan.
fn needs_card(conn: &Connection, account_id: &str, plan: &PlanRow) -> rusqlite::Result<bool> {
    if plan.plan.price_in_cents == 0 {
        return Ok(false);
    }
    let (card, invoice): (bool, bool) = conn.query_row(
        "SELECT has_payment_method, billed_by_invoice FROM accounts WHERE id = ?1",
        [account_id],
        |r| Ok((r.get(0)?, r.get(1)?)),
    )?;
    Ok(!card && !invoice)
}

/// Where a person adds a card to the account: the mock's checkout page, or
/// `--checkout-url` and a fresh session ID.
fn checkout_url(state: &AppState, account_id: &str) -> String {
    match state.checkout_url() {
        Some(prefix) => format!("{prefix}{}", ids::checkout_session_id()),
        None => state.url(&format!("/_mock/checkout/{account_id}")),
    }
}

fn payment_method_required(state: &AppState, account_id: &str, detail: &str) -> ProblemBuilder {
    status_problem(
        StatusCode::UNPROCESSABLE_ENTITY,
        ProblemCode::PaymentMethodRequired,
        detail,
    )
    .action_url(checkout_url(state, account_id))
}

// ---------------------------------------------------------------- list & show

async fn list(
    State(state): State<AppState>,
    auth: Auth,
    q: QueryParams,
) -> Result<Json<Page<Cluster>>, ProblemBuilder> {
    auth.require(Op::ReadClusters)?;
    let page = PageParams::from_query(&q)?;
    let name = q.get("name").map(str::to_lowercase);
    if name
        .as_ref()
        .is_some_and(|n| n.chars().count() > NAME_MAX_LENGTH)
    {
        return Err(invalid_parameter(
            "name",
            format!("name must be at most {NAME_MAX_LENGTH} characters."),
        ));
    }
    let wanted_state = q.get("state");
    if let Some(s) = wanted_state
        && !STATES.contains(&s)
    {
        return Err(invalid_parameter(
            "state",
            format!("state must be one of {}.", STATES.join(", ")),
        ));
    }
    let single_tenant = match q.get("single_tenant") {
        None => None,
        Some("true") => Some(true),
        Some("false") => Some(false),
        Some(_) => {
            return Err(invalid_parameter(
                "single_tenant",
                "single_tenant must be true or false.",
            ));
        }
    };
    let (region, space) = (q.get("region"), q.get("space"));

    let clusters = state.db(|c| -> rusqlite::Result<Vec<Cluster>> {
        engine::advance_account(c, &auth.account_id, Timestamp::now(), state.step_ms())?;
        let rows: Vec<ClusterRow> = c
            .prepare(&format!(
                "SELECT {CLUSTER_COLUMNS} FROM clusters
                 WHERE account_id = ?1 AND terminated_at IS NULL ORDER BY rowid DESC"
            ))?
            .query_map([&auth.account_id], cluster_row)?
            .collect::<rusqlite::Result<_>>()?;
        let mut out = Vec::new();
        for row in rows {
            let keep = name
                .as_ref()
                .is_none_or(|n| row.name.to_lowercase().contains(n))
                && region.is_none_or(|r| row.region == r)
                && space.is_none_or(|s| row.space.as_deref() == Some(s))
                && single_tenant.is_none_or(|st| row.single_tenant == st);
            if !keep {
                continue;
            }
            let cluster = row.to_api(c)?;
            if wanted_state.is_none_or(|s| cluster.state.as_str() == s) {
                out.push(cluster);
            }
        }
        Ok(out)
    })?;
    Ok(Json(paginate(clusters, page)))
}

async fn show(
    State(state): State<AppState>,
    auth: Auth,
    Path(id): Path<String>,
) -> Result<Json<Cluster>, ProblemBuilder> {
    auth.require(Op::ReadClusters)?;
    let cluster = state.db(|c| -> Result<Cluster, ProblemBuilder> {
        let row = ClusterRow::find_current(c, &auth, &id, state.step_ms())?;
        Ok(row.to_api(c)?)
    })?;
    Ok(Json(cluster))
}

// ---------------------------------------------------------------- create

async fn create(
    State(state): State<AppState>,
    auth: Auth,
    headers: HeaderMap,
    body: Result<JsonBody, ProblemBuilder>,
) -> Result<Response, ProblemBuilder> {
    auth.require(Op::CreateCluster)?;
    let body = string_body(body?.0, CREATE_FIELDS, CREATE_REQUIRED)?;
    let action = start_idempotently(
        &state,
        &auth,
        &headers,
        "POST",
        "/v1/clusters",
        &body,
        |c| provision(&state, c, &auth, &body),
    )?;
    Ok(accept(&state, action))
}

/// Creates the cluster and starts its provision Action, or refuses.
fn provision(
    state: &AppState,
    conn: &Connection,
    auth: &Auth,
    body: &BTreeMap<String, String>,
) -> Result<ActionRow, ProblemBuilder> {
    let field = |k: &str| body.get(k).map(String::as_str);
    let name = field("name").unwrap_or_default();
    check_name(name)?;

    let plan_id = field("plan").unwrap_or_default();
    let region_id = field("region").unwrap_or_default();
    let plan = PlanRow::find(conn, plan_id)?;
    let region_exists: bool = conn.query_row(
        "SELECT EXISTS (SELECT 1 FROM regions WHERE id = ?1)",
        [region_id],
        |r| r.get(0),
    )?;

    let mut details: Vec<(&str, String)> = Vec::new();
    if plan.is_none() {
        details.push((
            "/plan",
            format!("No plan {plan_id} is available to this account."),
        ));
    }
    if !region_exists {
        details.push((
            "/region",
            format!("No region {region_id} accepts new clusters."),
        ));
    } else if let Some(p) = &plan
        && !p.plan.regions.iter().any(|r| r == region_id)
    {
        details.push((
            "/region",
            format!("Plan {plan_id} isn't offered in region {region_id}."),
        ));
    }
    if region_exists && let Some(space_id) = field("space") {
        let in_region: bool = conn.query_row(
            "SELECT EXISTS (SELECT 1 FROM spaces WHERE id = ?1 AND account_id = ?2 AND region = ?3)",
            [space_id, &auth.account_id, region_id],
            |r| r.get(0),
        )?;
        if !in_region {
            details.push((
                "/space",
                format!("No space {space_id} of this account is in region {region_id}."),
            ));
        }
    }
    // The release is the one asked for, if the plan offers it in the region,
    // or else the region's first in offer order (OpenSearch, then newest).
    let offered = plan.as_ref().map_or(&[][..], |p| p.releases_in(region_id));
    let release = match field("release") {
        None => offered.first().cloned(),
        Some(r) => offered.iter().find(|x| *x == r).cloned(),
    };
    if let Some(p) = &plan
        && release.is_none()
    {
        let r = field("release").unwrap_or_default();
        let region_offered = p.plan.regions.iter().any(|x| x == region_id);
        let unknown = field("release").is_some() && !p.plan.releases.iter().any(|x| x == r);
        if unknown || (region_offered && offered.is_empty()) {
            details.push((
                "/release",
                format!("No release {r} is available for this plan."),
            ));
        } else if region_offered {
            details.push((
                "/release",
                format!("Release {r} isn't offered for plan {plan_id} in region {region_id}."),
            ));
        }
    }
    if !details.is_empty() {
        return Err(validation_failed(details));
    }
    let plan = plan.expect("checked above");
    let release = release.expect("checked above");

    if plan.tier == "enterprise" && plan.dedicated {
        return Err(validation_failed_with(
            &format!(
                "Plan {plan_id} needs its own servers; Bonsai support provisions it. Contact support@bonsai.io."
            ),
            vec![(
                "/plan",
                format!("Plan {plan_id} needs its own servers; Bonsai support provisions it."),
            )],
        ));
    }
    if needs_card(conn, &auth.account_id, &plan)? {
        return Err(payment_method_required(
            state,
            &auth.account_id,
            "Add a payment method to the account before creating a paid cluster.",
        ));
    }
    // Billing starts in the request, before anything is built (v1.yml), so a
    // refused charge is answered here and nothing is created. A free plan
    // charges nothing, so it leaves the scenario for a paid create.
    if plan.plan.price_in_cents > 0
        && scenario::get(conn, "fail_next")?
            .as_ref()
            .and_then(|v| v.as_str())
            == Some("payment_failed")
    {
        scenario::clear(conn, "fail_next")?;
        return Err(status_problem(
            StatusCode::UNPROCESSABLE_ENTITY,
            ProblemCode::PaymentFailed,
            "Bonsai couldn't charge the account for this cluster. Add a card at action_url.",
        )
        .action_url(checkout_url(state, &auth.account_id)));
    }

    let now = Timestamp::now();
    let outcome = Outcome::take_from_scenario(conn)?;
    let action = db::atomically(conn, |c| -> rusqlite::Result<ActionRow> {
        let id = ids::cluster_id(name);
        c.execute(
            "INSERT INTO clusters (id, account_id, name, plan_id, region, space, release,
                single_tenant, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
            params![
                id,
                auth.account_id,
                name,
                plan.plan.id,
                region_id,
                field("space"),
                release,
                plan.plan.single_tenant,
                now.to_string()
            ],
        )?;
        engine::create_default_credential(c, &id, now)?;
        engine::create(
            c,
            &auth.account_id,
            &id,
            ActionType::Provision,
            None,
            outcome,
            now,
        )
    })?;
    Ok(action)
}

// ---------------------------------------------------------------- update

async fn update(
    State(state): State<AppState>,
    auth: Auth,
    Path(id): Path<String>,
    body: Result<JsonBody, ProblemBuilder>,
) -> Result<Json<Cluster>, ProblemBuilder> {
    auth.require(Op::UpdateCluster)?;
    let cluster = state.db(|c| -> Result<Cluster, ProblemBuilder> {
        let row = ClusterRow::find_current(c, &auth, &id, state.step_ms())?;
        let body = string_body(body?.0, NAME_FIELD, NAME_FIELD)?;
        if row.terminated_at.is_some() {
            return Err(cluster_terminated(&row.id));
        }
        let name = &body["name"];
        check_name(name)?;
        c.execute(
            "UPDATE clusters SET name = ?2 WHERE id = ?1",
            params![row.id, name],
        )?;
        let row = ClusterRow::find_current(c, &auth, &id, state.step_ms())?;
        Ok(row.to_api(c)?)
    })?;
    Ok(Json(cluster))
}

// ---------------------------------------------------------------- delete

async fn destroy(
    State(state): State<AppState>,
    auth: Auth,
    Path(id): Path<String>,
    q: QueryParams,
    headers: HeaderMap,
) -> Result<Response, ProblemBuilder> {
    auth.require(Op::DestroyCluster)?;
    state.db(|c| ClusterRow::find_current(c, &auth, &id, state.step_ms()))?;
    let Some(name) = q.get("name").map(str::to_string) else {
        return Err(status_problem(
            StatusCode::BAD_REQUEST,
            ProblemCode::NameConfirmationRequired,
            "Pass the cluster's current name as `name`, to confirm the deletion.",
        )
        .field("name", "name is required."));
    };
    let body = BTreeMap::from([("name".to_string(), name.clone())]);
    let path = format!("/v1/clusters/{id}");
    let action = start_idempotently(&state, &auth, &headers, "DELETE", &path, &body, |c| {
        let row = ClusterRow::find_current(c, &auth, &id, state.step_ms())?;
        if row.terminated_at.is_some() {
            return Err(cluster_terminated(&row.id));
        }
        if name != row.name {
            return Err(status_problem(
                StatusCode::UNPROCESSABLE_ENTITY,
                ProblemCode::NameMismatch,
                "`name` doesn't match the cluster's current name.",
            )
            .field(
                "name",
                "name must equal the cluster's current name exactly.",
            ));
        }
        let now = Timestamp::now();
        let outcome = Outcome::take_from_scenario(c)?;
        // Terminated as the Action starts, as on the Bonsai API, so the cluster
        // is `deprovisioning` until the Action finishes. Nothing running blocks
        // it, as on the Bonsai API: a cluster still provisioning
        // is terminated too, and its provision Action left to finish.
        Ok(db::atomically(c, |c| -> rusqlite::Result<ActionRow> {
            c.execute(
                "UPDATE clusters SET terminated_at = ?2 WHERE id = ?1",
                params![row.id, now.to_string()],
            )?;
            engine::create(
                c,
                &auth.account_id,
                &row.id,
                ActionType::Deprovision,
                None,
                outcome,
                now,
            )
        })?)
    })?;
    Ok(accept(&state, action))
}

// ---------------------------------------------------------------- plan changes

async fn change_plan(
    State(state): State<AppState>,
    auth: Auth,
    Path(id): Path<String>,
    headers: HeaderMap,
    body: Result<JsonBody, ProblemBuilder>,
) -> Result<Response, ProblemBuilder> {
    auth.require(Op::UpdateCluster)?;
    state.db(|c| ClusterRow::find_current(c, &auth, &id, state.step_ms()))?;
    let body = string_body(body?.0, PLAN_FIELD, PLAN_FIELD)?;
    let path = format!("/v1/clusters/{id}/plan_changes");
    let action = start_idempotently(&state, &auth, &headers, "POST", &path, &body, |c| {
        let row = ClusterRow::find_current(c, &auth, &id, state.step_ms())?;
        if row.terminated_at.is_some() {
            return Err(cluster_terminated(&row.id));
        }
        let plan_id = &body["plan"];
        let Some(plan) = PlanRow::find(c, plan_id)? else {
            return Err(validation_failed(vec![(
                "/plan",
                format!("No plan {plan_id} is available to this account."),
            )]));
        };
        if needs_card(c, &auth.account_id, &plan)? {
            return Err(payment_method_required(
                &state,
                &auth.account_id,
                "Add a payment method to the account before changing to a paid plan.",
            ));
        }
        let current = match &row.plan_id {
            Some(p) => PlanRow::find(c, p)?,
            None => None,
        };
        if let Some(refusal) = plan_change_refusal(current.as_ref(), &plan) {
            return Err(status_problem(
                StatusCode::UNPROCESSABLE_ENTITY,
                ProblemCode::PlanChangeRefused,
                refusal,
            ));
        }
        // Only another plan change blocks one; a running provision doesn't
        // (the Bonsai API checks only for a plan change in progress).
        if let Some(running) = row.running_plan_change(c)? {
            return Err(action_in_progress(&running, state.step_ms()));
        }
        let outcome = Outcome::take_from_scenario(c)?;
        Ok(engine::create(
            c,
            &auth.account_id,
            &row.id,
            ActionType::ChangePlan,
            Some(&plan.plan.id),
            outcome,
            Timestamp::now(),
        )?)
    })?;
    Ok(accept(&state, action))
}

/// Why a cluster on `current` can't move to `target`, if it can't: Enterprise
/// plans are changed by Bonsai support, in or out, as are plans on their own
/// servers.
fn plan_change_refusal(current: Option<&PlanRow>, target: &PlanRow) -> Option<String> {
    let Some(current) = current else {
        return Some("This cluster has no plan yet, so it has none to change.".into());
    };
    if current.plan.id == target.plan.id {
        return Some(format!(
            "The cluster is already on plan {}.",
            target.plan.id
        ));
    }
    if current.tier == "enterprise" || target.tier == "enterprise" {
        return Some("Bonsai support changes Enterprise plans. Contact support@bonsai.io.".into());
    }
    if target.dedicated {
        return Some(format!(
            "Plan {} needs its own servers; Bonsai support provisions it.",
            target.plan.id
        ));
    }
    None
}
