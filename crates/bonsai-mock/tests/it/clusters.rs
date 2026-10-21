//! Clusters (`listClusters`, `createCluster`, `getCluster`, `updateCluster`,
//! `deleteCluster`, `changeClusterPlan`), the Action engine behind them, and
//! idempotency. Every call goes through `Mock::call`, which validates the
//! response against `openapi/v1.yml`.

use crate::common::{Mock, create_body, header};
use bonsai_mock::{scenario, seed};
use serde_json::{Value, json};

fn ids(body: &Value) -> Vec<String> {
    body["data"]
        .as_array()
        .expect("data array")
        .iter()
        .map(|v| v["id"].as_str().unwrap().to_string())
        .collect()
}

fn detail_fields(body: &Value) -> Vec<Value> {
    body["details"]
        .as_array()
        .expect("details array")
        .iter()
        .map(|d| d["field"].clone())
        .collect()
}

/// A token on `globex` (no payment method) that can write clusters.
fn globex_writer(m: &Mock) -> String {
    seed::create_token(
        &m.db(),
        "globex",
        "writer",
        &["account:read", "clusters:read", "clusters:write"],
        None,
    )
    .unwrap()
}

#[tokio::test]
async fn create_then_poll_to_ready() {
    let m = Mock::start().await;
    let (status, h, body) = m
        .call(
            "POST",
            "/clusters",
            Some(&m.acme),
            Some(create_body("Search Prod", "standard-fir")),
            &[],
        )
        .await;
    assert_eq!(status, 202);
    let action = body.unwrap();
    let id = action["id"].as_str().unwrap();
    assert_eq!(action["type"], "cluster.provision");
    assert_eq!(action["state"], "new");
    assert_eq!(action["steps"], json!([]));
    assert_eq!(
        header(&h, "location").unwrap(),
        format!("{}v1/actions/{id}", m.h.origin)
    );
    assert_eq!(header(&h, "retry-after"), Some("1"));
    assert!(header(&h, "idempotent-replayed").is_none());

    let cluster_id = action["subject"]["id"].as_str().unwrap();
    assert!(cluster_id.starts_with("search-prod-"), "{cluster_id}");

    let (status, h, body) = m
        .call("GET", &format!("/actions/{id}"), Some(&m.acme), None, &[])
        .await;
    assert_eq!(status, 200);
    assert!(header(&h, "retry-after").is_none());
    let done = body.unwrap();
    assert_eq!(done["state"], "success");
    assert!(done["finished_at"].is_string());
    assert_eq!(done["error"], Value::Null);
    assert_eq!(done["result"]["cluster"], cluster_id);
    let credential = done["result"]["credential"].as_str().unwrap();
    let steps: Vec<&str> = done["steps"]
        .as_array()
        .unwrap()
        .iter()
        .map(|s| s["message"].as_str().unwrap())
        .collect();
    assert_eq!(
        steps,
        [
            "Reserving capacity",
            "Provisioning cluster",
            "Configuring routes",
            "Running health check"
        ]
    );
    assert_eq!(done["steps"][3]["state"], "success");

    let cluster = m.cluster(cluster_id).await;
    assert_eq!(cluster["state"], "ready");
    assert_eq!(cluster["name"], "Search Prod");
    assert_eq!(
        cluster["host"],
        format!("{cluster_id}.us-east-1.bonsaisearch.net")
    );
    assert_eq!(cluster["port"], 443);
    assert_eq!(
        cluster["plan"],
        json!({"id": "standard-fir", "name": "Standard Fir"})
    );
    assert_eq!(cluster["region"]["id"], "aws-us-east-1");
    assert_eq!(cluster["space"], Value::Null);
    // The region's first release in offer order is the default.
    assert_eq!(cluster["release"]["id"], "opensearch-3.8.0");
    assert_eq!(cluster["single_tenant"], false);
    // Polled at once, with the numbers usage's generator gives that slot.
    assert!(cluster["stats"]["docs"].as_u64().unwrap() > 0);
    assert_eq!(cluster["stats"]["shards"], 2);
    assert!(cluster["stats"]["polled_at"].is_string());
    assert_eq!(cluster["stats"]["limits"]["shards"], 10);
    assert_eq!(
        cluster["latest_action"],
        json!({"id": id, "type": "cluster.provision", "state": "success"})
    );
    assert_eq!(cluster["terminated_at"], Value::Null);

    let (status, _, body) = m
        .call(
            "GET",
            &format!("/clusters/{cluster_id}/credentials"),
            Some(&m.acme),
            None,
            &[],
        )
        .await;
    assert_eq!(status, 200);
    let creds = body.unwrap();
    assert_eq!(ids(&creds), [credential]);
    assert_eq!(creds["data"][0]["name"], "default");
    assert_eq!(creds["data"][0]["access_type"], "rw");
}

