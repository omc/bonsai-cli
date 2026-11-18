//! `bonsai cluster request` against an in-process `bonsai-mock` that answers its
//! clusters' own API (`local_clusters`), one fresh mock per test.

use std::process::Output;

use predicates::prelude::*;
use serde_json::{Value, json};

mod common;
use common::*;

const REGION: &str = "aws-us-east-1";

const THREE: &str = concat!(
    "{\"index\":{\"_id\":\"1\"}}\n{\"name\":\"Boston fern\"}\n",
    "{\"index\":{\"_id\":\"2\"}}\n{\"name\":\"Jade plant\"}\n",
    "{\"index\":{\"_id\":\"3\"}}\n{\"name\":\"Sword fern\"}",
);

/// Standard-alphabet base64 with padding: what an `Authorization: Basic` header
/// carries. A few lines here, so the crate needs no dependency for a test.
fn b64(bytes: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::new();
    for chunk in bytes.chunks(3) {
        let n = chunk
            .iter()
            .enumerate()
            .fold(0u32, |n, (i, b)| n | (*b as u32) << (16 - 8 * i));
        for i in 0..4 {
            if i <= chunk.len() {
                out.push(ALPHABET[(n >> (18 - 6 * i) & 63) as usize] as char);
            } else {
                out.push('=');
            }
        }
    }
    out
}

fn text(b: &[u8]) -> String {
    String::from_utf8_lossy(b).into_owned()
}

fn stderr(out: &Output) -> String {
    text(&out.stderr)
}

/// Creates a cluster and returns its ID.
fn create(env: &Env, name: &str, plan: &str) -> String {
    let out = env
        .authed()
        .args([
            "cluster", "create", name, "--plan", plan, "--region", REGION, "--json",
        ])
        .output()
        .unwrap();
    assert!(out.status.success(), "{}", stderr(&out));
    stdout_json(&out)["result"]["cluster"]
        .as_str()
        .unwrap()
        .to_owned()
}

/// `bonsai cluster request <cluster> <args…> --json`.
fn request(env: &Env, cluster: &str, args: &[&str]) -> Output {
    env.authed()
        .args(["cluster", "request", cluster])
        .args(args)
        .arg("--json")
        .output()
        .unwrap()
}

/// [`request`], which must succeed, as its JSON.
fn requested(env: &Env, cluster: &str, args: &[&str]) -> Value {
    let out = request(env, cluster, args);
    assert!(out.status.success(), "{args:?}: {}", stderr(&out));
    stdout_json(&out)
}

/// Loads [`THREE`] into `plants` through stdin.
fn three_plants(env: &Env, cluster: &str) {
    let out = env
        .authed()
        .args(["cluster", "request", cluster, "POST", "/plants/_bulk"])
        .args(["-d", "-", "--json"])
        .write_stdin(THREE)
        .output()
        .unwrap();
    assert!(out.status.success(), "{}", stderr(&out));
    assert_eq!(stdout_json(&out)["errors"], false);
}

/// Sets a scenario through the mock's admin API.
async fn scenario(env: &Env, key: &str, value: Value) {
    let res = bonsai_api::http_client()
        .no_proxy()
        .build()
        .unwrap()
        .put(
            env.mock
                .origin
                .join(&format!("/_mock/scenario/{key}"))
                .unwrap(),
        )
        .bearer_auth(&env.mock.admin_token)
        .json(&value)
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), 204, "{key}");
}

