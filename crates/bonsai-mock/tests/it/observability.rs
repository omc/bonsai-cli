//! Usage, metrics and top queries (`getClusterUsage`, `getClusterMetrics`,
//! `getClusterTopQueries`) and the `metrics_unavailable` scenario. Every call
//! goes through `Mock::call`, which validates the response against
//! `openapi/v1.yml`.

use crate::common::{Mock, header};
use bonsai_mock::{scenario, seed};
use jiff::{SignedDuration, Timestamp};
use serde_json::{Value, json};

const HISTOGRAM_MIN_MS: [u64; 17] = [
    0, 10, 20, 30, 40, 50, 60, 70, 80, 90, 100, 200, 300, 400, 500, 1000, 2000,
];

const HOSTILE: [&str; 4] = [
    "shoes\u{1b}[31mred",
    "price\rOVERWRITE",
    "abc\u{202e}cba",
    "tab\there",
];

fn ts(v: &Value) -> Timestamp {
    v.as_str().unwrap().parse().unwrap()
}

/// `t` as a query value, whole seconds, `Z`.
fn q(t: Timestamp) -> String {
    Timestamp::from_second(t.as_second()).unwrap().to_string()
}

/// A ready `standard-fir` cluster on `acme` that has existed for `days`.
async fn old_cluster(m: &Mock, days: i64) -> String {
    let id = m.ready_cluster("Observed", "standard-fir").await;
    let created = Timestamp::now() - SignedDuration::from_hours(24 * days);
    m.db()
        .execute(
            "UPDATE clusters SET created_at = ?2 WHERE id = ?1",
            (&id, created.to_string()),
        )
        .unwrap();
    id
}

async fn get(m: &Mock, path: &str) -> (u16, reqwest::header::HeaderMap, Value) {
    let (status, h, body) = m.call("GET", path, Some(&m.acme), None, &[]).await;
    (status, h, body.unwrap())
}

fn assert_bad(status: u16, problem: &Value, field: &str, message: &str) {
    assert_eq!(status, 400, "{problem}");
    assert_eq!(problem["code"], "invalid_parameter");
    assert_eq!(problem["detail"], message);
    assert_eq!(
        problem["details"],
        json!([{"field": field, "message": message}])
    );
}

// ---------------------------------------------------------------- usage

#[tokio::test]
async fn usage_defaults_to_the_last_hour() {
    let m = Mock::start().await;
    let id = old_cluster(&m, 2).await;
    let before = Timestamp::now();
    let (status, _, usage) = get(&m, &format!("/clusters/{id}/usage")).await;
    assert_eq!(status, 200);
    let (from, to) = (ts(&usage["from"]), ts(&usage["to"]));
    assert_eq!(to.duration_since(from), SignedDuration::from_hours(1));
    assert!(to >= Timestamp::from_second(before.as_second()).unwrap());

    let samples = usage["samples"].as_array().unwrap();
    assert_eq!(samples.len(), 6, "{usage}");
    for s in samples {
        let at = ts(&s["at"]);
        assert_eq!(at.as_second() % 600, 0, "every 10 minutes: {at}");
        assert!(at >= from && at <= to);
        assert_eq!(s["shards"], 2);
        assert_eq!(s["primary_shards"], 1);
        assert!(s["docs_deleted"].as_u64().unwrap() <= s["docs"].as_u64().unwrap());
    }
}

