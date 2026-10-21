//! Scenario flags: test knobs stored in the `scenario` table, so the server,
//! the admin API and the `bonsai-mock scenario` CLI all see the same values.
//!
//! Known keys: `fail_next` (`"payment_failed"|"action_failed"|"timed_out"`),
//! `metrics_unavailable` (bool), `rate_limit_next` (int).
//!
//! `fail_next` is taken once: `payment_failed` by the next `createCluster` that
//! passes validation (a `422 payment_failed`, nothing built), the other two by
//! the next Action started, which then errors at its second step.
//!
//! `metrics_unavailable`, while `true`, makes usage, metrics and top queries
//! answer `503 metrics_unavailable` with `Retry-After`, once their parameters
//! have been checked. It stays set until cleared.
//!
//! `rate_limit_next = n` makes the next `n` requests to a `/v1` route answer
//! `429 rate_limited` with `Retry-After: 1`. It fires in the rate-limit layer,
//! before the token is checked, so every request that matches a `/v1` route
//! takes one, whatever its token and method: the root, the proposed login and
//! token endpoints, and a wrong method (which would otherwise be a 404). An
//! unknown `/v1` path, `/v1/openapi.json` and everything under `/_mock` don't.

use std::collections::BTreeMap;

use rusqlite::{Connection, OptionalExtension, TransactionBehavior};
use serde_json::Value;

/// Every scenario key, as the admin API and `bonsai-mock scenario` take them.
pub const KEYS: &[&str] = &["fail_next", "metrics_unavailable", "rate_limit_next"];

/// The values `fail_next` takes.
pub const FAIL_NEXT: &[&str] = &["payment_failed", "action_failed", "timed_out"];

/// Checks that `key` is a scenario and `value` one it takes, or says why not.
pub fn validate(key: &str, value: &Value) -> Result<(), String> {
    let ok = match key {
        "fail_next" => value.as_str().is_some_and(|v| FAIL_NEXT.contains(&v)),
        "metrics_unavailable" => value.is_boolean(),
        "rate_limit_next" => value.is_u64(),
        _ => {
            return Err(format!(
                "unknown scenario {key}; scenarios are: {}",
                KEYS.join(", ")
            ));
        }
    };
    if ok {
        return Ok(());
    }
    Err(match key {
        "fail_next" => format!("fail_next must be one of {}", FAIL_NEXT.join(", ")),
        "metrics_unavailable" => "metrics_unavailable must be true or false".into(),
        _ => "rate_limit_next must be a whole number, 0 or more".into(),
    })
}

/// Every key that is set, with its value.
pub fn all(conn: &Connection) -> rusqlite::Result<BTreeMap<String, Value>> {
    conn.prepare("SELECT key FROM scenario ORDER BY key")?
        .query_map([], |r| r.get::<_, String>(0))?
        .collect::<rusqlite::Result<Vec<_>>>()?
        .into_iter()
        .filter_map(|k| get(conn, &k).transpose().map(|v| v.map(|v| (k, v))))
        .collect()
}

/// The value set for `key`, if any.
pub fn get(conn: &Connection, key: &str) -> rusqlite::Result<Option<Value>> {
    let raw: Option<String> = conn
        .query_row("SELECT value FROM scenario WHERE key = ?1", [key], |r| {
            r.get(0)
        })
        .optional()?;
    raw.map(|s| {
        serde_json::from_str(&s).map_err(|e| {
            rusqlite::Error::FromSqlConversionFailure(0, rusqlite::types::Type::Text, Box::new(e))
        })
    })
    .transpose()
}

/// Sets `key` to `value`, replacing any earlier value.
pub fn set(conn: &Connection, key: &str, value: &Value) -> rusqlite::Result<()> {
    conn.execute(
        "INSERT INTO scenario (key, value) VALUES (?1, ?2)
         ON CONFLICT (key) DO UPDATE SET value = excluded.value",
        (key, value.to_string()),
    )?;
    Ok(())
}

