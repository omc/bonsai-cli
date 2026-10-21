//! `bonsai cluster destroy`.

use bonsai_api::{Action, Client, Cluster, Response, new_idempotency_key};

use super::{started, wait_if};
use crate::CoreError;
use crate::events::EventSink;
use crate::wait::WaitOptions;

/// Deprovisions `cluster` and, with `wait`, waits for it.
///
/// `confirm_name` is sent as the API's `name` confirmation as given: the API is
/// what checks it against the cluster's current name, and refuses a mismatch with
/// `422 name_mismatch` before anything changes. `key` is the caller's
/// `Idempotency-Key`; `None` makes a fresh one for this run.
pub async fn destroy_cluster(
    api: &Client,
    cluster: &Cluster,
    confirm_name: &str,
    key: Option<&str>,
    wait: bool,
    opts: &WaitOptions,
    ev: &EventSink,
) -> Result<Response<Action>, CoreError> {
    let key = key.map_or_else(new_idempotency_key, str::to_owned);
    let action = api.delete_cluster(&cluster.id, confirm_name, &key).await?;
    started(&action, ev);
    wait_if(api, action, wait, opts, ev).await
}
