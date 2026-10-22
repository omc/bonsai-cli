//! The catalogue and accounts a fresh database starts with (`seed.json`), and
//! API tokens.

use std::collections::BTreeMap;

use jiff::Timestamp;
use rusqlite::{Connection, params};
use serde::Deserialize;
use sha2::{Digest, Sha256};

use crate::ids;

const SEED: &str = include_str!("seed.json");

/// Every scope a token can hold.
pub const ALL_SCOPES: &[&str] = &[
    "account:read",
    "clusters:read",
    "clusters:write",
    "credentials:read",
    "credentials:write",
    "query_logs:read",
];

/// The scopes of the token printed for each seeded account on first run.
pub const SEED_TOKEN_SCOPES: &[(&str, &[&str])] = &[
    ("acme", ALL_SCOPES),
    ("globex", &["account:read", "clusters:read"]),
];

/// A token created for a seeded account on first run.
#[derive(Debug, Clone)]
pub struct SeedToken {
    pub account_id: String,
    pub scopes: Vec<String>,
    pub token: String,
}

#[derive(Deserialize)]
struct Seed {
    accounts: Vec<SeedAccount>,
    regions: Vec<SeedRegion>,
    releases: Vec<SeedRelease>,
    plans: Vec<SeedPlan>,
    spaces: Vec<SeedSpace>,
}

#[derive(Deserialize)]
struct SeedAccount {
    id: String,
    name: String,
    contact_email_override: Option<String>,
    created_at: Timestamp,
    has_payment_method: bool,
    billed_by_invoice: bool,
    advanced_credentials: bool,
}

#[derive(Deserialize)]
struct SeedRegion {
    id: String,
    name: String,
    cloud: String,
    cloud_region: String,
}

#[derive(Deserialize)]
struct SeedRelease {
    id: String,
    name: String,
    engine: String,
    version: String,
}

#[derive(Deserialize)]
struct SeedPlan {
    id: String,
    name: String,
    price_in_cents: u64,
    billing_interval_months: u32,
    single_tenant: bool,
    private_network: bool,
    tier: String,
    dedicated: bool,
    /// Every release offered in some region, in offer order: OpenSearch first,
    /// then newest first.
    releases: Vec<String>,
    /// The releases offered in each region, in offer order. A region missing
    /// here isn't offered; the first release is a create's default there.
    offers: BTreeMap<String, Vec<String>>,
    max_credentials: Option<u32>,
    limit_docs: Option<u64>,
    limit_shards: Option<u64>,
    limit_bytes: Option<u64>,
    limit_bandwidth: Option<u64>,
}

#[derive(Deserialize)]
struct SeedSpace {
    id: String,
    account_id: String,
    name: String,
    region: String,
    private_network: bool,
}

/// `true` when no account exists yet, so the database has never been seeded.
pub fn is_empty(conn: &Connection) -> rusqlite::Result<bool> {
    let n: i64 = conn.query_row("SELECT count(*) FROM accounts", [], |r| r.get(0))?;
    Ok(n == 0)
}

/// Loads `seed.json` into a database that has no accounts yet. Rows are sorted
/// in the order `seed.json` lists them.
pub fn seed_if_empty(conn: &Connection) -> rusqlite::Result<()> {
    if !is_empty(conn)? {
        return Ok(());
    }
    let seed: Seed = serde_json::from_str(SEED).expect("seed.json is valid");
    let tx = conn.unchecked_transaction()?;
    for a in &seed.accounts {
        tx.execute(
            "INSERT INTO accounts (id, name, contact_email_override, created_at, has_payment_method,
                billed_by_invoice, advanced_credentials)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            params![
                a.id,
                a.name,
                a.contact_email_override,
                a.created_at.to_string(),
                a.has_payment_method,
                a.billed_by_invoice,
                a.advanced_credentials
            ],
        )?;
    }
    for (i, r) in seed.regions.iter().enumerate() {
        tx.execute(
            "INSERT INTO regions (id, name, cloud, cloud_region, sort) VALUES (?1, ?2, ?3, ?4, ?5)",
            params![r.id, r.name, r.cloud, r.cloud_region, i],
        )?;
    }
    for (i, r) in seed.releases.iter().enumerate() {
        tx.execute(
            "INSERT INTO releases (id, name, engine, version, sort) VALUES (?1, ?2, ?3, ?4, ?5)",
            params![r.id, r.name, r.engine, r.version, i],
        )?;
    }
    for (i, p) in seed.plans.iter().enumerate() {
        // A plan's regions are those it's offered in, in region order.
        let regions: Vec<&str> = seed
            .regions
            .iter()
            .map(|r| r.id.as_str())
            .filter(|r| p.offers.contains_key(*r))
            .collect();
        tx.execute(
            "INSERT INTO plans (id, name, price_in_cents, billing_interval_months, single_tenant, private_network,
                tier, dedicated, regions, releases, max_credentials, limit_docs, limit_shards, limit_bytes,
                limit_bandwidth, sort, offers)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17)",
            params![
                p.id,
                p.name,
                p.price_in_cents,
                p.billing_interval_months,
                p.single_tenant,
                p.private_network,
                p.tier,
                p.dedicated,
                serde_json::to_string(&regions).expect("strings serialize"),
                serde_json::to_string(&p.releases).expect("strings serialize"),
                p.max_credentials,
                p.limit_docs,
                p.limit_shards,
                p.limit_bytes,
                p.limit_bandwidth,
                i,
                serde_json::to_string(&p.offers).expect("strings serialize"),
            ],
        )?;
    }
    for s in &seed.spaces {
        tx.execute(
            "INSERT INTO spaces (id, account_id, name, region, private_network) VALUES (?1, ?2, ?3, ?4, ?5)",
            params![s.id, s.account_id, s.name, s.region, s.private_network],
        )?;
    }
    tx.commit()
}

