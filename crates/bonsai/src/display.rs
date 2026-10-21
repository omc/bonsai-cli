//! Formatting shared by the commands' human output, so the same value reads the
//! same everywhere: counts, prices, usage against a plan's limits, and times.

use bonsai_api::{ClusterStats, Plan};
use jiff::Timestamp;

use crate::output::{human_bytes, relative_time};

/// `1234567` as `1,234,567`.
pub fn group(n: u64) -> String {
    let digits = n.to_string();
    let mut out = String::with_capacity(digits.len() + digits.len() / 3);
    for (i, c) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(c);
    }
    out
}

/// `$50/mo`, `$12.50/mo`, `$5,000/mo`, `$500/yr`, `$90/3 mo`, or `Free`.
pub fn price(cents: u64, months: u32) -> String {
    if cents == 0 {
        return "Free".into();
    }
    let amount = if cents.is_multiple_of(100) {
        format!("${}", group(cents / 100))
    } else {
        format!("${}.{:02}", group(cents / 100), cents % 100)
    };
    match months {
        1 => format!("{amount}/mo"),
        12 => format!("{amount}/yr"),
        n => format!("{amount}/{n} mo"),
    }
}

/// [`price`] of a plan.
pub fn plan_price(plan: &Plan) -> String {
    price(plan.price_in_cents, plan.billing_interval_months)
}

/// `used / limit (NN%)`, `used (no limit)`, or `unknown` before the API has polled
/// the cluster. The percentage is rounded down, so 100% means at the limit.
pub fn against_limit(used: Option<u64>, limit: Option<u64>, fmt: fn(u64) -> String) -> String {
    match (used, limit) {
        (None, _) => "unknown".into(),
        (Some(u), None) => format!("{} (no limit)", fmt(u)),
        (Some(u), Some(0)) => format!("{} / {}", fmt(u), fmt(0)),
        (Some(u), Some(l)) => {
            let pct = u128::from(u) * 100 / u128::from(l);
            format!("{} / {} ({pct}%)", fmt(u), fmt(l))
        }
    }
}

/// The `Docs`, `Shards` and `Size` rows of `cluster show` and `usage`.
pub fn usage_rows(stats: &ClusterStats) -> [(&'static str, String); 3] {
    let limits = stats.limits.as_ref();
    [
        (
            "Docs",
            against_limit(stats.docs, limits.and_then(|l| l.docs), group),
        ),
        (
            "Shards",
            against_limit(stats.shards, limits.and_then(|l| l.shards), group),
        ),
        (
            "Size",
            against_limit(stats.bytes, limits.and_then(|l| l.bytes), human_bytes),
        ),
    ]
}

/// `2026-10-20 12:00:00 UTC (3h ago)`: whole seconds, then relative to `now`.
pub fn when(t: Timestamp, now: Timestamp) -> String {
    format!(
        "{} ({})",
        t.strftime("%Y-%m-%d %H:%M:%S UTC"),
        relative_time(t, now)
    )
}

/// A time in a table: compact (`6m ago`), or [`when`] under `--wide`.
pub fn table_time(t: Timestamp, now: Timestamp, wide: bool) -> String {
    if wide {
        when(t, now)
    } else {
        relative_time(t, now)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn groups_thousands() {
        assert_eq!(group(0), "0");
        assert_eq!(group(999), "999");
        assert_eq!(group(1000), "1,000");
        assert_eq!(group(1_234_567), "1,234,567");
    }

    #[test]
    fn prices() {
        assert_eq!(price(0, 1), "Free");
        assert_eq!(price(5000, 1), "$50/mo");
        assert_eq!(price(1250, 1), "$12.50/mo");
        assert_eq!(price(500_000, 1), "$5,000/mo");
        assert_eq!(price(50_000, 12), "$500/yr");
        assert_eq!(price(9000, 3), "$90/3 mo");
    }

    #[test]
    fn usage_against_limits() {
        assert_eq!(against_limit(Some(25), Some(100), group), "25 / 100 (25%)");
        assert_eq!(
            against_limit(Some(2500), Some(10_000), group),
            "2,500 / 10,000 (25%)"
        );
        assert_eq!(
            against_limit(Some(1536), Some(4096), human_bytes),
            "1.5 KiB / 4.0 KiB (37%)"
        );
        assert_eq!(against_limit(Some(5), None, group), "5 (no limit)");
        assert_eq!(against_limit(None, Some(5), group), "unknown");
        assert_eq!(against_limit(Some(3), Some(0), group), "3 / 0");
        assert_eq!(
            against_limit(Some(u64::MAX), Some(u64::MAX), |n| n.to_string()),
            format!("{0} / {0} (100%)", u64::MAX)
        );
    }

    #[test]
    fn times_are_utc_seconds_then_relative() {
        let now: Timestamp = "2026-10-20T12:00:00Z".parse().unwrap();
        let t: Timestamp = "2026-10-20T09:00:00.123456789Z".parse().unwrap();
        assert_eq!(when(t, now), "2026-10-20 09:00:00 UTC (3h ago)");
        assert_eq!(table_time(t, now, false), "3h ago");
        assert_eq!(table_time(t, now, true), when(t, now));
    }
}
