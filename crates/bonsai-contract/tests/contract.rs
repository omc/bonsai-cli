use bonsai_contract::*;
use serde_json::json;

fn c() -> Contract {
    Contract::load_default()
}
fn h() -> Vec<(String, String)> {
    vec![
        ("ratelimit".into(), "\"token\";r=59;t=60".into()),
        ("ratelimit-policy".into(), "\"token\";q=60;w=60".into()),
    ]
}

#[test]
fn lists_all_24_operations() {
    assert_eq!(c().operations().len(), 24);
    assert!(
        c().operations()
            .iter()
            .any(|o| o.id == "createCluster" && o.action_types == ["cluster.provision"])
    );
}

#[test]
fn matches_templated_paths() {
    assert_eq!(
        c().match_path("GET", "/clusters/abc-123/actions")
            .unwrap()
            .id,
        "listClusterActions"
    );
    assert!(c().match_path("GET", "/nope").is_none());
    assert_eq!(c().match_path("GET", "/").unwrap().id, "getApiRoot");
}

#[test]
fn accepts_valid_account() {
    let body = json!({"id":"acme","name":"Acme","contact_email_override":null,"created_at":"2026-01-01T00:00:00Z"});
    c().validate_response(&ResponseCheck {
        method: "GET",
        path: "/account",
        status: 200,
        content_type: Some("application/json"),
        headers: &h(),
        body: Some(&body),
    })
    .unwrap();
}

#[test]
fn rejects_missing_required_field() {
    let body = json!({"id":"acme","name":"Acme","created_at":"2026-01-01T00:00:00Z"});
    assert!(matches!(
        c().validate_response(&ResponseCheck {
            method: "GET",
            path: "/account",
            status: 200,
            content_type: Some("application/json"),
            headers: &h(),
            body: Some(&body)
        }),
        Err(Violation::Schema(_))
    ));
}

#[test]
fn nullable_allof_accepts_null_and_object() {
    let mut cl = json!({"id":"a","name":"a","state":"draft","plan":null,"region":{"id":"r","name":"r"},"space":null,"release":{"id":"x","name":"x"},"single_tenant":false,"host":null,"port":443,"stats":{"docs":null,"shards":null,"bytes":null,"limits":null,"polled_at":null},"latest_action":null,"created_at":"2026-01-01T00:00:00Z","terminated_at":null});
    c().validate_schema("Cluster", &cl).unwrap();
    cl["plan"] = json!({"id":"p","name":"p"});
    c().validate_schema("Cluster", &cl).unwrap();
    cl["plan"] = json!(5);
    assert!(c().validate_schema("Cluster", &cl).is_err());
}

#[test]
fn unknown_extensible_enum_value_is_valid() {
    let p = json!({"type":"https://bonsai.io/docs/api/errors/x","title":"t","status":400,"code":"brand_new_code","detail":"d","details":[]});
    c().validate_schema("Problem", &p).unwrap();
}

#[test]
fn rejects_undocumented_status() {
    assert!(matches!(
        c().validate_response(&ResponseCheck {
            method: "GET",
            path: "/account",
            status: 418,
            content_type: None,
            headers: &[],
            body: None
        }),
        Err(Violation::UndocumentedStatus { .. })
    ));
}

#[test]
fn requires_ratelimit_headers_on_2xx() {
    let body = json!({"name":"Bonsai API","version":"1.0.0-draft","docs_url":"https://x.io","openapi_url":"https://x.io/o"});
    assert!(matches!(
        c().validate_response(&ResponseCheck {
            method: "GET",
            path: "/",
            status: 200,
            content_type: Some("application/json"),
            headers: &[],
            body: Some(&body)
        }),
        Err(Violation::MissingHeader(_))
    ));
}

#[test]
fn problem_requires_problem_content_type() {
    let p = json!({"type":"t","title":"t","status":401,"code":"unauthorized","detail":"d","details":[]});
    assert!(matches!(
        c().validate_response(&ResponseCheck {
            method: "GET",
            path: "/account",
            status: 401,
            content_type: Some("application/json"),
            headers: &[],
            body: Some(&p)
        }),
        Err(Violation::WrongContentType { .. })
    ));
}

