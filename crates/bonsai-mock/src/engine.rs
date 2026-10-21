//! How Actions progress: by time, not by a worker.
//!
//! An Action stores when it started and its planned `outcome`. Step `k` (from
//! 0) completes `(k + 1) * step_ms` after the start; with `step_ms == 0` every
//! step completes on the next read. [`advance`] brings an Action's stored
//! steps, state and side effects up to `now`. It reads no clock of its own, is
//! idempotent, and every read of an Action or a cluster calls it first.
//!
//! As on the Bonsai API, a step records the state the Action moved to: `running` for
//! each step but the last, then `success` (or `error` for the step that
//! failed). A failed step's message, and the Action's `error.message`, are
//! fixed texts by code, never the cause. A provision that ends in error
//! terminates its cluster, as the Bonsai API does.

use bonsai_api::{
    Action, ActionError, ActionErrorCode, ActionResult, ActionState, ActionStep, ActionSubject,
    ActionType, SubjectType,
};
use jiff::{SignedDuration, Timestamp};
use rusqlite::{Connection, OptionalExtension, Row, params};

use crate::{db, ids, scenario};

/// The customer-facing steps of each Action type the mock starts.
pub fn steps_for(kind: &ActionType) -> &'static [&'static str] {
    match kind {
        ActionType::Provision => &[
            "Reserving capacity",
            "Provisioning cluster",
            "Configuring routes",
            "Running health check",
        ],
        ActionType::Deprovision => &[
            "Stopping billing",
            "Removing routes",
            "Deprovisioning cluster",
        ],
        ActionType::ChangePlan => &["Checking capacity", "Changing plan", "Updating billing"],
        _ => &[],
    }
}

/// How an Action ends, decided when it's created (from the `fail_next`
/// scenario) and stored with it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    Success,
    /// Errors at the second step with `action_failed`.
    ActionFailed,
    /// Errors at the second step with `timed_out`.
    TimedOut,
}

/// The step index (from 0) a failing outcome errors at.
const FAILING_STEP: usize = 1;

const ACTION_FAILED_MESSAGE: &str = "The Action failed. Bonsai support can see why.";
const TIMED_OUT_MESSAGE: &str = "The Action took too long and was stopped.";

impl Outcome {
    pub fn as_str(self) -> &'static str {
        match self {
            Outcome::Success => "success",
            Outcome::ActionFailed => "action_failed",
            Outcome::TimedOut => "timed_out",
        }
    }

    fn parse(s: &str) -> Self {
        match s {
            "action_failed" => Outcome::ActionFailed,
            "timed_out" => Outcome::TimedOut,
            _ => Outcome::Success,
        }
    }

    fn error(self) -> Option<ActionError> {
        let (code, message) = match self {
            Outcome::Success => return None,
            Outcome::ActionFailed => (ActionErrorCode::ActionFailed, ACTION_FAILED_MESSAGE),
            Outcome::TimedOut => (ActionErrorCode::TimedOut, TIMED_OUT_MESSAGE),
        };
        Some(ActionError {
            code,
            message: message.to_string(),
            action_url: None,
        })
    }

    /// The outcome for the next Action: consumes `fail_next` when it's
    /// `action_failed` or `timed_out`. Any other value is left for whatever
    /// reads it (`payment_failed` is `createCluster`'s).
    pub fn take_from_scenario(conn: &Connection) -> rusqlite::Result<Self> {
        let next = scenario::get(conn, "fail_next")?;
        let outcome = match next.as_ref().and_then(|v| v.as_str()) {
            Some("action_failed") => Outcome::ActionFailed,
            Some("timed_out") => Outcome::TimedOut,
            _ => return Ok(Outcome::Success),
        };
        scenario::clear(conn, "fail_next")?;
        Ok(outcome)
    }
}

/// An Action as stored.
#[derive(Debug, Clone, PartialEq)]
pub struct ActionRow {
    pub id: String,
    pub account_id: String,
    pub kind: ActionType,
    pub cluster_id: String,
    pub state: ActionState,
    pub steps: Vec<ActionStep>,
    /// How many planned steps have run.
    pub step_index: usize,
    pub started_at: Timestamp,
    pub finished_at: Option<Timestamp>,
    pub error: Option<ActionError>,
    pub result: Option<ActionResult>,
    /// The plan a `cluster.change_plan` moves to.
    pub plan_after: Option<String>,
    pub outcome: Outcome,
}

