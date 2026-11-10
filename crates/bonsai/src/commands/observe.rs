//! `bonsai cluster usage`, `cluster metrics` and `cluster queries`.
//!
//! `503 metrics_unavailable` comes straight back from the client (it isn't retried)
//! and is told as "metrics are unavailable right now, try again in N s", exit 1;
//! never as an empty graph.

use std::time::Duration;

use anyhow::Result;
use bonsai_api::{ApiError, Cluster, Health, MetricBucket, ProblemCode, TopQuery};
use bonsai_core::{CoreError, on_cluster, resolve_cluster};
use jiff::{SignedDuration, Timestamp};
use serde_json::Value;

use super::Ctx;
use crate::cli::{MetricsArgs, QueriesArgs, UsageArgs};
use crate::display::{group, usage_rows};
use crate::exit::Explained;
use crate::output::{Column, human_bytes, relative_time};
use crate::sanitize::sanitize_line;

const DAY: u64 = 86_400;

/// How many search terms human output shows without `--top`.
const DEFAULT_TOP: usize = 20;

/// `queries`: the term is truncated, never dropped.
const QUERY_COLUMNS: [Column; 4] = [
    Column::text_kept("QUERY", 20),
    Column::fixed("VOLUME", 4),
    Column::fixed("AVG TIME", 3),
    Column::fixed("AVG HITS", 2),
];

pub async fn usage(ctx: &Ctx, args: UsageArgs) -> Result<()> {
    let (from, to) = range(args.since, 90)?;
    ctx.abortable(async {
        let a = ctx.authed().await?;
        let client = &a.client;
        let arg = &args.cluster.cluster;
        if ctx.out.is_json() {
            let raw = on_cluster(client, arg, |id| async move {
                Ok(client
                    .get_cluster_usage(&id, Some(from), Some(to))
                    .await?
                    .raw)
            })
            .await
            .map_err(|e| explain_core(e, "clusters:read"))?;
            return ctx.out.json(&raw);
        }
        // Human output shows the cluster's current stats against its plan's limits,
        // which only the cluster has.
        let cluster = resolve_cluster(client, arg).await?;
        let usage = client
            .get_cluster_usage(&cluster.value.id, Some(from), Some(to))
            .await
            .map_err(|e| explain(e, "clusters:read"))?;
        let c = &cluster.value;
        let stats = &c.stats;
        let range = span(args.since);
        let mut rows = cluster_rows(c);
        rows.extend(usage_rows(stats));
        if let Some(t) = stats.polled_at {
            rows.push(("Polled", relative_time(t, Timestamp::now())));
        }
        let samples = &usage.value.samples;
        match worst_health(samples.iter().map(|s| &s.health)) {
            None => rows.push(("Health", format!("no samples in the last {range}"))),
            Some(h) => {
                rows.push(("Health", format!("{h} (worst in the last {range})")));
                let bytes: Vec<u64> = samples.iter().map(|s| s.bytes).collect();
                let docs: Vec<u64> = samples.iter().map(|s| s.docs).collect();
                rows.push(("Size trend", trend(&bytes, human_bytes)));
                rows.push(("Docs trend", trend(&docs, group)));
            }
        }
        ctx.out.kv(&rows)
    })
    .await
}

pub async fn metrics(ctx: &Ctx, args: MetricsArgs) -> Result<()> {
    let (from, to) = range(args.since, 30)?;
    ctx.abortable(async {
        let a = ctx.authed().await?;
        let client = &a.client;
        let arg = &args.cluster.cluster;
        let resolution = args.resolution.as_deref();
        if ctx.out.is_json() {
            let raw = on_cluster(client, arg, |id| async move {
                Ok(client
                    .get_cluster_metrics(&id, Some(from), Some(to), resolution)
                    .await?
                    .raw)
            })
            .await
            .map_err(|e| explain_core(e, "clusters:read"))?;
            return ctx.out.json(&raw);
        }
        let cluster = resolve_cluster(client, arg).await?;
        let metrics = client
            .get_cluster_metrics(&cluster.value.id, Some(from), Some(to), resolution)
            .await
            .map_err(|e| explain(e, "clusters:read"))?;
        let m = &metrics.value;
        let mut rows = cluster_rows(&cluster.value);
        rows.push((
            "Range",
            format!("last {}, {} buckets", span(args.since), m.resolution),
        ));
        if m.buckets.is_empty() {
            rows.push(("Requests", "none in this range".into()));
        } else {
            rows.extend(metric_rows(&m.buckets, &m.resolution));
        }
        ctx.out.kv(&rows)
    })
    .await
}

