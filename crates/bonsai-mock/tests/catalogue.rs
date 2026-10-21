//! The mock's root, account and catalogue operations, and the shared HTTP
//! conventions (auth, scopes, pagination, rate limits). Every call goes through
//! `Mock::call`, which validates the response against `openapi/v1.yml`.

mod common;

use bonsai_mock::{scenario, seed};
use common::{Mock, header};
use serde_json::{Value, json};

fn ids(body: &Value) -> Vec<String> {
    body["data"]
        .as_array()
        .expect("data array")
        .iter()
        .map(|v| v["id"].as_str().unwrap().to_string())
        .collect()
}

#[tokio::test]
async fn root_needs_no_token() {
    let m = Mock::start().await;
    let (status, _, body) = m.call("GET", "", None, None, &[]).await;
    assert_eq!(status, 200);
    let body = body.unwrap();
    assert_eq!(body["name"], "Bonsai API");
    assert_eq!(body["version"], m.contract.info_version());

    let (status, _, slash) = m.call("GET", "/", None, None, &[]).await;
    assert_eq!(status, 200);
    assert_eq!(slash.unwrap(), body);
}

#[tokio::test]
async fn account_needs_a_token() {
    let m = Mock::start().await;
    let (status, h, body) = m.call("GET", "/account", None, None, &[]).await;
    assert_eq!(status, 401);
    assert!(
        header(&h, "www-authenticate")
            .unwrap()
            .starts_with("Bearer"),
        "{h:?}"
    );
    assert_eq!(
        header(&h, "content-type").unwrap(),
        "application/problem+json"
    );
    let body = body.unwrap();
    assert_eq!(body["code"], "unauthorized");
    assert_eq!(
        body["type"],
        "https://bonsai.io/docs/api/errors/unauthorized"
    );
    assert_eq!(body["instance"], Value::Null);
    assert!(body.get("action_url").is_none(), "{body}");

    let (status, _, body) = m.call("GET", "/account", Some(&m.acme), None, &[]).await;
    assert_eq!(status, 200);
    let body = body.unwrap();
    assert_eq!(body["id"], "acme");
    assert_eq!(body["name"], "Acme Search");

    let (_, _, body) = m.call("GET", "/account", Some(&m.globex), None, &[]).await;
    assert_eq!(body.unwrap()["id"], "globex");
}

#[tokio::test]
async fn bad_token_gives_401() {
    let m = Mock::start().await;
    for bad in [
        "nope",
        "bonsai_0123abcd_deadbeef",
        "bonsai__",
        &format!("{}x", m.acme),
    ] {
        let (status, h, body) = m.call("GET", "/account", Some(bad), None, &[]).await;
        assert_eq!(status, 401, "token {bad:?}");
        assert!(header(&h, "www-authenticate").is_some());
        assert_eq!(body.unwrap()["code"], "unauthorized");
    }

    // A Basic credential is not a Bearer token.
    let (status, _, _) = m
        .call(
            "GET",
            "/account",
            None,
            None,
            &[("authorization", "Basic dXNlcjpwYXNz")],
        )
        .await;
    assert_eq!(status, 401);
}

#[tokio::test]
async fn expired_and_revoked_tokens_give_401() {
    let m = Mock::start().await;
    let db = m.db();
    let past = jiff::Timestamp::now() - jiff::SignedDuration::from_secs(60);
    let expired =
        seed::create_token(&db, "acme", "expired", &["account:read"], Some(past)).unwrap();
    let (status, _, _) = m.call("GET", "/account", Some(&expired), None, &[]).await;
    assert_eq!(status, 401);

    let revoked = seed::create_token(&db, "acme", "revoked", &["account:read"], None).unwrap();
    let (status, _, _) = m.call("GET", "/account", Some(&revoked), None, &[]).await;
    assert_eq!(status, 200);
    db.execute(
        "UPDATE tokens SET revoked_at = ?1 WHERE name = 'revoked'",
        [jiff::Timestamp::now().to_string()],
    )
    .unwrap();
    let (status, _, _) = m.call("GET", "/account", Some(&revoked), None, &[]).await;
    assert_eq!(status, 401);
}