/// As on the Bonsai API: with no release, a create gets the first release the
/// plan offers in the region, OpenSearch first, then newest. A plan's offer can
/// differ by region: Elasticsearch 9 is offered only on GCP.
#[tokio::test]
async fn default_release_is_the_regions_first() {
    let m = Mock::start().await;
    for (plan, region, release) in [
        ("sandbox", "aws-us-east-1", "opensearch-3.8.0"),
        ("standard-fir", "aws-us-west-2", "opensearch-3.8.0"),
        ("business-lg", "gcp-us-east4", "opensearch-3.8.0"),
        (
            "business-lg-esplat",
            "aws-us-east-1",
            "elasticsearch-8.17.5",
        ),
        ("business-lg-esplat", "gcp-us-east4", "elasticsearch-9.4.1"),
    ] {
        let (status, _, body) = m
            .call(
                "POST",
                "/clusters",
                Some(&m.acme),
                Some(json!({"name": plan, "plan": plan, "region": region})),
                &[],
            )
            .await;
        let body = body.unwrap();
        assert_eq!(status, 202, "{plan} in {region}: {body}");
        let cluster = m.cluster(body["subject"]["id"].as_str().unwrap()).await;
        assert_eq!(cluster["release"]["id"], release, "{plan} in {region}");
    }
}

#[tokio::test]
async fn a_provisioning_cluster_has_no_host_or_stats() {
    let m = Mock::start_with(60_000).await;
    let action = m.create("slow", "sandbox").await;
    let cluster = m.cluster(action["subject"]["id"].as_str().unwrap()).await;
    assert_eq!(cluster["state"], "provisioning");
    assert_eq!(cluster["host"], Value::Null);
    assert_eq!(
        cluster["stats"]["docs"],
        Value::Null,
        "never polled is null, not 0"
    );
    assert_eq!(cluster["stats"]["polled_at"], Value::Null);
    assert_eq!(cluster["latest_action"]["state"], "new");
}

#[tokio::test]
async fn create_validation() {
    let m = Mock::start().await;
    let post = |body: Value| m.call("POST", "/clusters", Some(&m.acme), Some(body), &[]);

    // Unknown field: 400 naming it.
    let (status, _, body) =
        post(json!({"name": "x", "plan": "sandbox", "region": "aws-us-east-1", "foo": "1"})).await;
    assert_eq!(status, 400);
    let body = body.unwrap();
    assert_eq!(body["code"], "invalid_parameter");
    assert_eq!(detail_fields(&body), [json!("/foo")]);

    // A value that isn't a string, and a missing required field, in one problem.
    let (status, _, body) = post(json!({"name": 7, "plan": "sandbox"})).await;
    assert_eq!(status, 400);
    let body = body.unwrap();
    assert_eq!(body["code"], "invalid_parameter");
    let fields = detail_fields(&body);
    assert!(fields.contains(&json!("/name")), "{body}");
    assert!(fields.contains(&json!("/region")), "{body}");

    // Not an object.
    let (status, _, body) = post(json!(["name"])).await;
    assert_eq!(status, 400);
    let body = body.unwrap();
    assert_eq!(body["code"], "invalid_parameter");
    assert_eq!(detail_fields(&body), [Value::Null]);
    assert_eq!(
        body["details"][0]["message"],
        "The body must be a JSON object."
    );

    // Unknown plan.
    let (status, _, body) = post(create_body("x", "gold")).await;
    assert_eq!(status, 422);
    let body = body.unwrap();
    assert_eq!(body["code"], "validation_failed");
    assert_eq!(detail_fields(&body), [json!("/plan")]);

    // Unknown region.
    let (status, _, body) = post(json!({"name": "x", "plan": "sandbox", "region": "mars-1"})).await;
    assert_eq!(status, 422);
    assert_eq!(detail_fields(&body.unwrap()), [json!("/region")]);

    // A release the plan doesn't offer.
    let (status, _, body) = post(json!({"name": "x", "plan": "sandbox", "region": "aws-us-east-1", "release": "elasticsearch-7.10.2"})).await;
    assert_eq!(status, 422);
    assert_eq!(detail_fields(&body.unwrap()), [json!("/release")]);

    // A plan not offered in the region: Sandbox is us-east-1 only.
    let (status, _, body) =
        post(json!({"name": "x", "plan": "sandbox", "region": "aws-eu-west-1"})).await;
    assert_eq!(status, 422);
    let body = body.unwrap();
    assert_eq!(detail_fields(&body), [json!("/region")]);
    assert_eq!(
        body["details"][0]["message"],
        "Plan sandbox isn't offered in region aws-eu-west-1."
    );

    // A release the plan offers, but not in this region.
    let (status, _, body) = post(json!({"name": "x", "plan": "standard-fir", "region": "aws-us-west-2", "release": "opensearch-2.19.6"})).await;
    assert_eq!(status, 422);
    let body = body.unwrap();
    assert_eq!(detail_fields(&body), [json!("/release")]);
    assert_eq!(
        body["details"][0]["message"],
        "Release opensearch-2.19.6 isn't offered for plan standard-fir in region aws-us-west-2."
    );

    // An Enterprise plan on its own servers. No public plan is one.
    m.insert_plan("enterprise-dedicated", "business-lg", "enterprise", true);
    let (status, _, body) = post(create_body("x", "enterprise-dedicated")).await;
    assert_eq!(status, 422);
    let body = body.unwrap();
    assert_eq!(body["code"], "validation_failed");
    assert_eq!(detail_fields(&body), [json!("/plan")]);
    assert!(
        body["detail"]
            .as_str()
            .unwrap()
            .contains("needs its own servers; Bonsai support provisions it"),
        "{body}"
    );

    // A space in another region.
    let (status, _, body) = post(json!({"name": "x", "plan": "business-lg", "region": "aws-eu-west-1", "space": "acme-private-use1"})).await;
    assert_eq!(status, 422);
    assert_eq!(detail_fields(&body.unwrap()), [json!("/space")]);

    // Name too long, or empty.
    let (status, _, body) = post(create_body(&"n".repeat(513), "sandbox")).await;
    assert_eq!(status, 422);
    assert_eq!(detail_fields(&body.unwrap()), [json!("/name")]);
    let (status, _, body) = post(create_body("", "sandbox")).await;
    assert_eq!(status, 422);
    assert_eq!(detail_fields(&body.unwrap()), [json!("/name")]);

    // Nothing was created by any of these.
    let (_, _, list) = m.call("GET", "/clusters", Some(&m.acme), None, &[]).await;
    assert_eq!(list.unwrap()["data"], json!([]));

    // A space in the right region is private placement.
    let (status, _, body) = post(json!({"name": "x", "plan": "business-lg", "region": "aws-us-east-1", "space": "acme-private-use1"})).await;
    assert_eq!(status, 202);
    let cluster = m
        .cluster(body.unwrap()["subject"]["id"].as_str().unwrap())
        .await;
    assert_eq!(cluster["space"]["id"], "acme-private-use1");
}

