//! The catalogue (`plans`, `regions`, `spaces`, `releases`), Actions (`action show`,
//! `wait`) and observability (`usage`, `metrics`, `queries`) commands against an
//! in-process `bonsai-mock`.

use std::time::{Duration, Instant};

use predicates::prelude::*;
use serde_json::{Value, json};

mod common;
use common::*;

/// Creates a sandbox cluster through `bonsai api`, so these tests don't depend on
/// `bonsai cluster create`. Returns the cluster's and its provision Action's IDs.
fn create_cluster(env: &Env, name: &str) -> (String, String) {
    let body = json!({ "name": name, "plan": "sandbox", "region": "aws-us-east-1" });
    let out = env
        .authed()
        .args(["api", "POST", "/clusters", "-d", &body.to_string()])
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let action = stdout_json(&out);
    (
        action["subject"]["id"].as_str().unwrap().to_owned(),
        action["id"].as_str().unwrap().to_owned(),
    )
}

/// The cluster's name and its ID on separate, labelled lines, as the API names them.
fn assert_names_and_ids(text: &str, name: &str, id: &str) {
    let value = |label: &str| {
        text.lines()
            .find_map(|l| l.strip_prefix(label).filter(|r| r.starts_with(' ')))
            .map(str::trim)
            .unwrap_or_else(|| panic!("no {label} line: {text}"))
            .to_owned()
    };
    assert_eq!(value("Cluster"), name);
    assert_eq!(value("ID"), id);
}

/// Sets a mock scenario through the admin API.
async fn set_scenario(env: &Env, key: &str, value: Value) {
    let url = env
        .mock
        .origin
        .join(&format!("_mock/scenario/{key}"))
        .unwrap();
    let res = bonsai_api::http_client()
        .build()
        .unwrap()
        .put(url)
        .bearer_auth(&env.mock.admin_token)
        .json(&value)
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), 204);
}

fn stdout(out: &std::process::Output) -> String {
    String::from_utf8(out.stdout.clone()).unwrap()
}

fn stderr(out: &std::process::Output) -> String {
    String::from_utf8(out.stderr.clone()).unwrap()
}

