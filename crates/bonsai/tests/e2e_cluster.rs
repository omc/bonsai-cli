//! `bonsai clusters`, `bonsai cluster …` and `bonsai url` against an in-process
//! `bonsai-mock`, one fresh mock per test.

use std::process::Output;

use predicates::prelude::*;
use serde_json::{Value, json};

mod common;
use common::*;

const REGION: &str = "aws-us-east-1";

/// A mock whose Actions take a minute a step, so they're still running.
async fn slow_env() -> Env {
    let db = tempfile::tempdir().unwrap();
    let opts = bonsai_mock::MockOptions {
        db_path: db.path().join("mock.sqlite"),
        step_ms: 60_000,
        public_url: None,
        checkout_url: None,
    };
    let mock = bonsai_mock::spawn(opts, "127.0.0.1:0".parse().unwrap())
        .await
        .unwrap();
    Env {
        mock,
        cfg: tempfile::tempdir().unwrap(),
        db,
    }
}

/// A call to the mock's admin API, which must answer 204.
async fn admin(env: &Env, method: reqwest::Method, path: &str, body: Value) {
    let res = bonsai_api::http_client()
        .no_proxy()
        .build()
        .unwrap()
        .request(method, env.mock.origin.join(path).unwrap())
        .bearer_auth(&env.mock.admin_token)
        .json(&body)
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), 204, "{path}: {}", res.text().await.unwrap());
}

async fn fail_next(env: &Env, how: &str) {
    admin(
        env,
        reqwest::Method::PUT,
        "/_mock/scenario/fail_next",
        json!(how),
    )
    .await;
}

/// A token on `globex` (no card on file) that may write clusters; the seeded one
/// may only read.
fn globex_writer(env: &Env) -> String {
    let conn = bonsai_mock::db::open(&env.db.path().join("mock.sqlite")).unwrap();
    bonsai_mock::seed::create_token(&conn, "globex", "e2e", bonsai_mock::seed::ALL_SCOPES, None)
        .unwrap()
}

fn text(b: &[u8]) -> String {
    String::from_utf8_lossy(b).into_owned()
}

fn stderr(out: &Output) -> String {
    text(&out.stderr)
}

/// Creates a cluster with `--json` and returns its ID.
fn create(env: &Env, name: &str, plan: &str) -> String {
    let out = env
        .authed()
        .args([
            "cluster", "create", name, "--plan", plan, "--region", REGION,
        ])
        .arg("--json")
        .output()
        .unwrap();
    assert!(out.status.success(), "{}", stderr(&out));
    stdout_json(&out)["result"]["cluster"]
        .as_str()
        .unwrap()
        .to_owned()
}