#[tokio::test]
async fn malformed_bodies() {
    let m = Mock::start().await;
    let json_ct = Some("application/json");
    let (status, _, body) = m
        .call_raw("POST", "/clusters", Some(&m.acme), b"{nope", json_ct, &[])
        .await;
    assert_eq!(status, 400);
    assert_eq!(body.unwrap()["code"], "invalid_json");

    for empty in [&b""[..], b"{}", b"  "] {
        let (status, _, body) = m
            .call_raw("POST", "/clusters", Some(&m.acme), empty, json_ct, &[])
            .await;
        assert_eq!(status, 400);
        assert_eq!(body.unwrap()["code"], "body_required");
    }

    let id = m.ready_cluster("raw", "sandbox").await;
    let (status, _, body) = m
        .call_raw(
            "PATCH",
            &format!("/clusters/{id}"),
            Some(&m.acme),
            b"",
            None,
            &[],
        )
        .await;
    assert_eq!(status, 400);
    assert_eq!(body.unwrap()["code"], "body_required");
    let (status, _, body) = m
        .call_raw(
            "POST",
            &format!("/clusters/{id}/plan_changes"),
            Some(&m.acme),
            b"[1,",
            json_ct,
            &[],
        )
        .await;
    assert_eq!(status, 400);
    assert_eq!(body.unwrap()["code"], "invalid_json");
}

#[tokio::test]
async fn writes_need_clusters_write() {
    let m = Mock::start().await;
    let (status, _, body) = m
        .call(
            "POST",
            "/clusters",
            Some(&m.globex),
            Some(create_body("x", "sandbox")),
            &[],
        )
        .await;
    assert_eq!(status, 403);
    let body = body.unwrap();
    assert_eq!(body["code"], "missing_scope");
    assert_eq!(body["details"][0]["scope"], "clusters:write");
}

#[tokio::test]
async fn create_needs_card_for_globex_paid_plan() {
    let m = Mock::start().await;
    let token = globex_writer(&m);
    let key = [("Idempotency-Key", "globex-create-1")];
    let body = create_body("Globex Search", "standard-fir");

    let (status, h, problem) = m
        .call("POST", "/clusters", Some(&token), Some(body.clone()), &key)
        .await;
    assert_eq!(status, 422);
    assert!(header(&h, "location").is_none());
    let problem = problem.unwrap();
    assert_eq!(problem["code"], "payment_method_required");
    assert_eq!(
        problem["action_url"],
        format!("{}_mock/checkout/globex", m.h.origin)
    );
    let (_, _, list) = m.call("GET", "/clusters", Some(&token), None, &[]).await;
    assert_eq!(list.unwrap()["data"], json!([]));

    // A free plan needs no card.
    let (status, _, _) = m
        .call(
            "POST",
            "/clusters",
            Some(&token),
            Some(create_body("Free", "sandbox")),
            &[],
        )
        .await;
    assert_eq!(status, 202);

    // Nothing was recorded against the key, so it works once a card is added.
    let (status, _) = m
        .admin("PUT", "/accounts/globex/payment_method", Some(json!(true)))
        .await;
    assert_eq!(status, 204);
    let (status, h, action) = m
        .call("POST", "/clusters", Some(&token), Some(body), &key)
        .await;
    assert_eq!(status, 202);
    assert!(header(&h, "idempotent-replayed").is_none());
    assert_eq!(action.unwrap()["type"], "cluster.provision");
}

#[tokio::test]
async fn invoice_billing_needs_no_card() {
    let m = Mock::start().await;
    let token = globex_writer(&m);
    m.db()
        .execute(
            "UPDATE accounts SET billed_by_invoice = 1 WHERE id = 'globex'",
            [],
        )
        .unwrap();
    let (status, _, _) = m
        .call(
            "POST",
            "/clusters",
            Some(&token),
            Some(create_body("Invoiced", "standard-fir")),
            &[],
        )
        .await;
    assert_eq!(status, 202);
}

