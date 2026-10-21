//! Credentials (`listClusterCredentials`, `createClusterCredential`,
//! `getClusterCredential`, `revokeClusterCredential`). Every call goes through
//! `Mock::call`, which validates the response against `openapi/v1.yml`.

mod common;

use bonsai_mock::seed;
use common::{Mock, header};
use serde_json::{Value, json};

const LAST_RW: &str = "The cluster's last active rw credential can't be revoked.";

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

fn creds(cluster: &str) -> String {
    format!("/clusters/{cluster}/credentials")
}

fn cred(cluster: &str, id: &str) -> String {
    format!("/clusters/{cluster}/credentials/{id}")
}

async fn post(m: &Mock, cluster: &str, body: Value) -> (u16, Option<Value>) {
    let (status, _, body) = m
        .call("POST", &creds(cluster), Some(&m.acme), Some(body), &[])
        .await;
    (status, body)
}

async fn list(m: &Mock, cluster: &str) -> Vec<String> {
    let (status, _, body) = m
        .call("GET", &creds(cluster), Some(&m.acme), None, &[])
        .await;
    assert_eq!(status, 200);
    ids(&body.unwrap())
}

#[tokio::test]
async fn create_list_get_revoke_round_trip() {
    let m = Mock::start().await;
    let cluster = m.ready_cluster("Creds", "standard-fir").await;
    let default = list(&m, &cluster).await;
    assert_eq!(default.len(), 1);

    let (status, h, body) = m
        .call(
            "POST",
            &creds(&cluster),
            Some(&m.acme),
            Some(json!({"name": "reader", "access_type": "ro"})),
            &[],
        )
        .await;
    assert_eq!(status, 201);
    let created = body.unwrap();
    let id = created["id"].as_str().unwrap().to_string();
    assert_eq!(
        header(&h, "location").unwrap(),
        format!("{}v1/clusters/{cluster}/credentials/{id}", m.h.origin)
    );
    assert_eq!(header(&h, "cache-control"), Some("no-store"));
    assert!(header(&h, "idempotent-replayed").is_none());
    assert_eq!(created["name"], "reader");
    assert_eq!(created["access_type"], "ro");
    assert_eq!(created["allowed_actions"], json!([]));
    assert_eq!(created["allowed_index_globs"], json!([]));
    assert_eq!(created["allowed_cidrs"], json!([]));
    let (user, pass) = (
        created["username"].as_str().unwrap(),
        created["password"].as_str().unwrap(),
    );
    let host = m.cluster(&cluster).await["host"]
        .as_str()
        .unwrap()
        .to_string();
    assert_eq!(created["url"], format!("https://{user}:{pass}@{host}"));

    // Newest first, and no secrets in the list.
    let (status, _, page) = m
        .call("GET", &creds(&cluster), Some(&m.acme), None, &[])
        .await;
    assert_eq!(status, 200);
    let page = page.unwrap();
    assert_eq!(ids(&page), [id.clone(), default[0].clone()]);
    assert!(page["data"][0].get("password").is_none());
    assert!(page["data"][0].get("url").is_none());

    let (status, h, shown) = m
        .call("GET", &cred(&cluster, &id), Some(&m.acme), None, &[])
        .await;
    assert_eq!(status, 200);
    assert_eq!(header(&h, "cache-control"), Some("no-store"));
    assert_eq!(shown.unwrap(), created);

    let (status, _, body) = m
        .call("DELETE", &cred(&cluster, &id), Some(&m.acme), None, &[])
        .await;
    assert_eq!(status, 204);
    assert_eq!(body, None);

    // A revoked credential is gone from the API.
    let (status, _, problem) = m
        .call("GET", &cred(&cluster, &id), Some(&m.acme), None, &[])
        .await;
    assert_eq!(status, 404);
    assert_eq!(problem.unwrap()["code"], "not_found");
    let (status, _, _) = m
        .call("DELETE", &cred(&cluster, &id), Some(&m.acme), None, &[])
        .await;
    assert_eq!(status, 404);
    assert_eq!(list(&m, &cluster).await, default);
}

#[tokio::test]
async fn unknown_cluster_or_credential_is_404() {
    let m = Mock::start().await;
    let cluster = m.ready_cluster("Creds", "standard-fir").await;
    for (method, path) in [
        ("GET", creds("nope")),
        ("GET", cred(&cluster, "cred-nope")),
        ("DELETE", cred(&cluster, "cred-nope")),
    ] {
        let (status, _, problem) = m.call(method, &path, Some(&m.acme), None, &[]).await;
        assert_eq!(status, 404, "{method} {path}");
        assert_eq!(problem.unwrap()["code"], "not_found");
    }
    let (status, _) = post(&m, "nope", json!({"name": "a", "access_type": "rw"})).await;
    assert_eq!(status, 404);
}

