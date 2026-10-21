//! The proposed endpoints `v1.yml` doesn't have yet: the browser login (`POST /v1/cli/logins`, `POST
//! /v1/cli/logins/token`, the approval page) and `GET`/`DELETE /v1/token`.
//! They aren't in the contract, so these tests check them by hand through
//! `Mock::call_unchecked`.

mod common;

use bonsai_mock::seed;
use common::{Mock, header};
use jiff::{SignedDuration, Timestamp};
use serde_json::{Value, json};

const USER_CODE_LETTERS: &str = "BCDFGHJKLMNPQRSTVWXZ";
const DEFAULT_SCOPES: [&str; 4] = [
    "account:read",
    "clusters:read",
    "clusters:write",
    "credentials:read",
];

fn start_body(hostname: &str) -> Value {
    json!({"client_name": "bonsai-cli", "hostname": hostname, "os": "linux"})
}

/// Starts a login, asserting the 200, and returns the grant.
async fn start(m: &Mock, hostname: &str) -> Value {
    let (status, h, body) = m
        .call_unchecked("POST", "/cli/logins", None, Some(start_body(hostname)), &[])
        .await;
    let body = body.unwrap();
    assert_eq!(status, 200, "{body}");
    assert!(header(&h, "ratelimit").is_some());
    body
}

async fn poll(m: &Mock, device_code: &str) -> (u16, Value) {
    let (status, h, body) = m
        .call_unchecked(
            "POST",
            "/cli/logins/token",
            None,
            Some(json!({"device_code": device_code})),
            &[],
        )
        .await;
    if status != 200 {
        assert_eq!(header(&h, "content-type"), Some("application/problem+json"));
    }
    (status, body.unwrap())
}

/// Polls and asserts a 400 Problem with `code`, returning it.
async fn poll_refused(m: &Mock, device_code: &str, code: &str) -> Value {
    let (status, body) = poll(m, device_code).await;
    assert_eq!(status, 400, "{body}");
    assert_eq!(body["code"], code, "{body}");
    assert_eq!(body["status"], 400);
    body
}

fn user_code(grant: &Value) -> &str {
    grant["user_code"].as_str().unwrap()
}

fn device_code(grant: &Value) -> &str {
    grant["device_code"].as_str().unwrap()
}

async fn approve(m: &Mock, grant: &Value, body: Value) -> u16 {
    m.admin(
        "POST",
        &format!("/cli/logins/{}/approve", user_code(grant)),
        Some(body),
    )
    .await
    .0
}

async fn page(m: &Mock, path: &str) -> (u16, String, String) {
    let resp = m.http.get(m.url(path)).send().await.unwrap();
    let status = resp.status().as_u16();
    let ct = resp.headers()["content-type"].to_str().unwrap().to_string();
    (status, ct, resp.text().await.unwrap())
}

async fn post_form(m: &Mock, path: &str, form: &[(&str, &str)]) -> (u16, String) {
    let body: String = url::form_urlencoded::Serializer::new(String::new())
        .extend_pairs(form)
        .finish();
    let resp = m
        .http
        .post(m.url(path))
        .header("content-type", "application/x-www-form-urlencoded")
        .body(body)
        .send()
        .await
        .unwrap();
    (resp.status().as_u16(), resp.text().await.unwrap())
}

// ---------------------------------------------------------------- the device flow

