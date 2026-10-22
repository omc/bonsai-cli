//! The `/_mock` admin API, the checkout page, `/v1/openapi.json`, and what a
//! wrong method on a `/v1` path answers.

use crate::common::{Mock, create_body, header};
use bonsai_mock::scenario;
use serde_json::{Value, json};

#[tokio::test]
async fn admin_needs_the_admin_token() {
    let m = Mock::start().await;
    for token in [None, Some(m.acme.as_str()), Some("mock_admin_nope")] {
        let (status, h, problem) = m
            .call_unchecked("GET", "/_mock/scenario", token, None, &[])
            .await;
        assert_eq!(status, 401, "{token:?}");
        assert!(header(&h, "www-authenticate").is_some());
        assert_eq!(problem.unwrap()["code"], "unauthorized");
    }
    let (status, body) = m.admin("GET", "/scenario", None).await;
    assert_eq!(status, 200);
    assert_eq!(body.unwrap(), json!({}));
}

#[tokio::test]
async fn scenario_set_show_clear() {
    let m = Mock::start().await;
    for (key, value) in [
        ("fail_next", json!("timed_out")),
        ("metrics_unavailable", json!(true)),
        ("rate_limit_next", json!(3)),
    ] {
        let (status, _) = m
            .admin("PUT", &format!("/scenario/{key}"), Some(value))
            .await;
        assert_eq!(status, 204, "{key}");
    }
    let (_, body) = m.admin("GET", "/scenario", None).await;
    assert_eq!(
        body.unwrap(),
        json!({"fail_next": "timed_out", "metrics_unavailable": true, "rate_limit_next": 3})
    );
    // Admin calls don't take from rate_limit_next.
    assert_eq!(
        scenario::get(&m.db(), "rate_limit_next").unwrap(),
        Some(json!(3))
    );

    let (status, _) = m.admin("DELETE", "/scenario/fail_next", None).await;
    assert_eq!(status, 204);
    let (status, _) = m.admin("DELETE", "/scenario/fail_next", None).await;
    assert_eq!(status, 204, "clearing an unset key is fine");
    assert_eq!(scenario::get(&m.db(), "fail_next").unwrap(), None);
}

#[tokio::test]
async fn scenario_values_are_checked() {
    let m = Mock::start().await;
    for (key, value) in [
        ("nope", json!(1)),
        ("fail_next", json!("bogus")),
        ("fail_next", json!(1)),
        ("metrics_unavailable", json!("yes")),
        ("rate_limit_next", json!(-1)),
        ("rate_limit_next", json!("2")),
    ] {
        let (status, problem) = m
            .admin("PUT", &format!("/scenario/{key}"), Some(value.clone()))
            .await;
        assert_eq!(status, 400, "{key} = {value}");
        assert_eq!(problem.unwrap()["code"], "invalid_parameter");
    }
    let (status, _) = m.admin("DELETE", "/scenario/nope", None).await;
    assert_eq!(status, 400);
    let (status, _, _) = m
        .call_unchecked(
            "PUT",
            "/_mock/scenario/fail_next",
            Some(&m.h.admin_token),
            None,
            &[],
        )
        .await;
    assert_eq!(status, 400);
    let (_, body) = m.admin("GET", "/scenario", None).await;
    assert_eq!(body.unwrap(), json!({}));
}

#[tokio::test]
async fn rate_limit_next_through_the_admin_api() {
    let m = Mock::start().await;
    m.admin("PUT", "/scenario/rate_limit_next", Some(json!(1)))
        .await;
    let (status, _, _) = m.call("GET", "/plans", Some(&m.acme), None, &[]).await;
    assert_eq!(status, 429);
    let (_, body) = m.admin("GET", "/scenario", None).await;
    assert_eq!(body.unwrap(), json!({}));
}