#[tokio::test]
async fn the_last_rw_credential_cant_be_revoked() {
    let m = Mock::start().await;
    let cluster = m.ready_cluster("Creds", "standard-fir").await;
    let default = list(&m, &cluster).await.remove(0);

    let (status, _, problem) = m
        .call(
            "DELETE",
            &cred(&cluster, &default),
            Some(&m.acme),
            None,
            &[],
        )
        .await;
    assert_eq!(status, 422);
    let problem = problem.unwrap();
    assert_eq!(problem["code"], "validation_failed");
    assert_eq!(problem["detail"], LAST_RW);
    assert_eq!(
        problem["details"],
        json!([{"field": null, "message": LAST_RW}])
    );

    // An ro credential doesn't count as rw.
    let (status, ro) = post(&m, &cluster, json!({"name": "ro", "access_type": "ro"})).await;
    assert_eq!(status, 201);
    let (status, _, _) = m
        .call(
            "DELETE",
            &cred(&cluster, &default),
            Some(&m.acme),
            None,
            &[],
        )
        .await;
    assert_eq!(status, 422);
    let ro = ro.unwrap()["id"].as_str().unwrap().to_string();
    let (status, _, _) = m
        .call("DELETE", &cred(&cluster, &ro), Some(&m.acme), None, &[])
        .await;
    assert_eq!(status, 204);

    // With a second rw, either can go, but not both.
    let (status, rw) = post(&m, &cluster, json!({"name": "rw2", "access_type": "rw"})).await;
    assert_eq!(status, 201);
    let rw = rw.unwrap()["id"].as_str().unwrap().to_string();
    let (status, _, _) = m
        .call(
            "DELETE",
            &cred(&cluster, &default),
            Some(&m.acme),
            None,
            &[],
        )
        .await;
    assert_eq!(status, 204);
    let (status, _, problem) = m
        .call("DELETE", &cred(&cluster, &rw), Some(&m.acme), None, &[])
        .await;
    assert_eq!(status, 422);
    assert_eq!(problem.unwrap()["detail"], LAST_RW);
    assert_eq!(list(&m, &cluster).await, [rw]);
}

#[tokio::test]
async fn a_hobby_cluster_has_one_credential() {
    let m = Mock::start().await;
    let cluster = m.ready_cluster("Hobby", "sandbox").await;
    let (status, problem) = post(&m, &cluster, json!({"name": "two", "access_type": "rw"})).await;
    assert_eq!(status, 422);
    let problem = problem.unwrap();
    assert_eq!(problem["code"], "validation_failed");
    assert_eq!(
        problem["details"],
        json!([{"field": null, "message": "A Hobby cluster may have one active credential."}])
    );
    assert_eq!(list(&m, &cluster).await.len(), 1);
}

#[tokio::test]
async fn rwm_and_acl_need_the_feature() {
    let m = Mock::start().await;
    let token = seed::create_token(
        &m.db(),
        "globex",
        "writer",
        &[
            "clusters:read",
            "clusters:write",
            "credentials:read",
            "credentials:write",
        ],
        None,
    )
    .unwrap();
    // Sandbox is free, so globex needs no card for it.
    let (status, _, action) = m
        .call(
            "POST",
            "/clusters",
            Some(&token),
            Some(common::create_body("Globex", "sandbox")),
            &[],
        )
        .await;
    assert_eq!(status, 202);
    let cluster = action.unwrap()["subject"]["id"]
        .as_str()
        .unwrap()
        .to_string();
    for access_type in ["rwm", "acl"] {
        let (status, _, problem) = m
            .call(
                "POST",
                &creds(&cluster),
                Some(&token),
                Some(json!({"name": "scoped", "access_type": access_type})),
                &[],
            )
            .await;
        assert_eq!(status, 403, "{access_type}");
        let problem = problem.unwrap();
        assert_eq!(problem["code"], "feature_not_enabled");
        assert_eq!(
            problem["detail"],
            format!("{access_type} credentials are not enabled for this token's owner.")
        );
    }
}