#[tokio::test]
async fn missing_scope_gives_403_naming_the_scope() {
    let m = Mock::start().await;
    let token = seed::create_token(&m.db(), "acme", "narrow", &["clusters:read"], None).unwrap();
    let (status, _, body) = m.call("GET", "/account", Some(&token), None, &[]).await;
    assert_eq!(status, 403);
    let body = body.unwrap();
    assert_eq!(body["code"], "missing_scope");
    assert_eq!(body["details"][0]["field"], Value::Null);
    assert_eq!(body["details"][0]["scope"], "account:read");

    let (status, _, _) = m.call("GET", "/plans", Some(&token), None, &[]).await;
    assert_eq!(status, 403);
}

#[tokio::test]
async fn plans_are_paginated() {
    let m = Mock::start().await;
    let (status, _, first) = m
        .call("GET", "/plans?limit=2", Some(&m.acme), None, &[])
        .await;
    assert_eq!(status, 200);
    let first = first.unwrap();
    assert_eq!(ids(&first), ["business-2x", "business-2x-esplat"]);
    let mut cursor = first["next_cursor"]
        .as_str()
        .expect("next_cursor")
        .to_string();

    let mut all = ids(&first);
    loop {
        let (status, _, page) = m
            .call(
                "GET",
                &format!("/plans?limit=2&cursor={cursor}"),
                Some(&m.acme),
                None,
                &[],
            )
            .await;
        assert_eq!(status, 200);
        let page = page.unwrap();
        all.extend(ids(&page));
        match page["next_cursor"].as_str() {
            Some(c) => cursor = c.to_string(),
            None => break,
        }
    }
    assert_eq!(
        all,
        [
            "business-2x",
            "business-2x-esplat",
            "business-capacity-2x",
            "business-capacity-2x-esplat",
            "business-capacity-lg",
            "business-capacity-lg-esplat",
            "business-capacity-xl",
            "business-capacity-xl-esplat",
            "business-lg",
            "business-lg-esplat",
            "business-memory-2x",
            "business-performance-2x",
            "business-performance-2x-esplat",
            "business-performance-4x",
            "business-performance-4x-esplat",
            "business-performance-8x",
            "business-performance-8x-esplat",
            "business-performance-lg",
            "business-performance-lg-esplat",
            "business-performance-xl",
            "business-performance-xl-esplat",
            "business-xl",
            "business-xl-esplat",
            "sandbox",
            "staging",
            "standard-birch",
            "standard-cork",
            "standard-elm",
            "standard-fir",
            "standard-oak",
            "standard-teak"
        ]
    );

    // The default page holds 25, and the next page the rest.
    let (_, _, page) = m.call("GET", "/plans", Some(&m.acme), None, &[]).await;
    let page = page.unwrap();
    assert_eq!(ids(&page), all[..25]);
    let cursor = page["next_cursor"].as_str().expect("next_cursor");
    let (_, _, page) = m
        .call(
            "GET",
            &format!("/plans?cursor={cursor}"),
            Some(&m.acme),
            None,
            &[],
        )
        .await;
    let page = page.unwrap();
    assert_eq!(ids(&page), all[25..]);
    assert_eq!(page["next_cursor"], Value::Null);

    for bad in ["0", "101", "abc", "-1", ""] {
        let (status, _, body) = m
            .call(
                "GET",
                &format!("/plans?limit={bad}"),
                Some(&m.acme),
                None,
                &[],
            )
            .await;
        if bad.is_empty() {
            assert_eq!(status, 200, "empty limit takes the default");
            continue;
        }
        assert_eq!(status, 400, "limit={bad}");
        let body = body.unwrap();
        assert_eq!(body["code"], "invalid_parameter");
        assert_eq!(body["details"][0]["field"], "limit");
    }

    // Not base64url; base64url("o:x"); base64url of three zero bytes.
    for bad in ["garbage!", "bzp4", "AAAA"] {
        let (status, _, body) = m
            .call(
                "GET",
                &format!("/plans?cursor={bad}"),
                Some(&m.acme),
                None,
                &[],
            )
            .await;
        assert_eq!(status, 400, "cursor={bad}");
        let body = body.unwrap();
        assert_eq!(body["code"], "invalid_parameter");
        assert_eq!(body["details"][0]["field"], "cursor");
    }
}