#[tokio::test]
async fn payment_method_switch() {
    let m = Mock::start().await;
    let writer = m.token(
        "globex",
        &["account:read", "clusters:read", "clusters:write"],
    );
    let create = || {
        m.call(
            "POST",
            "/clusters",
            Some(&writer),
            Some(create_body("Paid", "standard-fir")),
            &[],
        )
    };
    let (status, _, _) = create().await;
    assert_eq!(status, 422);

    let (status, _) = m
        .admin("PUT", "/accounts/globex/payment_method", Some(json!(true)))
        .await;
    assert_eq!(status, 204);
    let (status, _, _) = create().await;
    assert_eq!(status, 202);

    let (status, _) = m
        .admin("PUT", "/accounts/globex/payment_method", Some(json!(false)))
        .await;
    assert_eq!(status, 204);
    let (status, _, _) = create().await;
    assert_eq!(status, 422);

    let (status, _) = m
        .admin("PUT", "/accounts/nobody/payment_method", Some(json!(true)))
        .await;
    assert_eq!(status, 404);
    let (status, _) = m
        .admin("PUT", "/accounts/globex/payment_method", Some(json!("yes")))
        .await;
    assert_eq!(status, 400);
}

#[tokio::test]
async fn set_cluster_stats() {
    let m = Mock::start().await;
    let id = m.ready_cluster("Stats", "standard-fir").await;
    let (status, _) = m
        .admin(
            "POST",
            &format!("/clusters/{id}/stats"),
            Some(json!({"docs": 12, "shards": 3, "bytes": 4096})),
        )
        .await;
    assert_eq!(status, 204);
    let cluster = m.cluster(&id).await;
    assert_eq!(cluster["stats"]["docs"], 12);
    assert_eq!(cluster["stats"]["shards"], 3);
    assert_eq!(cluster["stats"]["bytes"], 4096);
    assert!(cluster["stats"]["polled_at"].is_string());

    let (status, _) = m
        .admin(
            "POST",
            "/clusters/nope/stats",
            Some(json!({"docs": 1, "shards": 1, "bytes": 1})),
        )
        .await;
    assert_eq!(status, 404);
    let (status, _) = m
        .admin(
            "POST",
            &format!("/clusters/{id}/stats"),
            Some(json!({"docs": -1})),
        )
        .await;
    assert_eq!(status, 400);
    // SQLite stores i64: a larger value is the caller's mistake, not a 500.
    let (status, body) = m
        .admin(
            "POST",
            &format!("/clusters/{id}/stats"),
            Some(json!({"docs": u64::MAX, "shards": 1, "bytes": 1})),
        )
        .await;
    assert_eq!(status, 400, "{body:?}");
    assert_eq!(body.unwrap()["details"][0]["field"], "/docs");
}

// ---------------------------------------------------------------- checkout

