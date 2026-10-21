//! A cluster's usage, request metrics and top queries (`getClusterUsage`,
//! `getClusterMetrics`, `getClusterTopQueries`), with the bounds and errors of
//! the Bonsai API.
//!
//! There's no metrics store: every value is derived from a hash of the
//! cluster's ID and the sample's or bucket's time slot, so the same request
//! always gets the same numbers. The `metrics_unavailable` scenario makes all
//! three answer 503, once their parameters have been checked.

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::routing::get;
use axum::{Json, Router};
use bonsai_api::{
    DurationPercentiles, Health, HistogramBucket, MetricBucket, MetricResponses, Metrics,
    ProblemCode, TopQueries, TopQuery, Usage, UsageSample,
};
use jiff::{SignedDuration, Timestamp};
use rusqlite::Connection;

use crate::http::{Auth, Op, QueryParams};
use crate::problem::{ProblemBuilder, invalid_parameter, status_problem};
use crate::routes::clusters::ClusterRow;
use crate::{AppState, scenario};

/// `Retry-After` on 503 `metrics_unavailable`, as the Bonsai API sends it.
pub const UNAVAILABLE_RETRY_AFTER: u64 = 30;

/// `from` defaults to this long before `to`.
const DEFAULT_RANGE: SignedDuration = SignedDuration::from_hours(1);
const USAGE_MAX_RANGE: SignedDuration = SignedDuration::from_hours(24 * 90);
const METRICS_MAX_RANGE: SignedDuration = SignedDuration::from_hours(24 * 30);
/// One usage sample per index-stats poll.
const USAGE_EVERY_SECS: i64 = 600;
const MAX_BUCKETS: f64 = 1_000.0;
/// Without `resolution`, the smallest giving fewer buckets than this.
const AUTO_BUCKETS: f64 = 100.0;

/// Bucket sizes, smallest first, in seconds.
const RESOLUTIONS: &[(&str, i64)] = &[
    ("1m", 60),
    ("30m", 1_800),
    ("1h", 3_600),
    ("6h", 21_600),
    ("12h", 43_200),
    ("1d", 86_400),
];

/// Where each `duration_histogram` bucket starts.
pub const HISTOGRAM_MIN_MS: [u64; 17] = [
    0, 10, 20, 30, 40, 50, 60, 70, 80, 90, 100, 200, 300, 400, 500, 1_000, 2_000,
];
/// How requests spread over the histogram, before noise.
const HISTOGRAM_WEIGHTS: [u64; 17] = [5, 30, 60, 70, 55, 40, 30, 20, 15, 10, 40, 15, 8, 4, 6, 3, 1];

/// Top-query windows, in seconds.
const WINDOWS: &[(&str, i64)] = &[
    ("1h", 3_600),
    ("1d", 86_400),
    ("7d", 604_800),
    ("14d", 1_209_600),
    ("30d", 2_592_000),
];
const DEFAULT_WINDOW: &str = "7d";

/// Search terms in every cluster's top queries. The last four are hostile on
/// purpose, for the CLI's sanitizer: an ANSI escape, a carriage return, a
/// right-to-left override and a tab.
pub const QUERIES: &[&str] = &[
    "shoes",
    "red shoes",
    "running shoes",
    "boots",
    "sandals",
    "sneakers",
    "rain jacket",
    "winter coat",
    "socks",
    "t-shirt",
    "jeans",
    "dress",
    "backpack",
    "wallet",
    "watch",
    "sunglasses",
    "gloves",
    "scarf",
    "hoodie",
    "shorts",
    "swimsuit",
    "gift card",
    "size guide",
    "returns",
    "shoes\u{1b}[31mred",
    "price\rOVERWRITE",
    "abc\u{202e}cba",
    "tab\there",
];

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/v1/clusters/{cluster_id}/usage", get(usage))
        .route("/v1/clusters/{cluster_id}/metrics", get(metrics))
        .route("/v1/clusters/{cluster_id}/queries", get(queries))
}

