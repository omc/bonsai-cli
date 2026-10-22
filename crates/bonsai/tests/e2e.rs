//! The real `bonsai` binary against an in-process `bonsai-mock`.
//!
//! Each test starts its own mock on a fresh database, so tokens it revokes and
//! clusters it creates don't leak into other tests. The runtime is multi-threaded so
//! the mock keeps serving while a test blocks on the binary.

use std::path::Path;
use std::process::{Child, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use predicates::prelude::*;
use serde_json::{Value, json};

mod common;
use common::*;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn version_prints_the_contract_version() {
    let env = Env::start().await;
    let contract = bonsai_contract::Contract::load_default();
    env.bonsai()
        .arg("version")
        .assert()
        .success()
        .stdout(predicate::str::contains(format!(
            "bonsai {}",
            env!("CARGO_PKG_VERSION")
        )))
        .stdout(predicate::str::contains(contract.info_version()));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn version_json_has_the_api_root() {
    let env = Env::start().await;
    let out = env.bonsai().args(["version", "--json"]).output().unwrap();
    assert!(out.status.success());
    let v = stdout_json(&out);
    assert_eq!(v["cli"], env!("CARGO_PKG_VERSION"));
    assert_eq!(v["api"]["name"], "Bonsai API");
    assert_eq!(v["contract"], v["api"]["version"]);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn whoami_with_an_env_token() {
    let env = Env::start().await;
    env.authed()
        .arg("whoami")
        .assert()
        .success()
        .stdout(predicate::str::contains("Acme Search"))
        .stdout(predicate::str::contains("env"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn whoami_human_output() {
    let env = Env::start().await;
    let out = env.authed().arg("whoami").output().unwrap();
    assert!(out.status.success());
    let stdout = String::from_utf8(out.stdout).unwrap();
    // The token's own key: `<key>` in `bonsai_<key>_<secret>`.
    let token = env.token();
    let key = token.split('_').nth(1).unwrap();
    assert!(stdout.contains(key), "{stdout}");
    insta::with_settings!({filters => vec![
        (r"127\.0\.0\.1:\d+", "127.0.0.1:PORT"),
        (key, "KEY"),
    ]}, {
        insta::assert_snapshot!(stdout);
    });
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn whoami_without_a_token_exits_3() {
    let env = Env::start().await;
    env.bonsai()
        .arg("whoami")
        .assert()
        .code(3)
        .stderr(predicate::str::contains("bonsai login"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn login_with_token_to_a_file() {
    let env = Env::start().await;
    env.bonsai()
        .args(["login", "--with-token", "--store-file"])
        .write_stdin(format!("  {}\n", env.token()))
        .assert()
        .success()
        .stderr(predicate::str::contains(format!(
            "Profile default now uses {}",
            env.mock.base_url
        )))
        .stderr(predicate::str::contains("Acme Search"))
        .stderr(predicate::str::contains("credentials.toml"))
        // Said once, not again by bonsai-core's log.
        .stderr(predicate::str::contains("plain text").count(1));

    let creds = std::fs::read_to_string(env.credentials_file()).unwrap();
    assert!(creds.contains(&env.token()));
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(env.credentials_file())
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o600);
    }
    // The token is bound to the host it was accepted by, so login saves the
    // URL, even one from BONSAI_API_URL.
    let config = std::fs::read_to_string(env.config_file()).unwrap();
    assert!(config.contains(env.mock.base_url.as_str()), "{config}");
    // Said when the profile's URL changes, and only then.
    let repointed = format!("Profile default now uses {}", env.mock.base_url);
    env.bonsai()
        .args(["login", "--with-token", "--store-file"])
        .write_stdin(env.token())
        .assert()
        .success()
        .stderr(predicate::str::contains(repointed.as_str()).not());

    env.bonsai()
        .arg("whoami")
        .assert()
        .success()
        .stdout(predicate::str::contains("Acme Search"))
        .stdout(predicate::str::contains("file"));
}

/// A token from the keychain or `credentials.toml` goes only to its
/// profile's host. `BONSAI_TOKEN` goes wherever it's pointed.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_stored_token_is_not_sent_to_another_host() {
    let env = Env::start().await;
    env.bonsai()
        .args(["login", "--with-token", "--store-file"])
        .write_stdin(env.token())
        .assert()
        .success();
    let other = wiremock::MockServer::start().await;
    let other_url = format!("{}/v1", other.uri());
    let mine = env.mock.base_url.host_str().unwrap().to_owned()
        + ":"
        + &env.mock.base_url.port().unwrap().to_string();
    let theirs = other_url
        .trim_start_matches("http://")
        .trim_end_matches("/v1")
        .to_owned();
    let refused = format!("the token for profile default belongs to {mine}, not {theirs}");
    env.bonsai()
        .args(["whoami", "--api-url", &other_url])
        .assert()
        .code(3)
        .stderr(predicate::str::contains(refused.as_str()))
        .stderr(predicate::str::contains("BONSAI_TOKEN"));
    env.bonsai()
        .env("BONSAI_API_URL", &other_url)
        .args(["cluster", "list"])
        .assert()
        .code(3)
        .stderr(predicate::str::contains(refused.as_str()));
    env.bonsai()
        .env("BONSAI_API_URL", &other_url)
        .args(["api", "GET", "/account"])
        .assert()
        .code(3);
    assert!(
        other.received_requests().await.unwrap().is_empty(),
        "the stored token reached another host"
    );

    // The profile's own host passes, however it's given.
    env.bonsai()
        .args(["whoami", "--api-url", env.mock.base_url.as_str()])
        .assert()
        .success()
        .stdout(predicate::str::contains("Acme Search"));

    // BONSAI_TOKEN isn't bound to a host.
    let out = env
        .authed()
        .args(["whoami", "--api-url", &other_url])
        .output()
        .unwrap();
    assert_ne!(
        out.status.code(),
        Some(3),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(!other.received_requests().await.unwrap().is_empty());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn login_saves_an_explicit_api_url() {
    let env = Env::start().await;
    env.bonsai()
        .env_remove("BONSAI_API_URL")
        .args([
            "login",
            "--with-token",
            "--store-file",
            "--profile",
            "local",
        ])
        .args(["--api-url", env.mock.base_url.as_str()])
        .write_stdin(env.token())
        .assert()
        .success();
    let config = std::fs::read_to_string(env.config_file()).unwrap();
    assert!(config.contains("[profiles.local]"), "{config}");
    assert!(config.contains(env.mock.base_url.as_str()), "{config}");

    // The profile now carries the URL on its own.
    env.bonsai()
        .env_remove("BONSAI_API_URL")
        .args(["whoami", "--profile", "local"])
        .assert()
        .success()
        .stdout(predicate::str::contains("Acme Search"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn login_with_token_without_a_keychain_needs_store_file() {
    let env = Env::start().await;
    env.bonsai()
        .args(["login", "--with-token"])
        .write_stdin(env.token())
        .assert()
        .failure()
        .stderr(predicate::str::contains("--store-file"));
    assert!(!env.credentials_file().exists());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn login_with_a_bad_token_stores_nothing() {
    let env = Env::start().await;
    env.bonsai()
        .args(["login", "--with-token", "--store-file"])
        .write_stdin("bonsai_nope_nope")
        .assert()
        .code(3);
    assert!(!env.credentials_file().exists());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn login_without_a_method_non_interactively_exits_2() {
    let env = Env::start().await;
    env.bonsai()
        .arg("login")
        .assert()
        .code(2)
        .stderr(predicate::str::contains("--with-token"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn login_on_the_web() {
    let env = Env::start().await;
    let mut child = std_bonsai(&env.mock, env.cfg.path())
        .args(["login", "--web", "--no-browser", "--store-file", "-vv"])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let stdout = lines(child.stdout.take().unwrap());
    let stderr = lines(child.stderr.take().unwrap());

    // Read stderr until the code and the link show.
    let mut seen = String::new();
    let mut code = None;
    let link = loop {
        let Ok(line) = stderr.recv_timeout(Duration::from_secs(10)) else {
            let _ = child.kill();
            panic!("no code and link within 10 s in: {seen}");
        };
        seen.push_str(&line);
        seen.push('\n');
        if let Some(rest) = line.split("code: ").nth(1) {
            code = Some(rest.trim().to_owned());
        }
        if code.is_some() && line.trim_start().starts_with("http") {
            break line.trim().to_owned();
        }
    };
    let code = code.unwrap();
    assert!(link.starts_with(env.mock.origin.as_str()), "{link}");
    assert!(
        link.contains(&code.replace('-', "")) || link.contains(&code),
        "{link}"
    );

    let approve = env
        .mock
        .origin
        .join(&format!("_mock/cli/logins/{code}/approve"))
        .unwrap();
    let res = bonsai_api::http_client()
        .build()
        .unwrap()
        .post(approve)
        .bearer_auth(&env.mock.admin_token)
        .json(&json!({}))
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), 204);

    let status = wait_for(&mut child, Duration::from_secs(10)).expect("login exits within 10 s");
    let rest: Vec<String> = stderr.try_iter().collect();
    let rest = rest.join("\n");
    let out: Vec<String> = stdout.try_iter().collect();
    assert!(status.success(), "{seen}{rest}");
    assert!(rest.contains("Logged in to Acme Search"), "{rest}");

    // The token never shows, -vv included.
    let creds = std::fs::read_to_string(env.credentials_file()).unwrap();
    let token = creds
        .split('"')
        .find(|s| s.starts_with("bonsai_"))
        .expect("a token in credentials.toml");
    for text in [&seen, &rest, &out.join("\n")] {
        assert!(!text.contains(token), "{text}");
    }

    env.bonsai()
        .arg("whoami")
        .assert()
        .success()
        .stdout(predicate::str::contains("file"))
        .stdout(predicate::str::contains("Bonsai CLI on"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn logout_revokes_the_token() {
    let env = Env::start().await;
    env.bonsai()
        .args(["login", "--with-token", "--store-file"])
        .write_stdin(env.token())
        .assert()
        .success();
    env.bonsai().arg("logout").assert().success();

    let creds = std::fs::read_to_string(env.credentials_file()).unwrap_or_default();
    assert!(!creds.contains(&env.token()), "{creds}");
    env.authed().arg("whoami").assert().code(3);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn logout_keep_token_only_forgets_it_locally() {
    let env = Env::start().await;
    env.bonsai()
        .args(["login", "--with-token", "--store-file"])
        .write_stdin(env.token())
        .assert()
        .success();
    env.bonsai()
        .args(["logout", "--keep-token"])
        .assert()
        .success()
        // BONSAI_KEYRING=off: it says it only removed the file's copy, since a
        // keychain copy from an earlier login would still be valid.
        .stderr(predicate::str::contains(
            "Removed the default token from credentials.toml",
        ))
        .stderr(predicate::str::contains("the keychain wasn't checked"));

    let creds = std::fs::read_to_string(env.credentials_file()).unwrap_or_default();
    assert!(!creds.contains(&env.token()), "{creds}");
    env.authed().arg("whoami").assert().success();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn api_paginate_merges_every_page() {
    let env = Env::start().await;
    for name in ["one", "two", "three"] {
        let body = json!({"name": name, "plan": "sandbox", "region": "aws-us-east-1"});
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
        assert_eq!(stdout_json(&out)["type"], "cluster.provision");
    }

    let out = env
        .authed()
        .args(["api", "GET", "/clusters?limit=1", "--paginate"])
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let all = stdout_json(&out);
    let names: Vec<&str> = all
        .as_array()
        .expect("a JSON array")
        .iter()
        .map(|c| c["name"].as_str().unwrap())
        .collect();
    assert_eq!(names.len(), 3, "{all}");
    for name in ["one", "two", "three"] {
        assert!(names.contains(&name), "{all}");
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn api_reads_the_body_from_a_file_and_stdin() {
    let env = Env::start().await;
    let file = env.cfg.path().join("body.json");
    std::fs::write(
        &file,
        json!({"name": "from-file", "plan": "sandbox", "region": "aws-us-east-1"}).to_string(),
    )
    .unwrap();
    env.authed()
        .args(["api", "POST", "/clusters", "-d"])
        .arg(format!("@{}", file.display()))
        .assert()
        .success();
    env.authed()
        .args(["api", "POST", "/clusters", "-d", "-"])
        .write_stdin(
            json!({"name": "from-stdin", "plan": "sandbox", "region": "aws-us-east-1"}).to_string(),
        )
        .assert()
        .success();

    let out = env
        .authed()
        .args(["api", "GET", "/clusters", "--paginate"])
        .output()
        .unwrap();
    let all = stdout_json(&out).to_string();
    assert!(
        all.contains("from-file") && all.contains("from-stdin"),
        "{all}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn api_errors_print_the_problem_and_map_the_exit_code() {
    let env = Env::start().await;
    let out = env
        .authed()
        .args(["api", "GET", "/clusters/nope"])
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(4));
    let problem: Value = serde_json::from_slice(&out.stderr).unwrap();
    assert_eq!(problem["code"], "not_found");

    env.authed()
        .args(["api", "GET", "/clusters", "-H", "Authorization: Bearer x"])
        .assert()
        .code(2);
    env.authed()
        .args(["api", "POST", "/clusters", "-d", "{not json"])
        .assert()
        .code(2);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn doctor_passes_against_the_mock() {
    let env = Env::start().await;
    env.authed()
        .arg("doctor")
        .assert()
        .success()
        .stdout(predicate::str::contains("✓"))
        .stdout(predicate::str::contains("✗").not());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn doctor_fails_without_a_token() {
    let env = Env::start().await;
    env.bonsai()
        .arg("doctor")
        .assert()
        .code(1)
        .stdout(predicate::str::contains("✗"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn completion_prints_a_script() {
    let env = Env::start().await;
    env.bonsai()
        .args(["completion", "bash"])
        .assert()
        .success()
        .stdout(predicate::str::contains("bonsai"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn errors_under_json_are_json_on_stderr() {
    let env = Env::start().await;
    let out = env
        .bonsai()
        .env("BONSAI_TOKEN", "bonsai_nope_nope")
        .args(["whoami", "--json"])
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(3));
    assert!(out.stdout.is_empty());
    let problem: Value = serde_json::from_slice(&out.stderr).unwrap();
    assert_eq!(problem["code"], "unauthorized");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_insecure_api_url_is_refused() {
    let env = Env::start().await;
    env.authed()
        .args(["whoami", "--api-url", "http://api.example.com/v1"])
        .assert()
        .code(2)
        .stderr(predicate::str::contains("http API URL"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn verbose_logs_never_show_the_token() {
    let env = Env::start().await;
    let login = env
        .bonsai()
        .args(["login", "--with-token", "--store-file", "-vv"])
        .write_stdin(env.token())
        .output()
        .unwrap();
    assert!(login.status.success());
    let whoami = env.authed().args(["whoami", "-vv"]).output().unwrap();
    assert!(whoami.status.success());
    let stderr = String::from_utf8(whoami.stderr.clone()).unwrap();
    assert!(stderr.contains("/v1/account"), "{stderr}");
    for out in [&login, &whoami] {
        for text in [&out.stdout, &out.stderr] {
            let text = String::from_utf8_lossy(text);
            assert!(!text.contains(&env.token()), "{text}");
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn api_output_keeps_the_value_and_escapes_controls() {
    let env = Env::start().await;
    let name = "del\u{7f}name\u{1b}[31mred";
    let body = json!({"name": name, "plan": "sandbox", "region": "aws-us-east-1"});
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
    let id = stdout_json(&out)["subject"]["id"]
        .as_str()
        .unwrap()
        .to_owned();

    let out = env
        .authed()
        .args(["api", "GET", &format!("/clusters/{id}")])
        .output()
        .unwrap();
    assert!(out.status.success());
    // JSON doesn't require DEL to be escaped; the CLI escapes it anyway, as
    // `\u007f`, so it never reaches a terminal raw. The value is unchanged.
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(!out.stdout.contains(&0x7f), "{text:?}");
    assert!(!out.stdout.contains(&0x1b), "{text:?}");
    assert!(text.contains("\\u007f"), "{text:?}");
    assert_eq!(stdout_json(&out)["name"], name);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn api_sends_an_idempotency_key_on_post_and_delete() {
    let env = Env::start().await;
    let body = json!({"name": "keyed", "plan": "sandbox", "region": "aws-us-east-1"});
    let out = env
        .authed()
        .args(["api", "POST", "/clusters", "-d", &body.to_string()])
        .output()
        .unwrap();
    assert!(out.status.success());
    let id = stdout_json(&out)["subject"]["id"]
        .as_str()
        .unwrap()
        .to_owned();
    env.authed()
        .args(["api", "GET", &format!("/clusters/{id}")])
        .assert()
        .success();
    env.authed()
        .args(["api", "DELETE", &format!("/clusters/{id}?name=keyed")])
        .assert()
        .success();

    // The mock records each key it was sent.
    let conn = bonsai_mock::db::open(&env.db.path().join("mock.sqlite")).unwrap();
    let mut stmt = conn
        .prepare("SELECT method, key FROM idempotency ORDER BY created_at")
        .unwrap();
    let keyed: Vec<(String, String)> = stmt
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
        .unwrap()
        .map(Result::unwrap)
        .collect();
    let methods: Vec<&str> = keyed.iter().map(|(m, _)| m.as_str()).collect();
    assert_eq!(methods, ["POST", "DELETE"], "{keyed:?}");
    assert_ne!(keyed[0].1, keyed[1].1);
}

#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_second_ctrl_c_exits_130_while_blocked() {
    let env = Env::start().await;
    let fifo = env.cfg.path().join("body.fifo");
    mkfifo(&fifo);
    let mut child = std_bonsai(&env.mock, env.cfg.path())
        .env("BONSAI_TOKEN", env.token())
        .args(["api", "POST", "/clusters", "-d"])
        .arg(format!("@{}", fifo.display()))
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    // Opening the fifo for writing returns once the binary has opened it for
    // reading, which is after its Ctrl-C handler is installed. Never written to,
    // so the binary's read blocks.
    let _writer = open_for_writing(&fifo, &mut child);
    let started = Instant::now();
    for _ in 0..2 {
        sigint(&child);
        std::thread::sleep(Duration::from_millis(100));
    }
    let status = wait_for(&mut child, Duration::from_secs(2)).expect("exits within 2 s");
    assert_eq!(status.code(), Some(130));
    assert!(started.elapsed() < Duration::from_secs(3));
}

#[cfg(unix)]
fn mkfifo(path: &Path) {
    let ok = std::process::Command::new("mkfifo")
        .arg(path)
        .status()
        .unwrap()
        .success();
    assert!(ok, "mkfifo {}", path.display());
}

/// Opens `fifo` for writing, which blocks until `child` opens it for reading;
/// kills `child` and fails after 10 s.
#[cfg(unix)]
fn open_for_writing(fifo: &Path, child: &mut Child) -> std::fs::File {
    let path = fifo.to_owned();
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let _ = tx.send(std::fs::OpenOptions::new().write(true).open(path));
    });
    match rx.recv_timeout(Duration::from_secs(10)) {
        Ok(file) => file.unwrap(),
        Err(_) => {
            let _ = child.kill();
            let _ = child.wait();
            panic!("the binary never opened {}", fifo.display());
        }
    }
}

#[cfg(unix)]
fn sigint(child: &Child) {
    let ok = std::process::Command::new("kill")
        .args(["-INT", &child.id().to_string()])
        .status()
        .unwrap()
        .success();
    assert!(ok);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_malformed_config_names_the_parse_error_once() {
    let env = Env::start().await;
    std::fs::create_dir_all(env.config_file().parent().unwrap()).unwrap();
    std::fs::write(env.config_file(), "active_profile = [\n").unwrap();
    let out = env.authed().arg("whoami").output().unwrap();
    assert_eq!(out.status.code(), Some(1));
    let stderr = String::from_utf8(out.stderr).unwrap();
    assert!(stderr.contains("config.toml"), "{stderr}");
    assert_eq!(stderr.matches("TOML parse error").count(), 1, "{stderr}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn json_errors_are_the_problem_as_the_server_sent_it() {
    let env = Env::start().await;
    let direct: Value = bonsai_api::http_client()
        .build()
        .unwrap()
        .get(format!("{}/account", env.mock.base_url))
        .bearer_auth("bonsai_nope_nope")
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let out = env
        .bonsai()
        .env("BONSAI_TOKEN", "bonsai_nope_nope")
        .args(["whoami", "--json"])
        .output()
        .unwrap();
    let shown: Value = serde_json::from_slice(&out.stderr).unwrap();
    assert_eq!(shown, direct);
}

#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn core_warnings_show_without_verbose() {
    use std::os::unix::fs::PermissionsExt;
    let env = Env::start().await;
    env.bonsai()
        .args(["login", "--with-token", "--store-file"])
        .write_stdin(env.token())
        .assert()
        .success();
    std::fs::set_permissions(
        env.credentials_file(),
        std::fs::Permissions::from_mode(0o644),
    )
    .unwrap();
    env.bonsai()
        .arg("whoami")
        .assert()
        .success()
        .stderr(predicate::str::contains("tightening"));
}

#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn non_interactive_login_never_opens_a_browser() {
    use std::os::unix::fs::PermissionsExt;
    let env = Env::start().await;
    // Every opener `open` might run, as a script that leaves a mark.
    let shims = env.cfg.path().join("shims");
    let mark = env.cfg.path().join("opened");
    std::fs::create_dir_all(&shims).unwrap();
    for name in [
        "xdg-open",
        "gio",
        "gnome-open",
        "kde-open",
        "wslview",
        "open",
        "sensible-browser",
        "x-www-browser",
        "firefox",
        "chromium",
    ] {
        let path = shims.join(name);
        std::fs::write(&path, format!("#!/bin/sh\ntouch '{}'\n", mark.display())).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    let path = format!(
        "{}:{}",
        shims.display(),
        std::env::var("PATH").unwrap_or_default()
    );
    let mut child = std_bonsai(&env.mock, env.cfg.path())
        .env("PATH", path)
        .env("BROWSER", shims.join("xdg-open"))
        // Non-interactive (CI=1, no TTY), without --no-browser.
        .args(["login", "--web", "--store-file"])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let stderr = lines(child.stderr.take().unwrap());
    let mut seen = String::new();
    while !seen.contains("http") {
        match stderr.recv_timeout(Duration::from_secs(10)) {
            Ok(line) => seen.push_str(&line),
            Err(_) => {
                let _ = child.kill();
                panic!("no link within 10 s: {seen}");
            }
        }
    }
    // Long enough for an opener to have run.
    std::thread::sleep(Duration::from_millis(500));
    let _ = child.kill();
    let _ = child.wait();
    assert!(!mark.exists(), "a browser was opened");
}

/// `config.toml` as a fifo blocks the runtime thread itself in `Config::load`: the
/// first Ctrl-C can't be acted on, and only the signal thread's second one ends it.
#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_second_ctrl_c_exits_even_with_the_runtime_blocked() {
    let env = Env::start().await;
    std::fs::create_dir_all(env.config_file().parent().unwrap()).unwrap();
    mkfifo(&env.config_file());
    let mut child = std_bonsai(&env.mock, env.cfg.path())
        .env("BONSAI_TOKEN", env.token())
        .arg("whoami")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    // The handler is installed before the command loads its config.
    let _writer = open_for_writing(&env.config_file(), &mut child);
    sigint(&child);
    std::thread::sleep(Duration::from_millis(300));
    assert!(
        child.try_wait().unwrap().is_none(),
        "the first Ctrl-C ended it"
    );
    sigint(&child);
    let status = wait_for(&mut child, Duration::from_secs(2)).expect("exits within 2 s");
    assert_eq!(status.code(), Some(130));
}

/// A second Ctrl-C exits while the main thread is stuck writing to a full pipe,
/// holding stdout's lock.
#[cfg(target_os = "linux")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_second_ctrl_c_exits_with_stdout_blocked() {
    use std::io::Read;
    use std::os::fd::AsRawFd;
    let env = Env::start().await;
    let (mut reader, writer) = std::io::pipe().unwrap();
    // A 4 KiB pipe; the bash completion script is far bigger.
    let set = unsafe { libc::fcntl(writer.as_raw_fd(), libc::F_SETPIPE_SZ, 4096) };
    assert!(set >= 4096, "F_SETPIPE_SZ failed");
    let mut child = std_bonsai(&env.mock, env.cfg.path())
        .args(["completion", "bash"])
        .stdin(Stdio::null())
        .stdout(writer)
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    // Output has started, so the handler is installed; then stop reading and let
    // the pipe fill.
    let mut first = [0u8; 64];
    reader.read_exact(&mut first).unwrap();
    std::thread::sleep(Duration::from_millis(300));
    assert!(child.try_wait().unwrap().is_none(), "it didn't block");
    sigint(&child);
    std::thread::sleep(Duration::from_millis(300));
    sigint(&child);
    let status = wait_for(&mut child, Duration::from_secs(2)).expect("exits within 2 s");
    assert_eq!(status.code(), Some(130));
    drop(reader);
}

/// A full disk fails the command: `bonsai url > .env` mustn't look like it worked.
#[cfg(target_os = "linux")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_failed_stdout_write_fails_the_command() {
    let env = Env::start().await;
    for args in [&["version", "--json"][..], &["completion", "bash"][..]] {
        let full = std::fs::OpenOptions::new()
            .write(true)
            .open("/dev/full")
            .unwrap();
        let out = std_bonsai(&env.mock, env.cfg.path())
            .args(args)
            .stdout(full)
            .output()
            .unwrap();
        assert_eq!(out.status.code(), Some(1), "{args:?}");
        let stderr = String::from_utf8_lossy(&out.stderr);
        assert!(stderr.contains("couldn't write"), "{args:?}: {stderr}");
    }
}

/// A store that can't be written to says why: the OS error, not just the path.
#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn logout_errors_keep_their_cause() {
    use std::os::unix::fs::PermissionsExt;
    let env = Env::start().await;
    env.bonsai()
        .args(["login", "--with-token", "--store-file"])
        .write_stdin(env.token())
        .assert()
        .success();
    let dir = env.config_file().parent().unwrap().to_owned();
    std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o500)).unwrap();
    // Root ignores the mode; nothing to test then.
    let writable = std::fs::write(dir.join("probe"), "").is_ok();
    let out = env
        .bonsai()
        .args(["logout", "--keep-token"])
        .output()
        .unwrap();
    std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700)).unwrap();
    if writable {
        return;
    }
    assert_eq!(out.status.code(), Some(1));
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("Permission denied"), "{stderr}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_404_without_a_problem_says_the_api_url_is_wrong() {
    // Something answers, but not Bonsai's /v1: a plain 404 with no Problem body.
    let server = wiremock::MockServer::start().await;
    wiremock::Mock::given(wiremock::matchers::any())
        .respond_with(wiremock::ResponseTemplate::new(404).set_body_string("Not Found"))
        .mount(&server)
        .await;
    let env = Env::start().await;
    let api_url = format!("{}/v1", server.uri());
    let out = env
        .authed()
        .env("BONSAI_API_URL", &api_url)
        .arg("whoami")
        .output()
        .unwrap();
    let err = String::from_utf8_lossy(&out.stderr);
    assert_eq!(out.status.code(), Some(4), "{err}");
    assert!(
        err.contains(&format!(
            "no Bonsai /v1 API at {api_url}; check --api-url or BONSAI_API_URL"
        )),
        "{err}"
    );
    // A Problem 404 is still the API's own answer.
    env.authed()
        .args(["cluster", "show", "nope"])
        .assert()
        .code(4)
        .stderr(predicate::str::contains("no Bonsai").not());
}

/// `api --paginate` stops on a cursor cycle (A -> B -> A) instead of looping.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn api_paginate_stops_on_a_cursor_cycle() {
    use wiremock::matchers::{method, path, query_param, query_param_is_missing};
    use wiremock::{Mock, MockServer, ResponseTemplate};
    let server = MockServer::start().await;
    let page = |next: &str| json!({"data": [{"n": next}], "next_cursor": next});
    Mock::given(method("GET"))
        .and(path("/v1/clusters"))
        .and(query_param_is_missing("cursor"))
        .respond_with(ResponseTemplate::new(200).set_body_json(page("a")))
        .mount(&server)
        .await;
    for (sent, next) in [("a", "b"), ("b", "a")] {
        Mock::given(method("GET"))
            .and(path("/v1/clusters"))
            .and(query_param("cursor", sent))
            .respond_with(ResponseTemplate::new(200).set_body_json(page(next)))
            .mount(&server)
            .await;
    }
    let env = Env::start().await;
    let out = env
        .authed()
        .env("BONSAI_API_URL", format!("{}/v1", server.uri()))
        .args(["api", "GET", "/clusters", "--paginate"])
        .timeout(Duration::from_secs(10))
        .output()
        .unwrap();
    assert_eq!(
        out.status.code(),
        Some(1),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(String::from_utf8_lossy(&out.stderr).contains("twice"));
    assert_eq!(server.received_requests().await.unwrap().len(), 3);
}

/// http on loopback never goes through a proxy: the token would leave the
/// machine in clear text.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_loopback_api_bypasses_the_proxy() {
    let env = Env::start().await;
    let dead = "http://127.0.0.1:9";
    env.authed()
        .env("HTTP_PROXY", dead)
        .env("http_proxy", dead)
        .env("ALL_PROXY", dead)
        .env("all_proxy", dead)
        .arg("whoami")
        .assert()
        .success()
        .stdout(predicate::str::contains("Acme Search"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_api_url_with_a_password_is_refused_without_echoing_it() {
    let env = Env::start().await;
    let mut url = env.mock.base_url.clone();
    url.set_username("u").unwrap();
    url.set_password(Some("hunter2")).unwrap();
    let out = env
        .authed()
        .args(["whoami", "--api-url", url.as_str()])
        .output()
        .unwrap();
    let err = String::from_utf8_lossy(&out.stderr);
    assert_eq!(out.status.code(), Some(2), "{err}");
    assert!(!err.contains("hunter2"), "{err}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn api_takes_a_path_copied_with_v1() {
    let env = Env::start().await;
    let out = env
        .authed()
        .args(["api", "GET", "/v1/plans"])
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(stdout_json(&out)["data"].is_array());
}

/// `doctor` and `version` name a bare 404 as a wrong API URL, as other commands
/// do, and `doctor` skips the token check behind it.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn doctor_and_version_name_a_wrong_api_url() {
    let server = wiremock::MockServer::start().await;
    wiremock::Mock::given(wiremock::matchers::any())
        .respond_with(wiremock::ResponseTemplate::new(404).set_body_string("Not Found"))
        .mount(&server)
        .await;
    let env = Env::start().await;
    let api_url = format!("{}/v1", server.uri());
    let wrong = format!("no Bonsai /v1 API at {api_url}; check --api-url or BONSAI_API_URL");
    let out = env
        .authed()
        .env("BONSAI_API_URL", &api_url)
        .arg("doctor")
        .output()
        .unwrap();
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(text.contains(&wrong), "{text}");
    let token = text.lines().find(|l| l.contains("Token valid")).unwrap();
    assert!(token.contains("skipped"), "{text}");
    assert!(!token.contains("HTTP 404"), "{text}");

    let out = env
        .authed()
        .env("BONSAI_API_URL", &api_url)
        .arg("version")
        .output()
        .unwrap();
    assert!(out.status.success());
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(err.contains(&wrong), "{err}");
}
