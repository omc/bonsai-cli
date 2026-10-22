//! Proves the mock implements every `v1.yml` operation and can answer with
//! every status the contract documents for it: one successful call per
//! `operationId`, then each error status, all through `Mock::call`, which
//! validates every response and records its `(operationId, status)`.
//!
//! A documented status the mock can't produce must be listed in
//! [`ALLOWED_GAPS`] with its reason, and a listed gap the test does produce
//! fails the test, so the list stays honest. `500` isn't documented anywhere.

use std::collections::BTreeSet;

use crate::common::{Mock, create_body};
use serde_json::{Value, json};

/// `(operationId, status, why the mock never answers with it)`.
const ALLOWED_GAPS: &[(&str, u16, &str)] = &[
    (
        "createCluster",
        409,
        "keys are looked up and recorded under one database lock (never in flight), Actions are never deleted (a replay always finds its Action), and nothing running blocks a create",
    ),
    (
        "deleteCluster",
        409,
        "as createCluster's, and nothing running blocks a delete either, as on the Bonsai API",
    ),
];

/// Every operation once, `(operationId, method, path, body)`, on the cluster
/// `c`, credential `cred` and Action `action`; `d` is deleted.
fn operations(
    c: &str,
    d: &str,
    cred: &str,
    action: &str,
) -> Vec<(&'static str, &'static str, String, Option<Value>)> {
    let cl = format!("/clusters/{c}");
    vec![
        ("getApiRoot", "GET", String::new(), None),
        ("getAccount", "GET", "/account".into(), None),
        ("listClusters", "GET", "/clusters".into(), None),
        (
            "createCluster",
            "POST",
            "/clusters".into(),
            Some(create_body("Another", "sandbox")),
        ),
        ("getCluster", "GET", cl.clone(), None),
        (
            "updateCluster",
            "PATCH",
            cl.clone(),
            Some(json!({"name": "Covered again"})),
        ),
        (
            "changeClusterPlan",
            "POST",
            format!("{cl}/plan_changes"),
            Some(json!({"plan": "standard-cork"})),
        ),
        ("listClusterActions", "GET", format!("{cl}/actions"), None),
        ("getAction", "GET", format!("/actions/{action}"), None),
        ("getClusterUsage", "GET", format!("{cl}/usage"), None),
        ("getClusterMetrics", "GET", format!("{cl}/metrics"), None),
        ("getClusterTopQueries", "GET", format!("{cl}/queries"), None),
        (
            "listClusterCredentials",
            "GET",
            format!("{cl}/credentials"),
            None,
        ),
        (
            "createClusterCredential",
            "POST",
            format!("{cl}/credentials"),
            Some(json!({"access_type": "ro", "name": "reader 2"})),
        ),
        (
            "getClusterCredential",
            "GET",
            format!("{cl}/credentials/{cred}"),
            None,
        ),
        (
            "revokeClusterCredential",
            "DELETE",
            format!("{cl}/credentials/{cred}"),
            None,
        ),
        ("listPlans", "GET", "/plans".into(), None),
        ("getPlan", "GET", "/plans/sandbox".into(), None),
        ("listRegions", "GET", "/regions".into(), None),
        ("listSpaces", "GET", "/spaces".into(), None),
        ("listReleases", "GET", "/releases".into(), None),
        (
            "deleteCluster",
            "DELETE",
            format!("/clusters/{d}?name=Doomed"),
            None,
        ),
    ]
}

async fn expect(
    m: &Mock,
    token: Option<&str>,
    method: &str,
    path: &str,
    body: Option<Value>,
    status: u16,
) {
    let (got, _, resp) = m.call(method, path, token, body, &[]).await;
    assert_eq!(got, status, "{method} {path}: {}", resp.unwrap_or_default());
}