// ---------------------------------------------------------------- shared

/// An RFC 3339 query parameter; absent or empty is `None`.
fn time_param(q: &QueryParams, field: &str) -> Result<Option<Timestamp>, ProblemBuilder> {
    q.get(field)
        .map(|v| {
            v.parse::<Timestamp>().map_err(|_| {
                invalid_parameter(
                    field,
                    format!("{field} must be an RFC 3339 date-time, e.g. 2026-10-14T15:00:00Z."),
                )
            })
        })
        .transpose()
}

/// `from` and `to`: `to` defaults to `now`, `from` to an hour before `to`, and
/// `from` must come first.
fn range(q: &QueryParams, now: Timestamp) -> Result<(Timestamp, Timestamp), ProblemBuilder> {
    let (from, to) = (time_param(q, "from")?, time_param(q, "to")?);
    let to = to.unwrap_or(now);
    let from = from.unwrap_or_else(|| to.saturating_sub(DEFAULT_RANGE).expect("a duration"));
    if from >= to {
        return Err(invalid_parameter("from", "from must be before to"));
    }
    Ok((from, to))
}

/// `t` cut to whole seconds, as the Bonsai API prints it.
fn whole_seconds(t: Timestamp) -> Timestamp {
    Timestamp::from_second(t.as_second()).expect("in range")
}

/// 503 `metrics_unavailable` while the scenario is set.
fn check_available(conn: &Connection) -> Result<(), ProblemBuilder> {
    let down = scenario::get(conn, "metrics_unavailable")?
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    if down {
        return Err(status_problem(
            StatusCode::SERVICE_UNAVAILABLE,
            ProblemCode::MetricsUnavailable,
            "The metrics store did not answer. Try again shortly.",
        )
        .retry_after(UNAVAILABLE_RETRY_AFTER));
    }
    Ok(())
}

/// FNV-1a: a stable hash of `s` (std's hashers may change between releases).
fn hash_str(s: &str) -> u64 {
    s.bytes().fold(0xcbf2_9ce4_8422_2325, |h, b| {
        (h ^ u64::from(b)).wrapping_mul(0x0100_0000_01b3)
    })
}

fn splitmix(x: u64) -> u64 {
    let mut z = x.wrapping_add(0x9e37_79b9_7f4a_7c15);
    z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
    z ^ (z >> 31)
}

/// Stable noise for `seed` (a cluster), `slot` (a time slot) and `salt` (a measure).
fn noise(seed: u64, slot: i64, salt: u64) -> u64 {
    splitmix(seed ^ splitmix((slot as u64) ^ splitmix(salt)))
}

fn round2(x: f64) -> f64 {
    (x * 100.0).round() / 100.0
}

// ---------------------------------------------------------------- usage

async fn usage(
    State(state): State<AppState>,
    auth: Auth,
    Path(id): Path<String>,
    q: QueryParams,
) -> Result<Json<Usage>, ProblemBuilder> {
    auth.require(Op::ReadClusters)?;
    let usage = state.db(|c| -> Result<Usage, ProblemBuilder> {
        let cluster = ClusterRow::find_current(c, &auth, &id, state.step_ms())?;
        let now = Timestamp::now();
        let (from, to) = range(&q, now)?;
        if to.duration_since(from) > USAGE_MAX_RANGE {
            return Err(invalid_parameter(
                "from",
                "The range can be at most 90 days",
            ));
        }
        check_available(c)?;
        Ok(Usage {
            from: whole_seconds(from),
            to: whole_seconds(to),
            samples: usage_samples(&cluster, from, to, now),
        })
    })?;
    Ok(Json(usage))
}

