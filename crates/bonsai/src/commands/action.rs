//! `bonsai action show` and `bonsai wait`.

use anyhow::Result;
use bonsai_api::{Action, ActionState, Client, Response};
use bonsai_core::{CoreError, Event, WaitOptions, wait_for_action};
use jiff::Timestamp;

use super::Ctx;
use crate::cli::{ActionCommand, WaitArgs};
use crate::display::when;
use crate::exit::Hint;
use crate::output::{Column, relative_time};
use crate::render_cluster::step_mark;

/// `action show`'s steps table: a step's state is never dropped.
const STEP_COLUMNS: [Column; 3] = [
    Column::fixed("STATE", 4).kept(),
    Column::fixed("AT", 2),
    Column::text("STEP", 3),
];

pub async fn run(ctx: &Ctx, command: ActionCommand) -> Result<()> {
    match command {
        ActionCommand::Show(arg) => ctx.abortable(show(ctx, &arg.action_id)).await,
    }
}

async fn show(ctx: &Ctx, id: &str) -> Result<()> {
    let a = ctx.authed().await?;
    let action = a.client.get_action(id).await?;
    if ctx.out.is_json() {
        return ctx.out.json(&action.raw);
    }
    print_action(ctx, &action.value, true)
}

/// Picks up waiting on an Action: its steps go to stderr as they appear; the
/// finished Action to stdout.
pub async fn wait(ctx: &Ctx, args: WaitArgs) -> Result<()> {
    let a = ctx.authed().await?;
    let action = ctx
        .abortable(async { Ok(a.client.get_action(&args.action_id).await?) })
        .await?;
    let opts = WaitOptions {
        timeout: args.timeout,
        cancel: ctx.cancel.clone(),
        ..WaitOptions::default()
    };
    match wait_rendering(ctx, &a.client, action, &opts).await {
        Ok(done) => {
            if ctx.out.is_json() {
                return ctx.out.json(&done.raw);
            }
            ctx.out.success(&format!(
                "{} {} succeeded",
                done.value.r#type, done.value.id
            ));
            print_action(ctx, &done.value, false)
        }
        Err(CoreError::ActionFailed(f)) => {
            // A script still gets the finished Action, with its error.
            if ctx.out.is_json()
                && let Ok(done) = a.client.get_action(&f.action_id).await
            {
                ctx.out.json(&done.raw)?;
            }
            // After the error: the Checkout link, for an Action-level payment_failed.
            let hint = f
                .action_url
                .as_ref()
                .map(|url| vec![format!("  Update the card (no Bonsai login needed): {url}")]);
            let err = anyhow::Error::new(CoreError::ActionFailed(f));
            Err(match hint {
                Some(lines) => err.context(Hint(lines)),
                None => err,
            })
        }
        Err(e) => Err(e.into()),
    }
}

/// [`wait_for_action`], printing its events to stderr as they come.
pub(crate) async fn wait_rendering(
    ctx: &Ctx,
    client: &Client,
    action: Response<Action>,
    opts: &WaitOptions,
) -> Result<Response<Action>, CoreError> {
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let waiting = async move {
        // `tx` drops when the wait ends, which ends the loop below.
        wait_for_action(client, action, opts, &tx).await
    };
    // A failed step is held back: the final error usually repeats its message, and
    // then it's said once, there.
    let printing = async {
        let mut failed = Vec::new();
        while let Some(event) = rx.recv().await {
            match event {
                Event::StepCompleted {
                    state: ActionState::Error,
                    ..
                } => failed.push(event),
                event => print_event(ctx, &event),
            }
        }
        failed
    };
    let (res, failed) = tokio::join!(waiting, printing);
    let repeated = |e: &Event| match (e, &res) {
        (Event::StepCompleted { message, .. }, Err(CoreError::ActionFailed(f))) => {
            *message == f.message
        }
        _ => false,
    };
    for event in failed.iter().filter(|e| !repeated(e)) {
        print_event(ctx, event);
    }
    res
}

/// One progress line on stderr for `event`; nothing under `--json`. Steps look as
/// `cluster create` shows them (`render_cluster::Progress`): `  ✓ <step>` for each
/// one done, `  ✗ <step>` for the one that failed.
pub(crate) fn print_event(ctx: &Ctx, event: &Event) {
    if ctx.out.is_json() {
        return;
    }
    match event {
        Event::ActionStarted {
            action_id,
            kind,
            subject,
        } => ctx
            .out
            .info(&format!("Started {kind} {action_id} on {subject}")),
        Event::StepCompleted { message, state, .. } => {
            ctx.out.info(&format!("  {} {message}", step_mark(state)))
        }
        Event::Retrying { wait, reason, .. } => ctx.out.info(&format!(
            "Retrying in {} s: {reason}",
            wait.as_secs().max(1)
        )),
        Event::CleanupStarted {
            cluster_id,
            cluster_name,
        } => ctx
            .out
            .info(&format!("Removing {cluster_name} ({cluster_id})")),
        Event::CleanupFinished { ok } => {
            if *ok {
                ctx.out.success("Removed");
            } else {
                ctx.out.warn("Couldn't remove it");
            }
        }
        Event::Waiting { .. } => {}
    }
}

/// An Action as `key  value` lines, then its steps as a table.
fn print_action(ctx: &Ctx, action: &Action, steps: bool) -> Result<()> {
    let now = Timestamp::now();
    let when = |t: Timestamp| when(t, now);
    let mut rows = vec![
        ("ID", action.id.clone()),
        ("Type", action.r#type.to_string()),
        ("State", action.state.to_string()),
        (
            "Subject",
            format!("{} {}", action.subject.r#type, action.subject.id),
        ),
        ("Created", when(action.created_at)),
    ];
    if let Some(t) = action.finished_at {
        rows.push(("Finished", when(t)));
    }
    if let Some(e) = &action.error {
        rows.push(("Error", format!("{}: {}", e.code, e.message)));
        if let Some(url) = &e.action_url {
            rows.push(("Checkout", url.clone()));
        }
    }
    if let Some(r) = &action.result {
        if let Some(c) = &r.cluster {
            rows.push(("Cluster", c.clone()));
        }
        if let Some(c) = &r.credential {
            rows.push(("Credential", c.clone()));
        }
    }
    ctx.out.kv(&rows)?;
    if steps && !action.steps.is_empty() {
        ctx.out.data_line("")?;
        let rows = action
            .steps
            .iter()
            .map(|s| {
                vec![
                    s.state.to_string(),
                    relative_time(s.at, now),
                    s.message.clone(),
                ]
            })
            .collect();
        ctx.out.table(&STEP_COLUMNS, rows, true)?;
    }
    Ok(())
}
