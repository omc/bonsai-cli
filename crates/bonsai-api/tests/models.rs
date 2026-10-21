use bonsai_api::*;
use bonsai_contract::Contract;
use serde::{Serialize, de::DeserializeOwned};
use serde_json::{Value, json};
use std::fmt::Debug;

/// The sample must satisfy the contract, deserialize, and re-serialize to the same JSON.
fn check<T: Serialize + DeserializeOwned + Debug + Clone + PartialEq>(schema: &str, sample: Value) {
    Contract::load_default()
        .validate_schema(schema, &sample)
        .unwrap_or_else(|v| panic!("sample for {schema} violates the contract: {v:?}"));
    let model: T = serde_json::from_value(sample.clone())
        .unwrap_or_else(|e| panic!("{schema} did not deserialize: {e}"));
    assert_eq!(serde_json::to_value(&model).unwrap(), sample, "{schema}");
}

const T: &str = "2026-01-01T00:00:00Z";

fn rf(id: &str) -> Value {
    json!({"id": id, "name": format!("{id} name")})
}

fn cluster(full: bool) -> Value {
    if full {
        json!({
            "id": "c-1", "name": "prod", "state": "ready", "plan": rf("standard"),
            "region": rf("aws-us-east-1"), "space": rf("sp-1"), "release": rf("opensearch-3.8.0"),
            "single_tenant": true, "host": "c-1.example.com", "port": 443,
            "stats": {"docs": 10, "shards": 2, "bytes": 2048, "polled_at": T,
                      "limits": {"docs": 100, "shards": null, "bytes": 4096, "daily_bandwidth_bytes": null}},
            "latest_action": {"id": "act_1", "type": "cluster.provision", "state": "success"},
            "created_at": T, "terminated_at": "2026-02-01T00:00:00Z"
        })
    } else {
        json!({
            "id": "c-2", "name": "draft", "state": "draft", "plan": null,
            "region": rf("aws-us-east-1"), "space": null, "release": rf("opensearch-3.8.0"),
            "single_tenant": false, "host": null, "port": 443,
            "stats": {"docs": null, "shards": null, "bytes": null, "polled_at": null, "limits": null},
            "latest_action": null, "created_at": T, "terminated_at": null
        })
    }
}

fn action(full: bool) -> Value {
    if full {
        json!({
            "id": "act_1", "type": "cluster.provision", "state": "success",
            "subject": {"type": "cluster", "id": "c-1"}, "created_at": T, "finished_at": T,
            "error": null, "result": {"cluster": "c-1", "credential": null},
            "steps": [{"state": "success", "message": "done", "at": T}]
        })
    } else {
        json!({
            "id": "act_2", "type": "cluster.deprovision", "state": "error",
            "subject": {"type": "cluster", "id": "c-1"}, "created_at": T, "finished_at": null,
            "error": {"code": "payment_failed", "message": "no card", "action_url": "https://example.com/pay"},
            "result": null, "steps": []
        })
    }
}

fn credential() -> Value {
    json!({"id": "cr-1", "name": null, "access_type": "acl", "allowed_actions": ["indices:data/read"],
           "allowed_index_globs": ["logs-*"], "allowed_cidrs": ["203.0.113.0/24"], "created_at": T})
}

#[test]
fn api_root() {
    check::<ApiRoot>(
        "ApiRoot",
        json!({"name": "Bonsai API", "version": "1.0.0", "docs_url": "https://bonsai.io/docs", "openapi_url": "https://api.bonsai.io/v1/openapi.json"}),
    );
}

#[test]
fn account() {
    check::<Account>(
        "Account",
        json!({"id": "acme", "name": "Acme", "contact_email_override": null, "created_at": T}),
    );
    check::<Account>(
        "Account",
        json!({"id": "acme", "name": "Acme", "contact_email_override": "a@b.co", "created_at": T}),
    );
}

#[test]
fn problem() {
    check::<Problem>(
        "Problem",
        json!({"type": "https://bonsai.io/docs/api/errors/not_found", "title": "Not found", "status": 404,
               "code": "not_found", "detail": "no such cluster", "instance": null, "action_url": null, "details": []}),
    );
    check::<Problem>(
        "Problem",
        json!({"type": "https://bonsai.io/docs/api/errors/missing_scope", "title": "Forbidden", "status": 403,
               "code": "missing_scope", "detail": "d", "instance": "/v1/x", "action_url": "https://example.com/pay",
               "details": [{"field": "/plan", "message": "bad", "scope": "clusters:write"}, {"field": null, "message": "m"}]}),
    );
}

