//! `bonsai clusters` and `bonsai cluster …`.

use std::time::Duration;

use anyhow::Result;
use bonsai_api::{
    Action, ActionState, ActionType, ApiError, Client, Cluster, ClusterCreate, ClusterQuery,
    ClusterState, ClusterUpdate, Page, Plan, ProblemCode, Response, collect_pages,
};
use bonsai_core::config::Settings;
use bonsai_core::urlcheck::may_open;
use bonsai_core::{
    ActionFailure, CleanupOutcome, CleanupPolicy, CoreError, CreateRequest, WaitOptions,
    create_cluster, destroy_cluster, resize_cluster, resolve_cluster, wait_for_action,
};
use jiff::Timestamp;
use serde_json::Value;
use tokio::time::Instant;
use url::Url;

use super::{Authed, Ctx};
use crate::cli::{
    ClusterArg, ClusterCommand, ClusterListArgs, ClusterWaitArgs, CreateArgs, DestroyArgs,
    HistoryArgs, OpenArgs, RenameArgs, ResizeArgs, WaitFlags, WaitFor,
};
use crate::display::plan_price;
use crate::exit::{Failed, Hint, Interrupted, TimedOut};
use crate::output::table_width;
use crate::render_cluster::{
    CreateChoice, HISTORY_COLUMNS, LIST_COLUMNS, Progress, history_block, history_row, list_row,
    missing_create_flags, plan_choice, plans_in_region, release_choices, rerun_hint, shell_quote,
    show_rows,
};
use crate::sanitize::sanitize;

/// How often `cluster wait` looks at the cluster when no Action is running.
const CLUSTER_POLL: Duration = Duration::from_secs(2);

pub async fn list(ctx: &Ctx, args: ClusterListArgs) -> Result<()> {
    let a = ctx.authed().await?;
    let query = ClusterQuery {
        name: args.name,
        region: args.region,
        state: args.state,
        ..ClusterQuery::default()
    };
    let (clusters, raw) = ctx
        .abortable(async {
            let (client, query) = (&a.client, &query);
            Ok(collect_pages(None, |cursor| async move {
                client.list_clusters(query, cursor.as_deref()).await
            })
            .await?)
        })
        .await?;
    if ctx.out.is_json() {
        return ctx.out.json(&Value::Array(raw));
    }
    if clusters.is_empty() {
        ctx.out.info("No clusters");
        return Ok(());
    }
    ctx.out.table(
        &LIST_COLUMNS,
        clusters.iter().map(list_row).collect(),
        args.wide,
    )
}

pub async fn run(ctx: &Ctx, command: ClusterCommand) -> Result<()> {
    match command {
        ClusterCommand::List(args) => list(ctx, args).await,
        ClusterCommand::Create(mut args) => {
            let key = args.idempotency_key.take();
            create(ctx, args, key).await
        }
        ClusterCommand::Show(args) => show(ctx, args).await,
        ClusterCommand::Open(args) => open(ctx, args).await,
        ClusterCommand::Rename(args) => rename(ctx, args).await,
        ClusterCommand::Resize(mut args) => {
            let key = args.idempotency_key.take();
            resize(ctx, args, key).await
        }
        ClusterCommand::Destroy(mut args) => {
            let key = args.idempotency_key.take();
            destroy(ctx, args, key).await
        }
        ClusterCommand::History(args) => history(ctx, args).await,
        ClusterCommand::Wait(args) => wait(ctx, args).await,
    }
}

fn usage(msg: impl Into<String>) -> anyhow::Error {
    CoreError::Usage(msg.into()).into()
}

/// A cancelled or interrupted prompt is [`Interrupted`]; anything else a failure.
pub(super) fn prompt_error(e: inquire::InquireError) -> anyhow::Error {
    match e {
        inquire::InquireError::OperationCanceled | inquire::InquireError::OperationInterrupted => {
            Interrupted.into()
        }
        e => anyhow::Error::new(e).context("couldn't prompt"),
    }
}

/// The cluster `arg` names, by ID or name.
pub(super) async fn resolve(ctx: &Ctx, client: &Client, arg: &str) -> Result<Response<Cluster>> {
    ctx.abortable(async { Ok(resolve_cluster(client, arg).await?) })
        .await
}

fn wait_options(ctx: &Ctx, timeout: Duration) -> WaitOptions {
    WaitOptions {
        timeout,
        cancel: ctx.cancel.clone(),
        ..WaitOptions::default()
    }
}

/// Every page of a list.
async fn all_pages<T, F, Fut>(ctx: &Ctx, fetch: F) -> Result<Vec<T>>
where
    F: FnMut(Option<String>) -> Fut,
    Fut: Future<Output = Result<Response<Page<T>>, ApiError>>,
{
    ctx.abortable(async { Ok(collect_pages(None, fetch).await?.0) })
        .await
}