const ACTION_COLUMNS: &str = "id, account_id, type, cluster_id, state, steps, step_index,
    started_at, finished_at, error, result, plan_after, outcome";

/// Reads a nullable JSON `TEXT` column.
fn json_opt<T: serde::de::DeserializeOwned>(r: &Row, idx: usize) -> rusqlite::Result<Option<T>> {
    let s: Option<String> = r.get(idx)?;
    s.map(|s| {
        serde_json::from_str(&s).map_err(|e| {
            rusqlite::Error::FromSqlConversionFailure(idx, rusqlite::types::Type::Text, Box::new(e))
        })
    })
    .transpose()
}

fn action_row(r: &Row) -> rusqlite::Result<ActionRow> {
    Ok(ActionRow {
        id: r.get(0)?,
        account_id: r.get(1)?,
        kind: db::text_enum(r, 2)?,
        cluster_id: r.get(3)?,
        state: db::text_enum(r, 4)?,
        steps: db::json(r, 5)?,
        step_index: r.get(6)?,
        started_at: db::ts(r, 7)?,
        finished_at: db::ts_opt(r, 8)?,
        error: json_opt(r, 9)?,
        result: json_opt(r, 10)?,
        plan_after: r.get(11)?,
        outcome: Outcome::parse(&r.get::<_, String>(12)?),
    })
}

impl ActionRow {
    pub fn find(conn: &Connection, id: &str) -> rusqlite::Result<Option<ActionRow>> {
        conn.query_row(
            &format!("SELECT {ACTION_COLUMNS} FROM actions WHERE id = ?1"),
            [id],
            action_row,
        )
        .optional()
    }

    /// A cluster's Actions, newest first.
    pub fn for_cluster(conn: &Connection, cluster_id: &str) -> rusqlite::Result<Vec<ActionRow>> {
        conn.prepare(&format!(
            "SELECT {ACTION_COLUMNS} FROM actions WHERE cluster_id = ?1 ORDER BY rowid DESC"
        ))?
        .query_map([cluster_id], action_row)?
        .collect()
    }

    pub fn is_finished(&self) -> bool {
        matches!(self.state, ActionState::Success | ActionState::Error)
    }

    /// Seconds until the next step, for `Retry-After`: at least 1, and `None`
    /// once the Action has finished.
    pub fn retry_after(&self, now: Timestamp, step_ms: u64) -> Option<u64> {
        if self.is_finished() {
            return None;
        }
        let next = step_time(self.started_at, self.step_index, step_ms);
        let ms = next.duration_since(now).as_millis().max(0);
        let secs = u64::try_from((ms + 999) / 1000).unwrap_or(u64::MAX);
        Some(secs.max(1))
    }

    pub fn to_api(&self) -> Action {
        Action {
            id: self.id.clone(),
            r#type: self.kind.clone(),
            state: self.state.clone(),
            subject: ActionSubject {
                r#type: SubjectType::Cluster,
                id: self.cluster_id.clone(),
            },
            created_at: self.started_at,
            finished_at: self.finished_at,
            error: self.error.clone(),
            result: self.result.clone(),
            steps: self.steps.clone(),
        }
    }
}

/// When step `index` (from 0) completes.
fn step_time(started_at: Timestamp, index: usize, step_ms: u64) -> Timestamp {
    let ms = step_ms.saturating_mul(index as u64 + 1);
    let ms = i64::try_from(ms).unwrap_or(i64::MAX);
    started_at
        .checked_add(SignedDuration::from_millis(ms))
        .unwrap_or(Timestamp::MAX)
}