/// `v1.yml`: a `/v1` create bills in the request, so a refused charge is a
/// `422 payment_failed` with nothing built, and the key works again later.
#[tokio::test]
async fn payment_failed_refuses_create() {
    let m = Mock::start().await;
    scenario::set(&m.db(), "fail_next", &json!("payment_failed")).unwrap();

    // A free plan charges nothing: it's created, and the scenario waits.
    m.create("Free", "sandbox").await;
    assert_eq!(
        scenario::get(&m.db(), "fail_next").unwrap(),
        Some(json!("payment_failed"))
    );
    let key = [("Idempotency-Key", "declined-1")];
    let body = create_body("Declined", "standard-fir");

    let (status, h, problem) = m
        .call("POST", "/clusters", Some(&m.acme), Some(body.clone()), &key)
        .await;
    assert_eq!(status, 422);
    assert!(header(&h, "location").is_none());
    let problem = problem.unwrap();
    assert_eq!(problem["code"], "payment_failed");
    assert_eq!(
        problem["action_url"],
        format!("{}_mock/checkout/acme", m.h.origin)
    );
    assert_eq!(scenario::get(&m.db(), "fail_next").unwrap(), None);
    let (_, _, list) = m.call("GET", "/clusters", Some(&m.acme), None, &[]).await;
    let names: Vec<Value> = list.unwrap()["data"]
        .as_array()
        .unwrap()
        .iter()
        .map(|c| c["name"].clone())
        .collect();
    assert_eq!(
        names,
        [json!("Free")],
        "nothing built for the declined create"
    );

    let (status, h, _) = m
        .call("POST", "/clusters", Some(&m.acme), Some(body), &key)
        .await;
    assert_eq!(status, 202);
    assert!(header(&h, "idempotent-replayed").is_none());
}

/// As on the Bonsai API, a create whose Action errors terminates its cluster at the error: it is
/// `deprovisioned`, leaves the list, has no credentials, and can't be deleted.
#[tokio::test]
async fn a_failed_provision_terminates_the_cluster() {
    let m = Mock::start().await;
    scenario::set(&m.db(), "fail_next", &json!("action_failed")).unwrap();
    let action = m.create("Doomed", "standard-fir").await;
    assert_eq!(scenario::get(&m.db(), "fail_next").unwrap(), None);
    let id = action["id"].as_str().unwrap();
    let cluster_id = action["subject"]["id"].as_str().unwrap().to_string();

    let failed = m.action(id).await;
    assert_eq!(failed["state"], "error");
    assert_eq!(failed["error"]["code"], "action_failed");
    assert!(failed["error"].get("action_url").is_none(), "{failed}");
    assert_eq!(failed["result"], Value::Null);
    let steps = failed["steps"].as_array().unwrap();
    assert_eq!(steps.len(), 2, "fails at the second step: {failed}");
    assert_eq!(steps[1]["state"], "error");
    assert_eq!(steps[1]["message"], failed["error"]["message"]);

    let cluster = m.cluster(&cluster_id).await;
    assert_eq!(cluster["state"], "deprovisioned", "{cluster}");
    assert_eq!(cluster["host"], Value::Null);
    assert_eq!(cluster["terminated_at"], failed["finished_at"], "{cluster}");

    let (status, _, list) = m.call("GET", "/clusters", Some(&m.acme), None, &[]).await;
    assert_eq!(status, 200);
    assert_eq!(list.unwrap()["data"], json!([]));

    // The create made its credential before the Action ran; a terminated
    // cluster has none active.
    let (status, _, creds) = m
        .call(
            "GET",
            &format!("/clusters/{cluster_id}/credentials"),
            Some(&m.acme),
            None,
            &[],
        )
        .await;
    assert_eq!(status, 200);
    assert_eq!(creds.unwrap()["data"], json!([]));

    let (status, _, del) = m
        .call(
            "DELETE",
            &format!("/clusters/{cluster_id}?name=Doomed"),
            Some(&m.acme),
            None,
            &[],
        )
        .await;
    assert_eq!(status, 422);
    assert_eq!(del.unwrap()["code"], "cluster_terminated");
}

#[tokio::test]
async fn timed_out_is_its_own_code() {
    let m = Mock::start().await;
    scenario::set(&m.db(), "fail_next", &json!("timed_out")).unwrap();
    let action = m.create("Slowpoke", "sandbox").await;
    let failed = m.action(action["id"].as_str().unwrap()).await;
    assert_eq!(failed["error"]["code"], "timed_out");
    let cluster = m.cluster(action["subject"]["id"].as_str().unwrap()).await;
    assert_eq!(cluster["state"], "deprovisioned");
}