#[tokio::test]
async fn usage_is_deterministic_and_grows() {
    let m = Mock::start().await;
    let id = old_cluster(&m, 3).await;
    let to = Timestamp::now() - SignedDuration::from_hours(1);
    let from = to - SignedDuration::from_hours(24);
    let path = format!("/clusters/{id}/usage?from={}&to={}", q(from), q(to));
    let (_, _, first) = get(&m, &path).await;
    let (_, _, again) = get(&m, &path).await;
    assert_eq!(first, again);

    let samples = first["samples"].as_array().unwrap();
    assert_eq!(samples.len(), 144);
    let bytes: Vec<u64> = samples
        .iter()
        .map(|s| s["bytes"].as_u64().unwrap())
        .collect();
    assert!(bytes.windows(2).all(|w| w[0] < w[1]), "bytes grow");
    let yellow: Vec<i64> = samples
        .iter()
        .filter(|s| s["health"] == "yellow")
        .map(|s| ts(&s["at"]).as_second() / 600)
        .collect();
    assert!(!yellow.is_empty());
    assert!(yellow.iter().all(|n| n % 37 == 0), "{yellow:?}");
    assert!(
        samples
            .iter()
            .all(|s| s["health"] == "green" || s["health"] == "yellow")
    );
}

#[tokio::test]
async fn usage_starts_at_the_cluster_creation() {
    let m = Mock::start().await;
    let id = old_cluster(&m, 1).await;
    let to = Timestamp::now();
    let from = to - SignedDuration::from_hours(48);
    let (status, _, usage) = get(
        &m,
        &format!("/clusters/{id}/usage?from={}&to={}", q(from), q(to)),
    )
    .await;
    assert_eq!(status, 200);
    let created = ts(&m.cluster(&id).await["created_at"]);
    let first = ts(&usage["samples"][0]["at"]);
    assert!(first >= created);
    assert!(first.duration_since(created) < SignedDuration::from_mins(10));
    // The asked-for range is echoed, not the sampled one.
    assert_eq!(
        ts(&usage["from"]),
        Timestamp::from_second(from.as_second()).unwrap()
    );
}

#[tokio::test]
async fn a_cluster_never_polled_has_no_samples() {
    let m = Mock::start_with(60_000).await;
    let action = m.create("Slow", "standard-fir").await;
    let id = action["subject"]["id"].as_str().unwrap();
    let (status, _, usage) = get(&m, &format!("/clusters/{id}/usage")).await;
    assert_eq!(status, 200);
    assert_eq!(usage["samples"], json!([]));
}

#[tokio::test]
async fn usage_range_bounds() {
    let m = Mock::start().await;
    let id = old_cluster(&m, 1).await;
    let now = Timestamp::now();
    let path = |from: Timestamp, to: Timestamp| {
        format!("/clusters/{id}/usage?from={}&to={}", q(from), q(to))
    };

    let (status, _, p) = get(&m, &path(now - SignedDuration::from_hours(24 * 91), now)).await;
    assert_bad(status, &p, "from", "The range can be at most 90 days");
    let (status, _, _) = get(&m, &path(now - SignedDuration::from_hours(24 * 90), now)).await;
    assert_eq!(status, 200);

    let (status, _, p) = get(&m, &path(now, now - SignedDuration::from_hours(1))).await;
    assert_bad(status, &p, "from", "from must be before to");
    let (status, _, p) = get(&m, &path(now, now)).await;
    assert_bad(status, &p, "from", "from must be before to");

    for field in ["from", "to"] {
        let (status, _, p) = get(&m, &format!("/clusters/{id}/usage?{field}=yesterday")).await;
        assert_bad(
            status,
            &p,
            field,
            &format!("{field} must be an RFC 3339 date-time, e.g. 2026-10-14T15:00:00Z."),
        );
    }

    let (status, _, p) = get(&m, "/clusters/nope/usage").await;
    assert_eq!(status, 404);
    assert_eq!(p["code"], "not_found");
}

// ---------------------------------------------------------------- metrics

