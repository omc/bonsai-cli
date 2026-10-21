//! `bonsai cluster resize`.

use bonsai_api::{Action, Client, PlanChange, Response, new_idempotency_key};

use super::{started, wait_if};
use crate::CoreError;
use crate::events::EventSink;
use crate::wait::WaitOptions;

/// Moves the cluster to `plan` and, with `wait`, waits for the change. `key` is the
/// caller's `Idempotency-Key`; `None` makes a fresh one for this run.
pub async fn resize_cluster(
    api: &Client,
    cluster_id: &str,
    plan: &str,
    key: Option<&str>,
    wait: bool,
    opts: &WaitOptions,
    ev: &EventSink,
) -> Result<Response<Action>, CoreError> {
    let body = PlanChange {
        plan: plan.to_owned(),
    };
    let key = key.map_or_else(new_idempotency_key, str::to_owned);
    let action = api.change_cluster_plan(cluster_id, &body, &key).await?;
    started(&action, ev);
    wait_if(api, action, wait, opts, ev).await
}