#[tokio::test]
async fn idempotent_replay() {
    let m = Mock::start().await;
    let key = [("Idempotency-Key", "create-abc")];
    let body = create_body("Replayed", "sandbox");
    let (status, _, first) = m
        .call("POST", "/clusters", Some(&m.acme), Some(body.clone()), &key)
        .await;
    assert_eq!(status, 202);
    let first = first.unwrap();

    let (status, h, again) = m
        .call("POST", "/clusters", Some(&m.acme), Some(body), &key)
        .await;
    assert_eq!(status, 202);
    assert_eq!(header(&h, "idempotent-replayed"), Some("true"));
    let again = again.unwrap();
    assert_eq!(again["id"], first["id"]);
    // Re-read as it is now: the Action has finished since.
    assert_eq!(again["state"], "success");
    assert!(
        header(&h, "location")
            .unwrap()
            .ends_with(first["id"].as_str().unwrap())
    );

    let (_, _, list) = m.call("GET", "/clusters", Some(&m.acme), None, &[]).await;
    assert_eq!(list.unwrap()["data"].as_array().unwrap().len(), 1);

    // Same key, another body.
    let (status, _, problem) = m
        .call(
            "POST",
            "/clusters",
            Some(&m.acme),
            Some(create_body("Other", "sandbox")),
            &key,
        )
        .await;
    assert_eq!(status, 422);
    assert_eq!(problem.unwrap()["code"], "idempotency_key_reused");

    // Keys belong to a token: another token's same key is a new request.
    let other = seed::create_token(&m.db(), "acme", "other", seed::ALL_SCOPES, None).unwrap();
    let (status, h, fresh) = m
        .call(
            "POST",
            "/clusters",
            Some(&other),
            Some(create_body("Replayed", "sandbox")),
            &key,
        )
        .await;
    assert_eq!(status, 202);
    assert!(header(&h, "idempotent-replayed").is_none());
    assert_ne!(fresh.unwrap()["id"], first["id"]);
}

#[tokio::test]
async fn idempotent_delete_replays_instead_of_cluster_terminated() {
    let m = Mock::start().await;
    let id = m.ready_cluster("gone", "sandbox").await;
    let key = [("Idempotency-Key", "del-1")];
    let path = format!("/clusters/{id}?name=gone");
    let (status, _, first) = m.call("DELETE", &path, Some(&m.acme), None, &key).await;
    assert_eq!(status, 202);
    let (status, h, again) = m.call("DELETE", &path, Some(&m.acme), None, &key).await;
    assert_eq!(status, 202);
    assert_eq!(header(&h, "idempotent-replayed"), Some("true"));
    assert_eq!(again.unwrap()["id"], first.unwrap()["id"]);
    // Without the key, a retry is told the cluster is gone.
    let (status, _, problem) = m.call("DELETE", &path, Some(&m.acme), None, &[]).await;
    assert_eq!(status, 422);
    assert_eq!(problem.unwrap()["code"], "cluster_terminated");
}

#[tokio::test]
async fn idempotency_key_must_be_printable() {
    let m = Mock::start().await;
    let long = "k".repeat(256);
    for bad in [long.as_str(), "tab\there"] {
        let (status, _, problem) = m
            .call(
                "POST",
                "/clusters",
                Some(&m.acme),
                Some(create_body("x", "sandbox")),
                &[("Idempotency-Key", bad)],
            )
            .await;
        assert_eq!(status, 400, "{bad:?}");
        let problem = problem.unwrap();
        assert_eq!(problem["code"], "invalid_parameter");
        assert_eq!(detail_fields(&problem), [json!("Idempotency-Key")]);
    }
    let ok = "k".repeat(255);
    let (status, _, _) = m
        .call(
            "POST",
            "/clusters",
            Some(&m.acme),
            Some(create_body("x", "sandbox")),
            &[("Idempotency-Key", ok.as_str())],
        )
        .await;
    assert_eq!(status, 202);
}

#[tokio::test]
async fn idempotency_records_expire_after_a_day() {
    let m = Mock::start().await;
    let key = [("Idempotency-Key", "old")];
    let (_, _, first) = m
        .call(
            "POST",
            "/clusters",
            Some(&m.acme),
            Some(create_body("a", "sandbox")),
            &key,
        )
        .await;
    m.db()
        .execute(
            "UPDATE idempotency SET created_at = '2020-01-01T00:00:00Z'",
            [],
        )
        .unwrap();
    // An expired key is free again, even for another body.
    let (status, h, second) = m
        .call(
            "POST",
            "/clusters",
            Some(&m.acme),
            Some(create_body("b", "sandbox")),
            &key,
        )
        .await;
    assert_eq!(status, 202);
    assert!(header(&h, "idempotent-replayed").is_none());
    assert_ne!(second.unwrap()["id"], first.unwrap()["id"]);
}

#[tokio::test]
async fn delete_name_rules() {
    let m = Mock::start().await;
    let id = m.ready_cluster("Search Prod", "sandbox").await;

    let (status, _, problem) = m
        .call(
            "DELETE",
            &format!("/clusters/{id}"),
            Some(&m.acme),
            None,
            &[],
        )
        .await;
    assert_eq!(status, 400);
    let problem = problem.unwrap();
    assert_eq!(problem["code"], "name_confirmation_required");
    assert_eq!(detail_fields(&problem), [json!("name")]);

    let (status, _, problem) = m
        .call(
            "DELETE",
            &format!("/clusters/{id}?name=search%20prod"),
            Some(&m.acme),
            None,
            &[],
        )
        .await;
    assert_eq!(status, 422);
    assert_eq!(problem.unwrap()["code"], "name_mismatch");

    let path = format!("/clusters/{id}?name=Search%20Prod");
    let (status, _, _) = m.call("DELETE", &path, Some(&m.acme), None, &[]).await;
    assert_eq!(status, 202);
    let (status, _, problem) = m.call("DELETE", &path, Some(&m.acme), None, &[]).await;
    assert_eq!(status, 422);
    assert_eq!(problem.unwrap()["code"], "cluster_terminated");

    // Deprovisioned: its credentials are revoked, and it's still shown.
    let cluster = m.cluster(&id).await;
    assert_eq!(cluster["state"], "deprovisioned");
    assert_eq!(cluster["latest_action"]["type"], "cluster.deprovision");
    let (_, _, creds) = m
        .call(
            "GET",
            &format!("/clusters/{id}/credentials"),
            Some(&m.acme),
            None,
            &[],
        )
        .await;
    assert_eq!(creds.unwrap()["data"], json!([]));

    let (status, _, problem) = m
        .call(
            "DELETE",
            "/clusters/nope-123456?name=x",
            Some(&m.acme),
            None,
            &[],
        )
        .await;
    assert_eq!(status, 404);
    assert_eq!(problem.unwrap()["code"], "not_found");
}