fn check_bucket(b: &Value) {
    let requests = b["requests"].as_u64().unwrap();
    let r = &b["responses"];
    let responses = ["status_2xx_3xx", "status_4xx", "status_5xx"]
        .iter()
        .map(|k| r[*k].as_u64().unwrap())
        .sum::<u64>();
    assert_eq!(responses, requests, "{b}");
    assert!(b["writes"].as_u64().unwrap() <= requests);
    let hist = b["duration_histogram"].as_array().unwrap();
    let mins: Vec<u64> = hist.iter().map(|h| h["min_ms"].as_u64().unwrap()).collect();
    assert_eq!(mins, HISTOGRAM_MIN_MS);
    let counted: u64 = hist.iter().map(|h| h["count"].as_u64().unwrap()).sum();
    assert_eq!(counted, requests, "{b}");
    let d = &b["duration_ms"];
    if requests == 0 {
        assert_eq!(d, &json!({"p50": null, "p95": null, "p99": null}));
    } else {
        let (p50, p95, p99) = (
            d["p50"].as_f64().unwrap(),
            d["p95"].as_f64().unwrap(),
            d["p99"].as_f64().unwrap(),
        );
        assert!(p50 <= p95 && p95 <= p99, "{d}");
    }
}

#[tokio::test]
async fn metrics_default_to_the_last_hour_by_the_minute() {
    let m = Mock::start().await;
    let id = old_cluster(&m, 1).await;
    let (status, _, metrics) = get(&m, &format!("/clusters/{id}/metrics")).await;
    assert_eq!(status, 200);
    assert_eq!(metrics["resolution"], "1m");
    let (from, to) = (ts(&metrics["from"]), ts(&metrics["to"]));
    assert_eq!(to.duration_since(from), SignedDuration::from_hours(1));
    let buckets = metrics["buckets"].as_array().unwrap();
    assert!((60..=61).contains(&buckets.len()), "{}", buckets.len());
    for b in buckets {
        assert_eq!(ts(&b["at"]).as_second() % 60, 0);
        check_bucket(b);
        let zero = (ts(&b["at"]).as_second() / 60) % 13 == 0;
        assert_eq!(b["requests"] == 0, zero, "{b}");
    }
    assert!(buckets.iter().any(|b| b["requests"] == 0));
}

#[tokio::test]
async fn metrics_pick_the_resolution() {
    let m = Mock::start().await;
    let id = old_cluster(&m, 1).await;
    let now = Timestamp::now();
    for (hours, resolution) in [
        (1, "1m"),
        (2, "30m"),
        (24, "30m"),
        (72, "1h"),
        (24 * 7, "6h"),
        (24 * 30, "12h"),
    ] {
        // 30 days less an hour, so `from` stays in bounds even if the
        // handler's clock has passed UTC midnight since `now` was taken.
        let margin = if hours == 24 * 30 { 1 } else { 0 };
        let from = now - SignedDuration::from_hours(hours - margin);
        let (status, _, metrics) = get(
            &m,
            &format!("/clusters/{id}/metrics?from={}&to={}", q(from), q(now)),
        )
        .await;
        assert_eq!(status, 200, "{hours} h");
        assert_eq!(metrics["resolution"], resolution, "{hours} h");
        let buckets = metrics["buckets"].as_array().unwrap();
        assert!(buckets.len() <= 101, "{hours} h: {}", buckets.len());
        buckets.iter().for_each(check_bucket);
    }

    // An explicit resolution is kept, and the series is stable.
    let from = now - SignedDuration::from_hours(6);
    let path = format!(
        "/clusters/{id}/metrics?from={}&to={}&resolution=1h",
        q(from),
        q(now)
    );
    let (_, _, first) = get(&m, &path).await;
    assert_eq!(first["resolution"], "1h");
    let (_, _, again) = get(&m, &path).await;
    assert_eq!(first, again);
}