/// Creates one token per seeded account, with the scopes in [`SEED_TOKEN_SCOPES`].
pub fn create_seed_tokens(conn: &Connection) -> rusqlite::Result<Vec<SeedToken>> {
    SEED_TOKEN_SCOPES
        .iter()
        .map(|(account, scopes)| {
            let token = create_token(conn, account, "Seed token", scopes, None)?;
            Ok(SeedToken {
                account_id: account.to_string(),
                scopes: scopes.iter().map(|s| s.to_string()).collect(),
                token,
            })
        })
        .collect()
}

/// Creates an API token for `account_id` and returns it as `bonsai_<key>_<secret>`.
/// Only the key and the secret's SHA-256 are stored, so the token can't be shown again.
pub fn create_token(
    conn: &Connection,
    account_id: &str,
    name: &str,
    scopes: &[&str],
    expires_at: Option<Timestamp>,
) -> rusqlite::Result<String> {
    let key = ids::hex(16);
    let secret = ids::hex(32);
    conn.execute(
        "INSERT INTO tokens (account_id, name, key_prefix, secret_sha256, scopes, expires_at, created_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
        params![
            account_id,
            name,
            key,
            sha256_hex(&secret),
            serde_json::to_string(scopes).expect("strings serialize"),
            expires_at.map(|t| t.to_string()),
            crate::db::now(),
        ],
    )?;
    Ok(format!("bonsai_{key}_{secret}"))
}

/// Lowercase hex SHA-256 of `s`.
pub fn sha256_hex(s: &str) -> String {
    Sha256::digest(s.as_bytes())
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn seeds_once() {
        let dir = tempfile::tempdir().unwrap();
        let conn = crate::db::open(&dir.path().join("s.sqlite")).unwrap();
        assert!(is_empty(&conn).unwrap());
        seed_if_empty(&conn).unwrap();
        assert!(!is_empty(&conn).unwrap());
        seed_if_empty(&conn).unwrap();
        let plans: i64 = conn
            .query_row("SELECT count(*) FROM plans", [], |r| r.get(0))
            .unwrap();
        assert_eq!(plans, 31);
    }

    /// Each plan's `releases` is exactly the releases its `offers` name, every
    /// offered region and release is seeded, and no region offers nothing.
    #[test]
    fn offers_agree_with_the_catalogue() {
        let seed: Seed = serde_json::from_str(SEED).unwrap();
        let regions: Vec<&str> = seed.regions.iter().map(|r| r.id.as_str()).collect();
        let releases: Vec<&str> = seed.releases.iter().map(|r| r.id.as_str()).collect();
        for p in &seed.plans {
            assert!(!p.offers.is_empty(), "{} is offered nowhere", p.id);
            for (region, offered) in &p.offers {
                assert!(regions.contains(&region.as_str()), "{}: {region}", p.id);
                assert!(!offered.is_empty(), "{}: {region} offers nothing", p.id);
                for r in offered {
                    assert!(p.releases.contains(r), "{}: {r} not in releases", p.id);
                    assert!(releases.contains(&r.as_str()), "{}: {r} not seeded", p.id);
                }
            }
            for r in &p.releases {
                assert!(
                    p.offers.values().any(|o| o.contains(r)),
                    "{}: {r} offered in no region",
                    p.id
                );
            }
        }
    }

    /// Each plan lists its releases, overall and in each region, in the offer
    /// order `/releases` sorts by: OpenSearch first, then newest first.
    #[test]
    fn plans_list_releases_in_offer_order() {
        let seed: Seed = serde_json::from_str(SEED).unwrap();
        let key = |id: &String| {
            let r = seed.releases.iter().find(|r| &r.id == id).unwrap();
            crate::routes::catalogue::offer_order(&r.engine, &r.version)
        };
        for p in &seed.plans {
            let lists = std::iter::once(("releases", &p.releases))
                .chain(p.offers.iter().map(|(region, l)| (region.as_str(), l)));
            for (what, list) in lists {
                assert!(
                    list.windows(2).all(|w| key(&w[0]) <= key(&w[1])),
                    "{} {what}: {list:?}",
                    p.id
                );
            }
        }
    }

    #[test]
    fn tokens_store_only_a_hash() {
        let dir = tempfile::tempdir().unwrap();
        let conn = crate::db::open(&dir.path().join("s.sqlite")).unwrap();
        seed_if_empty(&conn).unwrap();
        let token = create_token(&conn, "acme", "t", &["account:read"], None).unwrap();
        let rest = token.strip_prefix("bonsai_").unwrap();
        let (key, secret) = rest.split_once('_').unwrap();
        let stored: String = conn
            .query_row(
                "SELECT secret_sha256 FROM tokens WHERE key_prefix = ?1",
                [key],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(stored, sha256_hex(secret));
        assert!(create_token(&conn, "nobody", "t", &[], None).is_err());
    }
}
