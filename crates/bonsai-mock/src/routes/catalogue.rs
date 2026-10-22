//! Plans, regions, spaces and releases (`listPlans`, `getPlan`, `listRegions`,
//! `listSpaces`, `listReleases`), plus the queries other routes use to look
//! them up. Lists are in seed `sort` order, except releases, which are in offer
//! order. A plan's `offers` say where it can be created and with which releases;
//! the lists hold only what some plan's offers name.

use std::cmp::Reverse;
use std::collections::BTreeMap;

use axum::extract::{Path, State};
use axum::routing::get;
use axum::{Json, Router};
use bonsai_api::{ClusterLimits, Page, Plan, PlanOffer, Region, Release, Space};
use rusqlite::{Connection, OptionalExtension, Row};

use crate::http::{Auth, Op, PageParams, QueryParams, paginate};
use crate::problem::{ProblemBuilder, not_found};
use crate::{AppState, db};

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/v1/plans", get(list_plans))
        .route("/v1/plans/{plan_id}", get(show_plan))
        .route("/v1/regions", get(list_regions))
        .route("/v1/spaces", get(list_spaces))
        .route("/v1/releases", get(list_releases))
}

/// A plan with the columns the API doesn't show.
#[derive(Debug, Clone)]
pub struct PlanRow {
    pub plan: Plan,
    /// `hobby`, `standard`, `business` or `enterprise`.
    pub tier: String,
    /// Needs its own servers, which only Bonsai support provisions.
    pub dedicated: bool,
    /// `None` is unlimited.
    pub max_credentials: Option<u32>,
    /// `None` when the plan has no limits on record.
    pub limits: Option<ClusterLimits>,
}

const PLAN_COLUMNS: &str = "id, name, price_in_cents, billing_interval_months, single_tenant,
    private_network, regions, releases, tier, dedicated, max_credentials, limit_docs,
    limit_shards, limit_bytes, limit_bandwidth, offers";

fn plan_row(r: &Row) -> rusqlite::Result<PlanRow> {
    let limits = ClusterLimits {
        docs: r.get(11)?,
        shards: r.get(12)?,
        bytes: r.get(13)?,
        daily_bandwidth_bytes: r.get(14)?,
    };
    let none = limits.docs.is_none()
        && limits.shards.is_none()
        && limits.bytes.is_none()
        && limits.daily_bandwidth_bytes.is_none();
    let regions: Vec<String> = db::json(r, 6)?;
    let releases: Vec<String> = db::json(r, 7)?;
    let offers = plan_offers(regions, &releases, db::json(r, 15)?);
    Ok(PlanRow {
        plan: Plan {
            id: r.get(0)?,
            name: r.get(1)?,
            price_in_cents: r.get(2)?,
            billing_interval_months: r.get(3)?,
            single_tenant: r.get(4)?,
            private_network: r.get(5)?,
            regions: offers.iter().map(|o| o.region.clone()).collect(),
            releases,
            offers,
        },
        tier: r.get(8)?,
        dedicated: r.get(9)?,
        max_credentials: r.get(10)?,
        limits: (!none).then_some(limits),
    })
}

/// A plan's `offers` column as the contract's array, in region order (the
/// `regions` column, which seeding writes in region `sort` order). An empty
/// map, from before plans had offers, offers every one of `releases` in every
/// one of `regions`.
fn plan_offers(
    regions: Vec<String>,
    releases: &[String],
    mut offers: BTreeMap<String, Vec<String>>,
) -> Vec<PlanOffer> {
    if offers.is_empty() {
        return regions
            .into_iter()
            .map(|region| PlanOffer {
                region,
                releases: releases.to_vec(),
            })
            .collect();
    }
    let mut ordered: Vec<PlanOffer> = regions
        .into_iter()
        .filter_map(|region| {
            let releases = offers.remove(&region)?;
            Some(PlanOffer { region, releases })
        })
        .collect();
    // Any region the `regions` column missed, by ID.
    ordered.extend(
        offers
            .into_iter()
            .map(|(region, releases)| PlanOffer { region, releases }),
    );
    ordered
}

/// The order releases are offered in: OpenSearch first, then newest first.
/// Sort by it stably, so releases of one version keep their seed order.
pub fn offer_order(engine: &str, version: &str) -> (bool, Reverse<Vec<u64>>) {
    let parts = version.split('.').map(|p| p.parse().unwrap_or(0)).collect();
    (engine != "opensearch", Reverse(parts))
}

impl PlanRow {
    /// The releases this plan offers in `region`, in offer order, so the first
    /// is a create's default there. Empty when the plan isn't offered there.
    pub fn releases_in(&self, region: &str) -> &[String] {
        self.plan.releases_in(region).unwrap_or(&[])
    }

    /// Whether the plan can be created anywhere.
    pub fn is_offered(&self) -> bool {
        !self.plan.offers.is_empty()
    }

    pub fn all(conn: &Connection) -> rusqlite::Result<Vec<PlanRow>> {
        conn.prepare(&format!("SELECT {PLAN_COLUMNS} FROM plans ORDER BY sort"))?
            .query_map([], plan_row)?
            .collect()
    }