#[test]
fn extras() {
    assert_eq!(c().info_version(), "1.0.0-draft");
    let b = json!({"id":"a"});
    // 202 needs Location
    let contract = c();
    let r = contract.validate_response(&ResponseCheck {
        method: "DELETE",
        path: "/clusters/x",
        status: 202,
        content_type: Some("application/json"),
        headers: &h(),
        body: Some(&b),
    });
    assert!(
        matches!(r, Err(Violation::MissingHeader(ref n)) if n.eq_ignore_ascii_case("location"))
    );
    // bad date-time format rejected
    let acct = json!({"id":"a","name":"A","contact_email_override":null,"created_at":"yesterday"});
    assert!(c().validate_schema("Account", &acct).is_err());
    // 204 has no body
    c().validate_response(&ResponseCheck {
        method: "DELETE",
        path: "/clusters/x/credentials/y",
        status: 204,
        content_type: None,
        headers: &h(),
        body: None,
    })
    .unwrap();
    // missing content type with body
    let acct = json!({"id":"a","name":"A","contact_email_override":null,"created_at":"2026-01-01T00:00:00Z"});
    assert!(matches!(
        c().validate_response(&ResponseCheck {
            method: "GET",
            path: "/account",
            status: 200,
            content_type: None,
            headers: &h(),
            body: Some(&acct)
        }),
        Err(Violation::WrongContentType { .. })
    ));
    assert!(matches!(
        c().validate_response(&ResponseCheck {
            method: "GET",
            path: "/nope",
            status: 200,
            content_type: None,
            headers: &[],
            body: None
        }),
        Err(Violation::UnknownOperation { .. })
    ));
}

#[test]
fn problem_response_accepts_conforming_body() {
    let p = json!({"type":"t","title":"t","status":401,"code":"unauthorized","detail":"d","details":[]});
    let contract = c();
    let op = contract
        .validate_response(&ResponseCheck {
            method: "GET",
            path: "/account",
            status: 401,
            content_type: Some("application/problem+json"),
            headers: &[],
            body: Some(&p),
        })
        .unwrap();
    assert_eq!(op.id, "getAccount");
}

#[test]
fn problem_response_rejects_malformed_body() {
    let p = json!({"type":"t","title":"t","status":401,"detail":"d","details":[]});
    assert!(matches!(
        c().validate_response(&ResponseCheck {
            method: "GET",
            path: "/account",
            status: 401,
            content_type: Some("application/problem+json"),
            headers: &[],
            body: Some(&p),
        }),
        Err(Violation::Schema(_))
    ));
}

#[test]
fn uri_format_is_enforced() {
    let body = json!({"name":"Bonsai API","version":"1.0.0-draft","docs_url":"not a uri","openapi_url":"https://x.io/o"});
    assert!(matches!(
        c().validate_response(&ResponseCheck {
            method: "GET",
            path: "/",
            status: 200,
            content_type: Some("application/json"),
            headers: &h(),
            body: Some(&body),
        }),
        Err(Violation::Schema(_))
    ));
}

// ---------------------------------------------------------------- strict mode

fn strict() -> Contract {
    Contract::load_strict()
}

fn action(state: &str, error: serde_json::Value) -> serde_json::Value {
    json!({
        "id": "act_1", "type": "cluster.provision", "state": state,
        "subject": {"type": "cluster", "id": "c1"},
        "created_at": "2026-01-01T00:00:00Z", "finished_at": null,
        "error": error, "result": null, "steps": [],
    })
}

fn problem(status: u16, code: &str) -> serde_json::Value {
    json!({"type": "https://bonsai.io/docs/api/errors/x", "title": "t", "status": status,
           "code": code, "detail": "d", "details": []})
}

struct Case {
    method: &'static str,
    path: &'static str,
    status: u16,
    ct: &'static str,
    headers: Vec<(String, String)>,
    body: serde_json::Value,
}

impl Case {
    fn check(&self, c: &Contract) -> Result<(), Violation> {
        c.validate_response(&ResponseCheck {
            method: self.method,
            path: self.path,
            status: self.status,
            content_type: Some(self.ct),
            headers: &self.headers,
            body: Some(&self.body),
        })
        .map(|_| ())
    }
}

fn get_action(body: serde_json::Value, extra: &[(&str, &str)]) -> Case {
    let mut headers = h();
    headers.extend(extra.iter().map(|(k, v)| (k.to_string(), v.to_string())));
    Case {
        method: "GET",
        path: "/actions/act_1",
        status: 200,
        ct: "application/json",
        headers,
        body,
    }
}

fn refusal(
    method: &'static str,
    path: &'static str,
    status: u16,
    body: serde_json::Value,
    headers: Vec<(String, String)>,
) -> Case {
    Case {
        method,
        path,
        status,
        ct: "application/problem+json",
        headers,
        body,
    }
}