/// `--no-wait`: the started Action, raw under `--json`, otherwise how to follow it.
fn started(ctx: &Ctx, action: &Response<Action>, what: &str) -> Result<()> {
    if ctx.out.is_json() {
        return ctx.out.json(&action.raw);
    }
    ctx.out
        .success(&format!("Started {what} ({})", action.value.id));
    ctx.out.info(&format!(
        "  Follow it with: bonsai wait {}",
        action.value.id
    ));
    Ok(())
}

// ---------------------------------------------------------------- create

/// What the create asks for, once flags and prompts have filled it in.
struct Choice {
    plan: String,
    region: String,
    space: Option<String>,
    release: Option<String>,
}

async fn create(ctx: &Ctx, args: CreateArgs, idempotency_key: Option<String>) -> Result<()> {
    if !ctx.interactive {
        let missing = missing_create_flags(
            args.plan.as_deref(),
            args.region.as_deref(),
            args.space.as_deref(),
        );
        if !missing.is_empty() {
            return Err(usage(format!(
                "missing {}; pass {}, or run on a terminal to choose",
                missing.join(" and "),
                if missing.len() == 1 { "it" } else { "them" }
            )));
        }
    }
    let a = ctx.authed().await?;
    let choice = choose(ctx, &a.client, &args).await?;
    let body = ClusterCreate {
        name: args.name.clone(),
        plan: choice.plan.clone(),
        region: choice.region.clone(),
        space: choice.space.clone(),
        release: choice.release.clone(),
    };

    let (progress, events) = Progress::new(ctx.global.json, ctx.global.quiet, &args.name);
    let cleanup = if args.keep_failed {
        CleanupPolicy::Keep
    } else if ctx.interactive {
        let progress = progress.clone();
        CleanupPolicy::Ask(Box::new(move |name| {
            progress.suspend(|| {
                inquire::Confirm::new(&format!(
                    "Remove the failed cluster {}?",
                    sanitize(name, false)
                ))
                .with_default(true)
                .prompt()
                // Ctrl-C or Esc keeps the cluster; the summary says how to remove it.
                .unwrap_or(false)
            })
        }))
    } else {
        CleanupPolicy::Remove
    };
    let req = CreateRequest {
        body,
        wait: !args.wait.no_wait,
        cleanup,
        idempotency_key,
    };
    let opts = wait_options(ctx, args.wait.timeout);
    let res = progress
        .drive(create_cluster(&a.client, req, &opts, &events))
        .await;

    let action = match res {
        Ok(action) => action,
        Err(CoreError::Api(e)) if is_payment_refusal(&e) => {
            let rerun = rerun_hint(&CreateChoice {
                name: &args.name,
                plan: &choice.plan,
                region: &choice.region,
                space: choice.space.as_deref(),
                release: choice.release.as_deref(),
            });
            return Err(payment_refused(ctx, &a.settings, e, &rerun));
        }
        Err(CoreError::ActionFailed(failure)) => {
            if ctx.out.is_json() {
                // The failed Action, as for any create that waited; the error
                // report on stderr says what became of the cluster.
                match a.client.get_action(&failure.action_id).await {
                    Ok(done) => ctx.out.json(&done.raw)?,
                    Err(e) => tracing::debug!(error = %e, "couldn't read the failed Action"),
                }
            }
            let hint = Hint(create_failed_hint(&failure, &args.name));
            return Err(anyhow::Error::new(CoreError::ActionFailed(failure)).context(hint));
        }
        Err(e) => return Err(e.into()),
    };

    if args.wait.no_wait {
        return started(ctx, &action, &format!("creating {}", args.name));
    }
    if ctx.out.is_json() {
        return ctx.out.json(&action.raw);
    }
    let id = action
        .value
        .result
        .as_ref()
        .and_then(|r| r.cluster.clone())
        .unwrap_or_else(|| action.value.subject.id.clone());
    let host = match a.client.get_cluster(&id).await {
        Ok(c) => c.value.host.unwrap_or_default(),
        Err(e) => {
            tracing::debug!(error = %e, "couldn't read the new cluster's host");
            String::new()
        }
    };
    let ready = format!("{} is ready  {host}", args.name);
    ctx.out.success(ready.trim_end());
    ctx.out.info(&format!(
        "  Connection URL (includes the password): run bonsai url {}",
        shell_quote(&args.name)
    ));
    Ok(())
}