#[tokio::test]
async fn the_checkout_page_adds_a_card() {
    let m = Mock::start().await;
    let writer = m.token(
        "globex",
        &["account:read", "clusters:read", "clusters:write"],
    );
    let (status, _, problem) = m
        .call(
            "POST",
            "/clusters",
            Some(&writer),
            Some(create_body("Paid", "standard-fir")),
            &[],
        )
        .await;
    assert_eq!(status, 422);
    let action_url = problem.unwrap()["action_url"].as_str().unwrap().to_string();

    let resp = m.http.get(&action_url).send().await.unwrap();
    assert_eq!(resp.status().as_u16(), 200);
    assert!(
        resp.headers()["content-type"]
            .to_str()
            .unwrap()
            .starts_with("text/html")
    );
    let html = resp.text().await.unwrap();
    assert!(html.contains("Add card"), "{html}");
    assert!(html.contains(r#"method="post""#), "{html}");
    assert!(
        html.contains(r#"action="/_mock/checkout/globex""#),
        "{html}"
    );

    let resp = m.http.post(&action_url).send().await.unwrap();
    assert_eq!(resp.status().as_u16(), 200);
    let has: bool = m
        .db()
        .query_row(
            "SELECT has_payment_method FROM accounts WHERE id = 'globex'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert!(has);

    let (status, _, _) = m
        .call(
            "POST",
            "/clusters",
            Some(&writer),
            Some(create_body("Paid", "standard-fir")),
            &[],
        )
        .await;
    assert_eq!(status, 202);
}

/// The pages `dashboard_url` and `tokens_url` point at, which stand in for the
/// dashboard's.
#[tokio::test]
async fn the_dashboard_links_are_pages() {
    let m = Mock::start().await;
    let cluster = m.ready_cluster("Shown", "standard-fir").await;
    let link = m.cluster(&cluster).await["dashboard_url"]
        .as_str()
        .unwrap()
        .to_string();
    assert_eq!(link, m.url(&format!("/_mock/clusters/{cluster}")));
    let resp = m.http.get(&link).send().await.unwrap();
    assert_eq!(resp.status().as_u16(), 200);
    let html = resp.text().await.unwrap();
    assert!(html.contains("<h1>Shown</h1>"), "{html}");

    let resp = m
        .http
        .get(m.url("/_mock/clusters/%3Cb%3Enone"))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 404);
    assert!(!resp.text().await.unwrap().contains("<b>"));

    let resp = m.http.get(m.url("/_mock/api_tokens")).send().await.unwrap();
    assert_eq!(resp.status().as_u16(), 200);
    assert!(resp.text().await.unwrap().contains("API Tokens"));
}

#[tokio::test]
async fn checkout_for_an_unknown_account() {
    let m = Mock::start().await;
    let resp = m
        .http
        .get(m.url("/_mock/checkout/%3Cb%3Enobody"))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 404);
    assert!(!resp.text().await.unwrap().contains("<b>"));
    let resp = m
        .http
        .post(m.url("/_mock/checkout/nobody"))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 404);
}

// ---------------------------------------------------------------- openapi.json

#[tokio::test]
async fn openapi_json_is_the_contract() {
    let m = Mock::start().await;
    let (status, h, body) = m
        .call_unchecked("GET", "/openapi.json", None, None, &[])
        .await;
    assert_eq!(status, 200);
    assert!(
        header(&h, "content-type")
            .unwrap()
            .starts_with("application/json")
    );
    let spec = body.unwrap();
    let yaml: Value = serde_yaml::from_str(include_str!("../../../../openapi/v1.yml")).unwrap();
    assert_eq!(spec, yaml);

    // The root points at it.
    let (_, _, root) = m.call("GET", "", None, None, &[]).await;
    let root = root.unwrap();
    assert_eq!(root["openapi_url"], m.url("/v1/openapi.json"));
    assert_eq!(root["tokens_url"], m.url("/_mock/api_tokens"));
    assert_eq!(
        spec["info"]["version"],
        m.call("GET", "", None, None, &[]).await.2.unwrap()["version"]
    );
}

// ---------------------------------------------------------------- wrong methods

#[tokio::test]
async fn a_wrong_method_is_a_404_problem() {
    let m = Mock::start().await;
    // As the Bonsai API answers an unknown route: the token is checked, then 404 not_found.
    let (status, h, problem) = m
        .call_unchecked("POST", "/account", Some(&m.acme), None, &[])
        .await;
    assert_eq!(status, 404);
    assert_eq!(header(&h, "content-type"), Some("application/problem+json"));
    assert!(header(&h, "ratelimit").is_some());
    assert_eq!(problem.unwrap()["code"], "not_found");

    let (status, _, problem) = m
        .call_unchecked("PUT", "/clusters/x", None, None, &[])
        .await;
    assert_eq!(status, 401);
    assert_eq!(problem.unwrap()["code"], "unauthorized");

    let (status, _, problem) = m.call_unchecked("DELETE", "", None, None, &[]).await;
    assert_eq!(status, 404);
    assert_eq!(problem.unwrap()["code"], "not_found");
}

#[tokio::test]
async fn rate_limit_next_is_taken_by_any_v1_route_whatever_the_method() {
    let m = Mock::start().await;
    scenario::set(&m.db(), "rate_limit_next", &json!(3)).unwrap();
    // A wrong method, the root, and a request with no token all take one.
    let (status, _, _) = m
        .call_unchecked("POST", "/account", Some(&m.acme), None, &[])
        .await;
    assert_eq!(status, 429);
    let (status, _, _) = m.call("GET", "", None, None, &[]).await;
    assert_eq!(status, 429);
    let (status, _, _) = m.call("GET", "/account", None, None, &[]).await;
    assert_eq!(status, 429);
    assert_eq!(scenario::get(&m.db(), "rate_limit_next").unwrap(), None);
}

// ---------------------------------------------------------------- `bonsai-mock scenario`

fn scenario_cli(m: &Mock, args: &[&str]) -> std::process::Output {
    std::process::Command::new(env!("CARGO_BIN_EXE_bonsai-mock"))
        .arg("--db")
        .arg(&m.db_path)
        .arg("scenario")
        .args(args)
        .output()
        .expect("run bonsai-mock")
}

#[tokio::test]
async fn the_scenario_subcommands_share_the_table() {
    let m = Mock::start().await;
    let out = scenario_cli(&m, &["set", "rate_limit_next", "1"]);
    assert!(out.status.success(), "{out:?}");
    let out = scenario_cli(&m, &["set", "fail_next", "payment_failed"]);
    assert!(out.status.success(), "{out:?}");
    let out = scenario_cli(&m, &["show"]);
    assert!(out.status.success(), "{out:?}");
    let shown: Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(
        shown,
        json!({"fail_next": "payment_failed", "rate_limit_next": 1})
    );

    // The running server sees it at once.
    let (status, _, _) = m.call("GET", "/plans", Some(&m.acme), None, &[]).await;
    assert_eq!(status, 429);
    let (_, body) = m.admin("GET", "/scenario", None).await;
    assert_eq!(body.unwrap(), json!({"fail_next": "payment_failed"}));

    let out = scenario_cli(&m, &["set", "fail_next", "bogus"]);
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("fail_next must be one of"));
    let out = scenario_cli(&m, &["set", "metrics_unavailable", "true"]);
    assert!(out.status.success(), "{out:?}");

    let out = scenario_cli(&m, &["clear", "fail_next"]);
    assert!(out.status.success(), "{out:?}");
    assert_eq!(
        scenario::all(&m.db())
            .unwrap()
            .into_iter()
            .collect::<Vec<_>>(),
        [("metrics_unavailable".to_string(), json!(true))]
    );
    let out = scenario_cli(&m, &["clear"]);
    assert!(out.status.success(), "{out:?}");
    assert!(scenario::all(&m.db()).unwrap().is_empty());
    let out = scenario_cli(&m, &["clear", "nope"]);
    assert!(!out.status.success());
}

