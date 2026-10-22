//! `bonsai plans`, `regions`, `spaces` and `releases`.
//!
//! Each fetches every page. `--json` prints the items as the server sent them (after
//! the CLI-side filters); human output is a table.

use anyhow::Result;
use bonsai_api::{ApiError, Page, Plan, Release, Response, collect_pages};
use serde_json::Value;

use super::Ctx;
use crate::cli::{EngineArg, PlansArgs, ReleasesArgs, WideArgs};
use crate::display::plan_price;
use crate::output::Column;

const PLAN_COLUMNS: [Column; 7] = [
    Column::key("ID"),
    Column::text("NAME", 4),
    Column::fixed("PRICE", 5),
    Column::fixed("TENANCY", 3),
    Column::fixed("PRIVATE NETWORK", 2).wide(),
    Column::text("REGIONS", 1).wide(),
    Column::text("RELEASES", 1).wide(),
];

const REGION_COLUMNS: [Column; 4] = [
    Column::key("ID"),
    Column::text("NAME", 3),
    Column::fixed("CLOUD", 2),
    Column::fixed("CLOUD REGION", 1).wide(),
];

const SPACE_COLUMNS: [Column; 4] = [
    Column::key("ID"),
    Column::text("NAME", 3),
    Column::fixed("REGION", 2),
    Column::fixed("PRIVATE NETWORK", 1).wide(),
];

const RELEASE_COLUMNS: [Column; 4] = [
    Column::key("ID"),
    Column::fixed("ENGINE", 3),
    Column::fixed("VERSION", 2),
    Column::text("NAME", 1).wide(),
];

pub async fn plans(ctx: &Ctx, args: PlansArgs) -> Result<()> {
    ctx.abortable(async {
        let a = ctx.authed().await?;
        let client = &a.client;
        let (plans, raw) = all(|c| async move { client.list_plans(c.as_deref()).await }).await?;
        let (plans, raw) = filter(plans, raw, |p: &Plan| {
            plan_matches(p, args.region.as_deref(), args.release.as_deref())
        });
        if ctx.out.is_json() {
            return ctx.out.json(&Value::Array(raw));
        }
        let rows = plans
            .iter()
            .map(|p| {
                vec![
                    p.id.clone(),
                    p.name.clone(),
                    plan_price(p),
                    if p.single_tenant { "single" } else { "shared" }.to_owned(),
                    yes_no(p.private_network),
                    p.regions.join(", "),
                    p.releases.join(", "),
                ]
            })
            .collect();
        ctx.out.table(&PLAN_COLUMNS, rows, args.wide)
    })
    .await
}

/// Whether `plan` passes `--region` and `--release`: offered in the region, with
/// the release; with both, with the release in that region.
fn plan_matches(plan: &Plan, region: Option<&str>, release: Option<&str>) -> bool {
    let has = |releases: &[String], v: &str| releases.iter().any(|r| r == v);
    match (region, release) {
        (Some(r), Some(v)) => plan.releases_in(r).is_some_and(|o| has(o, v)),
        (Some(r), None) => plan.releases_in(r).is_some(),
        (None, Some(v)) => has(&plan.releases, v),
        (None, None) => true,
    }
}

pub async fn regions(ctx: &Ctx, args: WideArgs) -> Result<()> {
    ctx.abortable(async {
        let a = ctx.authed().await?;
        let client = &a.client;
        let (regions, raw) =
            all(|c| async move { client.list_regions(c.as_deref()).await }).await?;
        if ctx.out.is_json() {
            return ctx.out.json(&Value::Array(raw));
        }
        let rows = regions
            .iter()
            .map(|r| {
                vec![
                    r.id.clone(),
                    r.name.clone(),
                    r.cloud.to_string(),
                    r.cloud_region.clone(),
                ]
            })
            .collect();
        ctx.out.table(&REGION_COLUMNS, rows, args.wide)
    })
    .await
}

pub async fn spaces(ctx: &Ctx, args: WideArgs) -> Result<()> {
    ctx.abortable(async {
        let a = ctx.authed().await?;
        let client = &a.client;
        let (spaces, raw) = all(|c| async move { client.list_spaces(c.as_deref()).await }).await?;
        if ctx.out.is_json() {
            return ctx.out.json(&Value::Array(raw));
        }
        if spaces.is_empty() {
            ctx.out.info("This account has no private spaces");
            return Ok(());
        }
        let rows = spaces
            .iter()
            .map(|s| {
                vec![
                    s.id.clone(),
                    s.name.clone(),
                    s.region.clone(),
                    yes_no(s.private_network),
                ]
            })
            .collect();
        ctx.out.table(&SPACE_COLUMNS, rows, args.wide)
    })
    .await
}

pub async fn releases(ctx: &Ctx, args: ReleasesArgs) -> Result<()> {
    ctx.abortable(async {
        let a = ctx.authed().await?;
        let client = &a.client;
        let (releases, raw) =
            all(|c| async move { client.list_releases(c.as_deref()).await }).await?;
        let wanted = args.engine.map(|e| match e {
            EngineArg::Elasticsearch => "elasticsearch",
            EngineArg::Opensearch => "opensearch",
        });
        let (releases, raw) = filter(releases, raw, |r: &Release| {
            wanted.is_none_or(|w| r.engine.as_str() == w)
        });
        if ctx.out.is_json() {
            return ctx.out.json(&Value::Array(raw));
        }
        let rows = releases
            .iter()
            .map(|r| {
                vec![
                    r.id.clone(),
                    r.engine.to_string(),
                    r.version.clone(),
                    r.name.clone(),
                ]
            })
            .collect();
        ctx.out.table(&RELEASE_COLUMNS, rows, args.wide)
    })
    .await
}

/// Every page, typed and as it came.
async fn all<T, F, Fut>(fetch: F) -> Result<(Vec<T>, Vec<Value>)>
where
    F: FnMut(Option<String>) -> Fut,
    Fut: Future<Output = Result<Response<Page<T>>, ApiError>>,
{
    Ok(collect_pages(None, fetch).await?)
}

/// The items `keep` accepts, each with its raw JSON.
fn filter<T>(items: Vec<T>, raw: Vec<Value>, keep: impl Fn(&T) -> bool) -> (Vec<T>, Vec<Value>) {
    items.into_iter().zip(raw).filter(|(t, _)| keep(t)).unzip()
}

fn yes_no(b: bool) -> String {
    if b { "yes" } else { "no" }.to_owned()
}