    pub fn find(conn: &Connection, id: &str) -> rusqlite::Result<Option<PlanRow>> {
        conn.query_row(
            &format!("SELECT {PLAN_COLUMNS} FROM plans WHERE id = ?1"),
            [id],
            plan_row,
        )
        .optional()
    }
}

pub fn regions(conn: &Connection) -> rusqlite::Result<Vec<Region>> {
    conn.prepare("SELECT id, name, cloud, cloud_region FROM regions ORDER BY sort")?
        .query_map([], region_row)?
        .collect()
}

fn region_row(r: &Row) -> rusqlite::Result<Region> {
    Ok(Region {
        id: r.get(0)?,
        name: r.get(1)?,
        cloud: db::text_enum(r, 2)?,
        cloud_region: r.get(3)?,
    })
}

pub fn releases(conn: &Connection) -> rusqlite::Result<Vec<Release>> {
    conn.prepare("SELECT id, name, engine, version FROM releases ORDER BY sort")?
        .query_map([], |r| {
            Ok(Release {
                id: r.get(0)?,
                name: r.get(1)?,
                engine: db::text_enum(r, 2)?,
                version: r.get(3)?,
            })
        })?
        .collect()
}

/// The account's private spaces.
pub fn spaces(conn: &Connection, account_id: &str) -> rusqlite::Result<Vec<Space>> {
    conn.prepare(
        "SELECT id, name, region, private_network FROM spaces WHERE account_id = ?1 ORDER BY rowid",
    )?
    .query_map([account_id], |r| {
        Ok(Space {
            id: r.get(0)?,
            name: r.get(1)?,
            region: r.get(2)?,
            private_network: r.get(3)?,
        })
    })?
    .collect()
}

/// The plans offered somewhere. Plans don't differ by account here.
fn offered_plans(conn: &Connection) -> rusqlite::Result<Vec<Plan>> {
    Ok(PlanRow::all(conn)?
        .into_iter()
        .filter(PlanRow::is_offered)
        .map(|p| p.plan)
        .collect())
}

/// Whether some offered plan can be created in `region`.
fn is_offered_in(plans: &[Plan], region: &str) -> bool {
    plans.iter().any(|p| p.releases_in(region).is_some())
}

/// The regions some plan is offered in.
fn offered_regions(conn: &Connection) -> rusqlite::Result<Vec<Region>> {
    let plans = offered_plans(conn)?;
    let mut all = regions(conn)?;
    all.retain(|r| is_offered_in(&plans, &r.id));
    Ok(all)
}

/// The account's private spaces some plan can be placed in: every plan offered
/// in a space's region can be.
fn offered_spaces(conn: &Connection, account_id: &str) -> rusqlite::Result<Vec<Space>> {
    let plans = offered_plans(conn)?;
    let mut all = spaces(conn, account_id)?;
    all.retain(|s| is_offered_in(&plans, &s.region));
    Ok(all)
}

/// The releases some plan is offered with, in offer order.
fn offered_releases(conn: &Connection) -> rusqlite::Result<Vec<Release>> {
    let plans = offered_plans(conn)?;
    let mut all = releases(conn)?;
    all.retain(|r| {
        plans
            .iter()
            .flat_map(|p| &p.offers)
            .any(|o| o.releases.contains(&r.id))
    });
    all.sort_by_cached_key(|r| offer_order(r.engine.as_str(), &r.version));
    Ok(all)
}

async fn list_plans(
    State(state): State<AppState>,
    auth: Auth,
    q: QueryParams,
) -> Result<Json<Page<Plan>>, ProblemBuilder> {
    auth.require(Op::ReadPlans)?;
    let page = PageParams::from_query(&q)?;
    Ok(Json(paginate(state.db(offered_plans)?, page)))
}

async fn show_plan(
    State(state): State<AppState>,
    auth: Auth,
    Path(plan_id): Path<String>,
) -> Result<Json<Plan>, ProblemBuilder> {
    auth.require(Op::ReadPlans)?;
    let plan = state.db(|c| PlanRow::find(c, &plan_id))?;
    plan.map(|p| Json(p.plan)).ok_or_else(not_found)
}

async fn list_regions(
    State(state): State<AppState>,
    auth: Auth,
    q: QueryParams,
) -> Result<Json<Page<Region>>, ProblemBuilder> {
    auth.require(Op::ReadSpaces)?;
    let page = PageParams::from_query(&q)?;
    Ok(Json(paginate(state.db(offered_regions)?, page)))
}

async fn list_spaces(
    State(state): State<AppState>,
    auth: Auth,
    q: QueryParams,
) -> Result<Json<Page<Space>>, ProblemBuilder> {
    auth.require(Op::ReadSpaces)?;
    let page = PageParams::from_query(&q)?;
    Ok(Json(paginate(
        state.db(|c| offered_spaces(c, &auth.account_id))?,
        page,
    )))
}

async fn list_releases(
    State(state): State<AppState>,
    auth: Auth,
    q: QueryParams,
) -> Result<Json<Page<Release>>, ProblemBuilder> {
    auth.require(Op::ReadReleases)?;
    let page = PageParams::from_query(&q)?;
    Ok(Json(paginate(state.db(offered_releases)?, page)))
}
