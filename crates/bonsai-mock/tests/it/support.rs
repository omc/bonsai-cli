//! Support requests (`createSupportRequest`). Every call goes through
//! `Mock::call`, which validates the response against `openapi/v1.yml`.

use crate::common::Mock;
use serde_json::{Value, json};

fn request() -> Value {
    json!({
        "subject": "Search is slow",
        "message": "Queries take seconds since this morning.",
        "category": "cluster_availability",
    })
}

async fn file(m: &Mock, token: &str, body: Value) -> (u16, Value) {
    let (status, _, body) = m
        .call("POST", "/support_requests", Some(token), Some(body), &[])
        .await;
    (status, body.unwrap_or_default())
}

fn fields(problem: &Value) -> Vec<&str> {
    problem["details"]
        .as_array()
        .expect("details")
        .iter()
        .map(|d| d["field"].as_str().unwrap())
        .collect()
}

#[tokio::test]
async fn a_request_is_answered_with_an_id_and_what_was_sent() {
    let m = Mock::start().await;

    let (status, sent) = file(&m, &m.acme, request()).await;

    assert_eq!(status, 201, "{sent}");
    assert!(sent["id"].as_str().unwrap().starts_with("th_"), "{sent}");
    assert_eq!(sent["subject"], "Search is slow");
    assert_eq!(sent["category"], "cluster_availability");
    assert_eq!(sent["kind"], "question");
    // Never the message.
    assert!(sent.get("message").is_none(), "{sent}");
}

#[tokio::test]
async fn any_token_may_send_one() {
    let m = Mock::start().await;
    let no_scopes = m.token("acme", &[]);

    let (status, sent) = file(&m, &no_scopes, request()).await;

    assert_eq!(status, 201, "{sent}");
}

#[tokio::test]
async fn a_feature_request_about_a_cluster_with_context() {
    let m = Mock::start().await;
    let cluster = m.ready_cluster("Shop", "standard-fir").await;
    let mut body = request();
    body["kind"] = json!("feature_request");
    body["cluster"] = json!(cluster);
    body["context"] = json!("resize_cluster answered plan_change_refused.");

    let (status, sent) = file(&m, &m.acme, body).await;

    assert_eq!(status, 201, "{sent}");
    assert_eq!(sent["kind"], "feature_request");
}

#[tokio::test]
async fn values_that_are_not_accepted_are_named() {
    let m = Mock::start().await;
    for (field, value) in [
        ("category", json!("weather")),
        ("kind", json!("complaint")),
        ("cluster", json!("no-such-cluster")),
        ("subject", json!("   ")),
        ("subject", json!("x".repeat(201))),
        ("message", json!(" ")),
        ("message", json!("x".repeat(10_001))),
        ("context", json!("x".repeat(5_001))),
    ] {
        let mut body = request();
        body[field] = value;

        let (status, problem) = file(&m, &m.acme, body).await;

        assert_eq!(status, 422, "{field}: {problem}");
        assert_eq!(problem["code"], "validation_failed", "{field}");
        assert_eq!(fields(&problem), [format!("/{field}")], "{field}");
    }
}

#[tokio::test]
async fn another_accounts_cluster_is_not_this_accounts() {
    let m = Mock::start().await;
    let cluster = m.ready_cluster("Theirs", "standard-fir").await;
    let globex = m.token("globex", &[]);
    let mut body = request();
    body["cluster"] = json!(cluster);

    let (status, problem) = file(&m, &globex, body).await;

    assert_eq!(status, 422, "{problem}");
    assert_eq!(fields(&problem), ["/cluster"]);
}

#[tokio::test]
async fn a_body_that_is_not_the_schemas_is_a_400() {
    let m = Mock::start().await;

    let (status, problem) = file(&m, &m.acme, json!({"subject": "S", "message": "M"})).await;
    assert_eq!(status, 400, "{problem}");
    assert_eq!(fields(&problem), ["/category"]);

    let mut body = request();
    body["outage"] = json!("Yes");
    let (status, problem) = file(&m, &m.acme, body).await;
    assert_eq!(status, 400, "{problem}");
    assert_eq!(fields(&problem), ["/outage"]);
}

#[tokio::test]
async fn the_desk_not_answering_is_a_503_that_says_when_to_try_again() {
    let m = Mock::start().await;
    m.admin("PUT", "/scenario/support_unavailable", Some(json!(true)))
        .await;

    let (status, headers, problem) = m
        .call(
            "POST",
            "/support_requests",
            Some(&m.acme),
            Some(request()),
            &[],
        )
        .await;

    assert_eq!(status, 503);
    assert_eq!(problem.unwrap()["code"], "support_unavailable");
    assert_eq!(crate::common::header(&headers, "retry-after"), Some("60"));
}