/// Starts an Action of `kind` on `cluster_id` at `now`, in state `new`.
pub fn create(
    conn: &Connection,
    account_id: &str,
    cluster_id: &str,
    kind: ActionType,
    plan_after: Option<&str>,
    outcome: Outcome,
    now: Timestamp,
) -> rusqlite::Result<ActionRow> {
    let id = ids::action_id();
    conn.execute(
        "INSERT INTO actions (id, account_id, type, cluster_id, state, steps, step_index,
            started_at, plan_after, outcome)
         VALUES (?1, ?2, ?3, ?4, 'new', '[]', 0, ?5, ?6, ?7)",
        params![
            id,
            account_id,
            kind.as_str(),
            cluster_id,
            now.to_string(),
            plan_after,
            outcome.as_str()
        ],
    )?;
    Ok(ActionRow::find(conn, &id)?.expect("just inserted"))
}

/// A credential to store.
#[derive(Debug, Clone, Default)]
pub struct NewCredential<'a> {
    pub name: &'a str,
    pub access_type: &'a str,
    pub allowed_actions: &'a [String],
    pub allowed_index_globs: &'a [String],
    pub allowed_cidrs: &'a [String],
}

/// Stores a credential on `cluster_id`, with a new username and password, and
/// returns its ID.
pub fn insert_credential(
    conn: &Connection,
    cluster_id: &str,
    new: &NewCredential,
    now: Timestamp,
) -> rusqlite::Result<String> {
    let id = ids::credential_id();
    let json = |v: &[String]| serde_json::to_string(v).expect("strings serialize");
    conn.execute(
        "INSERT INTO credentials (id, cluster_id, name, access_type, allowed_actions,
            allowed_index_globs, allowed_cidrs, username, password, created_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
        params![
            id,
            cluster_id,
            new.name,
            new.access_type,
            json(new.allowed_actions),
            json(new.allowed_index_globs),
            json(new.allowed_cidrs),
            ids::lower_alnum(10),
            ids::lower_alnum(24),
            now.to_string()
        ],
    )?;
    Ok(id)
}

/// Creates a cluster's first credential, `rw` and named `default`, as
/// the Bonsai API's `/v1` create does in the request, before any Action runs.
pub fn create_default_credential(
    conn: &Connection,
    cluster_id: &str,
    now: Timestamp,
) -> rusqlite::Result<String> {
    let default = NewCredential {
        name: "default",
        access_type: "rw",
        ..NewCredential::default()
    };
    insert_credential(conn, cluster_id, &default, now)
}

/// The host a cluster is reached at: `<id>.<cloud region>.bonsaisearch.net`.
pub fn cluster_host(conn: &Connection, cluster_id: &str) -> rusqlite::Result<String> {
    let cloud_region: String = conn.query_row(
        "SELECT r.cloud_region FROM clusters c JOIN regions r ON r.id = c.region
         WHERE c.id = ?1",
        [cluster_id],
        |r| r.get(0),
    )?;
    Ok(format!("{cluster_id}.{cloud_region}.bonsaisearch.net"))
}