/// Fills in what the flags left out: the region from `--space`, and on a terminal
/// by asking for region, space, plan and release, in that order.
async fn choose(ctx: &Ctx, client: &Client, args: &CreateArgs) -> Result<Choice> {
    let mut space = args.space.clone();
    let mut region = args.region.clone();
    if let (Some(space_id), None) = (&space, &region) {
        let spaces = all_pages(
            ctx,
            |c| async move { client.list_spaces(c.as_deref()).await },
        )
        .await?;
        let found = spaces.iter().find(|s| &s.id == space_id).ok_or_else(|| {
            usage(format!(
                "no private space {space_id:?}; `bonsai spaces` lists them"
            ))
        })?;
        region = Some(found.region.clone());
    }
    let region = match region {
        Some(r) => r,
        None => {
            let (r, s) = ask_region_and_space(ctx, client).await?;
            space = s;
            r
        }
    };
    let (plan, prompted) = match &args.plan {
        Some(p) => (p.clone(), None),
        None => {
            let plan = ask_plan(ctx, client, &region).await?;
            (plan.id.clone(), Some(plan))
        }
    };
    // A release is asked for only with a plan that was: otherwise the API's default
    // (the region's first the space can place) applies, as it does for scripts.
    let release = match (&args.release, prompted) {
        (Some(r), _) => Some(r.clone()),
        (None, Some(plan)) => ask_release(ctx, client, &plan, &region).await?,
        (None, None) => None,
    };
    Ok(Choice {
        plan,
        region,
        space,
        release,
    })
}

const SHARED: &str = "Shared";

async fn ask_region_and_space(ctx: &Ctx, client: &Client) -> Result<(String, Option<String>)> {
    let regions = all_pages(
        ctx,
        |c| async move { client.list_regions(c.as_deref()).await },
    )
    .await?;
    if regions.is_empty() {
        anyhow::bail!("no region is open for new clusters");
    }
    let labels = regions
        .iter()
        .map(|r| sanitize(&format!("{} ({})", r.id, r.name), false).into_owned())
        .collect();
    let region = regions[select("Region", labels)?].id.clone();

    let spaces = all_pages(
        ctx,
        |c| async move { client.list_spaces(c.as_deref()).await },
    )
    .await?;
    let here: Vec<_> = spaces.iter().filter(|s| s.region == region).collect();
    if here.is_empty() {
        return Ok((region, None));
    }
    let mut labels = vec![SHARED.to_owned()];
    labels.extend(
        here.iter()
            .map(|s| sanitize(&format!("{} ({})", s.name, s.id), false).into_owned()),
    );
    let i = select("Network", labels)?;
    Ok((region, (i > 0).then(|| here[i - 1].id.clone())))
}

async fn ask_plan(ctx: &Ctx, client: &Client, region: &str) -> Result<Plan> {
    let plans = all_pages(
        ctx,
        |c| async move { client.list_plans(c.as_deref()).await },
    )
    .await?;
    let offered = plans_in_region(&plans, region);
    if offered.is_empty() {
        anyhow::bail!("no plan this account may use is offered in {region}");
    }
    let labels = offered
        .iter()
        .map(|p| sanitize(&plan_choice(p), false).into_owned())
        .collect();
    Ok(offered[select("Plan", labels)?].clone())
}

/// Asks for one of the releases the plan offers in `region`, the first (the API's
/// default there) preselected.
async fn ask_release(
    ctx: &Ctx,
    client: &Client,
    plan: &Plan,
    region: &str,
) -> Result<Option<String>> {
    let offered = plan.releases_in(region).ok_or_else(|| {
        usage(format!(
            "plan {} isn't offered in {region}; `bonsai plans --region {region}` lists the plans that are",
            plan.id
        ))
    })?;
    if offered.is_empty() {
        return Ok(None);
    }
    let releases = all_pages(
        ctx,
        |c| async move { client.list_releases(c.as_deref()).await },
    )
    .await?;
    let labels = release_choices(offered, &releases)
        .into_iter()
        .map(|l| sanitize(&l, false).into_owned())
        .collect();
    Ok(Some(offered[select("Release", labels)?].clone()))
}

/// The index of the option picked; the first is the default.
fn select(prompt: &str, options: Vec<String>) -> Result<usize> {
    let picked = inquire::Select::new(prompt, options)
        .with_starting_cursor(0)
        .raw_prompt()
        .map_err(prompt_error)?;
    Ok(picked.index)
}

fn is_payment_refusal(e: &ApiError) -> bool {
    matches!(
        e.code(),
        Some(ProblemCode::PaymentMethodRequired | ProblemCode::PaymentFailed)
    )
}

/// A `422 payment_method_required` or `payment_failed` (which change nothing),
/// as the command's error: on a terminal the offer to open the Checkout link comes
/// first, and the hint says what to do about the card, then `rerun`.
fn payment_refused(ctx: &Ctx, settings: &Settings, e: ApiError, rerun: &str) -> anyhow::Error {
    let code = e
        .code()
        .cloned()
        .unwrap_or(ProblemCode::PaymentMethodRequired);
    let mut lines = Vec::new();
    if ctx.interactive
        && !ctx.out.is_json()
        && let Some(url) = action_url(&e)
    {
        lines.extend(offer_to_open(settings, &code, url));
    }
    lines.extend(payment_hint(&code, rerun));
    anyhow::Error::new(CoreError::Api(e)).context(Hint(lines))
}

fn action_url(e: &ApiError) -> Option<&str> {
    match e {
        ApiError::Problem { problem, .. } => problem.action_url.as_deref(),
        _ => None,
    }
}

