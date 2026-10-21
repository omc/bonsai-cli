//! `bonsai cluster create`: create, wait, and clean up a failed create.

use bonsai_api::{
    Action, Client, ClusterCreate, ClusterState, ProblemCode, Response, new_idempotency_key,
};
use tokio::time::Instant;

use super::{started, wait_if};
use crate::events::{Event, EventSink, emit};
use crate::wait::{WaitOptions, deadline_after, wait_for_action};
use crate::{Cleanup, CleanupOutcome, CoreError, StopReason};

/// What to do with the cluster a failed create left behind.
pub enum CleanupPolicy {
    Remove,
    Keep,
    /// Asks, with the cluster's name; `true` removes it.
    Ask(Box<dyn Fn(&str) -> bool + Send + Sync>),
}

pub struct CreateRequest {
    pub body: ClusterCreate,
    pub wait: bool,
    pub cleanup: CleanupPolicy,
    /// The caller's own `Idempotency-Key`, for scripts that run their own retries
    /// (`--idempotency-key`). `None` makes a fresh one for this run.
    pub idempotency_key: Option<String>,
}

/// Creates a cluster with one `Idempotency-Key` for this run (the caller's, or a
/// fresh one) and, with `wait`, waits for its provision.
///
/// A refusal (`422 payment_method_required`, `422 payment_failed`, ...) builds nothing,
/// so it is returned as is. When the provision ends in error, whatever the code, the
/// cluster is re-read (twice when the first read finds it `failed` but not yet
/// terminated): Bonsai terminates a failed create itself, so when it is
/// already terminated the cleanup is [`CleanupOutcome::RemovedByBonsai`], no
/// `DELETE` is sent and the policy isn't consulted. Ctrl-C or the deadline during
/// the re-read stops there, removing nothing: [`CleanupOutcome::Unchecked`].
/// Only a cluster that is still there is the policy's to remove. The original
/// [`CoreError::ActionFailed`] is returned either way, with `cluster_id` set and
/// `cleanup` set when Bonsai had removed it or a removal was tried. A provision
/// wait that ends any other way (timeout, cancellation, a failed poll) never
/// removes anything.
///
/// Only a cluster this run created is removed. A create with a caller-supplied key
/// answered `Idempotent-Replayed: true` may have been created by an earlier run that
/// used the key, so it is never cleaned up, whatever the policy, and the policy
/// isn't consulted: the failure comes back with `replayed` set, and no `cleanup`
/// unless Bonsai had removed the cluster. A replay of a key made in this run is
/// this run's own retry, and cleaned up as usual.
///
/// `opts.timeout` bounds waiting on the provision and on the cleanup's deprovision
/// together; the cleanup's `DELETE` request itself is bounded only by the client's
/// retry deadline. A timeout reflects the last poll before the deadline.
pub async fn create_cluster(
    api: &Client,
    req: CreateRequest,
    opts: &WaitOptions,
    ev: &EventSink,
) -> Result<Response<Action>, CoreError> {
    let deadline = deadline_after(opts.timeout);
    // A key made here is this run's alone, so a replay of it can only be this run's
    // own retry (the client resends a keyed POST after a lost response). A caller's
    // key may also have been used by an earlier run: then a replay is ambiguous,
    // and the cluster is kept.
    let caller_key = req.idempotency_key.is_some();
    let key = req.idempotency_key.unwrap_or_else(new_idempotency_key);
    let action = api.create_cluster(&req.body, &key).await?;
    started(&action, ev);
    let cluster_id = action.value.subject.id.clone();
    let replayed = caller_key && action.idempotent_replayed;
    match wait_if(api, action, req.wait, opts, ev).await {
        Err(CoreError::ActionFailed(mut failure)) => {
            failure.cluster_id = Some(cluster_id.clone());
            failure.replayed = replayed;
            match reread(api, &cluster_id, opts, deadline).await {
                Reread::RemovedByBonsai => {
                    failure.cleanup = Some(Cleanup {
                        cluster_id,
                        outcome: CleanupOutcome::RemovedByBonsai,
                    });
                    return Err(CoreError::ActionFailed(failure));
                }
                // Ctrl-C or the deadline: nothing more is sent.
                Reread::Stopped(reason) => {
                    failure.cleanup = Some(Cleanup {
                        cluster_id,
                        outcome: CleanupOutcome::Unchecked { reason },
                    });
                    return Err(CoreError::ActionFailed(failure));
                }
                Reread::StillThere => {}
            }
            if !replayed && should_remove(&req.cleanup, &req.body.name) {
                let left = WaitOptions {
                    timeout: deadline.saturating_duration_since(Instant::now()),
                    cancel: opts.cancel.clone(),
                    default_poll: opts.default_poll,
                };
                failure.cleanup =
                    Some(remove_failed(api, &cluster_id, &req.body.name, &left, ev).await);
            }
            Err(CoreError::ActionFailed(failure))
        }
        other => other,
    }
}