/// Removes `key`. Clearing a key that isn't set is not an error.
pub fn clear(conn: &Connection, key: &str) -> rusqlite::Result<()> {
    conn.execute("DELETE FROM scenario WHERE key = ?1", [key])?;
    Ok(())
}

/// Consumes one unit of a counter scenario such as `rate_limit_next`: when
/// `key` holds a positive integer, decrements it (removing the key at zero) and
/// returns `true`. Otherwise returns `false` and changes nothing.
pub fn take_counter(conn: &Connection, key: &str) -> rusqlite::Result<bool> {
    let tx = rusqlite::Transaction::new_unchecked(conn, TransactionBehavior::Immediate)?;
    let n = get(&tx, key)?.and_then(|v| v.as_u64()).unwrap_or(0);
    if n == 0 {
        return Ok(false);
    }
    if n == 1 {
        clear(&tx, key)?;
    } else {
        set(&tx, key, &Value::from(n - 1))?;
    }
    tx.commit()?;
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn conn() -> (tempfile::TempDir, Connection) {
        let dir = tempfile::tempdir().unwrap();
        let c = crate::db::open(&dir.path().join("s.sqlite")).unwrap();
        (dir, c)
    }

    #[test]
    fn set_get_clear() {
        let (_d, c) = conn();
        assert_eq!(get(&c, "fail_next").unwrap(), None);
        set(&c, "fail_next", &json!("payment_failed")).unwrap();
        assert_eq!(get(&c, "fail_next").unwrap(), Some(json!("payment_failed")));
        set(&c, "fail_next", &json!("timed_out")).unwrap();
        assert_eq!(get(&c, "fail_next").unwrap(), Some(json!("timed_out")));
        clear(&c, "fail_next").unwrap();
        assert_eq!(get(&c, "fail_next").unwrap(), None);
        clear(&c, "fail_next").unwrap();
    }

    #[test]
    fn validates_keys_and_values() {
        assert!(validate("fail_next", &json!("action_failed")).is_ok());
        assert!(validate("fail_next", &json!("nope")).is_err());
        assert!(validate("metrics_unavailable", &json!(false)).is_ok());
        assert!(validate("metrics_unavailable", &json!(0)).is_err());
        assert!(validate("rate_limit_next", &json!(0)).is_ok());
        assert!(validate("rate_limit_next", &json!(-1)).is_err());
        assert!(validate("rate_limit_next", &json!(1.5)).is_err());
        assert!(validate("other", &json!(1)).is_err());
    }

    #[test]
    fn all_lists_every_key() {
        let (_d, c) = conn();
        assert!(all(&c).unwrap().is_empty());
        set(&c, "rate_limit_next", &json!(2)).unwrap();
        set(&c, "fail_next", &json!("timed_out")).unwrap();
        let every = all(&c).unwrap();
        assert_eq!(
            every.into_iter().collect::<Vec<_>>(),
            [
                ("fail_next".to_string(), json!("timed_out")),
                ("rate_limit_next".to_string(), json!(2))
            ]
        );
    }

    #[test]
    fn take_counter_counts_down_and_clears() {
        let (_d, c) = conn();
        assert!(!take_counter(&c, "rate_limit_next").unwrap());
        set(&c, "rate_limit_next", &json!(2)).unwrap();
        assert!(take_counter(&c, "rate_limit_next").unwrap());
        assert_eq!(get(&c, "rate_limit_next").unwrap(), Some(json!(1)));
        assert!(take_counter(&c, "rate_limit_next").unwrap());
        assert_eq!(get(&c, "rate_limit_next").unwrap(), None);
        assert!(!take_counter(&c, "rate_limit_next").unwrap());

        // Not a positive integer: left alone.
        set(&c, "rate_limit_next", &json!(true)).unwrap();
        assert!(!take_counter(&c, "rate_limit_next").unwrap());
        assert_eq!(get(&c, "rate_limit_next").unwrap(), Some(json!(true)));
    }
}
