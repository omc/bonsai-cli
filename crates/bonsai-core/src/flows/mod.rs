//! The CLI's multi-request operations. Each returns data and emits events; none prints.

mod create;
mod destroy;
mod login;
mod resize;
mod url;

pub use create::{CleanupPolicy, CreateRequest, create_cluster};
pub use destroy::destroy_cluster;
pub use login::{LoginOutcome, device_login};
pub use resize::resize_cluster;
pub use url::connection_url;

use bonsai_api::{Action, Client, Response};

use crate::CoreError;
use crate::events::{Event, EventSink, emit};
use crate::wait::{WaitOptions, wait_for_action};

/// Emits [`Event::ActionStarted`] for an Action a flow just started.
fn started(action: &Response<Action>, ev: &EventSink) {
    emit(
        ev,
        Event::ActionStarted {
            action_id: action.value.id.clone(),
            kind: action.value.r#type.clone(),
            subject: action.value.subject.id.clone(),
        },
    );
}

/// Waits for `action` when `wait` is set; otherwise returns it as started.
async fn wait_if(
    api: &Client,
    action: Response<Action>,
    wait: bool,
    opts: &WaitOptions,
    ev: &EventSink,
) -> Result<Response<Action>, CoreError> {
    if wait {
        wait_for_action(api, action, opts, ev).await
    } else {
        Ok(action)
    }
}