#[tokio::test]
async fn deprovisioning_while_the_action_runs() {
    let m = Mock::start_with(60_000).await;
    let action = m.create("slow", "sandbox").await;
    let id = action["subject"]["id"].as_str().unwrap().to_string();
    // Finish the provision by hand, so only the deprovision is slow.
    m.db()
        .execute("UPDATE actions SET started_at = '2020-01-01T00:00:00Z'", [])
        .unwrap();
    assert_eq!(m.cluster(&id).await["state"], "ready");

    let (status, h, _) = m
        .call(
            "DELETE",
            &format!("/clusters/{id}?name=slow"),
            Some(&m.acme),
            None,
            &[],
        )
        .await;
    assert_eq!(status, 202);
    assert!(header(&h, "retry-after").is_some());
    let cluster = m.cluster(&id).await;
    assert_eq!(cluster["state"], "deprovisioning");
    assert!(cluster["host"].is_string(), "{cluster}");
    let (_, _, list) = m.call("GET", "/clusters", Some(&m.acme), None, &[]).await;
    assert_eq!(list.unwrap()["data"], json!([]), "terminated at once");
}

#[tokio::test]
async fn action_in_progress() {
    let m = Mock::start_with(60_000).await;
    let action = m.create("busy", "standard-fir").await;
    let id = action["subject"]["id"].as_str().unwrap().to_string();
    // Finish the provision by hand, so only the plan change is slow.
    m.db()
        .execute("UPDATE actions SET started_at = '2020-01-01T00:00:00Z'", [])
        .unwrap();
    assert_eq!(m.cluster(&id).await["state"], "ready");

    let path = format!("/clusters/{id}/plan_changes");
    let (status, _, _) = m
        .call(
            "POST",
            &path,
            Some(&m.acme),
            Some(json!({"plan": "standard-cork"})),
            &[],
        )
        .await;
    assert_eq!(status, 202);

    // Another plan change waits for the running one.
    let (status, h, problem) = m
        .call(
            "POST",
            &path,
            Some(&m.acme),
            Some(json!({"plan": "sandbox"})),
            &[],
        )
        .await;
    assert_eq!(status, 409);
    let secs: u64 = header(&h, "retry-after").unwrap().parse().unwrap();
    assert!((1..=60).contains(&secs), "{secs}");
    assert_eq!(problem.unwrap()["code"], "action_in_progress");

    // A running plan change doesn't block a delete.
    let (status, _, action) = m
        .call(
            "DELETE",
            &format!("/clusters/{id}?name=busy"),
            Some(&m.acme),
            None,
            &[],
        )
        .await;
    assert_eq!(status, 202);
    assert_eq!(action.unwrap()["type"], "cluster.deprovision");
}

/// As on the Bonsai API: a running provision blocks neither a plan change nor a
/// delete. The delete terminates the cluster at once.
#[tokio::test]
async fn provisioning_blocks_nothing() {
    let m = Mock::start_with(60_000).await;
    let action = m.create("young", "standard-fir").await;
    let id = action["subject"]["id"].as_str().unwrap().to_string();
    assert_eq!(m.cluster(&id).await["state"], "provisioning");

    let (status, _, change) = m
        .call(
            "POST",
            &format!("/clusters/{id}/plan_changes"),
            Some(&m.acme),
            Some(json!({"plan": "standard-cork"})),
            &[],
        )
        .await;
    assert_eq!(status, 202);
    assert_eq!(change.unwrap()["type"], "cluster.change_plan");

    let (status, h, del) = m
        .call(
            "DELETE",
            &format!("/clusters/{id}?name=young"),
            Some(&m.acme),
            None,
            &[],
        )
        .await;
    assert_eq!(status, 202);
    assert!(header(&h, "retry-after").is_some());
    assert_eq!(del.unwrap()["type"], "cluster.deprovision");
    let cluster = m.cluster(&id).await;
    assert_eq!(cluster["state"], "deprovisioning");
    assert_eq!(cluster["latest_action"]["type"], "cluster.deprovision");
}