/// One sample per 10-minute slot from `max(from, created_at)` up to
/// `min(to, now)` (and termination), none if the cluster was never polled.
fn usage_samples(
    cluster: &ClusterRow,
    from: Timestamp,
    to: Timestamp,
    now: Timestamp,
) -> Vec<UsageSample> {
    if cluster.polled_at.is_none() {
        return Vec::new();
    }
    let start = from.max(cluster.created_at);
    let end = cluster
        .terminated_at
        .map_or(to.min(now), |t| t.min(to).min(now));
    let every_ms = USAGE_EVERY_SECS * 1_000;
    let first = (start.as_millisecond() + every_ms - 1).div_euclid(every_ms);
    let end_ms = end.as_millisecond();
    (first..)
        .take_while(|slot| slot * every_ms < end_ms)
        .map(|slot| sample(&cluster.id, cluster.created_at, slot))
        .collect()
}

/// The usage sample of the 10-minute slot holding `at`: what the cluster's
/// `stats` show once it's polled at `at`, so they agree with usage.
pub fn latest_sample(cluster_id: &str, created_at: Timestamp, at: Timestamp) -> UsageSample {
    let slot = at.as_millisecond().div_euclid(USAGE_EVERY_SECS * 1_000);
    sample(cluster_id, created_at, slot)
}

/// The sample of 10-minute slot number `slot` (from the epoch). Bytes and
/// docs grow from slot to slot from the cluster's creation; every 37th slot
/// is `yellow`.
fn sample(cluster_id: &str, created_at: Timestamp, slot: i64) -> UsageSample {
    let every_ms = USAGE_EVERY_SECS * 1_000;
    let born = created_at.as_millisecond().div_euclid(every_ms);
    let seed = hash_str(cluster_id);
    let base_bytes = 50_000_000 + seed % 200_000_000;
    let grow_bytes = 4_096 + seed % 16_384;
    let base_docs = 10_000 + seed % 50_000;
    let grow_docs = 5 + seed % 20;
    // Each step adds `grow` and a jitter under `grow`, so values only rise.
    let k = (slot - born).max(0) as u64;
    let docs = base_docs + k * grow_docs + noise(seed, slot, 2) % grow_docs;
    UsageSample {
        at: Timestamp::from_second(slot * USAGE_EVERY_SECS).expect("in range"),
        bytes: base_bytes + k * grow_bytes + noise(seed, slot, 1) % grow_bytes,
        docs,
        docs_deleted: docs / 40 + noise(seed, slot, 3) % 10,
        shards: 2,
        primary_shards: 1,
        health: if slot % 37 == 0 {
            Health::Yellow
        } else {
            Health::Green
        },
    }
}

// ---------------------------------------------------------------- metrics

async fn metrics(
    State(state): State<AppState>,
    auth: Auth,
    Path(id): Path<String>,
    q: QueryParams,
) -> Result<Json<Metrics>, ProblemBuilder> {
    auth.require(Op::ReadClusters)?;
    let metrics = state.db(|c| -> Result<Metrics, ProblemBuilder> {
        let cluster = ClusterRow::find_current(c, &auth, &id, state.step_ms())?;
        let now = Timestamp::now();
        let (from, to) = range(&q, now)?;
        if to.duration_since(from) > METRICS_MAX_RANGE {
            return Err(invalid_parameter(
                "from",
                "The range can be at most 30 days",
            ));
        }
        if from < oldest_from(now) {
            return Err(invalid_parameter("from", "from can be at most 30 days ago"));
        }
        let span = to.duration_since(from).as_secs_f64();
        let resolution = q.get("resolution").unwrap_or_else(|| auto_resolution(span));
        let Some(&(resolution, secs)) = RESOLUTIONS.iter().find(|(r, _)| *r == resolution) else {
            let names: Vec<&str> = RESOLUTIONS.iter().map(|(r, _)| *r).collect();
            return Err(invalid_parameter(
                "resolution",
                format!("resolution must be one of {}", names.join(", ")),
            ));
        };
        if span / secs as f64 > MAX_BUCKETS {
            return Err(invalid_parameter(
                "resolution",
                format!("The range and resolution give more than {MAX_BUCKETS} buckets"),
            ));
        }
        check_available(c)?;
        Ok(Metrics {
            from: whole_seconds(from),
            to: whole_seconds(to),
            resolution: resolution.to_string(),
            buckets: buckets(&cluster.id, from, to.min(now), secs),
        })
    })?;
    Ok(Json(metrics))
}

