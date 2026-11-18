//! The mock's SQLite schema, migrations and column helpers.
//!
//! JSON columns are `TEXT` holding serialized JSON; timestamps are RFC 3339
//! `TEXT`; booleans are `INTEGER` 0/1.

use std::path::Path;
use std::time::Duration;

use jiff::Timestamp;
use rusqlite::types::Type;
use serde::de::DeserializeOwned;

pub use rusqlite::{Connection, OptionalExtension, Row, params};

/// Each entry runs once, in order; `PRAGMA user_version` records how many ran.
/// Append new migrations; never edit one that has shipped.
const MIGRATIONS: &[&str] = &[MIGRATION_1, MIGRATION_2, MIGRATION_3, MIGRATION_4];

const MIGRATION_1: &str = r#"
CREATE TABLE accounts (
  id TEXT PRIMARY KEY,
  name TEXT NOT NULL,
  contact_email_override TEXT,
  created_at TEXT NOT NULL,
  has_payment_method INTEGER NOT NULL DEFAULT 0,
  billed_by_invoice INTEGER NOT NULL DEFAULT 0
);

CREATE TABLE tokens (
  id INTEGER PRIMARY KEY,
  account_id TEXT NOT NULL REFERENCES accounts(id),
  name TEXT NOT NULL,
  key_prefix TEXT NOT NULL UNIQUE,
  secret_sha256 TEXT NOT NULL,
  scopes TEXT NOT NULL,
  expires_at TEXT,
  revoked_at TEXT,
  created_at TEXT NOT NULL
);

CREATE TABLE regions (
  id TEXT PRIMARY KEY,
  name TEXT NOT NULL,
  cloud TEXT NOT NULL,
  cloud_region TEXT NOT NULL,
  sort INTEGER NOT NULL
);

CREATE TABLE releases (
  id TEXT PRIMARY KEY,
  name TEXT NOT NULL,
  engine TEXT NOT NULL,
  version TEXT NOT NULL,
  sort INTEGER NOT NULL
);

CREATE TABLE plans (
  id TEXT PRIMARY KEY,
  name TEXT NOT NULL,
  price_in_cents INTEGER NOT NULL,
  billing_interval_months INTEGER NOT NULL,
  single_tenant INTEGER NOT NULL,
  private_network INTEGER NOT NULL,
  tier TEXT NOT NULL,
  dedicated INTEGER NOT NULL,
  regions TEXT NOT NULL,
  releases TEXT NOT NULL,
  max_credentials INTEGER,
  limit_docs INTEGER,
  limit_shards INTEGER,
  limit_bytes INTEGER,
  limit_bandwidth INTEGER,
  sort INTEGER NOT NULL
);

CREATE TABLE spaces (
  id TEXT PRIMARY KEY,
  account_id TEXT NOT NULL REFERENCES accounts(id),
  name TEXT NOT NULL,
  region TEXT NOT NULL REFERENCES regions(id),
  private_network INTEGER NOT NULL
);

CREATE TABLE clusters (
  id TEXT PRIMARY KEY,
  account_id TEXT NOT NULL REFERENCES accounts(id),
  name TEXT NOT NULL,
  plan_id TEXT REFERENCES plans(id),
  region TEXT NOT NULL REFERENCES regions(id),
  space TEXT REFERENCES spaces(id),
  release TEXT NOT NULL REFERENCES releases(id),
  single_tenant INTEGER NOT NULL,
  host TEXT,
  created_at TEXT NOT NULL,
  terminated_at TEXT,
  docs INTEGER,
  shards INTEGER,
  bytes INTEGER,
  polled_at TEXT
);

CREATE TABLE actions (
  id TEXT PRIMARY KEY,
  account_id TEXT NOT NULL REFERENCES accounts(id),
  type TEXT NOT NULL,
  cluster_id TEXT NOT NULL REFERENCES clusters(id),
  state TEXT NOT NULL,
  steps TEXT NOT NULL,
  step_index INTEGER NOT NULL DEFAULT 0,
  started_at TEXT NOT NULL,
  finished_at TEXT,
  error TEXT,
  result TEXT,
  plan_after TEXT,
  outcome TEXT NOT NULL DEFAULT 'success'
);