/// After `422 payment_method_required` or `payment_failed`, which build nothing:
/// what to do about the card, and what to run then. Printed after the error and its
/// Checkout link.
fn payment_hint(code: &ProblemCode, rerun: &str) -> Vec<String> {
    let card = match code {
        ProblemCode::PaymentFailed => "  Update the card at that link (no Bonsai login needed).",
        _ => "  Add a card at that link (no Bonsai login needed).",
    };
    vec![card.into(), format!("  Then run: {rerun}")]
}

/// The question before opening the Checkout link.
fn open_prompt(code: &ProblemCode) -> &'static str {
    match code {
        ProblemCode::PaymentFailed => {
            "The card on file was refused. Open the checkout link to update it?"
        }
        _ => "This plan needs a card on file. Open the checkout link in your browser?",
    }
}

/// On a terminal, offers to open the Checkout link before the error is printed
/// (the error report prints the link itself). Returns a line for the hint when the
/// link isn't opened for a reason the user should know.
fn offer_to_open(settings: &Settings, code: &ProblemCode, url: &str) -> Option<String> {
    let allowed = Url::parse(url)
        .is_ok_and(|u| may_open(&u, &settings.api_url, settings.dashboard_url.as_ref()));
    if !allowed {
        return Some(
            "  Not opening the link: it isn't on Stripe Checkout or Bonsai's host.".into(),
        );
    }
    let open = inquire::Confirm::new(open_prompt(code))
        .with_default(true)
        .prompt()
        .unwrap_or(false);
    match open.then(|| open::that_detached(url)) {
        Some(Err(e)) => Some(format!(
            "  Couldn't open a browser ({e}); open the link above."
        )),
        _ => None,
    }
}

/// The provision ended in error: what became of the cluster and what to do next,
/// printed after the error. Never the idempotency key.
fn create_failed_hint(f: &ActionFailure, name: &str) -> Vec<String> {
    let destroy = |id: &str| {
        format!(
            "bonsai cluster destroy {id} --confirm {}",
            shell_quote(name)
        )
    };
    let Some(id) = f.cluster_id.as_deref() else {
        return Vec::new();
    };
    let mut lines = vec![match f
        .cleanup
        .as_ref()
        .map(|c| (&c.outcome, c.cluster_id.as_str()))
    {
        Some((CleanupOutcome::Removed { .. }, id)) => {
            format!("  ✓ Removed the failed cluster {name} ({id})")
        }
        Some((CleanupOutcome::RemovedByBonsai, id)) => {
            format!("  ✓ Bonsai removed the failed cluster {name} ({id})")
        }
        Some((
            CleanupOutcome::Pending {
                deprovision_action_id: act,
            },
            _,
        )) => format!(
            "  Removing the failed cluster {name} started but didn't finish; follow it with: bonsai wait {act}"
        ),
        Some((
            CleanupOutcome::DeprovisionFailed {
                deprovision_action_id: act,
            },
            id,
        )) => format!(
            "  Removing the failed cluster {name} failed ({act}); remove it with: {}",
            destroy(id)
        ),
        Some((CleanupOutcome::Unchecked { .. }, id)) => format!(
            "  The failed cluster {name} ({id}): stopped before checking it; see `bonsai cluster show {id}`"
        ),
        Some((CleanupOutcome::DeleteFailed, id)) => format!(
            "  Couldn't remove the failed cluster {name}; remove it with: {}",
            destroy(id)
        ),
        None if f.replayed => format!(
            "  Left the failed cluster {name} ({id}) in place: it may have been created by an earlier run that used this key. Remove it with: {}",
            destroy(id)
        ),
        None => format!(
            "  Kept the failed cluster {name} ({id}); remove it with: {}",
            destroy(id)
        ),
    }];
    if let Some(url) = &f.action_url {
        lines.push(format!("  Update the card: {url}"));
    }
    lines.push(if f.replayed {
        "  To create a new cluster, run the command again with a new --idempotency-key, or none."
            .into()
    } else {
        "  Run the command again to create a new cluster.".into()
    });
    lines
}

// ---------------------------------------------------------------- show, rename

async fn show(ctx: &Ctx, args: ClusterArg) -> Result<()> {
    let a = ctx.authed().await?;
    let cluster = resolve(ctx, &a.client, &args.cluster).await?;
    if ctx.out.is_json() {
        return ctx.out.json(&cluster.raw);
    }
    ctx.out.kv(&show_rows(&cluster.value, Timestamp::now()))
}

/// Prints the cluster's dashboard link, and opens it on a terminal unless
/// `--no-browser`: only on Bonsai's hosts, as any link the API hands out.
async fn open(ctx: &Ctx, args: OpenArgs) -> Result<()> {
    let settings = ctx.settings()?;
    let a = ctx.authed().await?;
    let cluster = resolve(ctx, &a.client, &args.cluster.cluster).await?.value;
    let url = cluster.dashboard_url;
    if ctx.out.is_json() {
        ctx.out.json(&serde_json::json!({ "dashboard_url": url }))?;
    } else {
        ctx.out.data_line(&sanitize(&url, false))?;
    }
    if args.no_browser || !ctx.interactive {
        return Ok(());
    }
    let allowed = Url::parse(&url)
        .is_ok_and(|u| may_open(&u, &settings.api_url, settings.dashboard_url.as_ref()));
    if !allowed {
        ctx.out
            .warn("Not opening that link: it isn't on Bonsai's host. Check it before you use it.");
    } else if let Err(e) = open::that_detached(&url) {
        ctx.out.warn(&format!(
            "Couldn't open a browser ({e}); open the link above"
        ));
    }
    Ok(())
}