/// The default credential of `cluster`, through `bonsai api`: `(id, password, url)`.
fn default_credential(env: &Env, cluster: &str) -> (String, String, String) {
    let list = env
        .authed()
        .args(["api", "GET", &format!("/clusters/{cluster}/credentials")])
        .output()
        .unwrap();
    let list = stdout_json(&list);
    let id = list["data"]
        .as_array()
        .unwrap()
        .iter()
        .find(|c| c["default"] == true)
        .unwrap()["id"]
        .as_str()
        .unwrap()
        .to_owned();
    let shown = env
        .authed()
        .args([
            "api",
            "GET",
            &format!("/clusters/{cluster}/credentials/{id}"),
        ])
        .output()
        .unwrap();
    let shown = stdout_json(&shown);
    let get = |k: &str| shown[k].as_str().unwrap().to_owned();
    (id, get("password"), get("url"))
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_index_from_its_creation_to_its_deletion() {
    let env = Env::start_local().await;
    let id = create(&env, "data", "sandbox");

    // The body from a file; the cluster by its name.
    let dir = tempfile::tempdir().unwrap();
    let mapping = dir.path().join("mapping.json");
    std::fs::write(
        &mapping,
        r#"{"mappings": {"properties": {"name": {"type": "text"}}}}"#,
    )
    .unwrap();
    let made = requested(
        &env,
        "data",
        &["PUT", "/plants", "-d", &format!("@{}", mapping.display())],
    );
    assert_eq!(made["acknowledged"], true);

    // From stdin, without the final newline the engine demands: the CLI adds it.
    three_plants(&env, &id);

    // The body on the command line, the method in lower case.
    let hits = requested(
        &env,
        &id,
        &[
            "post",
            "/plants/_search",
            "-d",
            r#"{"query": {"match": {"name": "fern"}}}"#,
        ],
    );
    assert_eq!(hits["hits"]["total"]["value"], 2);
    assert_eq!(hits["hits"]["hits"][0]["_source"]["name"], "Boston fern");
    assert_eq!(requested(&env, &id, &["GET", "/plants/_count"])["count"], 3);
    assert_eq!(
        requested(&env, &id, &["GET", "/plants/_doc/2"])["_source"],
        json!({"name": "Jade plant"})
    );

    // Text stays text, and JSON is one query parameter away.
    env.authed()
        .args(["cluster", "request", &id, "GET", "/_cat/indices?v"])
        .assert()
        .success()
        .stdout(predicate::str::starts_with("health").and(predicate::str::contains("plants")));
    let rows = requested(&env, &id, &["GET", "/_cat/indices?format=json"]);
    assert_eq!(rows[0]["docs.count"], "3");

    // A body the cluster doesn't have is no output and a note of the status.
    let head = request(&env, &id, &["HEAD", "/plants"]);
    assert!(head.status.success(), "{}", stderr(&head));
    assert!(head.stdout.is_empty());
    assert!(stderr(&head).contains("200 OK"), "{}", stderr(&head));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn deleting_an_index_needs_its_name_typed_back() {
    let env = Env::start_local().await;
    let id = create(&env, "data", "sandbox");
    three_plants(&env, &id);
    let still_there = |what: &str| {
        let head = request(&env, &id, &["HEAD", "/plants"]);
        assert!(head.status.success(), "{what}: {}", stderr(&head));
    };

    let out = request(&env, &id, &["DELETE", "/plants"]);
    assert_eq!(out.status.code(), Some(2), "{}", stderr(&out));
    assert!(
        stderr(&out).contains("--confirm plants"),
        "{}",
        stderr(&out)
    );
    assert!(out.stdout.is_empty());
    still_there("no --confirm");

    let out = request(&env, &id, &["DELETE", "/plants", "--confirm", "plant"]);
    assert_eq!(out.status.code(), Some(2), "{}", stderr(&out));
    assert!(
        stderr(&out).contains("nothing was sent"),
        "{}",
        stderr(&out)
    );
    still_there("a wrong --confirm");

    // A hop that decodes the path before it routes would read this as DELETE /plants.
    let out = request(&env, &id, &["DELETE", "/plants/%2F"]);
    assert_eq!(out.status.code(), Some(2), "{}", stderr(&out));
    still_there("an encoded slash");

    // Two readings of the path name different indices: refused, even with the name
    // typed back.
    let out = request(&env, &id, &["DELETE", "/plants%2F", "--confirm", "plants"]);
    assert_eq!(out.status.code(), Some(2), "{}", stderr(&out));
    assert!(stderr(&out).contains("could remove"), "{}", stderr(&out));
    still_there("a path with two readings");

    // A date-math name has to carry an encoded slash. It isn't refused for that: it
    // is asked for as written, and sent once it's typed back.
    let date_math = "/%3Cplants-%7Bnow%2Fd%7D%3E/_delete_by_query";
    let out = request(&env, &id, &["POST", date_math, "-d", "{}"]);
    assert_eq!(out.status.code(), Some(2), "{}", stderr(&out));
    assert!(
        stderr(&out).contains("--confirm '<plants-{now/d}>'"),
        "{}",
        stderr(&out)
    );
    let out = request(
        &env,
        &id,
        &[
            "POST",
            date_math,
            "-d",
            r#"{"query": {"match_all": {}}}"#,
            "--confirm",
            "<plants-{now/d}>",
        ],
    );
    // It reached the cluster: the mock has no such index, which is the cluster's 404,
    // not this CLI's refusal.
    assert_eq!(out.status.code(), Some(4), "{}", stderr(&out));
    assert_eq!(
        stdout_json(&out)["error"]["type"],
        "index_not_found_exception"
    );

    // The same rule for _delete_by_query, whatever the method's case or the dots
    // in the path.
    let all = r#"{"query": {"match_all": {}}}"#;
    let out = request(
        &env,
        &id,
        &["post", "/x/../plants/_delete_by_query", "-d", all],
    );
    assert_eq!(out.status.code(), Some(2), "{}", stderr(&out));
    assert_eq!(requested(&env, &id, &["GET", "/plants/_count"])["count"], 3);
    let done = requested(
        &env,
        &id,
        &[
            "POST",
            "/plants/_delete_by_query",
            "-d",
            all,
            "--confirm",
            "plants",
        ],
    );
    assert_eq!(done["deleted"], 3);

    // One document isn't the whole index: no confirmation.
    let out = request(&env, &id, &["DELETE", "/plants/_doc/1"]);
    assert_eq!(
        out.status.code(),
        Some(4),
        "already deleted: {}",
        stderr(&out)
    );

    let gone = requested(&env, &id, &["DELETE", "/plants", "--confirm", "plants"]);
    assert_eq!(gone["acknowledged"], true);
    let head = request(&env, &id, &["HEAD", "/plants"]);
    assert_eq!(head.status.code(), Some(4));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_error_is_the_clusters_body_a_line_and_a_code() {
    let env = Env::start_local().await;
    let id = create(&env, "data", "sandbox");

    let out = request(&env, &id, &["GET", "/nope/_search"]);
    assert_eq!(out.status.code(), Some(4), "{}", stderr(&out));
    let body = stdout_json(&out);
    assert_eq!(body["error"]["type"], "index_not_found_exception");
    assert_eq!(body["status"], 404);
    let line = "the cluster answered 404 Not Found: no such index [nope]";
    let said: Value = serde_json::from_str(stderr(&out).trim()).unwrap();
    assert_eq!(said, json!({ "error": line }));

    // Without --json the same line, as people read it.
    let out = env
        .authed()
        .args(["cluster", "request", &id, "GET", "/nope/_search"])
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(4));
    assert!(
        stderr(&out).contains(&format!("✗ {line}")),
        "{}",
        stderr(&out)
    );
    assert_eq!(stdout_json(&out)["status"], 404);

    // A document that isn't there: the cluster's answer is still the data.
    let out = request(&env, &id, &["PUT", "/plants", "-d", "{}"]);
    assert!(out.status.success(), "{}", stderr(&out));
    let out = request(&env, &id, &["GET", "/plants/_doc/9"]);
    assert_eq!(out.status.code(), Some(4));
    assert_eq!(stdout_json(&out)["found"], false);

    // Any other error status is 1.
    let out = request(&env, &id, &["GET", "/_nodes/hot_threads"]);
    assert_eq!(out.status.code(), Some(1), "{}", stderr(&out));
    assert!(
        stderr(&out).contains("no handler found"),
        "{}",
        stderr(&out)
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn statuses_are_ridden_out_or_explained() {
    let env = Env::start_local().await;
    let id = create(&env, "data", "sandbox");

    // A 429 is sent again; -v shows both attempts, so the scenario did fire.
    scenario(&env, "cluster_status_next", json!(429)).await;
    let out = request(&env, &id, &["GET", "/", "-v"]);
    assert!(out.status.success(), "{}", stderr(&out));
    assert!(
        stdout_json(&out)["version"]["number"].is_string(),
        "{}",
        text(&out.stdout)
    );
    let logged = stderr(&out);
    assert!(logged.contains("status=429"), "{logged}");
    assert!(logged.contains("status=200"), "{logged}");

    let human = |args: &[&str]| {
        env.authed()
            .args(["cluster", "request", &id])
            .args(args)
            .output()
            .unwrap()
    };
    // An endpoint that can answer 429 midway isn't sent again.
    scenario(&env, "cluster_status_next", json!(429)).await;
    let out = human(&["POST", "/plants/_update_by_query", "-d", "{}"]);
    assert_eq!(out.status.code(), Some(1), "{}", stderr(&out));
    assert!(
        stderr(&out).contains("429 Too Many Requests"),
        "{}",
        stderr(&out)
    );
    assert!(
        stderr(&out).contains("wasn't sent again"),
        "{}",
        stderr(&out)
    );

    scenario(&env, "cluster_status_next", json!(413)).await;
    let out = human(&["POST", "/_bulk", "-d", "{}"]);
    assert_eq!(out.status.code(), Some(1));
    assert!(
        stderr(&out).contains("413 Payload Too Large"),
        "{}",
        stderr(&out)
    );
    assert!(stderr(&out).contains("smaller parts"), "{}", stderr(&out));

    scenario(&env, "cluster_status_next", json!(501)).await;
    let out = human(&["POST", "/_reindex", "-d", "{}"]);
    assert_eq!(out.status.code(), Some(1));
    assert!(
        stderr(&out).contains("https://bonsai.io/docs/platform/unsupported-api-endpoints/"),
        "{}",
        stderr(&out)
    );

    scenario(&env, "cluster_status_next", json!(401)).await;
    let out = human(&["GET", "/"]);
    assert_eq!(out.status.code(), Some(3));
    assert!(stderr(&out).contains("revoked"), "{}", stderr(&out));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn credential_picks_which_credential_sends() {
    let env = Env::start_local().await;
    // Sandbox allows one credential; this plan allows a read-only one beside it.
    let id = create(&env, "data", "standard-fir");
    three_plants(&env, &id);
    let made = env
        .authed()
        .args(["api", "POST", &format!("/clusters/{id}/credentials")])
        .args(["-d", r#"{"name": "reader", "access_type": "ro"}"#])
        .output()
        .unwrap();
    assert!(made.status.success(), "{}", stderr(&made));
    let reader = stdout_json(&made)["id"].as_str().unwrap().to_owned();

    let counted = requested(
        &env,
        &id,
        &["GET", "/plants/_count", "--credential", &reader],
    );
    assert_eq!(counted["count"], 3);
    let out = request(&env, &id, &["PUT", "/herbs", "--credential", &reader]);
    assert_eq!(out.status.code(), Some(3), "{}", stderr(&out));
    assert_eq!(stdout_json(&out)["error"]["type"], "security_exception");

    // A credential that isn't the cluster's is the API's 404.
    let out = request(&env, &id, &["GET", "/", "--credential", "nope"]);
    assert_eq!(out.status.code(), Some(4), "{}", stderr(&out));
}

/// A pipe gets the cluster's bytes: nothing re-indented, sorted, or added.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn piped_output_is_the_clusters_bytes() {
    let env = Env::start_local().await;
    let id = create(&env, "data", "sandbox");
    three_plants(&env, &id);
    let (_, _, url) = default_credential(&env, &id);
    let url = url::Url::parse(&url).unwrap();
    let direct = bonsai_api::http_client()
        .no_proxy()
        .build()
        .unwrap()
        .get(env.mock.origin.join("/plants/_search?size=2").unwrap())
        .basic_auth(url.username(), url.password())
        .send()
        .await
        .unwrap()
        .bytes()
        .await
        .unwrap();
    assert!(!direct.ends_with(b"\n"), "the mock sends no final newline");
    for extra in [&[][..], &["--json"]] {
        let out = env
            .authed()
            .args(["cluster", "request", &id, "GET", "/plants/_search?size=2"])
            .args(extra)
            .output()
            .unwrap();
        assert!(out.status.success(), "{}", stderr(&out));
        assert_eq!(out.stdout, direct.as_ref(), "{extra:?}");
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_bad_request_is_refused_before_anything_is_fetched() {
    let env = Env::start_local().await;
    let cases: [(&[&str], &str); 6] = [
        (&["GE T", "/"], "isn't an HTTP method"),
        (&["GET", "_cat/indices"], "must start with /"),
        (
            &["GET", "/", "-H", "Authorization: Basic eDp5"],
            "Authorization",
        ),
        (&["GET", "/", "-H", "no colon"], "Name: value"),
        (&["TRACE", "/"], "TRACE"),
        // A refused confirmation makes no API request either.
        (&["DELETE", "/plants"], "--confirm plants"),
    ];
    for (args, says) in cases {
        // No token: a request to the API would be exit 3, not 2.
        let out = env
            .bonsai()
            .args(["cluster", "request", "data"])
            .args(args)
            .output()
            .unwrap();
        assert_eq!(out.status.code(), Some(2), "{args:?}: {}", stderr(&out));
        assert!(stderr(&out).contains(says), "{args:?}: {}", stderr(&out));
    }
    let out = env
        .bonsai()
        .args(["cluster", "request", "data", "GET", "/"])
        .output()
        .unwrap();
    assert_eq!(
        out.status.code(),
        Some(3),
        "not logged in: {}",
        stderr(&out)
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_password_is_in_no_output() {
    let env = Env::start_local().await;
    let id = create(&env, "data", "sandbox");
    assert_eq!(b64(b"user:pass"), "dXNlcjpwYXNz");
    let (_, password, url) = default_credential(&env, &id);
    assert!(password.len() >= 8, "a real password to look for");
    // What the Authorization header carries.
    let username = url::Url::parse(&url).unwrap().username().to_owned();
    let header = b64(format!("{username}:{password}").as_bytes());
    let requests: [&[&str]; 6] = [
        &["GET", "/"],
        &["PUT", "/plants", "-d", "{}"],
        &["POST", "/plants/_bulk", "-d", "{\"index\":{}}\n{\"a\":1}"],
        &["GET", "/nope/_search"],
        &["DELETE", "/plants"],
        &["DELETE", "/plants", "--confirm", "plants"],
    ];
    let flags: [&[&str]; 4] = [&[], &["--json"], &["-vv"], &["--json", "-vv"]];
    for args in requests {
        for extra in flags {
            let out = env
                .authed()
                .args(["cluster", "request", &id])
                .args(args)
                .args(extra)
                .output()
                .unwrap();
            for (name, stream) in [("stdout", &out.stdout), ("stderr", &out.stderr)] {
                for secret in [&password, &header] {
                    assert!(
                        !text(stream).contains(secret.as_str()),
                        "{args:?} {extra:?} {name}"
                    );
                }
            }
        }
    }
    // -v says what was sent, without the query.
    let out = env
        .authed()
        .args(["cluster", "request", &id, "GET", "/_cat/indices?v", "-v"])
        .output()
        .unwrap();
    let logged = stderr(&out);
    assert!(logged.contains("cluster request"), "{logged}");
    assert!(
        logged.contains("path=/_cat/indices") || logged.contains("path=\"/_cat/indices\""),
        "{logged}"
    );
    assert!(!logged.contains("indices?v"), "{logged}");
}