/// DNS rebinding: a page on another origin that resolves to 127.0.0.1 sends its
/// own `Host`. Only loopback names (and `public_url`'s host) are served.
#[tokio::test]
async fn a_foreign_host_header_is_refused() {
    let m = Mock::start().await;
    for host in ["evil.example", "evil.example:80", "127.0.0.1.evil.example"] {
        let (status, h, body) = m
            .call_unchecked("POST", "/v1/cli/logins", None, None, &[("host", host)])
            .await;
        assert_eq!(status, 421, "{host}");
        assert_eq!(header(&h, "content-type"), Some("application/problem+json"));
        let body = body.expect("a Problem");
        // No operation documents a 421, so only the body's shape is checked.
        bonsai_contract::Contract::load_default()
            .validate_schema("Problem", &body)
            .unwrap();
        assert_eq!(body["status"], 421);
        assert_eq!(body["code"], "invalid_parameter");
        assert_eq!(body["details"][0]["field"], "Host");
        let (status, _, _) = m
            .call_unchecked("GET", "/_mock/cli/approve", None, None, &[("host", host)])
            .await;
        assert_eq!(status, 421, "{host}");
    }
    for host in ["localhost", "127.0.0.1:1234", "[::1]:80"] {
        let (status, _, _) = m
            .call_unchecked("GET", "", None, None, &[("host", host)])
            .await;
        assert_eq!(status, 200, "{host}");
    }
}

#[tokio::test]
async fn spawn_refuses_a_non_loopback_address() {
    let dir = tempfile::tempdir().unwrap();
    let opts = bonsai_mock::MockOptions {
        db_path: dir.path().join("m.sqlite"),
        step_ms: 0,
        public_url: None,
        checkout_url: None,
    };
    let err = bonsai_mock::spawn(opts, "0.0.0.0:0".parse().unwrap())
        .await
        .err()
        .expect("refused");
    assert!(err.to_string().contains("loopback"), "{err}");
}