/// Every status the mock can give on a mock whose Actions finish on read.
async fn drive(m: &Mock) {
    let acme = Some(m.acme.as_str());
    let none = m.token("acme", &[]);
    let c = m.ready_cluster("Covered", "standard-fir").await;
    let d = m.ready_cluster("Doomed", "standard-fir").await;
    let action = m.cluster(&c).await["latest_action"]["id"]
        .as_str()
        .unwrap()
        .to_string();
    let creds = format!("/clusters/{c}/credentials");
    let (_, _, list) = m.call("GET", &creds, acme, None, &[]).await;
    let default_cred = list.unwrap()["data"][0]["id"].as_str().unwrap().to_string();
    let (status, _, cred) = m
        .call(
            "POST",
            &creds,
            acme,
            Some(json!({"access_type": "ro", "name": "reader"})),
            &[],
        )
        .await;
    assert_eq!(status, 201);
    let cred = cred.unwrap()["id"].as_str().unwrap().to_string();

    let ops = operations(&c, &d, &cred, &action);

    // 429 on every operation, 401 and 403 on every one that takes a token.
    for (id, method, path, body) in &ops {
        m.admin("PUT", "/scenario/rate_limit_next", Some(json!(1)))
            .await;
        expect(m, acme, method, path, body.clone(), 429).await;
        if *id != "getApiRoot" {
            // Each 401 counts towards the IP's failed-authentication limit.
            m.admin("DELETE", "/rate_limits", None).await;
            expect(m, None, method, path, body.clone(), 401).await;
            expect(m, Some(&none), method, path, body.clone(), 403).await;
        }
    }

    // One success each, in order: the credential is read, then revoked, and
    // `d` deleted last.
    for (_, method, path, body) in &ops {
        let (status, _, resp) = m.call(method, path, acme, body.clone(), &[]).await;
        assert!(
            (200..300).contains(&status),
            "{method} {path}: {status} {}",
            resp.unwrap_or_default()
        );
    }

    // 400
    let cl = format!("/clusters/{c}");
    for path in ["/clusters", "/plans", "/regions", "/spaces", "/releases"] {
        expect(m, acme, "GET", &format!("{path}?limit=0"), None, 400).await;
    }
    expect(m, acme, "GET", &format!("{cl}/actions?limit=0"), None, 400).await;
    expect(m, acme, "GET", &format!("{creds}?limit=0"), None, 400).await;
    expect(
        m,
        acme,
        "GET",
        &format!("{cl}/usage?from=yesterday"),
        None,
        400,
    )
    .await;
    expect(
        m,
        acme,
        "GET",
        &format!("{cl}/metrics?resolution=2m"),
        None,
        400,
    )
    .await;
    expect(
        m,
        acme,
        "GET",
        &format!("{cl}/queries?window=2y"),
        None,
        400,
    )
    .await;
    expect(
        m,
        acme,
        "POST",
        "/clusters",
        Some(json!({"name": "x"})),
        400,
    )
    .await;
    expect(m, acme, "PATCH", &cl, Some(json!({})), 400).await;
    expect(m, acme, "DELETE", &cl, None, 400).await;
    expect(
        m,
        acme,
        "POST",
        &format!("{cl}/plan_changes"),
        Some(json!({})),
        400,
    )
    .await;
    expect(m, acme, "POST", &creds, Some(json!({})), 400).await;

    // 404
    for (method, path, body) in [
        ("GET", "/clusters/nope".to_string(), None),
        ("PATCH", "/clusters/nope".into(), Some(json!({"name": "x"}))),
        ("DELETE", "/clusters/nope?name=x".into(), None),
        (
            "POST",
            "/clusters/nope/plan_changes".into(),
            Some(json!({"plan": "sandbox"})),
        ),
        ("GET", "/clusters/nope/actions".into(), None),
        ("GET", "/actions/act_0000000000000000".into(), None),
        ("GET", "/clusters/nope/usage".into(), None),
        ("GET", "/clusters/nope/metrics".into(), None),
        ("GET", "/clusters/nope/queries".into(), None),
        ("GET", "/clusters/nope/credentials".into(), None),
        (
            "POST",
            "/clusters/nope/credentials".into(),
            Some(json!({"access_type": "ro"})),
        ),
        ("GET", format!("{creds}/cred-nope"), None),
        ("DELETE", format!("{creds}/cred-nope"), None),
        ("GET", "/plans/nope".into(), None),
    ] {
        expect(m, acme, method, &path, body, 404).await;
    }

    // 422
    let long = "x".repeat(513);
    expect(
        m,
        acme,
        "POST",
        "/clusters",
        Some(create_body(&long, "sandbox")),
        422,
    )
    .await;
    expect(m, acme, "PATCH", &cl, Some(json!({"name": long})), 422).await;
    expect(m, acme, "DELETE", &format!("{cl}?name=wrong"), None, 422).await;
    expect(
        m,
        acme,
        "POST",
        &format!("{cl}/plan_changes"),
        Some(json!({"plan": "nope"})),
        422,
    )
    .await;
    expect(
        m,
        acme,
        "POST",
        &creds,
        Some(json!({"access_type": "zz", "name": "x"})),
        422,
    )
    .await;
    expect(
        m,
        acme,
        "DELETE",
        &format!("{creds}/{default_cred}"),
        None,
        422,
    )
    .await;

    // 409: a credential's key replayed after it was revoked.
    let key = [("Idempotency-Key", "coverage-1")];
    let body = json!({"access_type": "ro", "name": "keyed"});
    let (status, _, made) = m.call("POST", &creds, acme, Some(body.clone()), &key).await;
    assert_eq!(status, 201);
    let made = made.unwrap()["id"].as_str().unwrap().to_string();
    expect(m, acme, "DELETE", &format!("{creds}/{made}"), None, 204).await;
    let (status, _, _) = m.call("POST", &creds, acme, Some(body), &key).await;
    assert_eq!(status, 409);

    // 503
    m.admin("PUT", "/scenario/metrics_unavailable", Some(json!(true)))
        .await;
    for what in ["usage", "metrics", "queries"] {
        expect(m, acme, "GET", &format!("{cl}/{what}"), None, 503).await;
    }
    m.admin("DELETE", "/scenario/metrics_unavailable", None)
        .await;
}