pub async fn queries(ctx: &Ctx, args: QueriesArgs) -> Result<()> {
    ctx.abortable(async {
        let a = ctx.authed().await?;
        let client = &a.client;
        let arg = &args.cluster.cluster;
        let window = args.window.as_str();
        if ctx.out.is_json() {
            let mut raw = on_cluster(client, arg, |id| async move {
                Ok(client.get_cluster_top_queries(&id, Some(window)).await?.raw)
            })
            .await
            .map_err(|e| explain_core(e, "query_logs:read"))?;
            // As the server sent it, unless --top asked for fewer.
            if let Some(top) = args.top
                && let Some(Value::Array(q)) = raw.get_mut("queries")
            {
                q.truncate(top);
            }
            return ctx.out.json(&raw);
        }
        let cluster = resolve_cluster(client, arg).await?;
        let top = client
            .get_cluster_top_queries(&cluster.value.id, Some(window))
            .await
            .map_err(|e| explain(e, "query_logs:read"))?;
        let t = &top.value;
        let limit = args.top.unwrap_or(DEFAULT_TOP);
        let shown = t.queries.len().min(limit);
        ctx.out.info(&format!(
            "Top {shown} of {} search terms on {} (ID {}) over {}, computed {}",
            t.queries.len(),
            cluster.value.name,
            cluster.value.id,
            t.window,
            relative_time(t.computed_at, Timestamp::now()),
        ));
        if shown == 0 {
            return Ok(());
        }
        let rows = t.queries.iter().take(limit).map(query_row).collect();
        ctx.out.table(&QUERY_COLUMNS, rows, args.wide)
    })
    .await
}

/// One `queries` row. An end user's search term keeps no newline or tab either
/// (each becomes `�`), so a term can't shift the columns or pass for another one.
fn query_row(q: &TopQuery) -> Vec<String> {
    vec![
        sanitize_line(&q.query).into_owned(),
        group(q.volume),
        format!("{:.1} ms", q.avg_duration_ms),
        format!("{:.1}", q.avg_hits),
    ]
}

/// `(now - since, now)`, refusing more than `max_days` (the API's own bound) with
/// exit 2. Both ends are sent: a server that defaulted `to` to its own, later `now`
/// would find `--since 90d` over its 90-day bound.
pub(super) fn range(since: Duration, max_days: u64) -> Result<(Timestamp, Timestamp)> {
    if since > Duration::from_secs(max_days * DAY) {
        return Err(CoreError::Usage(format!("--since can be at most {max_days} days")).into());
    }
    if since.is_zero() {
        return Err(CoreError::Usage("--since must be more than 0".into()).into());
    }
    let since = SignedDuration::try_from(since)?;
    let now = Timestamp::now();
    Ok((now.checked_sub(since)?, now))
}

/// The short messages `usage`, `metrics` and `queries` print for these errors, with the
/// Problem kept as the cause: its exit code (1 or 3) and, under `--json`, its body.
/// [`explain`] for an error from [`on_cluster`]: the API's, explained; the rest as
/// they are.
pub(super) fn explain_core(e: CoreError, scope: &str) -> anyhow::Error {
    match e {
        CoreError::Api(e) => explain(e, scope),
        other => other.into(),
    }
}

pub(super) fn explain(e: ApiError, scope: &str) -> anyhow::Error {
    let msg = match e.code() {
        Some(ProblemCode::MetricsUnavailable) => {
            let when = match &e {
                ApiError::Problem {
                    retry_after: Some(d),
                    ..
                } => format!("try again in {} s", d.as_secs().max(1)),
                _ => "try again shortly".to_owned(),
            };
            format!("metrics are unavailable right now, {when}")
        }
        Some(ProblemCode::MissingScope) => {
            let scope = match &e {
                ApiError::Problem { problem, .. } => problem
                    .details
                    .iter()
                    .find_map(|d| d.scope.clone())
                    .unwrap_or_else(|| scope.to_owned()),
                _ => scope.to_owned(),
            };
            format!("this token lacks {scope}; create a token with it in the dashboard")
        }
        _ => return e.into(),
    };
    anyhow::Error::new(e).context(Explained(msg))
}

