//! Progress events the flows emit for the CLI to render. The flows never print.

use std::time::Duration;

use bonsai_api::{ActionState, ActionType};
use jiff::Timestamp;

#[derive(Debug, Clone, PartialEq)]
pub enum Event {
    /// A flow started an Action.
    ActionStarted {
        action_id: String,
        kind: ActionType,
        subject: String,
    },
    /// A step of the Action being waited on, once each.
    StepCompleted {
        message: String,
        state: ActionState,
        at: Timestamp,
    },
    /// About to sleep this long before polling the Action again.
    Waiting {
        retry_after: Duration,
    },
    Retrying {
        attempt: u32,
        wait: Duration,
        reason: String,
    },
    /// Removing the cluster a failed create left behind.
    CleanupStarted {
        cluster_id: String,
        cluster_name: String,
    },
    CleanupFinished {
        ok: bool,
    },
}

/// Where flows send their events. A closed receiver is ignored.
pub type EventSink = tokio::sync::mpsc::UnboundedSender<Event>;

/// Sends `event`, ignoring a receiver that's gone.
pub(crate) fn emit(sink: &EventSink, event: Event) {
    let _ = sink.send(event);
}