/// The start of the UTC day 30 days ago: the earliest `from`, so a client that
/// sends `now - 30 days` moments later isn't refused for the time in between.
fn oldest_from(now: Timestamp) -> Timestamp {
    let then = (now - METRICS_MAX_RANGE).as_second();
    Timestamp::from_second(then.div_euclid(86_400) * 86_400).expect("in range")
}

/// The smallest resolution giving fewer than 100 buckets over `span` seconds.
fn auto_resolution(span: f64) -> &'static str {
    RESOLUTIONS
        .iter()
        .find(|(_, secs)| span / (*secs as f64) < AUTO_BUCKETS)
        .map_or("1d", |(r, _)| r)
}

/// One bucket per `secs`-long slot, from the slot holding `from` up to `end`.
fn buckets(cluster_id: &str, from: Timestamp, end: Timestamp, secs: i64) -> Vec<MetricBucket> {
    let seed = hash_str(cluster_id) ^ splitmix(secs as u64);
    let size_ms = secs * 1_000;
    let end_ms = end.as_millisecond();
    (from.as_millisecond().div_euclid(size_ms)..)
        .take_while(|slot| slot * size_ms < end_ms)
        .map(|slot| bucket(seed, slot, secs))
        .collect()
}

/// The bucket for `slot`. Every 13th slot saw no requests, so its percentiles
/// are null.
fn bucket(seed: u64, slot: i64, secs: i64) -> MetricBucket {
    let at = Timestamp::from_second(slot * secs).expect("in range");
    let per_minute = 20 + noise(seed, slot, 1) % 180;
    let requests = if slot % 13 == 0 {
        0
    } else {
        per_minute * (secs as u64 / 60).max(1)
    };
    let share = |salt: u64, base: u64, spread: u64, per: u64| {
        requests * (base + noise(seed, slot, salt) % spread) / per
    };
    let status_5xx = share(3, 0, 4, 1_000);
    let status_4xx = share(4, 10, 30, 1_000);

    let weights: Vec<u64> = HISTOGRAM_WEIGHTS
        .iter()
        .enumerate()
        .map(|(i, w)| w * (4 + noise(seed, slot, 100 + i as u64) % 3))
        .collect();
    let total: u64 = weights.iter().sum();
    let mut counts: Vec<u64> = weights.iter().map(|w| requests * w / total).collect();
    counts[3] += requests - counts.iter().sum::<u64>();

    let p50 = 20.0 + (noise(seed, slot, 5) % 2_000) as f64 / 100.0;
    let p95 = p50 * (3.0 + (noise(seed, slot, 6) % 200) as f64 / 100.0);
    let p99 = p95 * (1.5 + (noise(seed, slot, 7) % 100) as f64 / 100.0);
    let percentile = |p: f64| (requests > 0).then(|| round2(p));
    MetricBucket {
        at,
        requests,
        writes: share(2, 5, 20, 100),
        responses: MetricResponses {
            status_2xx_3xx: requests - status_4xx - status_5xx,
            status_4xx,
            status_5xx,
        },
        duration_ms: DurationPercentiles {
            p50: percentile(p50),
            p95: percentile(p95),
            p99: percentile(p99),
        },
        duration_histogram: HISTOGRAM_MIN_MS
            .iter()
            .zip(counts)
            .map(|(&min_ms, count)| HistogramBucket { min_ms, count })
            .collect(),
        bytes_sent: share(8, 500, 3_000, 1),
        bytes_received: share(9, 100, 400, 1),
        queue_time_ms: if requests == 0 {
            0.0
        } else {
            round2((noise(seed, slot, 10) % 300) as f64 / 100.0)
        },
        concurrency: if requests == 0 {
            0.0
        } else {
            round2(per_minute as f64 / 60.0 * p50 / 1_000.0)
        },
    }
}