#[tokio::test]
async fn plan_change_refused_into_enterprise() {
    let m = Mock::start().await;
    // No public plan is Enterprise, so the test adds its own.
    m.insert_plan("enterprise-shared", "business-lg", "enterprise", false);
    m.insert_plan("enterprise-dedicated", "business-lg", "enterprise", true);
    let id = m.ready_cluster("tiered", "standard-fir").await;
    let path = format!("/clusters/{id}/plan_changes");
    for plan in ["enterprise-shared", "enterprise-dedicated"] {
        let (status, _, problem) = m
            .call(
                "POST",
                &path,
                Some(&m.acme),
                Some(json!({"plan": plan})),
                &[],
            )
            .await;
        assert_eq!(status, 422, "{plan}");
        assert_eq!(problem.unwrap()["code"], "plan_change_refused");
    }

    // Out of Enterprise is refused too.
    let ent = m.ready_cluster("ent", "enterprise-shared").await;
    let (status, _, problem) = m
        .call(
            "POST",
            &format!("/clusters/{ent}/plan_changes"),
            Some(&m.acme),
            Some(json!({"plan": "standard-cork"})),
            &[],
        )
        .await;
    assert_eq!(status, 422);
    assert_eq!(problem.unwrap()["code"], "plan_change_refused");

    let (status, _, problem) = m
        .call(
            "POST",
            &path,
            Some(&m.acme),
            Some(json!({"plan": "gold"})),
            &[],
        )
        .await;
    assert_eq!(status, 422);
    let problem = problem.unwrap();
    assert_eq!(problem["code"], "validation_failed");
    assert_eq!(detail_fields(&problem), [json!("/plan")]);

    let (status, _, problem) = m
        .call(
            "POST",
            &path,
            Some(&m.acme),
            Some(json!({"plan": "sandbox", "size": "xl"})),
            &[],
        )
        .await;
    assert_eq!(status, 400);
    assert_eq!(detail_fields(&problem.unwrap()), [json!("/size")]);
}

#[tokio::test]
async fn plan_change_needs_a_card_for_a_paid_plan() {
    let m = Mock::start().await;
    let token = globex_writer(&m);
    let (_, _, action) = m
        .call(
            "POST",
            "/clusters",
            Some(&token),
            Some(create_body("free", "sandbox")),
            &[],
        )
        .await;
    let id = action.unwrap()["subject"]["id"]
        .as_str()
        .unwrap()
        .to_string();
    let (status, _, problem) = m
        .call(
            "POST",
            &format!("/clusters/{id}/plan_changes"),
            Some(&token),
            Some(json!({"plan": "standard-fir"})),
            &[],
        )
        .await;
    assert_eq!(status, 422);
    let problem = problem.unwrap();
    assert_eq!(problem["code"], "payment_method_required");
    assert!(
        problem["action_url"]
            .as_str()
            .unwrap()
            .ends_with("/_mock/checkout/globex")
    );
}

#[tokio::test]
async fn plan_change_ok() {
    let m = Mock::start().await;
    let id = m.ready_cluster("grow", "standard-fir").await;
    let key = [("Idempotency-Key", "grow-1")];
    let (status, h, action) = m
        .call(
            "POST",
            &format!("/clusters/{id}/plan_changes"),
            Some(&m.acme),
            Some(json!({"plan": "standard-cork"})),
            &key,
        )
        .await;
    assert_eq!(status, 202);
    let action = action.unwrap();
    assert_eq!(action["type"], "cluster.change_plan");
    assert!(header(&h, "location").is_some());

    let done = m.action(action["id"].as_str().unwrap()).await;
    assert_eq!(done["state"], "success");
    assert_eq!(done["result"], Value::Null);
    assert_eq!(done["steps"].as_array().unwrap().len(), 3);
    let cluster = m.cluster(&id).await;
    assert_eq!(cluster["plan"]["id"], "standard-cork");
    assert_eq!(cluster["stats"]["limits"]["shards"], 60);
    assert_eq!(cluster["state"], "ready");

    // Replayed with the same key: the same Action.
    let (status, h, again) = m
        .call(
            "POST",
            &format!("/clusters/{id}/plan_changes"),
            Some(&m.acme),
            Some(json!({"plan": "standard-cork"})),
            &key,
        )
        .await;
    assert_eq!(status, 202);
    assert_eq!(header(&h, "idempotent-replayed"), Some("true"));
    assert_eq!(again.unwrap()["id"], action["id"]);
}

#[tokio::test]
async fn rename() {
    let m = Mock::start().await;
    let id = m.ready_cluster("old name", "sandbox").await;
    let path = format!("/clusters/{id}");
    let (status, _, body) = m
        .call(
            "PATCH",
            &path,
            Some(&m.acme),
            Some(json!({"name": "New Name"})),
            &[],
        )
        .await;
    assert_eq!(status, 200);
    let body = body.unwrap();
    assert_eq!(body["name"], "New Name");
    assert_eq!(body["id"], id, "the ID never changes");

    let (status, _, problem) = m
        .call(
            "PATCH",
            &path,
            Some(&m.acme),
            Some(json!({"plan": "x"})),
            &[],
        )
        .await;
    assert_eq!(status, 400);
    let fields = detail_fields(&problem.unwrap());
    assert!(fields.contains(&json!("/plan")));
    assert!(fields.contains(&json!("/name")));

    let (status, _, problem) = m
        .call(
            "PATCH",
            &path,
            Some(&m.acme),
            Some(json!({"name": "n".repeat(513)})),
            &[],
        )
        .await;
    assert_eq!(status, 422);
    assert_eq!(detail_fields(&problem.unwrap()), [json!("/name")]);

    let (status, _, _) = m
        .call(
            "PATCH",
            "/clusters/nope-000000",
            Some(&m.acme),
            Some(json!({"name": "x"})),
            &[],
        )
        .await;
    assert_eq!(status, 404);

    m.call(
        "DELETE",
        &format!("{path}?name=New%20Name"),
        Some(&m.acme),
        None,
        &[],
    )
    .await;
    let (status, _, problem) = m
        .call(
            "PATCH",
            &path,
            Some(&m.acme),
            Some(json!({"name": "again"})),
            &[],
        )
        .await;
    assert_eq!(status, 422);
    assert_eq!(problem.unwrap()["code"], "cluster_terminated");
}