/// The cluster's name and ID, each labelled: `ID` as the API calls it.
fn cluster_rows(c: &Cluster) -> Vec<(&'static str, String)> {
    vec![("Cluster", c.name.clone()), ("ID", c.id.clone())]
}

/// `30d`, `12h`, `90m` or `45s`: the largest unit that divides `d`.
fn span(d: Duration) -> String {
    let s = d.as_secs();
    match s {
        _ if s >= DAY && s.is_multiple_of(DAY) => format!("{}d", s / DAY),
        _ if s >= 3600 && s.is_multiple_of(3600) => format!("{}h", s / 3600),
        _ if s >= 60 && s.is_multiple_of(60) => format!("{}m", s / 60),
        _ => format!("{s}s"),
    }
}

/// Red, then yellow, then a health this CLI doesn't know, then green.
fn worst_health<'a>(seen: impl Iterator<Item = &'a Health>) -> Option<&'a Health> {
    let rank = |h: &Health| match h {
        Health::Green => 0,
        Health::Yellow => 2,
        Health::Red => 3,
        _ => 1,
    };
    seen.max_by_key(|h| rank(h))
}

/// `1.0 MiB → 2.0 MiB`: the first and last value over the range.
fn trend(values: &[u64], fmt: fn(u64) -> String) -> String {
    match (values.first(), values.last()) {
        (Some(&a), Some(&b)) => format!("{} → {}", fmt(a), fmt(b)),
        _ => String::new(),
    }
}

/// Totals for each measure. A rate is over the range's requests. Percentiles can't
/// be averaged across buckets, so a latency is the median bucket's ("typical") and
/// the slowest bucket's, named by its size (`resolution`, e.g. `1m`).
fn metric_rows(buckets: &[MetricBucket], resolution: &str) -> Vec<(&'static str, String)> {
    let sum = |f: fn(&MetricBucket) -> u64| buckets.iter().map(f).sum::<u64>();
    let requests = sum(|b| b.requests);
    let rate = |label: &'static str, f: fn(&MetricBucket) -> u64| {
        let shown = if requests == 0 {
            "-".to_owned()
        } else {
            format!("{:.1}%", sum(f) as f64 / requests as f64 * 100.0)
        };
        (label, shown)
    };
    let latency = |label: &'static str, f: fn(&MetricBucket) -> Option<f64>| {
        let mut seen: Vec<f64> = buckets
            .iter()
            .filter_map(f)
            .filter(|v| v.is_finite())
            .collect();
        let shown = match seen.len() {
            0 => "-".to_owned(),
            1 => format!("{:.1} ms", seen[0]),
            _ => {
                let slowest = seen.iter().copied().fold(f64::MIN, f64::max);
                let typical = median(&mut seen);
                format!("{typical:.1} ms typical, {slowest:.1} ms in the slowest {resolution}")
            }
        };
        (label, shown)
    };
    vec![
        ("Requests", group(requests)),
        ("Writes", group(sum(|b| b.writes))),
        rate("4xx", |b| b.responses.status_4xx),
        rate("5xx", |b| b.responses.status_5xx),
        ("Bytes in", human_bytes(sum(|b| b.bytes_received))),
        ("Bytes out", human_bytes(sum(|b| b.bytes_sent))),
        latency("p50", |b| b.duration_ms.p50),
        latency("p95", |b| b.duration_ms.p95),
        latency("p99", |b| b.duration_ms.p99),
    ]
}