#[tokio::test]
async fn plan_shapes_and_unknown_plan() {
    let m = Mock::start().await;
    let (status, _, body) = m
        .call("GET", "/plans/sandbox", Some(&m.acme), None, &[])
        .await;
    assert_eq!(status, 200);
    let body = body.unwrap();
    assert_eq!(body["price_in_cents"], 0);
    assert_eq!(body["releases"], json!(["opensearch-3.8.0"]));
    assert_eq!(body["regions"], json!(["aws-us-east-1"]));

    // Standard is AWS only; its releases are the union over its regions, in
    // offer order: OpenSearch first, then newest.
    let (_, _, body) = m
        .call("GET", "/plans/standard-fir", Some(&m.acme), None, &[])
        .await;
    let body = body.unwrap();
    assert_eq!(body["single_tenant"], false);
    assert_eq!(
        body["regions"],
        json!([
            "aws-ap-southeast-2",
            "aws-eu-central-1",
            "aws-eu-west-1",
            "aws-us-east-1",
            "aws-us-west-2"
        ])
    );
    assert_eq!(
        body["releases"],
        json!([
            "opensearch-3.8.0",
            "opensearch-2.19.6",
            "elasticsearch-7.10.2"
        ])
    );

    let (_, _, body) = m
        .call("GET", "/plans/business-lg", Some(&m.acme), None, &[])
        .await;
    let body = body.unwrap();
    assert_eq!(body["single_tenant"], true);
    assert_eq!(body["private_network"], true);
    assert_eq!(body["regions"].as_array().unwrap().len(), 6);

    let (status, _, body) = m.call("GET", "/plans/nope", Some(&m.acme), None, &[]).await;
    assert_eq!(status, 404);
    assert_eq!(body.unwrap()["code"], "not_found");
}

#[tokio::test]
async fn regions_and_releases_in_order() {
    let m = Mock::start().await;
    let (_, _, regions) = m.call("GET", "/regions", Some(&m.globex), None, &[]).await;
    assert_eq!(
        ids(&regions.unwrap()),
        [
            "aws-ap-southeast-2",
            "aws-eu-central-1",
            "aws-eu-west-1",
            "aws-us-east-1",
            "aws-us-west-2",
            "gcp-us-east4"
        ]
    );
    let (_, _, releases) = m.call("GET", "/releases", Some(&m.globex), None, &[]).await;
    assert_eq!(
        ids(&releases.unwrap()),
        [
            "elasticsearch-7.2.0",
            "elasticsearch-7.10.2",
            "elasticsearch-8.17.5",
            "elasticsearch-9.4.1",
            "opensearch-2.6.0-mt",
            "opensearch-2.6.0",
            "opensearch-2.19.2",
            "opensearch-2.19.3",
            "opensearch-2.19.6",
            "opensearch-3.8.0"
        ]
    );
}

#[tokio::test]
async fn spaces_belong_to_the_tokens_account() {
    let m = Mock::start().await;
    let (status, _, body) = m.call("GET", "/spaces", Some(&m.globex), None, &[]).await;
    assert_eq!(status, 200);
    let body = body.unwrap();
    assert_eq!(body["data"], json!([]));
    assert_eq!(body["next_cursor"], Value::Null);

    let (_, _, body) = m.call("GET", "/spaces", Some(&m.acme), None, &[]).await;
    let body = body.unwrap();
    assert_eq!(ids(&body), ["acme-private-use1"]);
    assert_eq!(body["data"][0]["region"], "aws-us-east-1");
    assert_eq!(body["data"][0]["private_network"], true);
}

#[tokio::test]
async fn every_response_has_rate_limit_headers() {
    let m = Mock::start().await;
    let (_, h1, _) = m.call("GET", "/account", Some(&m.acme), None, &[]).await;
    let (_, h2, _) = m.call("GET", "/regions", Some(&m.acme), None, &[]).await;
    assert_eq!(
        header(&h1, "ratelimit-policy"),
        Some("\"token\";q=600;w=60")
    );
    let remaining = |h: &reqwest::header::HeaderMap| -> u32 {
        let v = header(h, "ratelimit").unwrap();
        assert!(v.starts_with("\"token\";r="), "{v}");
        v.split(";r=")
            .nth(1)
            .unwrap()
            .split(';')
            .next()
            .unwrap()
            .parse()
            .unwrap()
    };
    assert_eq!(remaining(&h1), 599);
    assert_eq!(remaining(&h2), 598);
    // A different token has its own counter.
    let (_, h3, _) = m.call("GET", "/account", Some(&m.globex), None, &[]).await;
    assert_eq!(remaining(&h3), 599);
    // Errors carry them too.
    let (_, h4, _) = m.call("GET", "/account", None, None, &[]).await;
    assert!(header(&h4, "ratelimit").is_some());
    // The root is limited by IP.
    let (_, h5, _) = m.call("GET", "", None, None, &[]).await;
    assert!(header(&h5, "ratelimit").is_some());
}