/// Brings the Action up to `now`: runs each step due since the last call,
/// applying its side effect, then stores the steps, state and result.
pub fn advance(
    conn: &Connection,
    action_id: &str,
    now: Timestamp,
    step_ms: u64,
) -> rusqlite::Result<()> {
    db::atomically(conn, |conn| {
        let Some(mut action) = ActionRow::find(conn, action_id)? else {
            return Ok(());
        };
        if action.is_finished() {
            return Ok(());
        }
        let names = steps_for(&action.kind);
        let due = if step_ms == 0 {
            names.len()
        } else {
            let elapsed = now.duration_since(action.started_at).as_millis().max(0);
            let done = elapsed / i128::from(step_ms);
            usize::try_from(done).unwrap_or(usize::MAX).min(names.len())
        };
        if due <= action.step_index {
            return Ok(());
        }

        for k in action.step_index..due {
            let at = step_time(action.started_at, k, step_ms);
            action.step_index = k + 1;
            if k == FAILING_STEP
                && let Some(error) = action.outcome.error()
            {
                action.steps.push(ActionStep {
                    state: ActionState::Error,
                    message: error.message.clone(),
                    at,
                });
                action.state = ActionState::Error;
                action.finished_at = Some(at);
                action.error = Some(error);
                if action.kind == ActionType::Provision {
                    // As the Bonsai API does when a provision errors:
                    // a failed create is terminated (and its billing stopped),
                    // so the cluster is `deprovisioned` from here on.
                    conn.execute(
                        "UPDATE clusters SET terminated_at = ?2 WHERE id = ?1 AND terminated_at IS NULL",
                        params![action.cluster_id, at.to_string()],
                    )?;
                }
                break;
            }
            apply_step(conn, &action, names[k])?;
            let last = k + 1 == names.len();
            action.state = if last {
                ActionState::Success
            } else {
                ActionState::Running
            };
            action.steps.push(ActionStep {
                state: action.state.clone(),
                message: names[k].to_string(),
                at,
            });
            if last {
                action.finished_at = Some(at);
                action.result = finish(conn, &action, at)?;
            }
        }
        conn.execute(
            "UPDATE actions SET state = ?2, steps = ?3, step_index = ?4, finished_at = ?5,
                error = ?6, result = ?7
             WHERE id = ?1",
            params![
                action.id,
                action.state.as_str(),
                serde_json::to_string(&action.steps).expect("steps serialize"),
                action.step_index,
                action.finished_at.map(|t| t.to_string()),
                action
                    .error
                    .as_ref()
                    .map(|e| serde_json::to_string(e).expect("error serializes")),
                action
                    .result
                    .as_ref()
                    .map(|r| serde_json::to_string(r).expect("result serializes")),
            ],
        )?;
        Ok(())
    })
}

/// [`advance`] for every unfinished Action on a cluster.
pub fn advance_cluster(
    conn: &Connection,
    cluster_id: &str,
    now: Timestamp,
    step_ms: u64,
) -> rusqlite::Result<()> {
    let ids: Vec<String> = conn
        .prepare(
            "SELECT id FROM actions WHERE cluster_id = ?1 AND state IN ('new', 'running')
             ORDER BY rowid",
        )?
        .query_map([cluster_id], |r| r.get(0))?
        .collect::<rusqlite::Result<_>>()?;
    for id in ids {
        advance(conn, &id, now, step_ms)?;
    }
    Ok(())
}

/// [`advance`] for every unfinished Action in an account.
pub fn advance_account(
    conn: &Connection,
    account_id: &str,
    now: Timestamp,
    step_ms: u64,
) -> rusqlite::Result<()> {
    let ids: Vec<String> = conn
        .prepare(
            "SELECT id FROM actions WHERE account_id = ?1 AND state IN ('new', 'running')
             ORDER BY rowid",
        )?
        .query_map([account_id], |r| r.get(0))?
        .collect::<rusqlite::Result<_>>()?;
    for id in ids {
        advance(conn, &id, now, step_ms)?;
    }
    Ok(())
}

/// The side effect of completing one step.
fn apply_step(conn: &Connection, action: &ActionRow, step: &str) -> rusqlite::Result<()> {
    if action.kind == ActionType::Provision && step == "Configuring routes" {
        conn.execute(
            "UPDATE clusters SET host = ?2 WHERE id = ?1",
            params![action.cluster_id, cluster_host(conn, &action.cluster_id)?],
        )?;
    }
    Ok(())
}

/// The cluster's usage sample at `at`, from usage's generator.
fn latest_usage(
    conn: &Connection,
    cluster_id: &str,
    at: Timestamp,
) -> rusqlite::Result<bonsai_api::UsageSample> {
    let created_at = conn.query_row(
        "SELECT created_at FROM clusters WHERE id = ?1",
        [cluster_id],
        |r| db::ts(r, 0),
    )?;
    Ok(crate::routes::observability::latest_sample(
        cluster_id, created_at, at,
    ))
}