CREATE TABLE credentials (
  id TEXT PRIMARY KEY,
  cluster_id TEXT NOT NULL REFERENCES clusters(id),
  name TEXT,
  access_type TEXT NOT NULL,
  allowed_actions TEXT NOT NULL DEFAULT '[]',
  allowed_index_globs TEXT NOT NULL DEFAULT '[]',
  allowed_cidrs TEXT NOT NULL DEFAULT '[]',
  username TEXT NOT NULL,
  password TEXT NOT NULL,
  created_at TEXT NOT NULL,
  revoked_at TEXT
);

CREATE TABLE idempotency (
  token_id INTEGER NOT NULL,
  key TEXT NOT NULL,
  fingerprint TEXT NOT NULL,
  method TEXT NOT NULL,
  path TEXT NOT NULL,
  status INTEGER NOT NULL,
  resource_kind TEXT,
  resource_id TEXT,
  created_at TEXT NOT NULL,
  PRIMARY KEY (token_id, key)
);

CREATE TABLE cli_logins (
  device_code_sha256 TEXT PRIMARY KEY,
  user_code TEXT NOT NULL UNIQUE,
  client_name TEXT NOT NULL,
  hostname TEXT NOT NULL,
  os TEXT NOT NULL,
  ip TEXT NOT NULL,
  state TEXT NOT NULL,
  account_id TEXT REFERENCES accounts(id),
  scopes TEXT,
  expires_in_days INTEGER,
  token_plain TEXT,
  created_at TEXT NOT NULL,
  expires_at TEXT NOT NULL,
  last_poll_at TEXT
);

CREATE TABLE scenario (
  key TEXT PRIMARY KEY,
  value TEXT NOT NULL
);

CREATE TABLE request_log (
  id INTEGER PRIMARY KEY,
  at TEXT NOT NULL,
  method TEXT NOT NULL,
  path TEXT NOT NULL,
  status INTEGER NOT NULL
);
"#;

/// Whether the account may create `rwm` and `acl` credentials (an account
/// feature on the Bonsai API). `globex` may not.
const MIGRATION_2: &str = r#"
ALTER TABLE accounts ADD COLUMN advanced_credentials INTEGER NOT NULL DEFAULT 1;
UPDATE accounts SET advanced_credentials = 0 WHERE id = 'globex';
"#;

/// The releases a plan offers in each region (`{region: [release, ...]}`, in
/// offer order). `{}` offers every release in `releases` in every region in
/// `regions`, as plans did before.
const MIGRATION_3: &str = r#"
ALTER TABLE plans ADD COLUMN offers TEXT NOT NULL DEFAULT '{}';
"#;

/// What `local_clusters` keeps: each cluster's indices and their documents (see
/// `es`). `body` is the JSON an index was created with, `source` a document.
const MIGRATION_4: &str = r#"
CREATE TABLE es_indices (
  cluster_id TEXT NOT NULL REFERENCES clusters(id),
  name TEXT NOT NULL,
  body TEXT NOT NULL DEFAULT '{}',
  created_at TEXT NOT NULL,
  PRIMARY KEY (cluster_id, name)
);

CREATE TABLE es_docs (
  cluster_id TEXT NOT NULL,
  idx TEXT NOT NULL,
  id TEXT NOT NULL,
  source TEXT NOT NULL,
  PRIMARY KEY (cluster_id, idx, id),
  FOREIGN KEY (cluster_id, idx) REFERENCES es_indices(cluster_id, name) ON DELETE CASCADE
);
"#;

