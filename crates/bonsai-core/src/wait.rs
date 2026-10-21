//! Waiting on an Action.

use std::time::Duration;

use bonsai_api::{Action, ActionErrorCode, ActionState, Client, Response};
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;

use crate::events::{Event, EventSink, emit};
use crate::{ActionFailure, CoreError};

pub struct WaitOptions {
    /// How long to wait before giving up with [`CoreError::Timeout`]; 30 minutes by default.
    pub timeout: Duration,
    /// Stops waiting (not the Action) with [`CoreError::Cancelled`].
    pub cancel: CancellationToken,
    /// The poll interval when the API sends no `Retry-After`; 2 s by default.
    pub default_poll: Duration,
}

impl Default for WaitOptions {
    fn default() -> Self {
        Self {
            timeout: Duration::from_secs(30 * 60),
            cancel: CancellationToken::new(),
            default_poll: Duration::from_secs(2),
        }
    }
}

/// What `Instant + Duration` saturates to: tokio's own far future, 30 years ahead.
const FAR_FUTURE: Duration = Duration::from_secs(86_400 * 365 * 30);

/// `now + d`, or 30 years from now when that overflows.
pub(crate) fn deadline_after(d: Duration) -> Instant {
    let now = Instant::now();
    now.checked_add(d).unwrap_or_else(|| now + FAR_FUTURE)
}

/// Polls `GET /actions/{id}` until the Action reaches `success` or `error`.
///
/// The first poll is immediate; later ones sleep for the `Retry-After` the previous
/// response sent, or `default_poll`. `timeout` bounds the whole wait, polls in flight
/// included. Each step is emitted once, by index, those already in `action`
/// included. An unknown state keeps polling. Returns the final response; an `error`
/// state is [`CoreError::ActionFailed`] with no cluster and no cleanup, and a poll
/// that fails after the client's retries is [`CoreError::WaitFailed`].
pub async fn wait_for_action(
    api: &Client,
    action: Response<Action>,
    opts: &WaitOptions,
    events: &EventSink,
) -> Result<Response<Action>, CoreError> {
    let action_id = action.value.id.clone();
    let deadline = deadline_after(opts.timeout);
    let cancelled = || CoreError::Cancelled {
        action_id: action_id.clone(),
    };
    let timed_out = || CoreError::Timeout {
        action_id: action_id.clone(),
        timeout: opts.timeout,
    };
    let mut current = action;
    let mut steps_seen = 0;
    let mut first = true;
    loop {
        steps_seen = emit_new_steps(&current.value, steps_seen, events);
        if current.value.state.is_terminal() {
            if current.value.state == ActionState::Error {
                return Err(CoreError::ActionFailed(Box::new(failure(&current.value))));
            }
            return Ok(current);
        }
        if !first {
            let now = Instant::now();
            if now >= deadline {
                return Err(timed_out());
            }
            let wait = match current.retry_after {
                Some(d) if !d.is_zero() => d,
                _ => opts.default_poll,
            }
            .min(deadline - now);
            emit(events, Event::Waiting { retry_after: wait });
            tokio::select! {
                biased;
                () = opts.cancel.cancelled() => return Err(cancelled()),
                () = tokio::time::sleep(wait) => {}
            }
        }
        first = false;
        current = tokio::select! {
            biased;
            () = opts.cancel.cancelled() => return Err(cancelled()),
            res = api.get_action(&action_id) => res.map_err(|e| CoreError::WaitFailed {
                action_id: action_id.clone(),
                source: Box::new(e),
            })?,
            () = tokio::time::sleep_until(deadline) => return Err(timed_out()),
        };
    }
}

/// Emits the steps after the first `seen`; returns how many have been emitted.
fn emit_new_steps(action: &Action, seen: usize, events: &EventSink) -> usize {
    for step in action.steps.iter().skip(seen) {
        emit(
            events,
            Event::StepCompleted {
                message: step.message.clone(),
                state: step.state.clone(),
                at: step.at,
            },
        );
    }
    seen.max(action.steps.len())
}

/// Why an Action in state `error` failed.
fn failure(done: &Action) -> ActionFailure {
    let (code, message, action_url) = match &done.error {
        Some(e) => (e.code.clone(), e.message.clone(), e.action_url.clone()),
        None => (
            ActionErrorCode::Unknown("unknown".into()),
            "the Action ended in error without saying why".into(),
            None,
        ),
    };
    ActionFailure {
        action_id: done.id.clone(),
        code,
        message,
        action_url,
        cluster_id: None,
        cleanup: None,
        replayed: false,
    }
}