#[test]
fn problem_with_unknown_code_keeps_it() {
    let p: Problem = serde_json::from_value(json!({"type": "t", "title": "t", "status": 418, "code": "teapot", "detail": "d", "details": []})).unwrap();
    assert_eq!(p.code, ProblemCode::Unknown("teapot".into()));
}

#[test]
fn clusters() {
    check::<Cluster>("Cluster", cluster(true));
    check::<Cluster>("Cluster", cluster(false));
    check::<Page<Cluster>>(
        "ClusterList",
        json!({"data": [cluster(true), cluster(false)], "next_cursor": "abc"}),
    );
    check::<Page<Cluster>>("ClusterList", json!({"data": [], "next_cursor": null}));
}

#[test]
fn cluster_requests() {
    let full = json!({"name": "n", "plan": "standard", "region": "aws-us-east-1", "space": "sp", "release": "os"});
    Contract::load_default()
        .validate_schema("ClusterCreate", &full)
        .unwrap();
    let m: ClusterCreate = serde_json::from_value(full.clone()).unwrap();
    assert_eq!(serde_json::to_value(&m).unwrap(), full);
    let min = json!({"name": "n", "plan": "standard", "region": "aws-us-east-1"});
    Contract::load_default()
        .validate_schema("ClusterCreate", &min)
        .unwrap();
    let m: ClusterCreate = serde_json::from_value(min.clone()).unwrap();
    assert_eq!(serde_json::to_value(&m).unwrap(), min);
    check::<ClusterUpdate>("ClusterUpdate", json!({"name": "x"}));
    check::<PlanChange>("PlanChange", json!({"plan": "business"}));
}

#[test]
fn actions() {
    check::<Action>("Action", action(true));
    check::<Action>("Action", action(false));
    check::<Page<Action>>(
        "ActionList",
        json!({"data": [action(true)], "next_cursor": null}),
    );
    check::<Action>(
        "Action",
        json!({"id": "act_3", "type": "cluster.enable", "state": "error", "subject": {"type": "cluster", "id": "c"},
               "created_at": T, "finished_at": T, "error": {"code": "timed_out", "message": "m"}, "result": null, "steps": []}),
    );
}

fn bucket(p: Option<f64>) -> Value {
    let hist: Vec<Value> = [
        0, 10, 20, 30, 40, 50, 60, 70, 80, 90, 100, 200, 300, 400, 500, 1000, 2000,
    ]
    .iter()
    .map(|m| json!({"min_ms": m, "count": 1}))
    .collect();
    json!({"at": T, "requests": 17, "writes": 3,
           "responses": {"status_2xx_3xx": 15, "status_4xx": 1, "status_5xx": 1},
           "duration_ms": {"p50": p, "p95": p, "p99": p}, "duration_histogram": hist,
           "bytes_sent": 100, "bytes_received": 50, "queue_time_ms": 1.5, "concurrency": 2.0})
}

#[test]
fn observability() {
    check::<Usage>(
        "Usage",
        json!({"from": T, "to": "2026-01-02T00:00:00Z", "samples": [
            {"at": T, "bytes": 1, "docs": 2, "docs_deleted": 3, "shards": 4, "primary_shards": 2, "health": "yellow"}]}),
    );
    check::<Usage>("Usage", json!({"from": T, "to": T, "samples": []}));
    check::<Metrics>(
        "Metrics",
        json!({"from": T, "to": T, "resolution": "1h", "buckets": [bucket(Some(12.5)), bucket(None)]}),
    );
    check::<TopQueries>(
        "TopQueries",
        json!({"window": "1d", "window_start_at": T, "window_end_at": T, "computed_at": T,
               "queries": [{"query": "foo", "volume": 9, "avg_duration_ms": 3.5, "avg_hits": 10.0}]}),
    );
}