/// Opens (creating if needed) the database at `path` in WAL mode with foreign
/// keys on, and runs any migrations it hasn't had yet.
pub fn open(path: &Path) -> rusqlite::Result<Connection> {
    let conn = Connection::open(path)?;
    conn.busy_timeout(Duration::from_secs(5))?;
    conn.pragma_update_and_check(None, "journal_mode", "WAL", |r| r.get::<_, String>(0))?;
    conn.pragma_update(None, "foreign_keys", "ON")?;
    migrate(&conn)?;
    Ok(conn)
}

/// Deletes the database file and its WAL and shared-memory files, if present.
pub fn remove(path: &Path) -> std::io::Result<()> {
    for suffix in ["", "-wal", "-shm"] {
        let mut p = path.as_os_str().to_owned();
        p.push(suffix);
        match std::fs::remove_file(&p) {
            Err(e) if e.kind() != std::io::ErrorKind::NotFound => return Err(e),
            _ => {}
        }
    }
    Ok(())
}

fn migrate(conn: &Connection) -> rusqlite::Result<()> {
    let applied: usize = conn.pragma_query_value(None, "user_version", |r| r.get(0))?;
    for (i, sql) in MIGRATIONS.iter().enumerate().skip(applied) {
        let tx = conn.unchecked_transaction()?;
        tx.execute_batch(sql)?;
        tx.pragma_update(None, "user_version", i + 1)?;
        tx.commit()?;
    }
    Ok(())
}

/// Runs `f` in a transaction, unless one is already open on `conn`, in which
/// case `f` joins it. An `Err` rolls back (only the transaction this opened).
pub fn atomically<T, E: From<rusqlite::Error>>(
    conn: &Connection,
    f: impl FnOnce(&Connection) -> Result<T, E>,
) -> Result<T, E> {
    if !conn.is_autocommit() {
        return f(conn);
    }
    let tx = conn.unchecked_transaction()?;
    let out = f(&tx)?;
    tx.commit()?;
    Ok(out)
}

/// The current time, as stored in timestamp columns.
pub fn now() -> String {
    Timestamp::now().to_string()
}

/// Reads an RFC 3339 `TEXT` column.
pub fn ts(row: &Row, idx: usize) -> rusqlite::Result<Timestamp> {
    let s: String = row.get(idx)?;
    s.parse()
        .map_err(|e| rusqlite::Error::FromSqlConversionFailure(idx, Type::Text, Box::new(e)))
}

/// Reads a nullable RFC 3339 `TEXT` column.
pub fn ts_opt(row: &Row, idx: usize) -> rusqlite::Result<Option<Timestamp>> {
    let s: Option<String> = row.get(idx)?;
    s.map(|s| {
        s.parse()
            .map_err(|e| rusqlite::Error::FromSqlConversionFailure(idx, Type::Text, Box::new(e)))
    })
    .transpose()
}

/// Reads a JSON `TEXT` column.
pub fn json<T: DeserializeOwned>(row: &Row, idx: usize) -> rusqlite::Result<T> {
    let s: String = row.get(idx)?;
    serde_json::from_str(&s)
        .map_err(|e| rusqlite::Error::FromSqlConversionFailure(idx, Type::Text, Box::new(e)))
}

/// Reads a plain `TEXT` column into a string enum (`Cloud`, `ActionState`, ...).
pub fn text_enum<T: DeserializeOwned>(row: &Row, idx: usize) -> rusqlite::Result<T> {
    let s: String = row.get(idx)?;
    serde_json::from_value(serde_json::Value::String(s))
        .map_err(|e| rusqlite::Error::FromSqlConversionFailure(idx, Type::Text, Box::new(e)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn migrations_run_once() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("m.sqlite");
        let conn = open(&path).unwrap();
        let v: usize = conn
            .pragma_query_value(None, "user_version", |r| r.get(0))
            .unwrap();
        assert_eq!(v, MIGRATIONS.len());
        drop(conn);
        // Reopening doesn't re-run migration 1 (which would fail on CREATE TABLE).
        open(&path).unwrap();
    }
}