async fn rename(ctx: &Ctx, args: RenameArgs) -> Result<()> {
    let a = ctx.authed().await?;
    let cluster = resolve(ctx, &a.client, &args.cluster).await?.value;
    let body = ClusterUpdate {
        name: args.new_name,
    };
    let renamed = ctx
        .abortable(async { Ok(a.client.update_cluster(&cluster.id, &body).await?) })
        .await?;
    if ctx.out.is_json() {
        return ctx.out.json(&renamed.raw);
    }
    ctx.out.success(&format!(
        "Renamed {} to {} ({})",
        cluster.name, renamed.value.name, renamed.value.id
    ));
    Ok(())
}

// ---------------------------------------------------------------- resize, destroy

async fn resize(ctx: &Ctx, args: ResizeArgs, idempotency_key: Option<String>) -> Result<()> {
    let a = ctx.authed().await?;
    let cluster = resolve(ctx, &a.client, &args.cluster).await?.value;
    if ctx.interactive && !confirm_resize(ctx, &a, &cluster, &args.plan).await? {
        // A choice, not a failure.
        ctx.out.notice(&format!(
            "Left {} on its plan; nothing changed",
            cluster.name
        ));
        return Ok(());
    }
    let (progress, events) = Progress::new(ctx.global.json, ctx.global.quiet, &cluster.name);
    let opts = wait_options(ctx, args.wait.timeout);
    let res = progress
        .drive(resize_cluster(
            &a.client,
            &cluster.id,
            &args.plan,
            idempotency_key.as_deref(),
            !args.wait.no_wait,
            &opts,
            &events,
        ))
        .await;
    let action = match res {
        Ok(action) => action,
        Err(CoreError::Api(e)) if is_payment_refusal(&e) => {
            let rerun = format!(
                "bonsai cluster resize {} {}",
                shell_quote(&cluster.id),
                shell_quote(&args.plan)
            );
            return Err(payment_refused(ctx, &a.settings, e, &rerun));
        }
        Err(e) => return Err(e.into()),
    };
    finished(
        ctx,
        &args.wait,
        &action,
        &format!("changing the plan of {}", cluster.name),
        &format!("{} is on {} now", cluster.name, args.plan),
    )
}

/// Shows the old and new plan with their prices, and asks.
async fn confirm_resize(ctx: &Ctx, a: &Authed, cluster: &Cluster, plan: &str) -> Result<bool> {
    let label = |p: &Plan| format!("{} ({}, {})", p.name, p.id, plan_price(p));
    let new = ctx
        .abortable(async { Ok(a.client.get_plan(plan).await?) })
        .await?;
    let old = match &cluster.plan {
        Some(p) => match a.client.get_plan(&p.id).await {
            Ok(old) => label(&old.value),
            Err(_) => format!("{} ({})", p.name, p.id),
        },
        None => "none".into(),
    };
    ctx.out.notice(&format!("  From  {old}"));
    ctx.out.notice(&format!("  To    {}", label(&new.value)));
    inquire::Confirm::new(&format!(
        "Change the plan of {}?",
        sanitize(&cluster.name, false)
    ))
    .with_default(true)
    .prompt()
    .map_err(prompt_error)
}

async fn destroy(ctx: &Ctx, args: DestroyArgs, idempotency_key: Option<String>) -> Result<()> {
    if args.confirm.is_none() && !ctx.interactive {
        return Err(usage(
            "destroying a cluster needs --confirm <its name> when prompts are off",
        ));
    }
    let a = ctx.authed().await?;
    let cluster = resolve(ctx, &a.client, &args.cluster.cluster).await?.value;
    // `--confirm` goes to the API as given, which checks it (422 name_mismatch).
    let confirm = match args.confirm {
        Some(name) => name,
        None => {
            let typed = inquire::Text::new(&format!(
                "Type {} to destroy {}:",
                sanitize(&cluster.name, false),
                cluster.id
            ))
            .prompt()
            .map_err(prompt_error)?;
            if typed != cluster.name {
                return Err(usage(format!(
                    "that isn't {}'s name; nothing was destroyed",
                    cluster.id
                )));
            }
            typed
        }
    };
    let (progress, events) = Progress::new(ctx.global.json, ctx.global.quiet, &cluster.name);
    let opts = wait_options(ctx, args.wait.timeout);
    let action = progress
        .drive(destroy_cluster(
            &a.client,
            &cluster,
            &confirm,
            idempotency_key.as_deref(),
            !args.wait.no_wait,
            &opts,
            &events,
        ))
        .await?;
    finished(
        ctx,
        &args.wait,
        &action,
        &format!("destroying {}", cluster.name),
        &format!("Destroyed {} ({})", cluster.name, cluster.id),
    )
}