#[tokio::test]
async fn metrics_bounds() {
    let m = Mock::start().await;
    let id = old_cluster(&m, 1).await;
    let now = Timestamp::now();
    let path = |from: Timestamp, to: Timestamp, extra: &str| {
        format!(
            "/clusters/{id}/metrics?from={}&to={}{extra}",
            q(from),
            q(to)
        )
    };
    let days = |n: i64| SignedDuration::from_hours(24 * n);

    let (status, _, p) = get(&m, &path(now - days(31), now, "")).await;
    assert_bad(status, &p, "from", "The range can be at most 30 days");

    let from = now - days(32);
    let (status, _, p) = get(&m, &path(from, from + days(1), "")).await;
    assert_bad(status, &p, "from", "from can be at most 30 days ago");

    // The start of the UTC day 30 days ago is still in bounds. The handler
    // reads the clock after the test does, so retry if UTC midnight passed
    // in between (the bound then moves a day).
    let start_of_day_30_days_ago = || {
        (Timestamp::now() - days(30))
            .to_zoned(jiff::tz::TimeZone::UTC)
            .start_of_day()
            .unwrap()
            .timestamp()
    };
    let day_start = loop {
        let day_start = start_of_day_30_days_ago();
        let (status, _, body) = get(&m, &path(day_start, day_start + days(1), "")).await;
        if start_of_day_30_days_ago() == day_start {
            assert_eq!(status, 200, "{body}");
            break day_start;
        }
    };
    let (status, _, p) = get(
        &m,
        &path(
            day_start - SignedDuration::from_secs(1),
            day_start + days(1),
            "",
        ),
    )
    .await;
    assert_bad(status, &p, "from", "from can be at most 30 days ago");

    let (status, _, p) = get(&m, &path(now, now - days(1), "")).await;
    assert_bad(status, &p, "from", "from must be before to");

    let (status, _, p) = get(&m, &path(now - days(1), now, "&resolution=2m")).await;
    assert_bad(
        status,
        &p,
        "resolution",
        "resolution must be one of 1m, 30m, 1h, 6h, 12h, 1d",
    );
    let (status, _, p) = get(&m, &path(now - days(1), now, "&resolution=1m")).await;
    assert_bad(
        status,
        &p,
        "resolution",
        "The range and resolution give more than 1000 buckets",
    );
    let (status, _, _) = get(
        &m,
        &path(now - SignedDuration::from_mins(1000), now, "&resolution=1m"),
    )
    .await;
    assert_eq!(status, 200);
}

// ---------------------------------------------------------------- top queries

#[tokio::test]
async fn top_queries() {
    let m = Mock::start().await;
    let id = old_cluster(&m, 1).await;
    let (status, _, top) = get(&m, &format!("/clusters/{id}/queries")).await;
    assert_eq!(status, 200);
    assert_eq!(top["window"], "7d");
    assert_eq!(
        ts(&top["window_end_at"]).duration_since(ts(&top["window_start_at"])),
        SignedDuration::from_hours(24 * 7)
    );
    let queries = top["queries"].as_array().unwrap();
    assert!(!queries.is_empty() && queries.len() <= 100);
    let strings: Vec<&str> = queries
        .iter()
        .map(|q| q["query"].as_str().unwrap())
        .collect();
    for hostile in HOSTILE {
        assert!(strings.contains(&hostile), "{hostile:?} missing");
    }
    let volumes: Vec<u64> = queries
        .iter()
        .map(|q| q["volume"].as_u64().unwrap())
        .collect();
    assert!(volumes.windows(2).all(|w| w[0] >= w[1]), "by volume");

    let (_, _, again) = get(&m, &format!("/clusters/{id}/queries")).await;
    assert_eq!(again["queries"], top["queries"]);

    let (status, _, day) = get(&m, &format!("/clusters/{id}/queries?window=1d")).await;
    assert_eq!(status, 200);
    assert_eq!(day["window"], "1d");

    let (status, _, p) = get(&m, &format!("/clusters/{id}/queries?window=2d")).await;
    assert_bad(
        status,
        &p,
        "window",
        "window must be one of 1h, 1d, 7d, 14d, 30d",
    );
}

