//! Random IDs and secrets.

use rand::Rng;

const LOWER_ALNUM: &[u8] = b"0123456789abcdefghijklmnopqrstuvwxyz";
const HEX: &[u8] = b"0123456789abcdef";

/// `n` random characters from `alphabet`.
pub fn random_from(alphabet: &[u8], n: usize) -> String {
    let mut rng = rand::rng();
    (0..n)
        .map(|_| alphabet[rng.random_range(0..alphabet.len())] as char)
        .collect()
}

/// `n` random lowercase hex characters.
pub fn hex(n: usize) -> String {
    random_from(HEX, n)
}

/// `n` random characters from `[0-9a-z]`.
pub fn lower_alnum(n: usize) -> String {
    random_from(LOWER_ALNUM, n)
}

/// An Action ID: `act_` and 16 random `[0-9a-z]`.
pub fn action_id() -> String {
    format!("act_{}", lower_alnum(16))
}

/// A Stripe Checkout session ID as the demo shows it: `cs_test_` and 24 random
/// `[0-9a-z]`.
pub fn checkout_session_id() -> String {
    format!("cs_test_{}", lower_alnum(24))
}

/// A credential ID: `cred-` and 12 random `[0-9a-z]`, matching the
/// contract's `^[A-Za-z0-9-]+$`.
pub fn credential_id() -> String {
    format!("cred-{}", lower_alnum(12))
}

/// A cluster ID: the name's slug, cut to 40 characters, then `-` and 6 random
/// `[a-z0-9]`. Matches `^[a-z0-9-]+$` and stays within 64 characters.
pub fn cluster_id(name: &str) -> String {
    let mut slug = String::new();
    for c in name.chars().flat_map(char::to_lowercase) {
        if c.is_ascii_alphanumeric() {
            slug.push(c);
        } else if !slug.is_empty() && !slug.ends_with('-') {
            slug.push('-');
        }
    }
    let slug: String = slug.chars().take(40).collect();
    let slug = slug.trim_end_matches('-');
    let suffix = lower_alnum(6);
    if slug.is_empty() {
        suffix
    } else {
        format!("{slug}-{suffix}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn action_ids_match_the_contract_pattern() {
        let id = action_id();
        assert_eq!(id.len(), 20);
        assert!(id.starts_with("act_"));
        assert!(
            id[4..]
                .bytes()
                .all(|b| b.is_ascii_digit() || b.is_ascii_lowercase())
        );
    }

    #[test]
    fn cluster_ids_are_slugs() {
        let id = cluster_id("My Search -- Cluster!");
        let (slug, suffix) = id.rsplit_once('-').unwrap();
        assert_eq!(slug, "my-search-cluster");
        assert_eq!(suffix.len(), 6);

        let long = cluster_id(&"a".repeat(100));
        assert_eq!(long.len(), 40 + 1 + 6);

        let odd = cluster_id("ÉÉ ---");
        assert!(
            odd.bytes()
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
        );
        assert!(!odd.starts_with('-'));
    }
}