/// `clusters --json`, as `(name, state)`.
fn listed(env: &Env) -> Vec<(String, String)> {
    let out = env
        .authed()
        .args(["cluster", "list", "--json"])
        .output()
        .unwrap();
    assert!(out.status.success(), "{}", stderr(&out));
    stdout_json(&out)
        .as_array()
        .unwrap()
        .iter()
        .map(|c| {
            (
                c["name"].as_str().unwrap().to_owned(),
                c["state"].as_str().unwrap().to_owned(),
            )
        })
        .collect()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn ci_example() {
    let env = Env::start().await;
    let id = create(&env, "ci-1", "sandbox");

    env.authed()
        .args(["cluster", "url", &id])
        .assert()
        .success()
        .stdout(predicate::str::is_match(r"^https://[^:]+:[^@]+@.+\n$").unwrap());

    env.authed()
        .args(["cluster", "destroy", &id, "--confirm", "ci-1"])
        .assert()
        .success();

    let out = env
        .authed()
        .args(["cluster", "show", &id, "--json"])
        .output()
        .unwrap();
    assert!(out.status.success(), "{}", stderr(&out));
    assert_eq!(stdout_json(&out)["state"], "deprovisioned");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn create_human_output_names_the_host_and_url_command() {
    let env = Env::start().await;
    env.authed()
        .args([
            "cluster", "create", "hello", "--plan", "sandbox", "--region", REGION,
        ])
        .assert()
        .success()
        .stderr(predicate::str::contains("✓ hello is ready  hello-"))
        .stderr(predicate::str::contains(
            "\n  Connection URL (includes the password): run bonsai cluster url hello\n",
        ))
        .stderr(predicate::str::contains("Reserving capacity"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn create_payment_method_required() {
    let env = Env::start().await;
    let out = env
        .bonsai()
        .env("BONSAI_TOKEN", globex_writer(&env))
        .args(["cluster", "create", "no-card", "--plan", "standard-fir"])
        .args(["--region", REGION])
        .output()
        .unwrap();
    let err = stderr(&out);
    assert_eq!(out.status.code(), Some(6), "{err}");
    assert!(
        err.starts_with("✗ This plan needs a card on file.\n"),
        "{err}"
    );
    // The CLI's words, not the API's (which name a JSON field).
    assert!(!err.contains("action_url"), "{err}");
    assert!(!err.contains("Unprocessable"), "{err}");
    assert!(err.contains("checkout"), "{err}");
    assert!(
        err.contains(
            "Then run: bonsai cluster create no-card --plan standard-fir --region aws-us-east-1"
        ),
        "{err}"
    );
    assert!(!err.to_lowercase().contains("idempotency"), "{err}");
    // The error, then the Checkout link (once), then what to run.
    let at = |needle: &str| {
        err.find(needle)
            .unwrap_or_else(|| panic!("{needle}: {err}"))
    };
    assert!(at("✗") < at("/_mock/checkout/globex"), "{err}");
    assert!(at("/_mock/checkout/globex") < at("Then run:"), "{err}");
    assert_eq!(err.matches("/_mock/checkout/").count(), 1, "{err}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn create_payment_failed() {
    let env = Env::start().await;
    fail_next(&env, "payment_failed").await;
    let out = env
        .authed()
        .args(["cluster", "create", "unpaid", "--plan", "standard-fir"])
        .args(["--region", REGION])
        .output()
        .unwrap();
    let err = stderr(&out);
    assert_eq!(out.status.code(), Some(6), "{err}");
    assert!(
        err.starts_with("✗ The card on file was refused; nothing was done.\n"),
        "{err}"
    );
    assert!(!err.contains("Add a card"), "{err}");
    assert!(!err.contains("action_url"), "{err}");
    assert!(!err.contains("Unprocessable"), "{err}");
    assert!(err.contains("Then run:"), "{err}");
    assert!(!err.to_lowercase().contains("idempotency"), "{err}");
    // The error, then the Checkout link (once), then what to do about the refused
    // card and what to run.
    let at = |needle: &str| {
        err.find(needle)
            .unwrap_or_else(|| panic!("{needle}: {err}"))
    };
    assert!(at("✗") < at("/_mock/checkout/acme"), "{err}");
    assert!(at("/_mock/checkout/acme") < at("Update the card"), "{err}");
    assert!(at("Update the card") < at("Then run:"), "{err}");
    assert_eq!(err.matches("/_mock/checkout/").count(), 1, "{err}");
    assert!(listed(&env).is_empty());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn create_action_failed_cleans_up() {
    let env = Env::start().await;
    fail_next(&env, "action_failed").await;
    let out = env
        .authed()
        .args(["cluster", "create", "broken", "--plan", "sandbox"])
        .args(["--region", REGION])
        .output()
        .unwrap();
    let err = stderr(&out);
    assert_eq!(out.status.code(), Some(5), "{err}");
    // Bonsai terminated it itself: no removal of the CLI's own.
    assert!(
        err.contains("Bonsai removed the failed cluster broken"),
        "{err}"
    );
    assert!(!err.contains("Removing the failed cluster"), "{err}");
    assert!(!err.contains("bonsai cluster destroy"), "{err}");
    assert!(err.contains("again"), "{err}");
    assert!(listed(&env).iter().all(|(name, _)| name != "broken"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn create_action_failed_tells_what_happened_after_the_error() {
    let env = Env::start().await;
    fail_next(&env, "action_failed").await;
    let out = env
        .authed()
        .args(["cluster", "create", "broken", "--plan", "sandbox"])
        .args(["--region", REGION])
        .output()
        .unwrap();
    let err = stderr(&out);
    let at = |needle: &str| {
        err.find(needle)
            .unwrap_or_else(|| panic!("{needle}: {err}"))
    };
    assert!(
        at("✗ action act_") < at("Bonsai removed the failed cluster broken"),
        "{err}"
    );
    assert!(
        at("Bonsai removed the failed cluster") < at("Run the command again"),
        "{err}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn create_action_failed_json_reports_the_cleanup() {
    let env = Env::start().await;
    fail_next(&env, "action_failed").await;
    let out = env
        .authed()
        .args(["cluster", "create", "broken", "--plan", "sandbox"])
        .args(["--region", REGION, "--json"])
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(5), "{}", stderr(&out));
    let action = stdout_json(&out);
    assert_eq!(action["state"], "error");
    assert_eq!(action["error"]["code"], "action_failed");
    let id = action["subject"]["id"].as_str().unwrap();
    let problem: Value = serde_json::from_slice(&out.stderr).unwrap();
    assert!(
        problem["error"].as_str().unwrap().contains("failed"),
        "{problem}"
    );
    assert_eq!(problem["cleanup"]["cluster_id"], id, "{problem}");
    assert_eq!(
        problem["cleanup"]["outcome"], "removed_by_bonsai",
        "{problem}"
    );
    assert_eq!(
        problem["cleanup"]["deprovision_action_id"],
        Value::Null,
        "{problem}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_replayed_failed_create_is_not_removed() {
    let env = Env::start().await;
    fail_next(&env, "action_failed").await;
    let run = |extra: &[&str]| {
        env.authed()
            .args(["cluster", "create", "replayed", "--plan", "sandbox"])
            .args(["--region", REGION, "--idempotency-key", "ci-run-9"])
            .args(extra)
            .output()
            .unwrap()
    };
    let first = run(&["--keep-failed"]);
    assert_eq!(first.status.code(), Some(5), "{}", stderr(&first));
    let again = run(&[]);
    let err = stderr(&again);
    assert_eq!(again.status.code(), Some(5), "{err}");
    // Bonsai removed it after the first run; the rerun removes nothing itself.
    assert!(err.contains("Bonsai removed the failed cluster"), "{err}");
    assert!(!err.contains("Removing the failed cluster"), "{err}");
    assert!(err.contains("new --idempotency-key"), "{err}");
    assert!(!err.contains("ci-run-9"), "{err}");
    assert!(listed(&env).is_empty());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn create_action_failed_keep_failed() {
    let env = Env::start().await;
    fail_next(&env, "action_failed").await;
    let out = env
        .authed()
        .args(["cluster", "create", "broken", "--plan", "sandbox"])
        .args(["--region", REGION, "--keep-failed"])
        .output()
        .unwrap();
    let err = stderr(&out);
    assert_eq!(out.status.code(), Some(5), "{err}");
    // Nothing left to keep: Bonsai removed it.
    assert!(
        err.contains("Bonsai removed the failed cluster broken"),
        "{err}"
    );
    assert!(!err.contains("bonsai cluster destroy"), "{err}");
    assert!(listed(&env).is_empty());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn noninteractive_missing_flag() {
    let env = Env::start().await;
    env.authed()
        .args(["cluster", "create", "x"])
        .assert()
        .code(2)
        .stderr(predicate::str::contains("--plan"))
        .stderr(predicate::str::contains("--region"));
    env.authed()
        .args(["cluster", "create", "x", "--plan", "sandbox"])
        .assert()
        .code(2)
        .stderr(predicate::str::contains("--region"));
    assert!(listed(&env).is_empty());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn destroy_requires_confirm() {
    let env = Env::start().await;
    let id = create(&env, "keep-me", "sandbox");
    env.authed()
        .args(["cluster", "destroy", &id])
        .assert()
        .code(2)
        .stderr(predicate::str::contains("--confirm"));
    env.authed()
        .args(["cluster", "destroy", &id, "--confirm", "keep-you"])
        .assert()
        .code(8)
        .stderr(predicate::str::contains("name_mismatch"))
        .stderr(predicate::str::contains("✗ --confirm doesn't match"))
        .stderr(predicate::str::contains("`name`").not());
    assert_eq!(listed(&env), [("keep-me".to_owned(), "ready".to_owned())]);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn ambiguous_name() {
    let env = Env::start().await;
    let a = create(&env, "twin", "sandbox");
    let b = create(&env, "twin", "sandbox");
    env.authed()
        .args(["cluster", "show", "twin"])
        .assert()
        .code(2)
        .stderr(predicate::str::contains(&a))
        .stderr(predicate::str::contains(&b));
    env.authed()
        .args(["cluster", "url", "twin"])
        .assert()
        .code(2)
        .stdout(predicate::str::is_empty());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn not_found() {
    let env = Env::start().await;
    env.authed()
        .args(["cluster", "show", "nope"])
        .assert()
        .code(4);
}

/// The password of `cluster_id`'s first credential, read through `bonsai api`.
fn password_of(env: &Env, cluster_id: &str) -> String {
    let creds = env
        .authed()
        .args(["api", "GET", &format!("/clusters/{cluster_id}/credentials")])
        .output()
        .unwrap();
    let cred = stdout_json(&creds)["data"][0]["id"]
        .as_str()
        .unwrap()
        .to_owned();
    let secret = env
        .authed()
        .args([
            "api",
            "GET",
            &format!("/clusters/{cluster_id}/credentials/{cred}"),
        ])
        .output()
        .unwrap();
    let password = stdout_json(&secret)["password"]
        .as_str()
        .unwrap()
        .to_owned();
    assert!(password.len() >= 8, "a real password to look for");
    password
}

fn assert_no_password(what: &str, out: &Output, password: &str) {
    assert!(!text(&out.stdout).contains(password), "{what} stdout");
    assert!(!text(&out.stderr).contains(password), "{what} stderr");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn no_password_anywhere_but_url() {
    let env = Env::start().await;
    let flags: [&[&str]; 4] = [&[], &["--json"], &["-vv"], &["--json", "-vv"]];

    // Every way to create: each output is checked against its own cluster's password.
    let mut first = None;
    for (i, extra) in flags.iter().enumerate() {
        let name = format!("secret-{i}");
        let out = env
            .authed()
            .args(["cluster", "create", &name, "--plan", "sandbox"])
            .args(["--region", REGION])
            .args(*extra)
            .output()
            .unwrap();
        assert!(out.status.success(), "create {extra:?}: {}", stderr(&out));
        let id = listed_ids(&env, &name);
        assert_no_password(&format!("create {extra:?}"), &out, &password_of(&env, &id));
        if extra == &["--json"] {
            let act = stdout_json(&out)["id"].as_str().unwrap().to_owned();
            first = Some((id, act));
        }
    }
    let (id, act) = first.unwrap();
    let password = password_of(&env, &id);

    let reads: [&[&str]; 12] = [
        &["cluster", "list"],
        &["cluster", "list"],
        &["cluster", "show", &id],
        &["cluster", "show", "secret-1"],
        &["cluster", "history", &id],
        &["cluster", "history", &id, "--steps"],
        &["cluster", "wait", &id],
        &["cluster", "usage", &id],
        &["cluster", "metrics", &id],
        &["cluster", "queries", &id],
        &["action", "show", &act],
        &["wait", &act],
    ];
    for read in reads {
        for extra in flags {
            let out = env.authed().args(read).args(extra).output().unwrap();
            assert!(out.status.success(), "{read:?} {extra:?}: {}", stderr(&out));
            assert_no_password(&format!("{read:?} {extra:?}"), &out, &password);
        }
    }

    for extra in flags {
        let out = env
            .authed()
            .args(["cluster", "url"])
            .arg(&id)
            .args(extra)
            .output()
            .unwrap();
        assert!(out.status.success(), "url {extra:?}: {}", stderr(&out));
        assert!(text(&out.stdout).contains(&password), "url {extra:?}");
        assert!(
            !text(&out.stderr).contains(&password),
            "url {extra:?} stderr"
        );
    }
}

/// The ID of the one listed cluster named `name`.
fn listed_ids(env: &Env, name: &str) -> String {
    let out = env
        .authed()
        .args(["cluster", "list", "--name", name, "--json"])
        .output()
        .unwrap();
    let v = stdout_json(&out);
    let found: Vec<&Value> = v
        .as_array()
        .unwrap()
        .iter()
        .filter(|c| c["name"] == name)
        .collect();
    assert_eq!(found.len(), 1, "{v}");
    found[0]["id"].as_str().unwrap().to_owned()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn url_prints_the_default_credentials_unless_another_is_named() {
    let env = Env::start().await;
    // Sandbox allows one credential; this plan allows more.
    let id = create(&env, "two-keys", "standard-fir");
    let creds = env
        .authed()
        .args(["api", "GET", &format!("/clusters/{id}/credentials")])
        .output()
        .unwrap();
    let first = stdout_json(&creds)["data"][0]["id"]
        .as_str()
        .unwrap()
        .to_owned();
    let made = env
        .authed()
        .args(["api", "POST", &format!("/clusters/{id}/credentials")])
        .args(["-d", r#"{"name": "second", "access_type": "rw"}"#])
        .output()
        .unwrap();
    assert!(made.status.success(), "{}", stderr(&made));
    let second = stdout_json(&made)["id"].as_str().unwrap().to_owned();
    let second_url = stdout_json(&made)["url"].as_str().unwrap().to_owned();
    let shown = env
        .authed()
        .args(["api", "GET", &format!("/clusters/{id}/credentials/{first}")])
        .output()
        .unwrap();
    let first_url = stdout_json(&shown)["url"].as_str().unwrap().to_owned();
    assert_ne!(first_url, second_url);

    // The first rw credential stays the default, as the dashboard shows it.
    env.authed()
        .args(["cluster", "url", &id])
        .assert()
        .success()
        .stdout(format!("{first_url}\n"));
    env.authed()
        .args(["cluster", "url", &id, "--credential", &second])
        .assert()
        .success()
        .stdout(format!("{second_url}\n"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cluster_open_prints_the_dashboard_link() {
    let env = Env::start().await;
    let id = create(&env, "open-me", "sandbox");
    let shown = env
        .authed()
        .args(["cluster", "show", &id, "--json"])
        .output()
        .unwrap();
    let link = stdout_json(&shown)["dashboard_url"]
        .as_str()
        .unwrap()
        .to_owned();
    assert!(link.ends_with(&format!("/_mock/clusters/{id}")), "{link}");

    // Prompts are off in tests, so it's printed, never opened.
    env.authed()
        .args(["cluster", "open", "open-me"])
        .assert()
        .success()
        .stdout(format!("{link}\n"));
    let out = env
        .authed()
        .args(["cluster", "open", &id, "--json"])
        .output()
        .unwrap();
    assert_eq!(stdout_json(&out)["dashboard_url"], link.as_str());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn bonsai_cluster_is_the_default_cluster() {
    let env = Env::start().await;
    let id = create(&env, "default-one", "sandbox");
    let out = env
        .authed()
        .env("BONSAI_CLUSTER", "default-one")
        .args(["cluster", "show", "--json"])
        .output()
        .unwrap();
    assert!(out.status.success(), "{}", stderr(&out));
    assert_eq!(stdout_json(&out)["id"], id.as_str());
    env.authed()
        .env("BONSAI_CLUSTER", &id)
        .args(["cluster", "url"])
        .assert()
        .success()
        .stdout(predicate::str::starts_with("https://"));
    // The argument wins over the variable.
    env.authed()
        .env("BONSAI_CLUSTER", &id)
        .args(["cluster", "show", "nope"])
        .assert()
        .code(4);
}

/// The API requests a command made, as `-v` logs them: `GET /v1/clusters/x`.
fn requests(out: &std::process::Output) -> Vec<String> {
    stderr(out)
        .lines()
        .filter(|l| l.contains("bonsai_api::retry: request "))
        .map(|l| {
            let field = |k: &str| {
                l.split(&format!("{k}="))
                    .nth(1)
                    .and_then(|r| r.split_whitespace().next())
                    .unwrap_or("")
                    .trim_matches('"')
                    .to_owned()
            };
            format!("{} {}", field("method"), field("path"))
        })
        .collect()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_name_resolves_without_fetching_the_cluster_twice() {
    let env = Env::start().await;
    let id = create(&env, "once", "sandbox");
    let out = env
        .authed()
        .args(["-v", "cluster", "show", "once", "--json"])
        .output()
        .unwrap();
    assert!(out.status.success(), "{}", stderr(&out));
    assert_eq!(stdout_json(&out)["id"], id.as_str());
    assert_eq!(
        requests(&out),
        ["GET /v1/clusters/once", "GET /v1/clusters"],
        "{}",
        stderr(&out)
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn verbose_explains_the_404_before_a_name_lookup() {
    let env = Env::start().await;
    let id = create(&env, "by-name", "sandbox");
    let out = env
        .authed()
        .args(["-v", "cluster", "show", "by-name", "--json"])
        .output()
        .unwrap();
    assert!(out.status.success(), "{}", stderr(&out));
    assert!(
        stderr(&out).contains("\"by-name\" isn't a cluster ID; looking it up by name"),
        "{}",
        stderr(&out)
    );
    // An ID resolves in one request, with no such line.
    let out = env
        .authed()
        .args(["-v", "cluster", "show", &id, "--json"])
        .output()
        .unwrap();
    assert!(
        !stderr(&out).contains("isn't a cluster ID"),
        "{}",
        stderr(&out)
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cluster_commands_live_under_cluster_only() {
    let env = Env::start().await;
    let id = create(&env, "grouped", "sandbox");
    for sub in ["url", "usage", "metrics", "queries", "history"] {
        let out = env
            .authed()
            .args(["cluster", sub, &id, "--json"])
            .output()
            .unwrap();
        assert!(out.status.success(), "cluster {sub}: {}", stderr(&out));
    }
    // The old shortcuts are gone, not aliases.
    for old in ["clusters", "url", "usage", "metrics", "queries", "history"] {
        env.authed()
            .args([old, &id])
            .assert()
            .code(2)
            .stdout(predicate::str::is_empty());
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn clusters_rides_out_a_429() {
    let env = Env::start().await;
    admin(
        &env,
        reqwest::Method::PUT,
        "/_mock/scenario/rate_limit_next",
        json!(1),
    )
    .await;
    let out = env
        .authed()
        .args(["cluster", "list", "--json"])
        .output()
        .unwrap();
    assert!(out.status.success(), "{}", stderr(&out));
    assert_eq!(stdout_json(&out), json!([]));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn url_env_prints_name_equals_url() {
    let env = Env::start().await;
    let id = create(&env, "dotenv", "sandbox");
    env.authed()
        .args(["cluster", "url", &id, "--env", "ELASTICSEARCH_URL"])
        .assert()
        .success()
        .stdout(predicate::str::is_match(r"^ELASTICSEARCH_URL=https://[^:]+:[^@]+@.+\n$").unwrap());
    env.authed()
        .args(["cluster", "url", &id, "--env", "NOT A NAME"])
        .assert()
        .code(2)
        .stdout(predicate::str::is_empty());
}

/// A git repository that ignores `.env`.
fn project() -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    let ok = std::process::Command::new("git")
        .args(["init", "-q"])
        .current_dir(dir.path())
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .status()
        .unwrap()
        .success();
    assert!(ok, "git init");
    std::fs::write(dir.path().join(".gitignore"), ".env\n").unwrap();
    dir
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn url_write_sets_the_variable_in_an_ignored_file_without_printing_it() {
    let env = Env::start().await;
    let id = create(&env, "secret", "sandbox");
    let printed = env.authed().args(["cluster", "url", &id]).output().unwrap();
    let url = String::from_utf8(printed.stdout).unwrap().trim().to_owned();
    let password = url::Url::parse(&url)
        .unwrap()
        .password()
        .unwrap()
        .to_owned();
    let dir = project();
    let dotenv = dir.path().join(".env");
    std::fs::write(&dotenv, "KEEP=1\nBONSAI_URL=old\n").unwrap();

    let out = env
        .authed()
        .current_dir(dir.path())
        .args(["cluster", "url", "secret", "--write", ".env"])
        .output()
        .unwrap();
    assert!(out.status.success(), "{}", stderr(&out));
    assert!(out.stdout.is_empty(), "printed: {:?}", out.stdout);
    assert!(!stderr(&out).contains(&password), "{}", stderr(&out));
    assert_eq!(
        std::fs::read_to_string(&dotenv).unwrap(),
        format!("KEEP=1\nBONSAI_URL={url}\n")
    );
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(&dotenv).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600);
    }

    let out = env
        .authed()
        .current_dir(dir.path())
        .args([
            "cluster", "url", "secret", "--write", ".env", "--env", "ES_URL", "--json",
        ])
        .output()
        .unwrap();
    assert!(out.status.success(), "{}", stderr(&out));
    let v = stdout_json(&out);
    assert_eq!(v["path"], ".env");
    assert_eq!(v["variable"], "ES_URL");
    assert_eq!(v["replaced"], false);
    assert!(v["url"].as_str().unwrap().contains("****"), "{v}");
    assert!(!v.to_string().contains(&password), "{v}");
    assert!(
        std::fs::read_to_string(&dotenv)
            .unwrap()
            .ends_with(&format!("ES_URL={url}\n"))
    );

    // Refused before the URL is fetched: a file git doesn't ignore, or one outside
    // the folder.
    for path in [".env.local", "../.env", "/tmp/bonsai.env"] {
        env.authed()
            .current_dir(dir.path())
            .args(["cluster", "url", "secret", "--write", path])
            .assert()
            .code(2)
            .stdout(predicate::str::is_empty());
    }
    assert!(!dir.path().join(".env.local").exists());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn wait_timeout() {
    let env = slow_env().await;
    env.authed()
        .args([
            "cluster", "create", "x", "--plan", "sandbox", "--region", REGION,
        ])
        .args(["--timeout", "1s"])
        .assert()
        .code(9)
        .stderr(predicate::str::contains("bonsai wait act_"));
    // Still running on the API's side: nothing was cleaned up.
    assert_eq!(listed(&env), [("x".to_owned(), "provisioning".to_owned())]);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn no_wait_then_cluster_wait() {
    let env = Env::start().await;
    let out = env
        .authed()
        .args([
            "cluster", "create", "later", "--plan", "sandbox", "--region", REGION,
        ])
        .args(["--no-wait", "--json"])
        .output()
        .unwrap();
    assert!(out.status.success(), "{}", stderr(&out));
    let id = stdout_json(&out)["subject"]["id"]
        .as_str()
        .unwrap()
        .to_owned();
    let out = env
        .authed()
        .args(["cluster", "wait", &id, "--json"])
        .output()
        .unwrap();
    assert!(out.status.success(), "{}", stderr(&out));
    assert_eq!(stdout_json(&out)["state"], "ready");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cluster_wait_times_out_with_exit_9() {
    let env = slow_env().await;
    env.authed()
        .args([
            "cluster", "create", "slow", "--plan", "sandbox", "--region", REGION,
        ])
        .arg("--no-wait")
        .assert()
        .success();
    env.authed()
        .args(["cluster", "wait", "slow", "--timeout", "1s"])
        .assert()
        .code(9)
        .stderr(predicate::str::contains("bonsai wait act_"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn resize() {
    let env = Env::start().await;
    let id = create(&env, "grow", "sandbox");
    env.authed()
        .args(["cluster", "resize", &id, "standard-fir"])
        .assert()
        .success();
    let out = env
        .authed()
        .args(["cluster", "show", &id, "--json"])
        .output()
        .unwrap();
    assert_eq!(stdout_json(&out)["plan"]["id"], "standard-fir");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn rename() {
    let env = Env::start().await;
    let id = create(&env, "old-name", "sandbox");
    env.authed()
        .args(["cluster", "rename", "old-name", "new-name"])
        .assert()
        .success();
    let out = env
        .authed()
        .args(["cluster", "show", &id, "--json"])
        .output()
        .unwrap();
    assert_eq!(stdout_json(&out)["name"], "new-name");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn list_filters_pass_through() {
    let env = Env::start().await;
    create(&env, "alpha", "sandbox");
    create(&env, "beta", "sandbox");
    let out = env
        .authed()
        .args([
            "cluster", "list", "--name", "alpha", "--state", "ready", "--json",
        ])
        .output()
        .unwrap();
    let v = stdout_json(&out);
    assert_eq!(v.as_array().unwrap().len(), 1, "{v}");
    assert_eq!(v[0]["name"], "alpha");
    env.authed()
        .args(["cluster", "list"])
        .assert()
        .success()
        .stdout(predicate::str::contains("NAME"))
        .stdout(predicate::str::contains("alpha"))
        .stdout(predicate::str::contains("beta"));
}

/// Filters for what changes from run to run: IDs, hosts and times.
fn filters() -> Vec<(&'static str, &'static str)> {
    vec![
        (r"act_[0-9a-z]{16}", "act_[ID]"),
        (r"snap-[0-9a-z]{6}", "snap-[ID]"),
        (r"\d{4}-\d{2}-\d{2} \d{2}:\d{2}:\d{2} UTC", "[TIMESTAMP]"),
        (r"\d{2}:\d{2}:\d{2}", "[TIME]"),
        (r"just now|\d+[smhd] ago", "[AGO]"),
    ]
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cluster_show_human() {
    let env = Env::start().await;
    let id = create(&env, "snap", "sandbox");
    let stats = json!({"docs": 2500, "shards": 1, "bytes": 48 * 1024 * 1024});
    admin(
        &env,
        reqwest::Method::POST,
        &format!("/_mock/clusters/{id}/stats"),
        stats,
    )
    .await;
    let out = env
        .authed()
        .args(["cluster", "show", &id])
        .output()
        .unwrap();
    assert!(out.status.success(), "{}", stderr(&out));
    insta::with_settings!({filters => filters()}, {
        insta::assert_snapshot!(text(&out.stdout));
    });
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cluster_history_steps_human() {
    let env = Env::start().await;
    let id = create(&env, "snap", "sandbox");
    env.authed()
        .args(["cluster", "resize", &id, "standard-fir"])
        .assert()
        .success();
    let out = env
        .authed()
        .args(["cluster", "history", &id, "--steps"])
        .output()
        .unwrap();
    assert!(out.status.success(), "{}", stderr(&out));
    insta::with_settings!({filters => filters()}, {
        insta::assert_snapshot!(text(&out.stdout));
    });
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn history_times_are_compact_unless_wide() {
    let env = Env::start().await;
    let id = create(&env, "timed", "sandbox");
    let run = |args: &[&str]| {
        let out = env.authed().args(args).output().unwrap();
        assert!(out.status.success(), "{}", stderr(&out));
        text(&out.stdout)
    };
    let short = run(&["cluster", "history", &id]);
    assert!(short.starts_with("ACTION "), "{short}");
    assert!(!short.contains(" UTC"), "{short}");
    let row = short.lines().nth(1).unwrap();
    assert!(row.ends_with(" ago") || row.ends_with("just now"), "{row}");
    // At 60 columns STARTED still fits.
    let narrow = env
        .authed()
        .env("COLUMNS", "60")
        .args(["cluster", "history", &id])
        .output()
        .unwrap();
    let narrow = text(&narrow.stdout);
    assert!(
        narrow.lines().next().unwrap().contains("STARTED"),
        "{narrow}"
    );
    assert_fits(&narrow, 60);
    // --wide adds STARTED AT, the full time; it's dropped first, so --wide never
    // shows fewer columns than the default.
    let wide = run(&["cluster", "history", &id, "--wide"]);
    assert!(wide.starts_with("ACTION "), "{wide}");
    assert!(
        wide.lines().next().unwrap().contains("STARTED AT"),
        "{wide}"
    );
    assert!(wide.lines().nth(1).unwrap().contains(" UTC"), "{wide}");
    for cols in ["60", "80", "120"] {
        let header = |extra: &[&str]| {
            let out = env
                .authed()
                .env("COLUMNS", cols)
                .args(["cluster", "history", &id])
                .args(extra)
                .output()
                .unwrap();
            let t = text(&out.stdout);
            t.lines().next().unwrap().to_owned()
        };
        let (short, wide) = (header(&[]), header(&["--wide"]));
        assert!(wide.contains("STARTED"), "{cols}: {wide}");
        // Column names, not the header line: widths follow the rows, and the
        // relative STARTED time ("just now", "2 seconds ago") can change width
        // between the two runs.
        let names = |h: &str| h.split_whitespace().map(str::to_owned).collect::<Vec<_>>();
        assert!(
            names(&wide).starts_with(&names(&short)),
            "{cols}: {short:?} vs {wide:?}"
        );
    }
    let steps = run(&["cluster", "history", &id, "--steps"]);
    assert!(!steps.contains(" UTC"), "{steps}");
    let steps = run(&["cluster", "history", &id, "--steps", "-w"]);
    assert!(
        steps.lines().all(|l| l.is_empty() || l.contains(" UTC (")),
        "{steps}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_caller_key_is_sent_and_never_printed() {
    let env = slow_env().await;
    let key = "ci-run-42-create";
    let run = || {
        env.authed()
            .args([
                "cluster", "create", "keyed", "--plan", "sandbox", "--region", REGION,
            ])
            .args(["--idempotency-key", key, "--no-wait", "--json"])
            .output()
            .unwrap()
    };
    let (first, again) = (run(), run());
    assert!(first.status.success(), "{}", stderr(&first));
    assert!(again.status.success(), "{}", stderr(&again));
    assert_eq!(stdout_json(&first)["id"], stdout_json(&again)["id"]);
    assert_eq!(listed(&env).len(), 1);
    for out in [&first, &again] {
        assert!(!stderr(out).contains(key));
    }

    env.authed()
        .args(["cluster", "create", "keyed-2", "--plan", "standard-fir"])
        .args(["--region", REGION, "--idempotency-key", key])
        .assert()
        .code(8)
        .stderr(predicate::str::contains(key).not());
}

/// A create whose provision failed: `cluster wait` reports that failure (exit 5)
/// even with no time left, and though Bonsai has already removed the cluster.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cluster_wait_on_a_failed_create_exits_5() {
    let env = Env::start().await;
    fail_next(&env, "action_failed").await;
    let out = env
        .authed()
        .args(["cluster", "create", "broken", "--plan", "sandbox"])
        .args(["--region", REGION, "--no-wait", "--json"])
        .output()
        .unwrap();
    assert!(out.status.success(), "{}", stderr(&out));
    let id = stdout_json(&out)["subject"]["id"]
        .as_str()
        .unwrap()
        .to_owned();
    for timeout in ["0s", "30s"] {
        let out = env
            .authed()
            .args(["cluster", "wait", &id, "--timeout", timeout])
            .output()
            .unwrap();
        assert_eq!(out.status.code(), Some(5), "{timeout}: {}", stderr(&out));
    }
}

/// Ctrl-C during a wait exits 130 (as any Ctrl-C does), and still says how to
/// resume.
#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn ctrl_c_during_a_wait_exits_130_with_the_resume_hint() {
    use std::process::Stdio;
    let env = slow_env().await;
    let out = env
        .authed()
        .args(["cluster", "create", "slow", "--plan", "sandbox"])
        .args(["--region", REGION, "--no-wait", "--json"])
        .output()
        .unwrap();
    assert!(out.status.success(), "{}", stderr(&out));
    let act = stdout_json(&out)["id"].as_str().unwrap().to_owned();
    for args in [vec!["wait", act.as_str()], vec!["cluster", "wait", "slow"]] {
        let mut child = std_bonsai(&env.mock, env.cfg.path())
            .env("BONSAI_TOKEN", env.token())
            .args(&args)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        let err = lines(child.stderr.take().unwrap());
        std::thread::sleep(std::time::Duration::from_millis(700));
        let ok = std::process::Command::new("kill")
            .args(["-INT", &child.id().to_string()])
            .status()
            .unwrap()
            .success();
        assert!(ok);
        let status = wait_for(&mut child, std::time::Duration::from_secs(5)).expect("exits");
        let text: Vec<String> = err.try_iter().collect();
        assert_eq!(status.code(), Some(130), "{args:?}: {text:?}");
        assert!(
            text.iter()
                .any(|l| l.contains(&format!("bonsai wait {act}"))),
            "{args:?}: {text:?}"
        );
    }
}

/// `--limit` is the total, not the page size, which the contract caps at 100.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn history_limit_is_a_total_and_at_least_one() {
    let env = Env::start().await;
    let id = create(&env, "busy", "sandbox");
    for limit in ["1", "150", "500"] {
        let out = env
            .authed()
            .args(["cluster", "history", &id, "--limit", limit, "--json"])
            .output()
            .unwrap();
        assert!(out.status.success(), "--limit {limit}: {}", stderr(&out));
        assert_eq!(
            stdout_json(&out).as_array().unwrap().len(),
            1,
            "--limit {limit}"
        );
    }
    env.authed()
        .args(["cluster", "history", &id, "--limit", "0"])
        .assert()
        .code(2);
}

/// A plan change on an account with no card is refused with the same code as a
/// create; the wording fits both, and the rerun line is the resize.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn resize_without_a_card_says_what_to_rerun() {
    let env = Env::start().await;
    let token = globex_writer(&env);
    let out = env
        .bonsai()
        .env("BONSAI_TOKEN", &token)
        .args([
            "cluster", "create", "free", "--plan", "sandbox", "--region", REGION, "--json",
        ])
        .output()
        .unwrap();
    assert!(out.status.success(), "{}", stderr(&out));
    let id = stdout_json(&out)["subject"]["id"]
        .as_str()
        .unwrap()
        .to_owned();
    let out = env
        .bonsai()
        .env("BONSAI_TOKEN", &token)
        .args(["cluster", "resize", &id, "standard-fir"])
        .output()
        .unwrap();
    let err = stderr(&out);
    assert_eq!(out.status.code(), Some(6), "{err}");
    assert!(
        err.starts_with("✗ This plan needs a card on file.\n"),
        "{err}"
    );
    assert!(!err.contains("create a cluster"), "{err}");
    assert!(err.contains("/_mock/checkout/globex"), "{err}");
    assert!(
        err.contains(&format!(
            "Then run: bonsai cluster resize {id} standard-fir"
        )),
        "{err}"
    );
}

// ---------------------------------------------------------------- table width

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn clusters_fit_columns_and_keep_ids_whole() {
    let env = Env::start().await;
    let ids: Vec<String> = ["my-search-cluster", "logs-production", "sandbox"]
        .iter()
        .map(|n| create(&env, n, "sandbox"))
        .collect();
    let out = env
        .authed()
        .env("COLUMNS", "60")
        .args(["cluster", "list"])
        .output()
        .unwrap();
    assert!(out.status.success(), "{}", stderr(&out));
    let listing = text(&out.stdout);
    assert_fits(&listing, 60);
    for id in &ids {
        assert!(
            listing
                .lines()
                .any(|l| l.split_whitespace().any(|w| w == id)),
            "{id} whole in\n{listing}"
        );
    }
    let header: Vec<&str> = listing
        .lines()
        .next()
        .unwrap()
        .split("  ")
        .filter(|h| !h.is_empty())
        .collect();
    assert_eq!(header.first(), Some(&"NAME"), "{listing}");
    assert!(
        !listing.contains('…'),
        "keys and fixed columns are never cut\n{listing}"
    );

    // Only the keys left: printed whole, wider than the width.
    let out = env
        .authed()
        .env("COLUMNS", "20")
        .args(["cluster", "list"])
        .output()
        .unwrap();
    let narrow = text(&out.stdout);
    assert!(narrow.starts_with("NAME "), "{narrow}");
    for line in narrow.lines() {
        assert_eq!(line.split_whitespace().count(), 2, "{narrow}");
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn clusters_default_and_wide_columns() {
    let env = Env::start().await;
    create(&env, "alpha", "sandbox");
    let header = |args: &[&str]| {
        let out = env.authed().args(args).output().unwrap();
        assert!(out.status.success(), "{}", stderr(&out));
        let t = text(&out.stdout);
        t.lines()
            .next()
            .unwrap()
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" ")
    };
    assert_eq!(
        header(&["cluster", "list"]),
        "NAME ID STATE PLAN REGION SIZE"
    );
    assert_eq!(
        header(&["cluster", "list", "--wide"]),
        "NAME ID STATE PLAN REGION RELEASE DOCS SIZE"
    );
}