#[tokio::test]
async fn start_grants_a_device_code_and_a_user_code() {
    let m = Mock::start().await;
    let grant = start(&m, "laptop").await;

    let dc = device_code(&grant);
    assert_eq!(dc.len(), 32);
    assert!(dc.bytes().all(|b| b.is_ascii_alphanumeric()));

    let uc = user_code(&grant);
    let (a, b) = uc.split_once('-').unwrap();
    for half in [a, b] {
        assert_eq!(half.len(), 4, "{uc}");
        assert!(half.chars().all(|c| USER_CODE_LETTERS.contains(c)), "{uc}");
    }

    let uri = format!("{}_mock/cli/approve", m.h.origin);
    assert_eq!(grant["verification_uri"], uri);
    assert_eq!(
        grant["verification_uri_complete"],
        format!("{uri}?user_code={uc}")
    );
    assert_eq!(grant["expires_in"], 600);
    assert_eq!(grant["interval"], 1);

    // Only the device code's hash is stored.
    let (hash, stored_state): (String, String) = m
        .db()
        .query_row(
            "SELECT device_code_sha256, state FROM cli_logins WHERE user_code = ?1",
            [uc],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap();
    assert_eq!(hash, seed::sha256_hex(dc));
    assert_eq!(stored_state, "pending");
    let dump: String = m
        .db()
        .query_row(
            "SELECT group_concat(coalesce(device_code_sha256, '') || user_code || hostname || os || ip || state || coalesce(token_plain, ''), '|') FROM cli_logins",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert!(!dump.contains(dc));
}

#[tokio::test]
async fn full_flow_pending_slow_down_approved_then_spent() {
    let m = Mock::start().await;
    let grant = start(&m, "build-box").await;
    let dc = device_code(&grant).to_string();

    let pending = poll_refused(&m, &dc, "authorization_pending").await;
    assert_eq!(
        pending["type"],
        "https://bonsai.io/docs/api/errors/authorization_pending"
    );
    // Sooner than `interval` after the last poll.
    poll_refused(&m, &dc, "slow_down").await;

    let status = approve(
        &m,
        &grant,
        json!({"account": "acme", "scopes": ["account:read", "clusters:read"], "expires_in_days": 30}),
    )
    .await;
    assert_eq!(status, 204);

    let (status, body) = poll(&m, &dc).await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(
        body["account"],
        json!({"id": "acme", "name": "Acme Search"})
    );
    assert_eq!(body["scopes"], json!(["account:read", "clusters:read"]));
    let expires: Timestamp = body["expires_at"].as_str().unwrap().parse().unwrap();
    let in_30_days = Timestamp::now() + SignedDuration::from_hours(24 * 30);
    assert!(expires.duration_until(in_30_days).abs() < SignedDuration::from_mins(1));

    // The token works, and is named after the machine.
    let token = body["token"].as_str().unwrap().to_string();
    let (status, _, account) = m.call("GET", "/account", Some(&token), None, &[]).await;
    assert_eq!(status, 200);
    assert_eq!(account.unwrap()["id"], "acme");
    let (status, _, info) = m
        .call_unchecked("GET", "/token", Some(&token), None, &[])
        .await;
    assert_eq!(status, 200);
    assert_eq!(info.unwrap()["name"], "Bonsai CLI on build-box");

    // Handed out once: the login is spent, and the plain token is gone.
    poll_refused(&m, &dc, "expired_token").await;
    let (state, plain): (String, Option<String>) = m
        .db()
        .query_row(
            "SELECT state, token_plain FROM cli_logins WHERE user_code = ?1",
            [user_code(&grant)],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap();
    assert_eq!(state, "spent");
    assert_eq!(plain, None);

    // Approved once only.
    let status = approve(&m, &grant, json!({})).await;
    assert_eq!(status, 409);
}

#[tokio::test]
async fn approve_defaults() {
    let m = Mock::start().await;
    let grant = start(&m, "h").await;
    assert_eq!(approve(&m, &grant, json!({})).await, 204);
    let (status, body) = poll(&m, device_code(&grant)).await;
    assert_eq!(status, 200);
    assert_eq!(body["account"]["id"], "acme");
    assert_eq!(body["scopes"], json!(DEFAULT_SCOPES));
    let expires: Timestamp = body["expires_at"].as_str().unwrap().parse().unwrap();
    let in_90_days = Timestamp::now() + SignedDuration::from_hours(24 * 90);
    assert!(expires.duration_until(in_90_days).abs() < SignedDuration::from_mins(1));
}

#[tokio::test]
async fn approve_checks_its_body() {
    let m = Mock::start().await;
    let grant = start(&m, "h").await;
    for body in [
        json!({"account": "nobody"}),
        json!({"scopes": ["root"]}),
        json!({"expires_in_days": 7}),
        json!([1]),
    ] {
        let status = approve(&m, &grant, body.clone()).await;
        assert_eq!(status, 400, "{body}");
    }
    // An unknown user code.
    let (status, _) = m
        .admin("POST", "/cli/logins/BCDF-GHJK/approve", Some(json!({})))
        .await;
    assert_eq!(status, 404);
    // Still pending.
    poll_refused(&m, device_code(&grant), "authorization_pending").await;
}

#[tokio::test]
async fn denied() {
    let m = Mock::start().await;
    let grant = start(&m, "h").await;
    let (status, _) = m
        .admin(
            "POST",
            &format!("/cli/logins/{}/deny", user_code(&grant)),
            None,
        )
        .await;
    assert_eq!(status, 204);
    poll_refused(&m, device_code(&grant), "access_denied").await;
    // A denied login can't be approved after all.
    assert_eq!(approve(&m, &grant, json!({})).await, 409);
}

#[tokio::test]
async fn expired_and_unknown_codes_answer_alike() {
    let m = Mock::start().await;
    let grant = start(&m, "h").await;
    m.db()
        .execute(
            "UPDATE cli_logins SET expires_at = ?2 WHERE user_code = ?1",
            (
                user_code(&grant),
                (Timestamp::now() - SignedDuration::from_secs(1)).to_string(),
            ),
        )
        .unwrap();
    let expired = poll_refused(&m, device_code(&grant), "expired_token").await;
    let unknown = poll_refused(&m, &"x".repeat(32), "expired_token").await;
    // Polling never reveals whether a code exists.
    assert_eq!(expired, unknown);

    // An expired login can't be approved, and its page is gone.
    assert_eq!(approve(&m, &grant, json!({})).await, 404);
    let (status, _, _) = page(
        &m,
        &format!("/_mock/cli/approve?user_code={}", user_code(&grant)),
    )
    .await;
    assert_eq!(status, 404);
}

#[tokio::test]
async fn poll_needs_a_device_code() {
    let m = Mock::start().await;
    for body in [json!({}), json!({"device_code": 1}), json!([])] {
        let (status, _, problem) = m
            .call_unchecked("POST", "/cli/logins/token", None, Some(body.clone()), &[])
            .await;
        assert_eq!(status, 400, "{body}");
        assert_eq!(problem.unwrap()["code"], "invalid_parameter", "{body}");
    }
}

#[tokio::test]
async fn start_checks_its_body() {
    let m = Mock::start().await;
    let (status, _, problem) = m
        .call_unchecked(
            "POST",
            "/cli/logins",
            None,
            Some(json!({"client_name": "bonsai-cli", "os": "linux"})),
            &[],
        )
        .await;
    assert_eq!(status, 400);
    let problem = problem.unwrap();
    assert_eq!(problem["code"], "invalid_parameter");
    assert_eq!(problem["details"][0]["field"], "/hostname");

    let long = "h".repeat(256);
    let (status, _, problem) = m
        .call_unchecked("POST", "/cli/logins", None, Some(start_body(&long)), &[])
        .await;
    assert_eq!(status, 400);
    assert_eq!(problem.unwrap()["details"][0]["field"], "/hostname");
}

#[tokio::test]
async fn starting_is_limited_to_ten_a_minute_per_ip() {
    let m = Mock::start().await;
    for _ in 0..10 {
        start(&m, "h").await;
    }
    let (status, h, problem) = m
        .call_unchecked("POST", "/cli/logins", None, Some(start_body("h")), &[])
        .await;
    assert_eq!(status, 429);
    assert!(header(&h, "retry-after").is_some());
    assert_eq!(problem.unwrap()["code"], "rate_limited");
    // Polling isn't limited by it.
    let (status, _) = poll(&m, &"x".repeat(32)).await;
    assert_eq!(status, 400);
}

// ---------------------------------------------------------------- the approval page

#[tokio::test]
async fn approve_page_shows_the_request_and_escapes_it() {
    let m = Mock::start().await;
    let (status, _, _) = m
        .call_unchecked(
            "POST",
            "/cli/logins",
            None,
            Some(json!({
                "client_name": "bonsai-cli</title>",
                "hostname": "<script>alert(1)</script>",
                "os": "\"><img src=x onerror=alert(2)>",
            })),
            &[],
        )
        .await;
    assert_eq!(status, 200);
    let uc: String = m
        .db()
        .query_row("SELECT user_code FROM cli_logins", [], |r| r.get(0))
        .unwrap();

    let (status, ct, html) = page(&m, &format!("/_mock/cli/approve?user_code={uc}")).await;
    assert_eq!(status, 200);
    assert!(ct.starts_with("text/html"), "{ct}");
    assert!(
        html.contains("&lt;script&gt;alert(1)&lt;/script&gt;"),
        "{html}"
    );
    assert!(!html.contains("<script>"), "{html}");
    assert!(!html.contains("<img"), "{html}");
    assert!(!html.contains("bonsai-cli</title>"), "{html}");
    assert!(html.contains("&quot;&gt;&lt;img"), "{html}");

    assert!(html.contains("Only approve this if you just ran"));
    assert!(html.contains(&uc));
    assert!(html.contains("127.0.0.1"));
    assert!(html.contains(r#"action="/_mock/cli/approve""#));
    assert!(html.contains(r#"method="post""#));
    for account in ["acme", "globex"] {
        assert!(html.contains(&format!(r#"value="{account}""#)), "{account}");
    }
    for scope in seed::ALL_SCOPES {
        let checked = DEFAULT_SCOPES.contains(scope);
        let input = format!(r#"name="scope" value="{scope}""#);
        let line = html.lines().find(|l| l.contains(&input)).unwrap();
        assert_eq!(line.contains("checked"), checked, "{line}");
    }
    let line = html
        .lines()
        .find(|l| l.contains(r#"<option value="90""#))
        .unwrap();
    assert!(line.contains("selected"), "{line}");
    assert!(html.contains(r#"value="30""#) && html.contains(r#"value="365""#));
    assert!(html.contains(r#"name="decision" value="approve""#));
    assert!(html.contains(r#"name="decision" value="deny""#));
}

#[tokio::test]
async fn approve_page_for_an_unknown_code_is_a_404() {
    let m = Mock::start().await;
    let (status, ct, html) = page(&m, "/_mock/cli/approve?user_code=<b>BCDF</b>").await;
    assert_eq!(status, 404);
    assert!(ct.starts_with("text/html"));
    assert!(!html.contains("<b>"));
    let (status, _, _) = page(&m, "/_mock/cli/approve").await;
    assert_eq!(status, 404);
}

#[tokio::test]
async fn approving_through_the_page() {
    let m = Mock::start().await;
    let grant = start(&m, "h").await;
    // The user code is matched however it's typed.
    let typed = user_code(&grant).to_lowercase().replace('-', "");
    let (status, html) = post_form(
        &m,
        "/_mock/cli/approve",
        &[
            ("user_code", typed.as_str()),
            ("account", "globex"),
            ("scope", "account:read"),
            ("scope", "clusters:read"),
            ("expires_in_days", "365"),
            ("decision", "approve"),
        ],
    )
    .await;
    assert_eq!(status, 200, "{html}");
    assert!(html.contains("Approved"), "{html}");

    let (status, body) = poll(&m, device_code(&grant)).await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["account"]["id"], "globex");
    assert_eq!(body["scopes"], json!(["account:read", "clusters:read"]));
    let expires: Timestamp = body["expires_at"].as_str().unwrap().parse().unwrap();
    let in_a_year = Timestamp::now() + SignedDuration::from_hours(24 * 365);
    assert!(expires.duration_until(in_a_year).abs() < SignedDuration::from_mins(1));
}

#[tokio::test]
async fn denying_through_the_page() {
    let m = Mock::start().await;
    let grant = start(&m, "h").await;
    let (status, html) = post_form(
        &m,
        "/_mock/cli/approve",
        &[("user_code", user_code(&grant)), ("decision", "deny")],
    )
    .await;
    assert_eq!(status, 200, "{html}");
    assert!(html.contains("Denied"), "{html}");
    poll_refused(&m, device_code(&grant), "access_denied").await;

    // Once decided, the form refuses.
    let (status, _) = post_form(
        &m,
        "/_mock/cli/approve",
        &[("user_code", user_code(&grant)), ("decision", "approve")],
    )
    .await;
    assert_eq!(status, 409);
}

#[tokio::test]
async fn the_page_form_is_checked() {
    let m = Mock::start().await;
    let grant = start(&m, "h").await;
    for form in [
        vec![("decision", "approve")],
        vec![("user_code", user_code(&grant)), ("decision", "maybe")],
        vec![
            ("user_code", user_code(&grant)),
            ("decision", "approve"),
            ("scope", "root"),
        ],
        vec![
            ("user_code", user_code(&grant)),
            ("decision", "approve"),
            ("expires_in_days", "7"),
        ],
    ] {
        let (status, _) = post_form(&m, "/_mock/cli/approve", &form).await;
        assert!(status == 400 || status == 404, "{form:?}: {status}");
    }
    poll_refused(&m, device_code(&grant), "authorization_pending").await;
}

// ---------------------------------------------------------------- /v1/token

#[tokio::test]
async fn token_shows_itself() {
    let m = Mock::start().await;
    let (status, h, body) = m
        .call_unchecked("GET", "/token", Some(&m.acme), None, &[])
        .await;
    assert_eq!(status, 200);
    assert!(header(&h, "ratelimit").is_some());
    let key = m
        .acme
        .strip_prefix("bonsai_")
        .unwrap()
        .split('_')
        .next()
        .unwrap();
    assert_eq!(
        body.unwrap(),
        json!({
            "name": "Seed token",
            "scopes": seed::ALL_SCOPES,
            "expires_at": null,
            "key_prefix": key,
        })
    );

    let (status, h, problem) = m.call_unchecked("GET", "/token", None, None, &[]).await;
    assert_eq!(status, 401);
    assert!(header(&h, "www-authenticate").is_some());
    assert_eq!(problem.unwrap()["code"], "unauthorized");
}

#[tokio::test]
async fn token_deletes_itself() {
    let m = Mock::start().await;
    let (status, _, body) = m
        .call_unchecked("DELETE", "/token", Some(&m.globex), None, &[])
        .await;
    assert_eq!(status, 204);
    assert_eq!(body, None);

    let (status, _, _) = m.call("GET", "/account", Some(&m.globex), None, &[]).await;
    assert_eq!(status, 401);
    let (status, _, _) = m
        .call_unchecked("GET", "/token", Some(&m.globex), None, &[])
        .await;
    assert_eq!(status, 401);
    let (status, _, _) = m
        .call_unchecked("DELETE", "/token", Some(&m.globex), None, &[])
        .await;
    assert_eq!(status, 401);

    // Only the calling token.
    let (status, _, _) = m.call("GET", "/account", Some(&m.acme), None, &[]).await;
    assert_eq!(status, 200);
}