// ---------------------------------------------------------------- top queries

async fn queries(
    State(state): State<AppState>,
    auth: Auth,
    Path(id): Path<String>,
    q: QueryParams,
) -> Result<Json<TopQueries>, ProblemBuilder> {
    auth.require(Op::ReadQueryLogs)?;
    let top = state.db(|c| -> Result<TopQueries, ProblemBuilder> {
        let cluster = ClusterRow::find_current(c, &auth, &id, state.step_ms())?;
        let window = q.get("window").unwrap_or(DEFAULT_WINDOW);
        let Some(&(window, secs)) = WINDOWS.iter().find(|(w, _)| *w == window) else {
            let names: Vec<&str> = WINDOWS.iter().map(|(w, _)| *w).collect();
            return Err(invalid_parameter(
                "window",
                format!("window must be one of {}", names.join(", ")),
            ));
        };
        check_available(c)?;
        // Computed once a minute, ending when it was computed.
        let computed_at = Timestamp::from_second(Timestamp::now().as_second().div_euclid(60) * 60)
            .expect("in range");
        Ok(TopQueries {
            window: window.to_string(),
            window_start_at: computed_at - SignedDuration::from_secs(secs),
            window_end_at: computed_at,
            computed_at,
            queries: top_queries(&cluster.id, secs),
        })
    })?;
    Ok(Json(top))
}

/// Every term in [`QUERIES`], by volume, highest first. A longer window has
/// proportionally more volume.
fn top_queries(cluster_id: &str, window_secs: i64) -> Vec<TopQuery> {
    let seed = hash_str(cluster_id);
    let hours = (window_secs / 3_600) as u64;
    let mut out: Vec<TopQuery> = QUERIES
        .iter()
        .map(|query| {
            let term = hash_str(query);
            TopQuery {
                query: query.to_string(),
                volume: (1 + noise(seed, 0, term) % 400) * hours,
                avg_duration_ms: round2(5.0 + (noise(seed, 1, term) % 5_000) as f64 / 100.0),
                avg_hits: round2((noise(seed, 2, term) % 100_000) as f64 / 100.0),
            }
        })
        .collect();
    out.sort_by(|a, b| b.volume.cmp(&a.volume).then_with(|| a.query.cmp(&b.query)));
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn auto_resolution_gives_fewer_than_100_buckets() {
        let h = 3_600.0;
        assert_eq!(auto_resolution(h), "1m");
        assert_eq!(auto_resolution(24.0 * h), "30m");
        assert_eq!(auto_resolution(30.0 * 24.0 * h), "12h");
        assert_eq!(auto_resolution(1.0), "1m");
    }

    #[test]
    fn oldest_from_is_a_utc_midnight() {
        let now: Timestamp = "2026-10-20T17:30:00Z".parse().unwrap();
        assert_eq!(
            oldest_from(now),
            "2026-09-20T00:00:00Z".parse::<Timestamp>().unwrap()
        );
    }

    #[test]
    fn buckets_are_stable_and_consistent() {
        let seed = hash_str("c-1");
        for slot in 0..40 {
            let b = bucket(seed, slot, 60);
            assert_eq!(b, bucket(seed, slot, 60));
            let r = &b.responses;
            assert_eq!(r.status_2xx_3xx + r.status_4xx + r.status_5xx, b.requests);
            let counted: u64 = b.duration_histogram.iter().map(|h| h.count).sum();
            assert_eq!(counted, b.requests);
            assert_eq!(b.requests == 0, slot % 13 == 0);
            assert_eq!(b.duration_ms.p50.is_none(), b.requests == 0);
        }
    }

    #[test]
    fn every_query_is_listed() {
        let top = top_queries("c-1", 604_800);
        assert_eq!(top.len(), QUERIES.len());
        assert!(top.len() <= 100);
        assert!(top.windows(2).all(|w| w[0].volume >= w[1].volume));
    }
}