/// What re-reading the failed create's cluster found.
enum Reread {
    /// Terminated, as Bonsai leaves a failed create.
    RemovedByBonsai,
    /// Still there, or unreadable: the policy decides.
    StillThere,
    /// Cancelled, or the deadline passed, while reading.
    Stopped(StopReason),
}

/// Re-reads the cluster, racing the read against cancellation and the deadline.
/// The API saves the Action's `error` before it terminates the failed cluster,
/// so a first read can still see it `failed` and not terminated: it is read once
/// more after `default_poll`. A failed read counts as "still there".
async fn reread(api: &Client, cluster_id: &str, opts: &WaitOptions, deadline: Instant) -> Reread {
    for attempt in 0..2 {
        if attempt > 0 {
            let pause = opts
                .default_poll
                .min(deadline.saturating_duration_since(Instant::now()));
            tokio::select! {
                biased;
                () = opts.cancel.cancelled() => return Reread::Stopped(StopReason::Cancelled),
                () = tokio::time::sleep(pause) => {}
            }
        }
        let read = tokio::select! {
            biased;
            () = opts.cancel.cancelled() => return Reread::Stopped(StopReason::Cancelled),
            () = tokio::time::sleep_until(deadline) => return Reread::Stopped(StopReason::Timeout),
            read = api.get_cluster(cluster_id) => read,
        };
        let c = match read {
            Ok(c) => c.value,
            Err(e) => {
                tracing::debug!(cluster_id, error = %e, "couldn't re-read the failed cluster");
                return Reread::StillThere;
            }
        };
        if c.terminated_at.is_some()
            || matches!(
                c.state,
                ClusterState::Deprovisioned | ClusterState::Deprovisioning
            )
        {
            return Reread::RemovedByBonsai;
        }
        if c.state != ClusterState::Failed {
            return Reread::StillThere;
        }
    }
    Reread::StillThere
}

fn should_remove(policy: &CleanupPolicy, name: &str) -> bool {
    match policy {
        CleanupPolicy::Remove => true,
        CleanupPolicy::Keep => false,
        CleanupPolicy::Ask(ask) => ask(name),
    }
}

/// Deletes the failed cluster with a fresh key (never the caller's: it's another
/// request) and waits for its deprovision within
/// `opts`. Never fails: an error here is logged and reported in the [`Cleanup`].
async fn remove_failed(
    api: &Client,
    cluster_id: &str,
    name: &str,
    opts: &WaitOptions,
    ev: &EventSink,
) -> Cleanup {
    emit(
        ev,
        Event::CleanupStarted {
            cluster_id: cluster_id.to_owned(),
            cluster_name: name.to_owned(),
        },
    );
    let outcome = match api
        .delete_cluster(cluster_id, name, &new_idempotency_key())
        .await
    {
        Ok(action) => {
            started(&action, ev);
            let deprovision_action_id = action.value.id.clone();
            match wait_for_action(api, action, opts, ev).await {
                Ok(_) => CleanupOutcome::Removed {
                    deprovision_action_id,
                },
                Err(CoreError::ActionFailed(f)) => {
                    tracing::warn!(cluster_id, error = %f.message, "the failed cluster's deprovision failed");
                    CleanupOutcome::DeprovisionFailed {
                        deprovision_action_id,
                    }
                }
                Err(e) => {
                    tracing::warn!(cluster_id, error = %e, "the failed cluster's deprovision wasn't seen to finish");
                    CleanupOutcome::Pending {
                        deprovision_action_id,
                    }
                }
            }
        }
        // Bonsai removed it between the re-read and the DELETE.
        Err(e) if e.code() == Some(&ProblemCode::ClusterTerminated) => {
            CleanupOutcome::RemovedByBonsai
        }
        Err(e) => {
            tracing::warn!(cluster_id, error = %e, "removing the failed cluster failed");
            CleanupOutcome::DeleteFailed
        }
    };
    emit(
        ev,
        Event::CleanupFinished {
            ok: outcome.removed(),
        },
    );
    Cleanup {
        cluster_id: cluster_id.to_owned(),
        outcome,
    }
}