#[tokio::test]
async fn access_types_follow_the_plan_tier() {
    let m = Mock::start().await;
    let standard = m.ready_cluster("Standard", "standard-fir").await;
    let (status, problem) = post(
        &m,
        &standard,
        json!({"name": "scoped", "access_type": "rwm"}),
    )
    .await;
    assert_eq!(status, 422);
    let problem = problem.unwrap();
    assert_eq!(problem["code"], "validation_failed");
    assert_eq!(problem["detail"], "The credential is not valid.");
    assert_eq!(
        problem["details"],
        json!([{
            "field": "/access_type",
            "message": "Access type Standard clusters are limited to rw, ro access types"
        }])
    );

    let business = m.ready_cluster("Business", "business-lg").await;
    let (status, body) = post(
        &m,
        &business,
        json!({
            "name": "app",
            "access_type": "acl",
            "allowed_actions": ["indices:data/read/*", "*"],
            "allowed_index_globs": ["logs-*"],
            "allowed_cidrs": ["203.0.113.0/24", " "],
        }),
    )
    .await;
    assert_eq!(status, 201);
    let body = body.unwrap();
    assert_eq!(body["access_type"], "acl");
    // "*" and blank entries are dropped, as the Bonsai API does.
    assert_eq!(body["allowed_actions"], json!(["indices:data/read/*"]));
    assert_eq!(body["allowed_index_globs"], json!(["logs-*"]));
    assert_eq!(body["allowed_cidrs"], json!(["203.0.113.0/24"]));
}

#[tokio::test]
async fn create_body_rules() {
    let m = Mock::start().await;
    let cluster = m.ready_cluster("Body", "standard-fir").await;
    let cases = [
        (json!({"name": "a"}), "/access_type"),
        (json!({"name": "a", "access_type": 1}), "/access_type"),
        (json!({"access_type": "rw"}), "/name"),
        (json!({"name": "", "access_type": "rw"}), "/name"),
        (
            json!({"name": "x".repeat(256), "access_type": "rw"}),
            "/name",
        ),
        (
            json!({"name": "a", "access_type": "rw", "allowed_cidrs": "10.0.0.0/8"}),
            "/allowed_cidrs",
        ),
        (
            json!({"name": "a", "access_type": "rw", "allowed_actions": [1]}),
            "/allowed_actions",
        ),
    ];
    for (body, field) in cases {
        let (status, problem) = post(&m, &cluster, body.clone()).await;
        assert_eq!(status, 400, "{body}");
        let problem = problem.unwrap();
        assert_eq!(problem["code"], "invalid_parameter", "{body}");
        assert_eq!(detail_fields(&problem), [json!(field)], "{body}");
    }

    let (status, problem) = post(&m, &cluster, json!(["rw"])).await;
    assert_eq!(status, 400);
    assert_eq!(detail_fields(&problem.unwrap()), [Value::Null]);

    let (status, _, problem) = m
        .call_raw(
            "POST",
            &creds(&cluster),
            Some(&m.acme),
            b"",
            Some("application/json"),
            &[],
        )
        .await;
    assert_eq!(status, 400);
    assert_eq!(problem.unwrap()["code"], "body_required");

    let (status, problem) = post(&m, &cluster, json!({"name": "a", "access_type": "admin"})).await;
    assert_eq!(status, 422);
    let problem = problem.unwrap();
    assert_eq!(problem["code"], "validation_failed");
    assert_eq!(detail_fields(&problem), [json!("/access_type")]);

    // A blank name passes the schema but not the record.
    let (status, problem) = post(&m, &cluster, json!({"name": "  ", "access_type": "rw"})).await;
    assert_eq!(status, 422);
    assert_eq!(
        problem.unwrap()["details"],
        json!([{"field": "/name", "message": "Name can't be blank"}])
    );

    // Fields the schema doesn't name are ignored, as the Bonsai API ignores them.
    let (status, _) = post(
        &m,
        &cluster,
        json!({"name": "a", "access_type": "rw", "extra": true}),
    )
    .await;
    assert_eq!(status, 201);
}

#[tokio::test]
async fn the_cluster_must_be_ready() {
    let m = Mock::start_with(60_000).await;
    let action = m.create("Slow", "standard-fir").await;
    let cluster = action["subject"]["id"].as_str().unwrap();
    let (status, problem) = post(&m, cluster, json!({"name": "a", "access_type": "rw"})).await;
    assert_eq!(status, 422);
    let problem = problem.unwrap();
    assert_eq!(problem["code"], "validation_failed");
    assert_eq!(
        problem["details"],
        json!([{
            "field": null,
            "message": "The cluster must be ready before it can get new credentials."
        }])
    );
}

