//! Actions (`listClusterActions`, `getAction`) and how they progress over
//! time. Every call goes through `Mock::call`, which validates the response
//! against `openapi/v1.yml`.

use crate::common::{Mock, header};
use serde_json::json;

fn is_action_id(id: &str) -> bool {
    id.len() == 20
        && id.starts_with("act_")
        && id[4..]
            .bytes()
            .all(|b| b.is_ascii_digit() || b.is_ascii_lowercase())
}

#[tokio::test]
async fn action_ids_are_opaque_slugs() {
    let m = Mock::start().await;
    let action = m.create("slugs", "sandbox").await;
    let id = action["id"].as_str().unwrap();
    assert!(is_action_id(id), "{id}");

    for bad in [
        "123",
        "act_123",
        "act_ABCDEFGHIJKLMNOP",
        "act_0000000000000000",
    ] {
        let (status, _, problem) = m
            .call("GET", &format!("/actions/{bad}"), Some(&m.acme), None, &[])
            .await;
        assert_eq!(status, 404, "{bad}");
        assert_eq!(problem.unwrap()["code"], "not_found");
    }

    // Another account's Action is a 404 like a missing one.
    let (status, _, _) = m
        .call("GET", &format!("/actions/{id}"), Some(&m.globex), None, &[])
        .await;
    assert_eq!(status, 404);
}

#[tokio::test]
async fn retry_after_while_running() {
    let m = Mock::start_with(2000).await;
    let action = m.create("polled", "sandbox").await;
    let id = action["id"].as_str().unwrap();
    let (status, h, body) = m
        .call("GET", &format!("/actions/{id}"), Some(&m.acme), None, &[])
        .await;
    assert_eq!(status, 200);
    let body = body.unwrap();
    assert!(
        matches!(body["state"].as_str(), Some("new" | "running")),
        "{body}"
    );
    assert_eq!(body["finished_at"], serde_json::Value::Null);
    let secs: u64 = header(&h, "retry-after").unwrap().parse().unwrap();
    assert!((1..=2).contains(&secs), "Retry-After {secs}");
}

#[tokio::test]
async fn lists_a_clusters_actions_newest_first() {
    let m = Mock::start().await;
    let created = m.create("history", "standard-fir").await;
    let cluster = created["subject"]["id"].as_str().unwrap().to_string();
    m.action(created["id"].as_str().unwrap()).await;
    let (status, _, change) = m
        .call(
            "POST",
            &format!("/clusters/{cluster}/plan_changes"),
            Some(&m.acme),
            Some(json!({"plan": "standard-cork"})),
            &[],
        )
        .await;
    assert_eq!(status, 202);
    let change = change.unwrap();

    let path = format!("/clusters/{cluster}/actions");
    let (status, _, body) = m.call("GET", &path, Some(&m.acme), None, &[]).await;
    assert_eq!(status, 200);
    let body = body.unwrap();
    let types: Vec<&str> = body["data"]
        .as_array()
        .unwrap()
        .iter()
        .map(|a| a["type"].as_str().unwrap())
        .collect();
    assert_eq!(types, ["cluster.change_plan", "cluster.provision"]);
    assert_eq!(body["data"][0]["id"], change["id"]);
    // Listing reads the Actions, so they've advanced.
    assert_eq!(body["data"][0]["state"], "success");
    assert_eq!(body["next_cursor"], serde_json::Value::Null);

    let (status, _, page) = m
        .call("GET", &format!("{path}?limit=1"), Some(&m.acme), None, &[])
        .await;
    assert_eq!(status, 200);
    let page = page.unwrap();
    assert_eq!(page["data"].as_array().unwrap().len(), 1);
    let cursor = page["next_cursor"].as_str().unwrap();
    let (_, _, rest) = m
        .call(
            "GET",
            &format!("{path}?limit=1&cursor={cursor}"),
            Some(&m.acme),
            None,
            &[],
        )
        .await;
    assert_eq!(rest.unwrap()["data"][0]["id"], created["id"]);

    let (status, _, _) = m
        .call(
            "GET",
            "/clusters/nope-000000/actions",
            Some(&m.acme),
            None,
            &[],
        )
        .await;
    assert_eq!(status, 404);
    let (status, _, _) = m.call("GET", &path, Some(&m.globex), None, &[]).await;
    assert_eq!(status, 404);
}