/// The middle value, or the mean of the middle two. `values` must be finite and
/// not empty; it's left sorted.
fn median(values: &mut [f64]) -> f64 {
    values.sort_by(f64::total_cmp);
    let mid = values.len() / 2;
    if values.len().is_multiple_of(2) {
        (values[mid - 1] + values[mid]) / 2.0
    } else {
        values[mid]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_query_with_a_newline_doesnt_read_as_a_space() {
        let q = |s: &str| TopQuery {
            query: s.into(),
            volume: 1,
            avg_duration_ms: 1.0,
            avg_hits: 1.0,
        };
        let a = query_row(&q("red\nshoes"));
        let b = query_row(&q("red shoes"));
        assert_eq!(a[0], "red\u{FFFD}shoes");
        assert_ne!(a[0], b[0]);
        assert_eq!(crate::output::cell(&a[0]), a[0]);
    }

    #[test]
    fn spans() {
        assert_eq!(span(Duration::from_secs(7 * DAY)), "7d");
        assert_eq!(span(Duration::from_secs(3600)), "1h");
        assert_eq!(span(Duration::from_secs(5400)), "90m");
        assert_eq!(span(Duration::from_secs(45)), "45s");
    }

    #[test]
    fn the_worst_health_wins() {
        let seen = [Health::Green, Health::Red, Health::Yellow];
        assert_eq!(worst_health(seen.iter()), Some(&Health::Red));
        let seen = [Health::Green, Health::Unknown("blue".into())];
        assert_eq!(
            worst_health(seen.iter()),
            Some(&Health::Unknown("blue".into()))
        );
        assert_eq!(worst_health([].iter()), None);
    }

    #[test]
    fn trends_show_first_and_last() {
        assert_eq!(trend(&[1024, 2048, 4096], human_bytes), "1.0 KiB → 4.0 KiB");
        assert_eq!(trend(&[], group), "");
    }

    #[test]
    fn since_is_bounded() {
        let (from, to) = range(Duration::from_secs(90 * DAY), 90).unwrap();
        assert_eq!(to.duration_since(from), SignedDuration::from_hours(24 * 90));
        assert!(range(Duration::from_secs(91 * DAY), 90).is_err());
        assert!(range(Duration::ZERO, 90).is_err());
    }

    fn bucket(requests: u64, s5xx: u64, p95: Option<f64>) -> MetricBucket {
        serde_json::from_value(serde_json::json!({
            "at": "2026-10-20T12:00:00Z",
            "requests": requests, "writes": 0,
            "responses": {"status_2xx_3xx": requests - s5xx, "status_4xx": 0, "status_5xx": s5xx},
            "duration_ms": {"p50": p95.map(|v| v / 2.0), "p95": p95, "p99": p95.map(|v| v * 2.0)},
            "duration_histogram": [],
            "bytes_sent": 0, "bytes_received": 0, "queue_time_ms": 0.0, "concurrency": 0.0
        }))
        .unwrap()
    }

    #[test]
    fn metrics_show_typical_and_slowest_latency_and_overall_rates() {
        let rows = metric_rows(
            &[
                bucket(100, 0, Some(10.0)),
                bucket(100, 10, Some(90.0)),
                bucket(100, 0, Some(20.0)),
                bucket(0, 0, None),
            ],
            "1m",
        );
        let get = |k: &str| rows.iter().find(|(l, _)| *l == k).unwrap().1.clone();
        assert_eq!(get("Requests"), "300");
        // 10 of 300 requests, not the mean of the buckets' rates.
        assert_eq!(get("5xx"), "3.3%");
        // The median bucket and the slowest; the empty bucket is no zero.
        assert_eq!(get("p95"), "20.0 ms typical, 90.0 ms in the slowest 1m");
        assert_eq!(get("p99"), "40.0 ms typical, 180.0 ms in the slowest 1m");
        let all: String = rows.iter().flat_map(|(k, v)| [*k, v.as_str()]).collect();
        assert!(
            !all.chars().any(|c| ('\u{2581}'..='\u{2588}').contains(&c)),
            "{all}"
        );
    }

    #[test]
    fn one_bucket_or_none_shows_one_latency() {
        let rows = metric_rows(&[bucket(5, 0, Some(12.0))], "1h");
        let get = |k: &str| rows.iter().find(|(l, _)| *l == k).unwrap().1.clone();
        assert_eq!(get("p95"), "12.0 ms");
        let rows = metric_rows(&[bucket(0, 0, None)], "1h");
        let get = |k: &str| rows.iter().find(|(l, _)| *l == k).unwrap().1.clone();
        assert_eq!(get("p95"), "-");
    }

    #[test]
    fn the_median_of_an_even_count_is_the_middle_pair_s_mean() {
        assert_eq!(median(&mut [4.0, 1.0, 3.0, 2.0]), 2.5);
        assert_eq!(median(&mut [3.0, 1.0, 2.0]), 2.0);
    }
}