/// The outcome of a command that started an Action: the Action under `--json`,
/// otherwise how to follow it (`--no-wait`) or what it did.
fn finished(
    ctx: &Ctx,
    wait: &WaitFlags,
    action: &Response<Action>,
    doing: &str,
    done: &str,
) -> Result<()> {
    if wait.no_wait {
        return started(ctx, action, doing);
    }
    if ctx.out.is_json() {
        return ctx.out.json(&action.raw);
    }
    ctx.out.success(done);
    Ok(())
}

// ---------------------------------------------------------------- history, wait

/// The largest `limit` the contract allows for a page.
const MAX_PAGE: u32 = 100;

async fn history(ctx: &Ctx, args: HistoryArgs) -> Result<()> {
    let a = ctx.authed().await?;
    let cluster = resolve(ctx, &a.client, &args.cluster.cluster).await?.value;
    let limit = args.limit;
    // `--limit` is the total; a page holds at most 100 (the contract's cap).
    let page_size = limit.map(|l| l.min(MAX_PAGE));
    let (actions, raw) = ctx
        .abortable(async {
            let (client, id) = (&a.client, &cluster.id);
            Ok(
                collect_pages(limit.map(|l| l as usize), |cursor| async move {
                    client
                        .list_cluster_actions(id, page_size, cursor.as_deref())
                        .await
                })
                .await?,
            )
        })
        .await?;
    if ctx.out.is_json() {
        return ctx.out.json(&Value::Array(raw));
    }
    if actions.is_empty() {
        ctx.out.info(&format!("No actions on {}", cluster.name));
        return Ok(());
    }
    let now = Timestamp::now();
    let width = table_width();
    if !args.steps {
        let rows = actions.iter().map(|a| history_row(a, now)).collect();
        return ctx.out.table(&HISTORY_COLUMNS, rows, args.wide);
    }
    for (i, action) in actions.iter().enumerate() {
        if i > 0 {
            ctx.out.styled_line("")?;
        }
        for line in history_block(action, now, width, args.wide) {
            ctx.out.styled_line(&line)?;
        }
    }
    Ok(())
}

/// What `cluster wait` does next with the cluster as it is now.
#[derive(Debug, PartialEq, Eq)]
enum WaitStep {
    Ready,
    /// Being or been deprovisioned: it won't be ready.
    Gone,
    /// Wait for this Action: it's running, or it's the error that failed the cluster.
    Follow(String),
    /// `failed` with no failed Action to report.
    Failed,
    /// Not ready, nothing running: look again in a moment.
    Poll,
}

fn wait_step(c: &Cluster) -> WaitStep {
    match (&c.state, &c.latest_action) {
        (ClusterState::Ready, _) => return WaitStep::Ready,
        // A failed create Bonsai terminated itself: its failure is the answer.
        (ClusterState::Deprovisioned, Some(la))
            if la.r#type == ActionType::Provision && la.state == ActionState::Error =>
        {
            return WaitStep::Follow(la.id.clone());
        }
        (ClusterState::Deprovisioning | ClusterState::Deprovisioned, _) => return WaitStep::Gone,
        _ => {}
    }
    let failed = c.state == ClusterState::Failed;
    match &c.latest_action {
        Some(la) if !la.state.is_terminal() => WaitStep::Follow(la.id.clone()),
        Some(la) if failed && la.state == ActionState::Error => WaitStep::Follow(la.id.clone()),
        _ if failed => WaitStep::Failed,
        _ => WaitStep::Poll,
    }
}