/// Each response the lenient mode accepts and strict mode refuses.
#[test]
fn strict_mode_refuses_what_lenient_mode_lets_through() {
    let mut payment = problem(422, "payment_method_required");
    payment["action_url"] = json!(null);
    let mut no_scope = problem(403, "missing_scope");
    no_scope["details"] = json!([{"field": null, "message": "Missing scope x."}]);
    let cases = [
        (
            "a misspelt Action state",
            get_action(action("runing", json!(null)), &[]),
        ),
        (
            "payment_failed without action_url",
            get_action(
                action(
                    "error",
                    json!({"code": "payment_failed", "message": "m", "actionUrl": "https://x.example/"}),
                ),
                &[],
            ),
        ),
        (
            "a non-integer Retry-After",
            get_action(action("running", json!(null)), &[("retry-after", "soon")]),
        ),
        (
            "Retry-After: 0",
            get_action(action("running", json!(null)), &[("retry-after", "0")]),
        ),
        (
            "a body status that isn't the HTTP status",
            refusal(
                "POST",
                "/clusters",
                422,
                problem(200, "validation_failed"),
                vec![],
            ),
        ),
        (
            "a code the response doesn't document",
            refusal("POST", "/clusters", 422, problem(422, "not_found"), vec![]),
        ),
        (
            "payment_method_required without action_url",
            refusal("POST", "/clusters", 422, payment, vec![]),
        ),
        (
            "missing_scope without a scope",
            refusal("GET", "/account", 403, no_scope, vec![]),
        ),
        (
            "metrics_unavailable without Retry-After",
            refusal(
                "GET",
                "/clusters/c1/metrics",
                503,
                problem(503, "metrics_unavailable"),
                vec![],
            ),
        ),
    ];
    for (what, case) in &cases {
        case.check(&c())
            .unwrap_or_else(|e| panic!("lenient mode should accept {what}: {e}"));
        assert!(
            case.check(&strict()).is_err(),
            "strict mode accepted {what}"
        );
    }
}

#[test]
fn strict_mode_accepts_good_responses() {
    let mut payment = problem(422, "payment_failed");
    payment["action_url"] = json!("https://checkout.stripe.com/c/x");
    let mut scope = problem(403, "missing_scope");
    scope["details"] = json!([{"field": null, "message": "Missing scope x.", "scope": "x"}]);
    let retry = vec![("retry-after".to_string(), "30".to_string())];
    let cases = [
        get_action(action("running", json!(null)), &[("retry-after", "1")]),
        get_action(
            action(
                "error",
                json!({"code": "payment_failed", "message": "m", "action_url": "https://x.example/"}),
            ),
            &[],
        ),
        get_action(
            action("error", json!({"code": "action_failed", "message": "m"})),
            &[],
        ),
        refusal("POST", "/clusters", 422, payment, vec![]),
        refusal("GET", "/account", 403, scope, vec![]),
        refusal(
            "GET",
            "/clusters/c1/metrics",
            503,
            problem(503, "metrics_unavailable"),
            retry,
        ),
    ];
    for case in &cases {
        case.check(&strict())
            .unwrap_or_else(|e| panic!("{} {}: {e}", case.method, case.path));
    }
}

#[test]
fn all_of_requires_the_second_branchs_fields() {
    let list = json!({"next_cursor": null});
    assert!(c().validate_schema("ClusterList", &list).is_err());
    let cred = json!({
        "id": "c", "name": "default", "access_type": "rw", "allowed_actions": [],
        "allowed_index_globs": [], "allowed_cidrs": [], "created_at": "2026-01-01T00:00:00Z",
        "username": "u", "url": "https://u:p@h"
    });
    c().validate_schema("Credential", &cred).unwrap();
    assert!(c().validate_schema("CredentialWithSecret", &cred).is_err());
}

#[test]
fn a_201_needs_location() {
    let body = json!({
        "id": "c", "name": "default", "access_type": "rw", "allowed_actions": [],
        "allowed_index_globs": [], "allowed_cidrs": [], "created_at": "2026-01-01T00:00:00Z",
        "username": "u", "password": "p", "url": "https://u:p@h"
    });
    let check = |headers: &[(String, String)]| {
        c().validate_response(&ResponseCheck {
            method: "POST",
            path: "/clusters/c1/credentials",
            status: 201,
            content_type: Some("application/json"),
            headers,
            body: Some(&body),
        })
        .map(|_| ())
    };
    assert!(matches!(check(&h()), Err(Violation::MissingHeader(name)) if name == "Location"));
    let mut with = h();
    with.push(("location".into(), "/v1/clusters/c1/credentials/c".into()));
    check(&with).unwrap();
}