fn remaining(h: &reqwest::header::HeaderMap) -> u32 {
    header(h, "ratelimit")
        .unwrap()
        .split(";r=")
        .nth(1)
        .unwrap()
        .split(';')
        .next()
        .unwrap()
        .parse()
        .unwrap()
}

/// As on the Bonsai API, a request counts against a token only once it authenticated:
/// the key prefix isn't a secret, so a wrong secret mustn't drain its quota.
#[tokio::test]
async fn a_failed_authentication_doesnt_count_against_the_token() {
    let m = Mock::start().await;
    let (_, h, _) = m.call("GET", "/account", Some(&m.acme), None, &[]).await;
    let before = remaining(&h);
    let key = m
        .acme
        .strip_prefix("bonsai_")
        .unwrap()
        .split('_')
        .next()
        .unwrap();
    let forged = format!("bonsai_{key}_deadbeef");
    for _ in 0..5 {
        let (status, _, body) = m.call("GET", "/account", Some(&forged), None, &[]).await;
        assert_eq!(status, 401);
        assert_eq!(body.unwrap()["code"], "unauthorized");
    }
    let (_, h, _) = m.call("GET", "/account", Some(&m.acme), None, &[]).await;
    assert_eq!(remaining(&h), before - 1);
}

/// As on the Bonsai API: after 20 failed
/// authentications in the window, the IP gets `429
/// too_many_failed_authentications` before its credentials are checked, so a
/// good token from it is refused too.
#[tokio::test]
async fn twenty_failed_authentications_lock_out_the_ip() {
    let m = Mock::start().await;
    for _ in 0..20 {
        let (status, _, _) = m
            .call("GET", "/account", Some("bonsai_nope_nope"), None, &[])
            .await;
        assert_eq!(status, 401);
    }
    for token in [Some("bonsai_nope_nope"), Some(m.acme.as_str()), None] {
        let (status, h, body) = m.call("GET", "/account", token, None, &[]).await;
        assert_eq!(status, 429);
        assert_eq!(body.unwrap()["code"], "too_many_failed_authentications");
        assert_eq!(header(&h, "retry-after"), Some("60"));
        assert_eq!(
            header(&h, "ratelimit-policy"),
            Some("\"failed_authentication\";q=20;w=60")
        );
        assert_eq!(
            header(&h, "ratelimit"),
            Some("\"failed_authentication\";r=0;t=60")
        );
    }
}

#[tokio::test]
async fn rate_limit_next_forces_429() {
    let m = Mock::start().await;
    scenario::set(&m.db(), "rate_limit_next", &json!(2)).unwrap();

    for _ in 0..2 {
        let (status, h, body) = m.call("GET", "/plans", Some(&m.acme), None, &[]).await;
        assert_eq!(status, 429);
        assert_eq!(header(&h, "retry-after"), Some("1"));
        assert!(header(&h, "ratelimit").is_some());
        assert_eq!(body.unwrap()["code"], "rate_limited");
    }
    assert_eq!(scenario::get(&m.db(), "rate_limit_next").unwrap(), None);

    let (status, _, _) = m.call("GET", "/plans", Some(&m.acme), None, &[]).await;
    assert_eq!(status, 200);
}

#[tokio::test]
async fn unknown_route_is_a_problem() {
    let m = Mock::start().await;
    let resp = m
        .http
        .get(format!("{}/nope", m.h.base_url))
        .bearer_auth(&m.acme)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 404);
    assert_eq!(
        resp.headers()["content-type"].to_str().unwrap(),
        "application/problem+json"
    );
}

#[tokio::test]
async fn base_url_is_the_bound_address() {
    let m = Mock::start().await;
    assert_eq!(m.h.base_url.path(), "/v1");
    assert_ne!(m.h.base_url.port(), Some(0));
    assert_eq!(m.h.base_url.host_str(), Some("127.0.0.1"));
    assert!(!m.h.admin_token.is_empty());
}

#[tokio::test]
#[should_panic(expected = "contract violation")]
async fn call_panics_on_a_contract_violation() {
    let m = Mock::start().await;
    // No operation matches, so the response can't conform.
    m.call("GET", "/nope", Some(&m.acme), None, &[]).await;
}