/// The browser login, which takes no token: start one, poll it pending (400),
/// approve it and collect the token (200), then each one's 400 and 429.
async fn drive_login(m: &Mock) {
    let start = json!({"client_name": "bonsai 0.1.0", "hostname": "covered", "os": "linux"});
    let (status, _, grant) = m
        .call("POST", "/cli/logins", None, Some(start.clone()), &[])
        .await;
    assert_eq!(status, 200);
    let grant = grant.unwrap();
    let poll = json!({"device_code": grant["device_code"]});
    expect(
        m,
        None,
        "POST",
        "/cli/logins/token",
        Some(poll.clone()),
        400,
    )
    .await;
    let (status, _) = m
        .admin(
            "POST",
            &format!(
                "/cli/logins/{}/approve",
                grant["user_code"].as_str().unwrap()
            ),
            Some(json!({})),
        )
        .await;
    assert_eq!(status, 204);
    expect(
        m,
        None,
        "POST",
        "/cli/logins/token",
        Some(poll.clone()),
        200,
    )
    .await;

    expect(
        m,
        None,
        "POST",
        "/cli/logins",
        Some(json!({"os": "x"})),
        400,
    )
    .await;
    expect(m, None, "POST", "/cli/logins/token", Some(json!({})), 400).await;

    for (path, body) in [("/cli/logins", start), ("/cli/logins/token", poll)] {
        m.admin("PUT", "/scenario/rate_limit_next", Some(json!(1)))
            .await;
        expect(m, None, "POST", path, Some(body), 429).await;
    }
}

/// What needs an Action still running: `409 action_in_progress`.
async fn drive_slow(m: &Mock) {
    let acme = Some(m.acme.as_str());
    let action = m.create("Slow", "standard-fir").await;
    let id = action["id"].as_str().unwrap().to_string();
    let cluster = action["subject"]["id"].as_str().unwrap().to_string();
    // Steps take a minute, so the plan change below is surely still running at the
    // second POST, however loaded the machine. The provision is finished by
    // moving its start back an hour; the next read catches it up.
    let an_hour_ago = (jiff::Timestamp::now() - jiff::SignedDuration::from_hours(1)).to_string();
    m.db()
        .execute(
            "UPDATE actions SET started_at = ?2 WHERE id = ?1",
            (&id, &an_hour_ago),
        )
        .unwrap();
    assert_eq!(m.action(&id).await["state"], "success");
    let path = format!("/clusters/{cluster}/plan_changes");
    let body = json!({"plan": "standard-cork"});
    expect(m, acme, "POST", &path, Some(body.clone()), 202).await;
    expect(m, acme, "POST", &path, Some(body), 409).await;
}

#[tokio::test]
async fn every_operation_and_documented_status() {
    let m = Mock::start().await;
    let slow = Mock::start_with(60_000).await;
    drive(&m).await;
    drive_login(&m).await;
    drive_slow(&slow).await;
    let mut seen = m.seen();
    seen.extend(slow.seen());

    let ops = m.contract.operations();
    assert_eq!(ops.len(), 24);

    let gap = |id: &str, status: u16| {
        ALLOWED_GAPS
            .iter()
            .find(|(g, s, _)| *g == id && *s == status)
    };
    for (id, status, reason) in ALLOWED_GAPS {
        assert!(!reason.is_empty());
        assert!(
            ops.iter()
                .any(|o| o.id == *id && o.statuses.contains(status)),
            "ALLOWED_GAPS lists {id} {status}, which v1.yml doesn't document"
        );
    }

    let mut missing = Vec::new();
    let mut produced_gaps = Vec::new();
    let mut table = String::from("operation                    statuses\n");
    for op in ops {
        assert!(
            seen.iter()
                .any(|(id, s)| *id == op.id && (200..300).contains(s)),
            "{} was never answered with a 2xx",
            op.id
        );
        let mut cells = Vec::new();
        for &status in &op.statuses {
            let got = seen.contains(&(op.id.clone(), status));
            match (got, gap(&op.id, status)) {
                (true, None) => cells.push(format!("{status}")),
                (false, Some(_)) => cells.push(format!("{status}(gap)")),
                (false, None) => {
                    cells.push(format!("{status}(MISSING)"));
                    missing.push(format!("{} {status}", op.id));
                }
                (true, Some(_)) => {
                    cells.push(format!("{status}(produced, but listed as a gap)"));
                    produced_gaps.push(format!("{} {status}", op.id));
                }
            }
        }
        table.push_str(&format!("{:<28} {}\n", op.id, cells.join(" ")));
    }
    println!("{table}");
    for (id, status, reason) in ALLOWED_GAPS {
        println!("gap: {id} {status}: {reason}");
    }

    // Nothing outside the contract was recorded (`Mock::call` would have
    // panicked first), and nothing documented was left out.
    let documented: BTreeSet<(String, u16)> = ops
        .iter()
        .flat_map(|o| o.statuses.iter().map(|s| (o.id.clone(), *s)))
        .collect();
    assert!(seen.is_subset(&documented));
    assert!(missing.is_empty(), "never produced: {missing:?}\n{table}");
    assert!(
        produced_gaps.is_empty(),
        "listed in ALLOWED_GAPS but produced, so remove them: {produced_gaps:?}"
    );
}