#[tokio::test]
async fn top_queries_need_query_logs_read() {
    let m = Mock::start().await;
    let id = old_cluster(&m, 1).await;
    let token = seed::create_token(
        &m.db(),
        "acme",
        "no-logs",
        &["clusters:read", "credentials:read"],
        None,
    )
    .unwrap();
    let (status, _, problem) = m
        .call(
            "GET",
            &format!("/clusters/{id}/queries"),
            Some(&token),
            None,
            &[],
        )
        .await;
    assert_eq!(status, 403);
    let problem = problem.unwrap();
    assert_eq!(problem["code"], "missing_scope");
    assert_eq!(problem["details"][0]["scope"], "query_logs:read");

    // Usage and metrics need only clusters:read.
    for what in ["usage", "metrics"] {
        let (status, _, _) = m
            .call(
                "GET",
                &format!("/clusters/{id}/{what}"),
                Some(&token),
                None,
                &[],
            )
            .await;
        assert_eq!(status, 200, "{what}");
    }
}

// ---------------------------------------------------------------- scenario

#[tokio::test]
async fn metrics_unavailable_scenario() {
    let m = Mock::start().await;
    let id = old_cluster(&m, 1).await;
    scenario::set(&m.db(), "metrics_unavailable", &json!(true)).unwrap();
    for what in ["usage", "metrics", "queries"] {
        let (status, h, problem) = get(&m, &format!("/clusters/{id}/{what}")).await;
        assert_eq!(status, 503, "{what}");
        assert_eq!(problem["code"], "metrics_unavailable");
        assert_eq!(
            header(&h, "retry-after"),
            Some(bonsai_mock::routes::observability::UNAVAILABLE_RETRY_AFTER)
                .map(|s| s.to_string())
                .as_deref()
        );
    }
    // A bad parameter is still a 400: it's refused before the store is asked.
    let (status, _, _) = get(&m, &format!("/clusters/{id}/queries?window=2d")).await;
    assert_eq!(status, 400);

    scenario::set(&m.db(), "metrics_unavailable", &json!(false)).unwrap();
    let (status, _, _) = get(&m, &format!("/clusters/{id}/usage")).await;
    assert_eq!(status, 200);
    scenario::set(&m.db(), "metrics_unavailable", &json!(true)).unwrap();
    scenario::clear(&m.db(), "metrics_unavailable").unwrap();
    let (status, _, _) = get(&m, &format!("/clusters/{id}/metrics")).await;
    assert_eq!(status, 200);
}

#[tokio::test]
async fn cluster_stats_match_the_latest_usage_sample() {
    // Slow enough to backdate the cluster before its provision finishes, so
    // usage has samples from before it was provisioned.
    let m = Mock::start_with(50).await;
    let action = m.create("Stats", "standard-fir").await;
    let id = action["subject"]["id"].as_str().unwrap().to_string();
    let created = Timestamp::now() - SignedDuration::from_hours(24);
    m.db()
        .execute(
            "UPDATE clusters SET created_at = ?2 WHERE id = ?1",
            (&id, created.to_string()),
        )
        .unwrap();
    for _ in 0..100 {
        if m.cluster(&id).await["state"] == "ready" {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(25)).await;
    }
    let cluster = m.cluster(&id).await;
    assert_eq!(cluster["state"], "ready");
    let stats = &cluster["stats"];
    let polled = ts(&stats["polled_at"]);

    // The sample of the 10-minute slot the provision finished in.
    let slot_start = Timestamp::from_second(polled.as_second().div_euclid(600) * 600).unwrap();
    let (status, _, usage) = get(
        &m,
        &format!(
            "/clusters/{id}/usage?from={}&to={}",
            q(slot_start),
            q(slot_start + SignedDuration::from_secs(1))
        ),
    )
    .await;
    assert_eq!(status, 200);
    let samples = usage["samples"].as_array().unwrap();
    assert_eq!(samples.len(), 1, "{usage}");
    let sample = &samples[0];
    assert_eq!(stats["docs"], sample["docs"]);
    assert_eq!(stats["shards"], sample["shards"]);
    assert_eq!(stats["bytes"], sample["bytes"]);
    assert!(stats["bytes"].as_u64().unwrap() > 1_000_000);
}