async fn wait(ctx: &Ctx, args: ClusterWaitArgs) -> Result<()> {
    let WaitFor::Ready = args.for_state;
    let a = ctx.authed().await?;
    let start = Instant::now();
    let left = || args.timeout.saturating_sub(start.elapsed());
    let mut cluster = resolve(ctx, &a.client, &args.cluster.cluster).await?;
    let (progress, events) = Progress::new(ctx.global.json, ctx.global.quiet, &cluster.value.name);
    loop {
        let c = &cluster.value;
        let step = wait_step(c);
        // Following an Action that already failed takes no time.
        let finished = matches!(&step, WaitStep::Follow(_))
            && c.latest_action
                .as_ref()
                .is_some_and(|la| la.state == ActionState::Error);
        match step {
            WaitStep::Ready => break,
            WaitStep::Gone => anyhow::bail!("{} is {}; it won't be ready", c.name, c.state),
            WaitStep::Failed => return Err(cluster_failed(c)),
            // Every other step takes time: none starts once the time is up.
            _ if left().is_zero() && !finished => return Err(not_ready(c, args.timeout)),
            WaitStep::Follow(id) => {
                let action = ctx
                    .abortable(async { Ok(a.client.get_action(&id).await?) })
                    .await?;
                // Bounded by what's left; an Action in error fails here (exit 5).
                let opts = wait_options(ctx, left());
                progress
                    .drive(wait_for_action(&a.client, action, &opts, &events))
                    .await?;
            }
            WaitStep::Poll => {
                tokio::select! {
                    biased;
                    () = ctx.cancel.cancelled() => {
                        let resume = format!(
                            "  resume with: bonsai cluster wait {}",
                            shell_quote(&c.id)
                        );
                        return Err(anyhow::Error::new(Interrupted).context(Hint(vec![resume])));
                    }
                    () = tokio::time::sleep(CLUSTER_POLL.min(left())) => {}
                }
            }
        }
        let id = cluster.value.id.clone();
        cluster = ctx
            .abortable(async { Ok(a.client.get_cluster(&id).await?) })
            .await?;
    }
    if ctx.out.is_json() {
        return ctx.out.json(&cluster.raw);
    }
    ctx.out.success(&format!(
        "{} is {}",
        cluster.value.name, cluster.value.state
    ));
    Ok(())
}

/// A `failed` cluster with no failed Action to report: exit 5, as an Action error.
fn cluster_failed(c: &Cluster) -> anyhow::Error {
    Failed(format!(
        "{} failed; `bonsai cluster history {} --steps` shows its Actions",
        c.name, c.id
    ))
    .into()
}

