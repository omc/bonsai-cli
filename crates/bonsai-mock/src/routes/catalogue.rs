//! Plans, regions, spaces and releases (`listPlans`, `getPlan`, `listRegions`,
//! `listSpaces`, `listReleases`), plus the queries other routes use to look
//! them up. Lists are in seed `sort` order.

use std::collections::BTreeMap;

use axum::extract::{Path, State};
use axum::routing::get;
use axum::{Json, Router};
use bonsai_api::{ClusterLimits, Page, Plan, Region, Release, Space};
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
    /// The releases offered in each region, in offer order. Empty offers every
    /// release in `plan.releases` in every region in `plan.regions`.
    pub offers: BTreeMap<String, Vec<String>>,
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
    Ok(PlanRow {
        plan: Plan {
            id: r.get(0)?,
            name: r.get(1)?,
            price_in_cents: r.get(2)?,
            billing_interval_months: r.get(3)?,
            single_tenant: r.get(4)?,
            private_network: r.get(5)?,
            regions: db::json(r, 6)?,
            releases: db::json(r, 7)?,
        },
        tier: r.get(8)?,
        dedicated: r.get(9)?,
        max_credentials: r.get(10)?,
        limits: (!none).then_some(limits),
        offers: db::json(r, 15)?,
    })
}

impl PlanRow {
    /// The releases this plan offers in `region`, in offer order, so the first
    /// is a create's default there. Empty when the plan isn't offered there.
    pub fn releases_in(&self, region: &str) -> &[String] {
        if self.offers.is_empty() {
            let offered = self.plan.regions.iter().any(|r| r == region);
            return if offered { &self.plan.releases } else { &[] };
        }
        self.offers.get(region).map_or(&[], Vec::as_slice)
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

async fn list_plans(
    State(state): State<AppState>,
    auth: Auth,
    q: QueryParams,
) -> Result<Json<Page<Plan>>, ProblemBuilder> {
    auth.require(Op::ReadPlans)?;
    let page = PageParams::from_query(&q)?;
    let plans = state.db(PlanRow::all)?;
    Ok(Json(paginate(
        plans.into_iter().map(|p| p.plan).collect(),
        page,
    )))
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
    Ok(Json(paginate(state.db(regions)?, page)))
}

async fn list_spaces(
    State(state): State<AppState>,
    auth: Auth,
    q: QueryParams,
) -> Result<Json<Page<Space>>, ProblemBuilder> {
    auth.require(Op::ReadSpaces)?;
    let page = PageParams::from_query(&q)?;
    Ok(Json(paginate(
        state.db(|c| spaces(c, &auth.account_id))?,
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
    Ok(Json(paginate(state.db(releases)?, page)))
}