/// The side effect of the last step, and the Action's `result`.
fn finish(
    conn: &Connection,
    action: &ActionRow,
    at: Timestamp,
) -> rusqlite::Result<Option<ActionResult>> {
    let stamp = at.to_string();
    match action.kind {
        ActionType::Provision => {
            // Polled at once, with the numbers usage gives this moment.
            let polled = latest_usage(conn, &action.cluster_id, at)?;
            conn.execute(
                "UPDATE clusters SET polled_at = ?2, docs = ?3, shards = ?4, bytes = ?5 WHERE id = ?1",
                params![action.cluster_id, stamp, polled.docs, polled.shards, polled.bytes],
            )?;
            let credential: Option<String> = conn
                .query_row(
                    "SELECT id FROM credentials WHERE cluster_id = ?1 ORDER BY rowid LIMIT 1",
                    [&action.cluster_id],
                    |r| r.get(0),
                )
                .optional()?;
            Ok(Some(ActionResult {
                cluster: Some(action.cluster_id.clone()),
                credential,
            }))
        }
        ActionType::Deprovision => {
            // Revoked as the Bonsai API revokes a credential: the
            // password is forgotten.
            conn.execute(
                "UPDATE credentials SET revoked_at = ?2, password = '' WHERE cluster_id = ?1 AND revoked_at IS NULL",
                params![action.cluster_id, stamp],
            )?;
            Ok(None)
        }
        ActionType::ChangePlan => {
            if let Some(plan) = &action.plan_after {
                conn.execute(
                    "UPDATE clusters SET plan_id = ?2,
                        single_tenant = (SELECT single_tenant FROM plans WHERE id = ?2)
                     WHERE id = ?1",
                    params![action.cluster_id, plan],
                )?;
            }
            Ok(None)
        }
        _ => Ok(None),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn setup() -> (tempfile::TempDir, Connection) {
        let dir = tempfile::tempdir().unwrap();
        let conn = db::open(&dir.path().join("e.sqlite")).unwrap();
        crate::seed::seed_if_empty(&conn).unwrap();
        conn.execute(
            "INSERT INTO clusters (id, account_id, name, plan_id, region, release, single_tenant, created_at)
             VALUES ('c-1', 'acme', 'c', 'standard-fir', 'aws-eu-west-1', 'opensearch-3.8.0', 0, ?1)",
            [db::now()],
        )
        .unwrap();
        create_default_credential(&conn, "c-1", t0()).unwrap();
        (dir, conn)
    }

    fn t0() -> Timestamp {
        "2026-01-01T00:00:00Z".parse().unwrap()
    }

    fn at(ms: i64) -> Timestamp {
        t0().checked_add(SignedDuration::from_millis(ms)).unwrap()
    }

    fn start(conn: &Connection, kind: ActionType, outcome: Outcome) -> String {
        create(conn, "acme", "c-1", kind, None, outcome, t0())
            .unwrap()
            .id
    }

    fn get(conn: &Connection, id: &str) -> ActionRow {
        ActionRow::find(conn, id).unwrap().unwrap()
    }

    #[test]
    fn steps_complete_by_time() {
        let (_d, c) = setup();
        let id = start(&c, ActionType::Provision, Outcome::Success);

        advance(&c, &id, at(999), 1000).unwrap();
        let a = get(&c, &id);
        assert_eq!(a.state, ActionState::New);
        assert!(a.steps.is_empty());
        assert_eq!(a.retry_after(at(999), 1000), Some(1));
        assert_eq!(a.retry_after(at(0), 1000), Some(1));
        assert_eq!(a.retry_after(at(0), 2500), Some(3));

        advance(&c, &id, at(2500), 1000).unwrap();
        let a = get(&c, &id);
        assert_eq!(a.state, ActionState::Running);
        assert_eq!(a.step_index, 2);
        assert_eq!(a.steps[1].message, "Provisioning cluster");
        assert_eq!(a.steps[1].state, ActionState::Running);
        assert_eq!(a.steps[1].at, at(2000));
        assert_eq!(a.retry_after(at(2500), 1000), Some(1));

        // Idempotent: the same `now` changes nothing.
        advance(&c, &id, at(2500), 1000).unwrap();
        assert_eq!(get(&c, &id), a);

        advance(&c, &id, at(60_000), 1000).unwrap();
        let a = get(&c, &id);
        assert_eq!(a.state, ActionState::Success);
        assert_eq!(a.steps.len(), 4);
        assert_eq!(a.steps[3].state, ActionState::Success);
        assert_eq!(a.finished_at, Some(at(4000)));
        assert_eq!(a.retry_after(at(60_000), 1000), None);
        let result = a.result.unwrap();
        assert_eq!(result.cluster.as_deref(), Some("c-1"));
        let (cred, user): (String, String) = c
            .query_row(
                "SELECT id, username FROM credentials WHERE cluster_id = 'c-1'",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!(result.credential, Some(cred));
        assert!(!user.is_empty());
        let (host, docs): (String, i64) = c
            .query_row(
                "SELECT host, docs FROM clusters WHERE id = 'c-1'",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!(host, "c-1.eu-west-1.bonsaisearch.net");
        // Usage's numbers for the slot it finished in, not zeros.
        assert!(docs > 0);
    }

    #[test]
    fn step_ms_zero_finishes_on_the_next_read() {
        let (_d, c) = setup();
        let id = start(&c, ActionType::Provision, Outcome::Success);
        assert_eq!(get(&c, &id).retry_after(t0(), 0), Some(1));
        advance(&c, &id, t0(), 0).unwrap();
        assert_eq!(get(&c, &id).state, ActionState::Success);
    }

    #[test]
    fn failing_outcomes_error_at_the_second_step() {
        let (_d, c) = setup();
        for (outcome, code) in [
            (Outcome::ActionFailed, ActionErrorCode::ActionFailed),
            (Outcome::TimedOut, ActionErrorCode::TimedOut),
        ] {
            let id = start(&c, ActionType::Provision, outcome);
            advance(&c, &id, at(60_000), 1000).unwrap();
            let a = get(&c, &id);
            assert_eq!(a.state, ActionState::Error);
            assert_eq!(a.steps.len(), 2);
            assert_eq!(a.steps[1].state, ActionState::Error);
            let error = a.error.unwrap();
            assert_eq!(error.code, code);
            assert_eq!(a.steps[1].message, error.message);
            assert_eq!(error.action_url, None);
            assert_eq!(a.result, None);
            assert_eq!(a.finished_at, Some(at(2000)));
        }
        // Neither reached "Configuring routes".
        let host: Option<String> = c
            .query_row("SELECT host FROM clusters WHERE id = 'c-1'", [], |r| {
                r.get(0)
            })
            .unwrap();
        assert_eq!(host, None);
    }

    #[test]
    fn deprovision_revokes_credentials_and_change_plan_moves_the_plan() {
        let (_d, c) = setup();
        let p = start(&c, ActionType::Provision, Outcome::Success);
        advance(&c, &p, at(0), 0).unwrap();

        let id = create(
            &c,
            "acme",
            "c-1",
            ActionType::ChangePlan,
            Some("standard-cork"),
            Outcome::Success,
            t0(),
        )
        .unwrap()
        .id;
        advance(&c, &id, at(0), 0).unwrap();
        let plan: String = c
            .query_row("SELECT plan_id FROM clusters WHERE id = 'c-1'", [], |r| {
                r.get(0)
            })
            .unwrap();
        assert_eq!(plan, "standard-cork");
        assert_eq!(get(&c, &id).result, None);

        let id = start(&c, ActionType::Deprovision, Outcome::Success);
        advance(&c, &id, at(0), 0).unwrap();
        let active: i64 = c
            .query_row(
                "SELECT count(*) FROM credentials WHERE revoked_at IS NULL",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(active, 0);
    }

    #[test]
    fn fail_next_is_taken_once() {
        let (_d, c) = setup();
        assert_eq!(Outcome::take_from_scenario(&c).unwrap(), Outcome::Success);
        scenario::set(&c, "fail_next", &serde_json::json!("timed_out")).unwrap();
        assert_eq!(Outcome::take_from_scenario(&c).unwrap(), Outcome::TimedOut);
        assert_eq!(Outcome::take_from_scenario(&c).unwrap(), Outcome::Success);
        // payment_failed is createCluster's to take.
        scenario::set(&c, "fail_next", &serde_json::json!("payment_failed")).unwrap();
        assert_eq!(Outcome::take_from_scenario(&c).unwrap(), Outcome::Success);
        assert!(scenario::get(&c, "fail_next").unwrap().is_some());
    }
}