/// A cluster still not ready at the timeout, with no running Action to resume:
/// [`TimedOut`] (exit 9), pointing at `cluster wait` itself.
fn not_ready(c: &Cluster, timeout: Duration) -> anyhow::Error {
    TimedOut(format!(
        "{} is still {} after {}; run `bonsai cluster wait {}` again",
        c.name,
        c.state,
        bonsai_core::human_duration(timeout),
        c.id
    ))
    .into()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::render_cluster::cleanup_outcome;
    use bonsai_api::ActionErrorCode;
    use bonsai_core::Cleanup;
    use serde_json::json;

    fn failure(cleanup: Option<Cleanup>, replayed: bool) -> ActionFailure {
        ActionFailure {
            action_id: "act_1".into(),
            code: ActionErrorCode::ActionFailed,
            message: "m".into(),
            action_url: None,
            cluster_id: Some("db-abc123".into()),
            cleanup,
            replayed,
        }
    }

    fn cleanup(outcome: CleanupOutcome) -> Option<Cleanup> {
        Some(Cleanup {
            cluster_id: "db-abc123".into(),
            outcome,
        })
    }

    fn removed() -> CleanupOutcome {
        CleanupOutcome::Removed {
            deprovision_action_id: "act_2".into(),
        }
    }

    fn pending() -> CleanupOutcome {
        CleanupOutcome::Pending {
            deprovision_action_id: "act_2".into(),
        }
    }

    fn deprovision_failed() -> CleanupOutcome {
        CleanupOutcome::DeprovisionFailed {
            deprovision_action_id: "act_2".into(),
        }
    }

    fn cluster(state: &str, latest: Option<(&str, &str)>) -> Cluster {
        serde_json::from_value(json!({
            "id": "db-abc123", "name": "db", "state": state,
            "plan": null, "region": {"id": "r", "name": "R"}, "space": null,
            "release": {"id": "v", "name": "V"}, "single_tenant": false,
            "host": null, "port": 443,
            "stats": {"docs": null, "shards": null, "bytes": null, "polled_at": null, "limits": null},
            "latest_action": latest.map(|(id, state)| json!({"id": id, "type": "cluster.provision", "state": state})),
            "dashboard_url": "https://app.bonsai.io/accounts/acme/clusters/c", "created_at": "2026-10-20T12:00:00Z", "terminated_at": null
        }))
        .unwrap()
    }

    #[test]
    fn the_failed_create_hint_says_what_became_of_the_cluster() {
        let first = |f: ActionFailure| create_failed_hint(&f, "db")[0].clone();
        assert!(first(failure(cleanup(removed()), false)).contains("✓ Removed"));
        assert!(
            first(failure(cleanup(CleanupOutcome::RemovedByBonsai), false))
                .contains("✓ Bonsai removed the failed cluster db (db-abc123)")
        );
        assert!(first(failure(cleanup(pending()), false)).contains("bonsai wait act_2"));
        // A failed deprovision isn't "still running": destroy it.
        let failed = first(failure(cleanup(deprovision_failed()), false));
        assert!(
            failed.contains("bonsai cluster destroy db-abc123 --confirm db"),
            "{failed}"
        );
        assert!(!failed.contains("bonsai wait"), "{failed}");
        assert!(
            first(failure(cleanup(CleanupOutcome::DeleteFailed), false))
                .contains("bonsai cluster destroy db-abc123 --confirm db")
        );
        assert!(first(failure(None, false)).contains("Kept"));
        let unchecked = first(failure(
            cleanup(CleanupOutcome::Unchecked {
                reason: bonsai_core::StopReason::Cancelled,
            }),
            false,
        ));
        assert!(
            unchecked.contains("stopped before checking it; see `bonsai cluster show db-abc123`"),
            "{unchecked}"
        );
        assert!(!unchecked.contains("Kept"), "{unchecked}");
        let replayed = create_failed_hint(&failure(None, true), "db");
        assert!(
            replayed[0].contains("may have been created by an earlier run that used this key"),
            "{replayed:?}"
        );
        assert!(
            replayed[1].contains("new --idempotency-key"),
            "{replayed:?}"
        );
    }

    #[test]
    fn each_cleanup_has_one_outcome() {
        let outcome = |f: ActionFailure| cleanup_outcome(&f).map(|o| (o.0, o.2.map(str::to_owned)));
        let act = || Some("act_2".to_owned());
        assert_eq!(
            outcome(failure(cleanup(removed()), false)),
            Some(("removed", act()))
        );
        assert_eq!(
            outcome(failure(cleanup(CleanupOutcome::RemovedByBonsai), false)),
            Some(("removed_by_bonsai", None))
        );
        assert_eq!(
            outcome(failure(cleanup(pending()), false)),
            Some(("deprovisioning", act()))
        );
        assert_eq!(
            outcome(failure(cleanup(deprovision_failed()), false)),
            Some(("remove_failed", act()))
        );
        assert_eq!(
            outcome(failure(cleanup(CleanupOutcome::DeleteFailed), false)),
            Some(("remove_failed", None))
        );
        assert_eq!(outcome(failure(None, false)), Some(("kept", None)));
        assert_eq!(
            outcome(failure(
                cleanup(CleanupOutcome::Unchecked {
                    reason: bonsai_core::StopReason::Timeout
                }),
                false
            )),
            Some(("unchecked", None))
        );
        assert_eq!(outcome(failure(None, true)), Some(("replayed", None)));
        let mut f = failure(None, false);
        f.cluster_id = None;
        assert_eq!(cleanup_outcome(&f), None);
    }

    #[test]
    fn a_refused_card_isnt_a_missing_one() {
        let missing = payment_hint(&ProblemCode::PaymentMethodRequired, "r");
        assert!(missing[0].contains("Add a card"), "{missing:?}");
        let refused = payment_hint(&ProblemCode::PaymentFailed, "r");
        assert!(refused[0].contains("Update the card"), "{refused:?}");
        assert!(!refused[0].contains("Add a card"), "{refused:?}");
        assert!(open_prompt(&ProblemCode::PaymentFailed).contains("refused"));
        assert!(open_prompt(&ProblemCode::PaymentMethodRequired).contains("needs a card"));
    }

    #[test]
    fn the_payment_hint_ends_with_the_rerun() {
        let lines = payment_hint(
            &ProblemCode::PaymentMethodRequired,
            "bonsai cluster create x --plan p --region r",
        );
        assert_eq!(
            lines.last().unwrap(),
            "  Then run: bonsai cluster create x --plan p --region r"
        );
    }

    #[test]
    fn wait_follows_only_a_running_action_or_the_one_that_failed_it() {
        use WaitStep::*;
        assert_eq!(wait_step(&cluster("ready", None)), Ready);
        assert_eq!(wait_step(&cluster("deprovisioned", None)), Gone);
        // A failed create Bonsai terminated: its failure is the answer (exit 5).
        assert_eq!(
            wait_step(&cluster("deprovisioned", Some(("a", "error")))),
            Follow("a".into())
        );
        assert_eq!(
            wait_step(&cluster("deprovisioning", Some(("a", "running")))),
            Gone
        );
        assert_eq!(
            wait_step(&cluster("provisioning", Some(("a", "running")))),
            Follow("a".into())
        );
        assert_eq!(
            wait_step(&cluster("failed", Some(("a", "error")))),
            Follow("a".into())
        );
        // A failed cluster whose latest Action didn't fail, or that has none: there's
        // nothing to follow, and it won't get better by itself.
        assert_eq!(
            wait_step(&cluster("failed", Some(("a", "success")))),
            Failed
        );
        assert_eq!(wait_step(&cluster("failed", None)), Failed);
        // Not ready and nothing running: look again later, within the timeout.
        assert_eq!(
            wait_step(&cluster("provisioning", Some(("a", "success")))),
            Poll
        );
        assert_eq!(wait_step(&cluster("draft", None)), Poll);
        assert_eq!(
            crate::exit::exit_code(&cluster_failed(&cluster("failed", None))),
            5
        );
    }

    #[test]
    fn a_cluster_that_isnt_ready_in_time_exits_9() {
        let e = not_ready(&cluster("read_only", None), Duration::from_secs(90));
        assert_eq!(crate::exit::exit_code(&e), 9);
        assert_eq!(
            e.to_string(),
            "db is still read_only after 1m 30s; run `bonsai cluster wait db-abc123` again"
        );
    }
}