#[tokio::test]
async fn a_terminated_cluster_has_no_credentials() {
    let m = Mock::start().await;
    let cluster = m.ready_cluster("gone", "standard-fir").await;
    let default = list(&m, &cluster).await.remove(0);
    let (status, _, _) = m
        .call(
            "DELETE",
            &format!("/clusters/{cluster}?name=gone"),
            Some(&m.acme),
            None,
            &[],
        )
        .await;
    assert_eq!(status, 202);

    assert!(list(&m, &cluster).await.is_empty());
    let (status, _, _) = m
        .call("GET", &cred(&cluster, &default), Some(&m.acme), None, &[])
        .await;
    assert_eq!(status, 404);
    let (status, problem) = post(&m, &cluster, json!({"name": "a", "access_type": "rw"})).await;
    assert_eq!(status, 422);
    assert_eq!(problem.unwrap()["code"], "cluster_terminated");
}

#[tokio::test]
async fn scopes() {
    let m = Mock::start().await;
    let cluster = m.ready_cluster("Scoped", "standard-fir").await;
    let default = list(&m, &cluster).await.remove(0);
    let reader = seed::create_token(
        &m.db(),
        "acme",
        "reader",
        &["clusters:read", "credentials:read"],
        None,
    )
    .unwrap();

    for (method, path, body) in [
        (
            "POST",
            creds(&cluster),
            Some(json!({"name": "a", "access_type": "rw"})),
        ),
        ("DELETE", cred(&cluster, &default), None),
    ] {
        let (status, _, problem) = m.call(method, &path, Some(&reader), body, &[]).await;
        assert_eq!(status, 403, "{method}");
        let problem = problem.unwrap();
        assert_eq!(problem["code"], "missing_scope");
        assert_eq!(problem["details"][0]["scope"], "credentials:write");
    }

    // globex's seeded token reads clusters, not credentials.
    for path in [creds(&cluster), cred(&cluster, &default)] {
        let (status, _, problem) = m.call("GET", &path, Some(&m.globex), None, &[]).await;
        assert_eq!(status, 403);
        assert_eq!(problem.unwrap()["details"][0]["scope"], "credentials:read");
    }
}

#[tokio::test]
async fn idempotent_create() {
    let m = Mock::start().await;
    let cluster = m.ready_cluster("Idem", "standard-fir").await;
    let key = [("Idempotency-Key", "cred-1")];
    let body = json!({"name": "app", "access_type": "rw", "allowed_cidrs": []});
    let (status, _, first) = m
        .call(
            "POST",
            &creds(&cluster),
            Some(&m.acme),
            Some(body.clone()),
            &key,
        )
        .await;
    assert_eq!(status, 201);
    let first = first.unwrap();

    let (status, h, again) = m
        .call(
            "POST",
            &creds(&cluster),
            Some(&m.acme),
            Some(body.clone()),
            &key,
        )
        .await;
    assert_eq!(status, 201);
    assert_eq!(header(&h, "idempotent-replayed"), Some("true"));
    let id = first["id"].as_str().unwrap();
    assert!(header(&h, "location").unwrap().ends_with(id));
    assert_eq!(again.unwrap(), first);
    assert_eq!(list(&m, &cluster).await.len(), 2);

    // Same key, another body.
    let (status, _, problem) = m
        .call(
            "POST",
            &creds(&cluster),
            Some(&m.acme),
            Some(json!({"name": "app", "access_type": "rw", "allowed_cidrs": ["10.0.0.0/8"]})),
            &key,
        )
        .await;
    assert_eq!(status, 422);
    assert_eq!(problem.unwrap()["code"], "idempotency_key_reused");

    // Once revoked, the credential can't be replayed.
    let (status, _, _) = m
        .call("DELETE", &cred(&cluster, id), Some(&m.acme), None, &[])
        .await;
    assert_eq!(status, 204);
    let (status, h, problem) = m
        .call("POST", &creds(&cluster), Some(&m.acme), Some(body), &key)
        .await;
    assert_eq!(status, 409);
    assert!(header(&h, "retry-after").is_none());
    assert_eq!(problem.unwrap()["code"], "idempotency_replay_unavailable");
}

#[tokio::test]
async fn a_refused_create_doesnt_record_its_key() {
    let m = Mock::start().await;
    let cluster = m.ready_cluster("Idem", "standard-fir").await;
    let key = [("Idempotency-Key", "cred-2")];
    let (status, _, _) = m
        .call(
            "POST",
            &creds(&cluster),
            Some(&m.acme),
            Some(json!({"name": "a", "access_type": "rwm"})),
            &key,
        )
        .await;
    assert_eq!(status, 422);
    let (status, h, _) = m
        .call(
            "POST",
            &creds(&cluster),
            Some(&m.acme),
            Some(json!({"name": "a", "access_type": "ro"})),
            &key,
        )
        .await;
    assert_eq!(status, 201);
    assert!(header(&h, "idempotent-replayed").is_none());
}