#[test]
fn credentials() {
    check::<Credential>("Credential", credential());
    let mut with_secret = credential();
    with_secret["name"] = json!("ci");
    with_secret["username"] = json!("u");
    with_secret["password"] = json!("hunter2");
    with_secret["url"] = json!("https://u:hunter2@c-1.example.com");
    check::<CredentialWithSecret>("CredentialWithSecret", with_secret);
    check::<Page<Credential>>(
        "CredentialList",
        json!({"data": [credential()], "next_cursor": null}),
    );
    check::<CredentialCreate>(
        "CredentialCreate",
        json!({"name": "ci", "access_type": "ro"}),
    );
    check::<CredentialCreate>(
        "CredentialCreate",
        json!({"name": "ci", "access_type": "acl", "allowed_actions": ["a"], "allowed_index_globs": ["i*"], "allowed_cidrs": ["10.0.0.0/8"]}),
    );
}

#[test]
fn catalogue() {
    let plan = json!({"id": "standard", "name": "Standard", "price_in_cents": 5000, "billing_interval_months": 1,
                      "single_tenant": false, "private_network": true, "regions": ["aws-us-east-1"], "releases": ["os"]});
    check::<Plan>("Plan", plan.clone());
    check::<Page<Plan>>("PlanList", json!({"data": [plan], "next_cursor": null}));
    let region = json!({"id": "aws-us-east-1", "name": "US East", "cloud": "aws", "cloud_region": "us-east-1"});
    check::<Region>("Region", region.clone());
    check::<Page<Region>>("RegionList", json!({"data": [region], "next_cursor": null}));
    let space =
        json!({"id": "sp", "name": "Space", "region": "aws-us-east-1", "private_network": false});
    check::<Space>("Space", space.clone());
    check::<Page<Space>>("SpaceList", json!({"data": [space], "next_cursor": null}));
    let release = json!({"id": "opensearch-3.8.0", "name": "OpenSearch 3.8.0", "engine": "opensearch", "version": "3.8.0"});
    check::<Release>("Release", release.clone());
    check::<Page<Release>>(
        "ReleaseList",
        json!({"data": [release], "next_cursor": "n"}),
    );
}

#[test]
fn unknown_cluster_state_keeps_raw_value() {
    let s: ClusterState = serde_json::from_str("\"hibernating\"").unwrap();
    assert_eq!(s, ClusterState::Unknown("hibernating".into()));
    assert_eq!(serde_json::to_string(&s).unwrap(), "\"hibernating\"");
}

#[test]
fn known_enum_roundtrips_and_displays() {
    let s: ActionType = serde_json::from_str("\"cluster.change_plan\"").unwrap();
    assert_eq!(s, ActionType::ChangePlan);
    assert_eq!(s.to_string(), "cluster.change_plan");
    assert_eq!(ProblemCode::PaymentFailed.as_str(), "payment_failed");
}

#[test]
fn unknown_action_state_is_not_terminal() {
    assert!(!ActionState::Unknown("paused".into()).is_terminal());
    assert!(!ActionState::Running.is_terminal());
    assert!(ActionState::Error.is_terminal());
    assert!(ActionState::Success.is_terminal());
}

#[test]
fn missing_required_field_fails() {
    assert!(
        serde_json::from_value::<Account>(json!({"id": "a", "name": "n", "created_at": T}))
            .is_err()
    );
}

#[test]
fn secret_debug_is_masked() {
    let mut v = credential();
    v["username"] = json!("user1");
    v["password"] = json!("hunter2");
    v["url"] = json!("https://user1:hunter2@host");
    let c: CredentialWithSecret = serde_json::from_value(v).unwrap();
    let dbg = format!("{c:?}");
    assert!(!dbg.contains("hunter2"), "{dbg}");
    assert!(dbg.contains("user1"));
    assert!(!dbg.contains("user1:"), "{dbg}");
    assert!(!dbg.contains("@host"), "{dbg}");
}

#[test]
fn problem_detail_requires_field_key() {
    assert!(serde_json::from_value::<ProblemDetail>(json!({"message": "m"})).is_err());
    assert!(
        serde_json::from_value::<ProblemDetail>(json!({"field": null, "message": "m"})).is_ok()
    );
}

#[test]
fn secret_credential_name_is_required_but_nullable() {
    let mut v = credential();
    v["username"] = json!("u");
    v["password"] = json!("p");
    v["url"] = json!("https://u:p@h");
    check::<CredentialWithSecret>("CredentialWithSecret", v.clone());
    v.as_object_mut().unwrap().remove("name");
    assert!(serde_json::from_value::<CredentialWithSecret>(v).is_err());
}

#[test]
fn page_requires_next_cursor() {
    assert!(serde_json::from_value::<Page<Cluster>>(json!({"data": []})).is_err());
}