#[tokio::test]
async fn other_accounts_clusters_are_not_found() {
    let m = Mock::start().await;
    let id = m.ready_cluster("mine", "sandbox").await;
    let (status, _, problem) = m
        .call(
            "GET",
            &format!("/clusters/{id}"),
            Some(&m.globex),
            None,
            &[],
        )
        .await;
    assert_eq!(status, 404);
    assert_eq!(problem.unwrap()["code"], "not_found");
    let (_, _, list) = m.call("GET", "/clusters", Some(&m.globex), None, &[]).await;
    assert_eq!(list.unwrap()["data"], json!([]));
}

#[tokio::test]
async fn list_filters_and_terminated_hidden() {
    let m = Mock::start().await;
    let a = m.ready_cluster("Alpha Search", "sandbox").await;
    let b = m.ready_cluster("Beta", "business-lg").await;
    let (status, _, body) = m
        .call(
            "POST",
            "/clusters",
            Some(&m.acme),
            Some(json!({"name": "Private alpha", "plan": "business-lg", "region": "aws-us-east-1", "space": "acme-private-use1"})),
            &[],
        )
        .await;
    assert_eq!(status, 202);
    let p = body.unwrap()["subject"]["id"].as_str().unwrap().to_string();
    let (status, _, body) = m
        .call(
            "POST",
            "/clusters",
            Some(&m.acme),
            Some(json!({"name": "Gamma", "plan": "standard-fir", "region": "aws-eu-west-1"})),
            &[],
        )
        .await;
    assert_eq!(status, 202);
    let g = body.unwrap()["subject"]["id"].as_str().unwrap().to_string();
    let doomed = m.ready_cluster("Doomed alpha", "sandbox").await;
    m.call(
        "DELETE",
        &format!("/clusters/{doomed}?name=Doomed%20alpha"),
        Some(&m.acme),
        None,
        &[],
    )
    .await;

    let mr = &m;
    let list = |q: &'static str| async move {
        let (status, _, body) = mr
            .call("GET", &format!("/clusters{q}"), Some(&mr.acme), None, &[])
            .await;
        assert_eq!(status, 200, "{q}");
        ids(&body.unwrap())
    };
    // Newest first; the terminated one isn't listed.
    assert_eq!(
        list("").await,
        [g.as_str(), p.as_str(), b.as_str(), a.as_str()]
    );
    assert_eq!(list("?name=ALPHA").await, [p.as_str(), a.as_str()]);
    assert_eq!(list("?region=aws-eu-west-1").await, [g.as_str()]);
    assert_eq!(list("?region=nowhere").await, Vec::<String>::new());
    assert_eq!(list("?space=acme-private-use1").await, [p.as_str()]);
    assert_eq!(list("?single_tenant=false").await, [g.as_str(), a.as_str()]);
    assert_eq!(list("?single_tenant=true").await, [p.as_str(), b.as_str()]);
    assert_eq!(
        list("?state=ready").await,
        [g.as_str(), p.as_str(), b.as_str(), a.as_str()]
    );
    assert_eq!(list("?state=deprovisioned").await, Vec::<String>::new());
    assert_eq!(list("?limit=2").await, [g.as_str(), p.as_str()]);

    for (q, field) in [
        ("?state=sleeping", "state"),
        ("?single_tenant=yes", "single_tenant"),
        ("?limit=0", "limit"),
    ] {
        let (status, _, problem) = m
            .call("GET", &format!("/clusters{q}"), Some(&m.acme), None, &[])
            .await;
        assert_eq!(status, 400, "{q}");
        assert_eq!(detail_fields(&problem.unwrap()), [json!(field)]);
    }
    let long = format!("/clusters?name={}", "n".repeat(513));
    let (status, _, _) = m.call("GET", &long, Some(&m.acme), None, &[]).await;
    assert_eq!(status, 400);

    // The terminated cluster is still shown by ID.
    assert_eq!(m.cluster(&doomed).await["state"], "deprovisioned");
}

#[tokio::test]
async fn single_tenant_follows_the_plan() {
    let m = Mock::start().await;
    let st = m.ready_cluster("st", "business-lg").await;
    let mt = m.ready_cluster("mt", "sandbox").await;
    let (_, _, body) = m
        .call(
            "GET",
            "/clusters?single_tenant=true",
            Some(&m.acme),
            None,
            &[],
        )
        .await;
    assert_eq!(ids(&body.unwrap()), [st.as_str()]);
    let (_, _, body) = m
        .call(
            "GET",
            "/clusters?single_tenant=false",
            Some(&m.acme),
            None,
            &[],
        )
        .await;
    assert_eq!(ids(&body.unwrap()), [mt.as_str()]);
}