// ---------------------------------------------------------------- catalogue

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn plans_human_output() {
    let env = Env::start().await;
    let out = env.authed().arg("plans").output().unwrap();
    assert!(out.status.success(), "{}", stderr(&out));
    insta::assert_snapshot!(stdout(&out));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn plans_human_output_at_60_columns() {
    let env = Env::start().await;
    let out = env
        .authed()
        .env("COLUMNS", "60")
        .arg("plans")
        .output()
        .unwrap();
    assert!(out.status.success(), "{}", stderr(&out));
    let text = stdout(&out);
    assert_fits(&text, 60);
    for id in ["sandbox", "standard-fir", "business-performance-lg-esplat"] {
        assert!(
            text.lines().any(|l| l.starts_with(&format!("{id} "))),
            "{id} whole in\n{text}"
        );
    }
    insta::assert_snapshot!(text);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn plans_wide_piped_is_whole() {
    let env = Env::start().await;
    let out = env.authed().args(["plans", "-w"]).output().unwrap();
    assert!(out.status.success(), "{}", stderr(&out));
    let text = stdout(&out);
    let header: Vec<&str> = text
        .lines()
        .next()
        .unwrap()
        .split("  ")
        .filter(|h| !h.is_empty())
        .map(str::trim)
        .collect();
    assert_eq!(
        header,
        [
            "ID",
            "NAME",
            "PRICE",
            "TENANCY",
            "PRIVATE NETWORK",
            "REGIONS",
            "RELEASES"
        ]
    );
    assert!(!text.contains('…'), "{text}");
    let row = text
        .lines()
        .find(|l| l.starts_with("standard-fir "))
        .unwrap();
    assert!(
        row.contains(
            "aws-ap-southeast-2, aws-eu-central-1, aws-eu-west-1, aws-us-east-1, aws-us-west-2"
        ),
        "{row}"
    );
    assert!(
        row.ends_with("opensearch-3.8.0, opensearch-2.19.6, elasticsearch-7.10.2"),
        "{row}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn regions_human_output() {
    let env = Env::start().await;
    let out = env.authed().arg("regions").output().unwrap();
    assert!(out.status.success(), "{}", stderr(&out));
    insta::assert_snapshot!(stdout(&out));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn regions_plan_lists_where_the_plan_is_offered() {
    let env = Env::start().await;
    let json = |args: &[&str]| {
        let out = env.authed().args(args).arg("--json").output().unwrap();
        assert!(out.status.success(), "{}", stderr(&out));
        stdout_json(&out)
    };
    let plans = json(&["plans"]);
    let sandbox = plans
        .as_array()
        .unwrap()
        .iter()
        .find(|p| p["id"] == "sandbox")
        .unwrap();
    let mut want: Vec<&str> = sandbox["regions"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| r.as_str().unwrap())
        .collect();
    want.sort_unstable();
    let all = json(&["regions"]);
    assert!(all.as_array().unwrap().len() > want.len(), "a real filter");

    let got = json(&["regions", "--plan", "sandbox"]);
    let mut ids: Vec<&str> = got
        .as_array()
        .unwrap()
        .iter()
        .map(|r| r["id"].as_str().unwrap())
        .collect();
    ids.sort_unstable();
    assert_eq!(ids, want);
    // Each item is the region as the API sent it.
    let full = all.as_array().unwrap();
    for r in got.as_array().unwrap() {
        assert!(full.contains(r), "{r}");
    }

    assert_eq!(json(&["regions", "--plan", "no-such-plan"]), json!([]));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn plans_show_regions_unless_one_is_given() {
    let env = Env::start().await;
    let header = |args: &[&str]| {
        let out = env.authed().args(args).output().unwrap();
        assert!(out.status.success(), "{}", stderr(&out));
        stdout(&out).lines().next().unwrap().to_owned()
    };
    assert!(header(&["plans"]).contains("REGIONS"));
    assert!(!header(&["plans", "--region", "aws-us-east-1"]).contains("REGIONS"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn releases_human_output() {
    let env = Env::start().await;
    let out = env.authed().arg("releases").output().unwrap();
    assert!(out.status.success(), "{}", stderr(&out));
    insta::assert_snapshot!(stdout(&out));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn plans_json_is_the_raw_items_array() {
    let env = Env::start().await;
    let out = env.authed().args(["plans", "--json"]).output().unwrap();
    assert!(out.status.success(), "{}", stderr(&out));
    let v = stdout_json(&out);
    let plans = v.as_array().expect("an array");
    assert!(plans.iter().any(|p| p["id"] == "sandbox"), "{v}");
    // As the server sent it, not a page.
    assert!(plans[0].get("price_in_cents").is_some(), "{v}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn plans_filter_on_the_cli_side() {
    let env = Env::start().await;
    let out = env
        .authed()
        .args(["plans", "--release", "elasticsearch-7.10.2", "--json"])
        .output()
        .unwrap();
    assert!(out.status.success(), "{}", stderr(&out));
    let v = stdout_json(&out);
    let ids: Vec<&str> = v
        .as_array()
        .unwrap()
        .iter()
        .map(|p| p["id"].as_str().unwrap())
        .collect();
    assert!(!ids.is_empty());
    assert!(!ids.contains(&"sandbox"), "{ids:?}");

    let out = env
        .authed()
        .args(["plans", "--region", "nowhere-1", "--json"])
        .output()
        .unwrap();
    assert!(out.status.success(), "{}", stderr(&out));
    assert_eq!(stdout_json(&out), json!([]));

    // With both, the release must be offered in that region: Standard offers
    // OpenSearch 2.19.6, but not in us-west-2.
    let ids = |region: &str| -> Vec<String> {
        let out = env
            .authed()
            .args([
                "plans",
                "--region",
                region,
                "--release",
                "opensearch-2.19.6",
            ])
            .arg("--json")
            .output()
            .unwrap();
        assert!(out.status.success(), "{}", stderr(&out));
        let v = stdout_json(&out);
        v.as_array()
            .unwrap()
            .iter()
            .map(|p| p["id"].as_str().unwrap().to_owned())
            .collect()
    };
    let west = ids("aws-us-west-2");
    assert!(west.contains(&"business-lg".to_owned()), "{west:?}");
    assert!(!west.contains(&"standard-fir".to_owned()), "{west:?}");
    assert!(ids("aws-us-east-1").contains(&"standard-fir".to_owned()));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn releases_filter_by_engine() {
    let env = Env::start().await;
    let out = env
        .authed()
        .args(["releases", "--engine", "opensearch", "--json"])
        .output()
        .unwrap();
    assert!(out.status.success(), "{}", stderr(&out));
    let v = stdout_json(&out);
    let releases = v.as_array().unwrap();
    assert!(!releases.is_empty());
    assert!(releases.iter().all(|r| r["engine"] == "opensearch"), "{v}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn spaces_lists_the_accounts_spaces() {
    let env = Env::start().await;
    env.authed()
        .arg("spaces")
        .assert()
        .success()
        .stdout(predicate::str::contains("acme-private-use1"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn spaces_says_when_there_are_none() {
    let env = Env::start().await;
    let globex = env
        .mock
        .seeded_tokens
        .iter()
        .find(|t| t.account_id == "globex")
        .unwrap()
        .token
        .clone();
    env.bonsai()
        .env("BONSAI_TOKEN", &globex)
        .arg("spaces")
        .assert()
        .success()
        .stdout("")
        .stderr(predicate::str::contains("no private spaces"));
    let out = env
        .bonsai()
        .env("BONSAI_TOKEN", &globex)
        .args(["spaces", "--json"])
        .output()
        .unwrap();
    assert_eq!(stdout_json(&out), json!([]));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn rate_limited_then_ok() {
    let env = Env::start().await;
    set_scenario(&env, "rate_limit_next", json!(1)).await;
    env.authed()
        .arg("plans")
        .assert()
        .success()
        .stdout(predicate::str::contains("sandbox"));
}

// ---------------------------------------------------------------- actions

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn action_show_prints_the_action() {
    let env = Env::start().await;
    let (cluster, action) = create_cluster(&env, "shown");
    let out = env
        .authed()
        .args(["action", "show", &action])
        .output()
        .unwrap();
    assert!(out.status.success(), "{}", stderr(&out));
    let text = stdout(&out);
    assert!(text.contains(&action), "{text}");
    assert!(text.contains("cluster.provision"), "{text}");
    assert!(text.contains(&cluster), "{text}");
    // Created keeps the full time; the steps table's AT is compact, so at 60
    // columns every step name is whole.
    assert!(text.contains("Created     20"), "{text}");
    let out = env
        .authed()
        .env("COLUMNS", "60")
        .args(["action", "show", &action])
        .output()
        .unwrap();
    let narrow = stdout(&out);
    let steps: Vec<&str> = narrow
        .lines()
        .skip_while(|l| !l.starts_with("STATE "))
        .collect();
    assert_eq!(steps.len(), 5, "{narrow}");
    assert!(!narrow.contains('…'), "{narrow}");
    assert!(steps[1..].iter().all(|l| !l.contains(" UTC")), "{narrow}");
    assert!(narrow.contains("Running health check"), "{narrow}");

    let out = env
        .authed()
        .args(["action", "show", &action, "--json"])
        .output()
        .unwrap();
    assert_eq!(stdout_json(&out)["id"], action.as_str());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn action_show_unknown_exits_4() {
    let env = Env::start().await;
    env.authed()
        .args(["action", "show", "act_0000000000000000"])
        .assert()
        .code(4);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn wait_follows_the_action_to_success() {
    let env = Env::start().await;
    let (cluster, action) = create_cluster(&env, "waited");
    let out = env
        .authed()
        .args(["wait", &action, "--json"])
        .output()
        .unwrap();
    assert!(out.status.success(), "{}", stderr(&out));
    // --json: no progress on stderr.
    assert_eq!(stderr(&out), "");
    let v = stdout_json(&out);
    assert_eq!(v["state"], "success");
    assert_eq!(v["result"]["cluster"], cluster.as_str());

    let out = env.authed().args(["wait", &action]).output().unwrap();
    assert!(out.status.success(), "{}", stderr(&out));
    let err = stderr(&out);
    assert!(err.contains("succeeded"), "{err}");
    // Steps as create shows them: every finished step is a ✓.
    assert!(err.contains("  ✓ Reserving capacity"), "{err}");
    assert!(!err.contains("… Reserving capacity"), "{err}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn wait_timeout_exits_9_with_the_resume_hint() {
    let db = tempfile::tempdir().unwrap();
    let cfg = tempfile::tempdir().unwrap();
    let opts = bonsai_mock::MockOptions {
        db_path: db.path().join("mock.sqlite"),
        step_ms: 60_000,
        public_url: None,
        checkout_url: None,
    };
    let mock = bonsai_mock::spawn(opts, "127.0.0.1:0".parse().unwrap())
        .await
        .unwrap();
    let token = mock
        .seeded_tokens
        .iter()
        .find(|t| t.account_id == "acme")
        .unwrap()
        .token
        .clone();
    let body = json!({ "name": "slow", "plan": "sandbox", "region": "aws-us-east-1" });
    let out = bonsai(&mock, cfg.path())
        .env("BONSAI_TOKEN", &token)
        .args(["api", "POST", "/clusters", "-d", &body.to_string()])
        .output()
        .unwrap();
    assert!(out.status.success());
    let action = stdout_json(&out)["id"].as_str().unwrap().to_owned();

    let started = Instant::now();
    bonsai(&mock, cfg.path())
        .env("BONSAI_TOKEN", &token)
        .args(["wait", &action, "--timeout", "1s"])
        .assert()
        .code(9)
        .stderr(predicate::str::contains(format!("bonsai wait {action}")));
    assert!(started.elapsed() < Duration::from_secs(10));
}

// ---------------------------------------------------------------- observability

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn usage_shows_the_plan_limits() {
    let env = Env::start().await;
    let (cluster, _) = create_cluster(&env, "used");
    let out = env
        .authed()
        .args(["cluster", "usage", &cluster])
        .output()
        .unwrap();
    assert!(out.status.success(), "{}", stderr(&out));
    let text = stdout(&out);
    for want in ["Docs", "Shards", "Size", "Health"] {
        assert!(text.contains(want), "{want}: {text}");
    }
    assert_names_and_ids(&text, "used", &cluster);
    assert!(text.contains('%'), "{text}");

    let out = env
        .authed()
        .args(["cluster", "usage", &cluster, "--json"])
        .output()
        .unwrap();
    assert!(out.status.success(), "{}", stderr(&out));
    assert!(stdout_json(&out)["samples"].is_array());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn usage_refuses_more_than_90_days() {
    let env = Env::start().await;
    env.authed()
        .args(["cluster", "usage", "anything", "--since", "91d"])
        .assert()
        .code(2)
        .stderr(predicate::str::contains("90 days"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn metrics_shows_typical_and_slowest_latency() {
    let env = Env::start().await;
    let (cluster, _) = create_cluster(&env, "measured");
    let out = env
        .authed()
        .args(["cluster", "metrics", &cluster])
        .output()
        .unwrap();
    assert!(out.status.success(), "{}", stderr(&out));
    let text = stdout(&out);
    for want in ["Requests", "5xx", "p50", "p95", "p99"] {
        assert!(text.contains(want), "{want}: {text}");
    }
    assert_names_and_ids(&text, "measured", &cluster);
    // Latency says what it is, and no line is a sparkline.
    for l in ["p50", "p95", "p99"] {
        let line = text.lines().find(|x| x.starts_with(l)).unwrap();
        assert!(
            line.contains(" ms typical, ")
                || line.trim_end().ends_with(" ms")
                || line.trim_end().ends_with(" -"),
            "{line}"
        );
        assert!(!line.contains("bucket"), "{line}");
    }
    assert!(
        !text.chars().any(|c| ('\u{2581}'..='\u{2588}').contains(&c)),
        "{text}"
    );
    assert!(!text.contains("avg"), "{text}");

    let out = env
        .authed()
        .args(["cluster", "metrics", &cluster, "--json"])
        .output()
        .unwrap();
    assert!(out.status.success(), "{}", stderr(&out));
    assert!(stdout_json(&out)["buckets"].is_array());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn metrics_unavailable() {
    let env = Env::start().await;
    let (cluster, _) = create_cluster(&env, "dark");
    set_scenario(&env, "metrics_unavailable", json!(true)).await;
    for command in ["metrics", "usage"] {
        let started = Instant::now();
        env.authed()
            .args(["cluster", command, &cluster])
            .assert()
            .code(1)
            .stdout("")
            .stderr("✗ metrics are unavailable right now, try again in 30 s\n");
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "{command}: {:?}",
            started.elapsed()
        );
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn queries_sanitized() {
    let env = Env::start().await;
    let (cluster, _) = create_cluster(&env, "searched");
    // Every term, the hostile ones included.
    let out = env
        .authed()
        .args(["cluster", "queries", &cluster, "--top", "100"])
        .output()
        .unwrap();
    assert!(out.status.success(), "{}", stderr(&out));
    let text = stdout(&out);
    assert!(text.contains("red shoes"), "{text}");
    assert!(text.contains("OVERWRITE"), "{text}");
    let note = stderr(&out);
    assert!(
        note.contains(&format!("on searched (ID {cluster})")),
        "{note}"
    );
    for bad in ['\x1b', '\r', '\u{202e}', '\t'] {
        assert!(!text.contains(bad), "{bad:?} in {text:?}");
    }
    // Fitted to a narrow width, still sanitized.
    let out = env
        .authed()
        .env("COLUMNS", "30")
        .args(["cluster", "queries", &cluster, "--top", "100"])
        .output()
        .unwrap();
    assert!(out.status.success(), "{}", stderr(&out));
    let narrow = stdout(&out);
    assert_fits(&narrow, 30);
    assert!(narrow.starts_with("QUERY "), "{narrow}");
    assert!(narrow.contains("red shoes"), "{narrow}");
    assert_no_raw_unsafe(&narrow);

    // --json and `bonsai api` escape every control, bidi and format character
    // too: the value is the server's, but none reaches stdout raw.
    let server = server_json(&env, &format!("/clusters/{cluster}/queries")).await;
    let out = env
        .authed()
        .args(["cluster", "queries", &cluster, "--json"])
        .output()
        .unwrap();
    assert!(out.status.success(), "{}", stderr(&out));
    let raw = stdout(&out);
    assert!(raw.contains("\\u001b"), "{raw}");
    assert!(raw.contains("\\u202e"), "{raw}");
    assert_no_raw_unsafe(&raw);
    let v = stdout_json(&out);
    assert!(
        v["queries"]
            .as_array()
            .unwrap()
            .iter()
            .any(|q| q["query"] == "shoes\u{1b}[31mred"),
        "{v}"
    );
    assert_eq!(without_clock(v), without_clock(server.clone()));

    let out = env
        .authed()
        .args(["api", "GET", &format!("/clusters/{cluster}/queries")])
        .output()
        .unwrap();
    assert!(out.status.success(), "{}", stderr(&out));
    assert_no_raw_unsafe(&stdout(&out));
    assert_eq!(without_clock(stdout_json(&out)), without_clock(server));
}

/// `path` under `/v1` straight from the mock, as the server sent it.
async fn server_json(env: &Env, path: &str) -> Value {
    let url = format!("{}{path}", env.mock.base_url.as_str().trim_end_matches('/'));
    bonsai_api::http_client()
        .no_proxy()
        .build()
        .unwrap()
        .get(url)
        .bearer_auth(env.token())
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap()
}

/// Top queries without the times that move with the clock.
fn without_clock(mut v: Value) -> Value {
    for k in ["computed_at", "window_start_at", "window_end_at"] {
        v.as_object_mut().unwrap().remove(k);
    }
    v
}

fn assert_no_raw_unsafe(text: &str) {
    for bad in ['\x1b', '\r', '\u{202e}', '\u{7f}', '\t'] {
        assert!(!text.contains(bad), "{bad:?} in {text:?}");
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn queries_top_limits_the_rows() {
    let env = Env::start().await;
    let (cluster, _) = create_cluster(&env, "few");
    let out = env
        .authed()
        .args(["cluster", "queries", &cluster, "--top", "3", "--json"])
        .output()
        .unwrap();
    assert!(out.status.success(), "{}", stderr(&out));
    assert_eq!(stdout_json(&out)["queries"].as_array().unwrap().len(), 3);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn queries_missing_scope() {
    let env = Env::start().await;
    let (cluster, _) = create_cluster(&env, "private");
    // An acme token without query_logs:read.
    let conn = bonsai_mock::db::open(&env.db.path().join("mock.sqlite")).unwrap();
    let token = bonsai_mock::seed::create_token(
        &conn,
        "acme",
        "no query logs",
        &["account:read", "clusters:read"],
        None,
    )
    .unwrap();
    drop(conn);
    env.bonsai()
        .env("BONSAI_TOKEN", &token)
        .args(["cluster", "queries", &cluster])
        .assert()
        .code(3)
        .stdout("")
        .stderr(predicate::str::contains(
            "this token lacks query_logs:read; create a token with it in the dashboard",
        ));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn observe_takes_the_cluster_name_and_bonsai_cluster() {
    let env = Env::start().await;
    let (cluster, _) = create_cluster(&env, "by-name");
    env.authed()
        .args(["cluster", "queries", "by-name", "--top", "1", "--json"])
        .assert()
        .success();
    let out = env
        .authed()
        .env("BONSAI_CLUSTER", &cluster)
        .args(["cluster", "usage", "--json"])
        .output()
        .unwrap();
    assert!(out.status.success(), "{}", stderr(&out));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn wait_action_error_exits_5() {
    let env = Env::start().await;
    set_scenario(&env, "fail_next", json!("action_failed")).await;
    let (_, action) = create_cluster(&env, "doomed");
    let out = env
        .authed()
        .args(["wait", &action, "--json"])
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(5), "{}", stderr(&out));
    let v = stdout_json(&out);
    assert_eq!(v["state"], "error");
    assert_eq!(v["error"]["code"], "action_failed");

    let out = env.authed().args(["wait", &action]).output().unwrap();
    assert_eq!(out.status.code(), Some(5), "{}", stderr(&out));
    let err = stderr(&out);
    let message = v["error"]["message"].as_str().unwrap();
    // Said once, in the final error, not again as a step.
    assert_eq!(err.matches(message).count(), 1, "{err}");
    assert!(err.contains(&format!("action {action} failed")), "{err}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn usage_and_metrics_take_their_longest_range() {
    let env = Env::start().await;
    let (cluster, _) = create_cluster(&env, "long");
    for (command, since) in [("usage", "90d"), ("metrics", "30d")] {
        let out = env
            .authed()
            .args(["cluster", command, &cluster, "--since", since, "--json"])
            .output()
            .unwrap();
        assert!(out.status.success(), "{command}: {}", stderr(&out));
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn queries_json_is_the_whole_list_without_top() {
    let env = Env::start().await;
    let (cluster, _) = create_cluster(&env, "all-terms");
    let out = env
        .authed()
        .args(["cluster", "queries", &cluster, "--json"])
        .output()
        .unwrap();
    assert!(out.status.success(), "{}", stderr(&out));
    // The mock has 28 terms: more than the human default of 20.
    assert_eq!(stdout_json(&out)["queries"].as_array().unwrap().len(), 28);

    let out = env
        .authed()
        .args(["cluster", "queries", &cluster])
        .output()
        .unwrap();
    assert!(out.status.success(), "{}", stderr(&out));
    // A header and 20 rows.
    assert_eq!(stdout(&out).lines().count(), 21, "{}", stdout(&out));
}

/// A reader that closes the pipe before `bonsai` writes (`bonsai … | head` after
/// head has gone): silent, exit 0.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_closed_pipe_is_silent() {
    let env = Env::start().await;
    let (cluster, _) = create_cluster(&env, "piped");
    for args in [
        vec!["plans", "--json"],
        vec!["plans"],
        vec!["cluster", "queries", cluster.as_str(), "--json"],
        vec!["cluster", "queries", cluster.as_str()],
    ] {
        let mut child = std_bonsai(&env.mock, env.cfg.path())
            .env("BONSAI_TOKEN", env.token())
            .args(&args)
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .unwrap();
        // Close the read end before the request has come back.
        drop(child.stdout.take());
        let out = child.wait_with_output().unwrap();
        // No error; `queries` still notes what it shows, on stderr.
        let err = stderr(&out);
        assert!(
            !err.contains("couldn't write") && !err.contains("error") && !err.contains('✗'),
            "{args:?}: {err}"
        );
        assert!(out.status.success(), "{args:?}: {:?}", out.status);
    }
}
