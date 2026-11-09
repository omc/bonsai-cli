//! `bonsai mcp`: the stdio MCP server, driven over JSON-RPC against an in-process
//! `bonsai-mock`.

use std::io::Write;
use std::path::Path;
use std::process::{Child, ChildStdin, Stdio};
use std::sync::mpsc::Receiver;
use std::time::Duration;

use serde_json::{Value, json};
use tempfile::TempDir;

mod common;
use common::*;

/// The newest protocol version with an `initialize` handshake.
const PROTOCOL: &str = "2025-11-25";

/// A running `bonsai mcp`, initialized.
struct Mcp {
    child: Child,
    stdin: Option<ChildStdin>,
    out: Receiver<String>,
    err: Receiver<String>,
    next_id: u64,
}

impl Mcp {
    fn start(env: &Env, dir: &Path, args: &[&str]) -> Self {
        let mut child = std_bonsai(&env.mock, env.cfg.path())
            .env("BONSAI_TOKEN", env.token())
            .arg("mcp")
            .args(args)
            .current_dir(dir)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        let stdin = child.stdin.take().unwrap();
        let out = lines(child.stdout.take().unwrap());
        let err = lines(child.stderr.take().unwrap());
        let mut mcp = Self {
            child,
            stdin: Some(stdin),
            out,
            err,
            next_id: 1,
        };
        let init = mcp.request(
            "initialize",
            json!({
                "protocolVersion": PROTOCOL,
                "capabilities": {},
                "clientInfo": { "name": "test", "version": "0" },
            }),
        );
        assert_eq!(init["result"]["serverInfo"]["name"], "bonsai", "{init}");
        mcp.send(&json!({ "jsonrpc": "2.0", "method": "notifications/initialized" }));
        mcp
    }

    fn send(&mut self, msg: &Value) {
        let stdin = self.stdin.as_mut().expect("stdin is open");
        writeln!(stdin, "{msg}").unwrap();
        stdin.flush().unwrap();
    }

    /// Sends a request and returns its response. Every stdout line must be JSON-RPC.
    fn request(&mut self, method: &str, params: Value) -> Value {
        let id = self.next_id;
        self.next_id += 1;
        self.send(&json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params }));
        loop {
            let line = self
                .out
                .recv_timeout(Duration::from_secs(30))
                .unwrap_or_else(|_| panic!("no answer to {method}; stderr: {:?}", self.stderr()));
            let msg: Value = serde_json::from_str(&line)
                .unwrap_or_else(|e| panic!("stdout line isn't JSON ({e}): {line:?}"));
            assert_eq!(msg["jsonrpc"], "2.0", "{msg}");
            if msg["id"] == id {
                return msg;
            }
        }
    }

    fn tools(&mut self) -> Vec<Value> {
        let res = self.request("tools/list", json!({}));
        res["result"]["tools"].as_array().unwrap().clone()
    }

    fn tool_names(&mut self) -> Vec<String> {
        let mut names: Vec<String> = self
            .tools()
            .iter()
            .map(|t| t["name"].as_str().unwrap().to_owned())
            .collect();
        names.sort();
        names
    }

    /// Calls `name`; returns `(is_error, text of the first content block)`.
    fn call(&mut self, name: &str, args: Value) -> (bool, String) {
        let res = self.request("tools/call", json!({ "name": name, "arguments": args }));
        let result = &res["result"];
        assert!(result.is_object(), "{name}: {res}");
        let text = result["content"][0]["text"]
            .as_str()
            .unwrap_or("")
            .to_owned();
        (result["isError"] == true, text)
    }

    /// Calls `name` and parses its result as JSON; panics on a tool error.
    fn call_json(&mut self, name: &str, args: Value) -> Value {
        let (err, text) = self.call(name, args);
        assert!(!err, "{name} failed: {text}");
        serde_json::from_str(&text).unwrap_or_else(|e| panic!("{name}: not JSON ({e}): {text}"))
    }

    fn stderr(&self) -> Vec<String> {
        self.err.try_iter().collect()
    }
}

impl Drop for Mcp {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// An empty project folder that is a git repository ignoring `.env`.
fn project() -> TempDir {
    let dir = tempfile::tempdir().unwrap();
    git(dir.path(), &["init", "-q"]);
    std::fs::write(dir.path().join(".gitignore"), ".env\n").unwrap();
    dir
}

fn git(dir: &Path, args: &[&str]) {
    let ok = std::process::Command::new("git")
        .args(args)
        .current_dir(dir)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .status()
        .unwrap()
        .success();
    assert!(ok, "git {args:?}");
}

/// Creates a sandbox cluster through `bonsai api`; returns its ID.
fn create_cluster(env: &Env, name: &str) -> String {
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
    stdout_json(&out)["subject"]["id"]
        .as_str()
        .unwrap()
        .to_owned()
}

/// The password of the cluster's first credential, read through `bonsai api`.
fn password_of(env: &Env, cluster_id: &str) -> String {
    let api = |path: String| {
        let out = env.authed().args(["api", "GET", &path]).output().unwrap();
        stdout_json(&out)
    };
    let creds = api(format!("/clusters/{cluster_id}/credentials"));
    let cred = creds["data"][0]["id"].as_str().unwrap();
    let secret = api(format!("/clusters/{cluster_id}/credentials/{cred}"));
    let password = secret["password"].as_str().unwrap().to_owned();
    assert!(password.len() >= 8, "a real password to look for");
    password
}

const READ_TOOLS: [&str; 14] = [
    "get_action",
    "get_cluster",
    "get_cluster_metrics",
    "get_cluster_top_queries",
    "get_cluster_usage",
    "list_cluster_actions",
    "list_clusters",
    "list_plans",
    "list_regions",
    "list_releases",
    "list_spaces",
    "wait_action",
    "whoami",
    "write_cluster_url",
];

const WRITE_TOOLS: [&str; 4] = [
    "create_cluster",
    "destroy_cluster",
    "rename_cluster",
    "resize_cluster",
];

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn lists_every_tool_with_its_hints() {
    let env = Env::start().await;
    let dir = project();
    let mut mcp = Mcp::start(&env, dir.path(), &[]);
    let mut want: Vec<&str> = READ_TOOLS.iter().chain(&WRITE_TOOLS).copied().collect();
    want.sort_unstable();
    assert_eq!(mcp.tool_names(), want);

    let tools = mcp.tools();
    let hints =
        |name: &str| tools.iter().find(|t| t["name"] == name).unwrap()["annotations"].clone();
    assert_eq!(hints("list_clusters")["readOnlyHint"], true);
    assert_eq!(hints("destroy_cluster")["readOnlyHint"], false);
    assert_eq!(hints("destroy_cluster")["destructiveHint"], true);
    assert_eq!(hints("create_cluster")["destructiveHint"], false);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn read_only_leaves_out_the_write_tools() {
    let env = Env::start().await;
    let dir = project();
    let mut mcp = Mcp::start(&env, dir.path(), &["--read-only"]);
    assert_eq!(mcp.tool_names(), READ_TOOLS);
    let res = mcp.request(
        "tools/call",
        json!({ "name": "destroy_cluster", "arguments": { "cluster": "x", "confirm": "x" } }),
    );
    assert!(
        res["error"].is_object() || res["result"]["isError"] == true,
        "{res}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn reads_clusters_and_the_catalogue() {
    let env = Env::start().await;
    let id = create_cluster(&env, "reader");
    let dir = project();
    let mut mcp = Mcp::start(&env, dir.path(), &[]);

    let account = mcp.call_json("whoami", json!({}));
    assert_eq!(account["id"], "acme", "{account}");

    let clusters = mcp.call_json("list_clusters", json!({}));
    assert!(
        clusters
            .as_array()
            .unwrap()
            .iter()
            .any(|c| c["id"] == id.as_str()),
        "{clusters}"
    );
    let by_name = mcp.call_json("get_cluster", json!({ "cluster": "reader" }));
    assert_eq!(by_name["id"], id.as_str());

    let plans = mcp.call_json("list_plans", json!({}));
    assert!(
        plans
            .as_array()
            .unwrap()
            .iter()
            .any(|p| p["id"] == "sandbox")
    );
    let regions = mcp.call_json("list_regions", json!({}));
    assert!(!regions.as_array().unwrap().is_empty());

    let history = mcp.call_json("list_cluster_actions", json!({ "cluster": id }));
    assert!(!history.as_array().unwrap().is_empty(), "{history}");
    let usage = mcp.call_json("get_cluster_usage", json!({ "cluster": id, "since": "7d" }));
    assert!(usage.is_object(), "{usage}");
    let metrics = mcp.call_json(
        "get_cluster_metrics",
        json!({ "cluster": id, "since": "1h" }),
    );
    assert!(metrics.is_object(), "{metrics}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_unknown_cluster_is_a_tool_error() {
    let env = Env::start().await;
    let dir = project();
    let mut mcp = Mcp::start(&env, dir.path(), &[]);
    let (err, text) = mcp.call("get_cluster", json!({ "cluster": "nope" }));
    assert!(err, "{text}");
    assert!(text.contains("nope"), "{text}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn creates_waits_resizes_renames_and_destroys() {
    let env = Env::start().await;
    let dir = project();
    let mut mcp = Mcp::start(&env, dir.path(), &[]);

    let action = mcp.call_json(
        "create_cluster",
        json!({ "name": "made", "plan": "sandbox", "region": "aws-us-east-1" }),
    );
    let id = action["subject"]["id"].as_str().unwrap().to_owned();
    let done = mcp.call_json("wait_action", json!({ "action": action["id"] }));
    assert_eq!(done["state"], "success", "{done}");

    let renamed = mcp.call_json(
        "rename_cluster",
        json!({ "cluster": id, "new_name": "kept" }),
    );
    assert_eq!(renamed["name"], "kept");

    let resize = mcp.call_json(
        "resize_cluster",
        json!({ "cluster": id, "plan": "standard-fir" }),
    );
    let done = mcp.call_json("wait_action", json!({ "action": resize["id"] }));
    assert_eq!(done["state"], "success", "{done}");

    let (err, text) = mcp.call(
        "destroy_cluster",
        json!({ "cluster": id, "confirm": "made" }),
    );
    assert!(err, "a stale name must not destroy: {text}");
    assert!(text.contains("kept"), "names the cluster's name: {text}");
    let still = mcp.call_json("get_cluster", json!({ "cluster": id }));
    assert_ne!(still["state"], "deprovisioned");

    let gone = mcp.call_json(
        "destroy_cluster",
        json!({ "cluster": id, "confirm": "kept" }),
    );
    let done = mcp.call_json("wait_action", json!({ "action": gone["id"] }));
    assert_eq!(done["state"], "success", "{done}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn writes_the_url_to_an_ignored_env_file_without_returning_it() {
    let env = Env::start().await;
    let id = create_cluster(&env, "secret");
    let password = password_of(&env, &id);
    let dir = project();
    std::fs::write(dir.path().join(".env"), "KEEP=1\nBONSAI_URL=old\n").unwrap();
    let mut mcp = Mcp::start(&env, dir.path(), &[]);

    let (err, text) = mcp.call("write_cluster_url", json!({ "cluster": "secret" }));
    assert!(!err, "{text}");
    assert!(!text.contains(&password), "the password came back: {text}");
    assert!(text.contains("****"), "a redacted URL: {text}");

    let file = std::fs::read_to_string(dir.path().join(".env")).unwrap();
    let lines: Vec<&str> = file.lines().collect();
    assert_eq!(lines.len(), 2, "{file}");
    assert_eq!(lines[0], "KEEP=1");
    assert!(lines[1].starts_with("BONSAI_URL=https://"), "{file}");
    assert!(lines[1].contains(&password), "{file}");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(dir.path().join(".env"))
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o600);
    }

    let (err, _) = mcp.call(
        "write_cluster_url",
        json!({ "cluster": "secret", "path": ".env.local", "variable": "ELASTICSEARCH_URL" }),
    );
    assert!(err, ".env.local isn't ignored");
    assert!(!dir.path().join(".env.local").exists());

    for path in ["../.env", "/tmp/bonsai.env"] {
        let (err, text) = mcp.call(
            "write_cluster_url",
            json!({ "cluster": "secret", "path": path }),
        );
        assert!(err, "{path}: {text}");
    }
    let (err, _) = mcp.call(
        "write_cluster_url",
        json!({ "cluster": "secret", "variable": "NOT-A-NAME" }),
    );
    assert!(err);

    for line in mcp.stderr() {
        assert!(!line.contains(&password), "stderr: {line}");
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn refuses_a_tracked_env_file() {
    let env = Env::start().await;
    create_cluster(&env, "tracked");
    let dir = tempfile::tempdir().unwrap();
    git(dir.path(), &["init", "-q"]);
    std::fs::write(dir.path().join(".env"), "A=1\n").unwrap();
    git(dir.path(), &["add", ".env"]);
    let mut mcp = Mcp::start(&env, dir.path(), &[]);
    let (err, text) = mcp.call("write_cluster_url", json!({ "cluster": "tracked" }));
    assert!(err, "{text}");
    assert_eq!(
        std::fs::read_to_string(dir.path().join(".env")).unwrap(),
        "A=1\n"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn without_a_token_it_exits_3_before_serving() {
    let env = Env::start().await;
    let dir = project();
    let out = std_bonsai(&env.mock, env.cfg.path())
        .arg("mcp")
        .current_dir(dir.path())
        .stdin(Stdio::null())
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(3));
    assert!(
        out.stdout.is_empty(),
        "{}",
        String::from_utf8_lossy(&out.stdout)
    );
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("bonsai login"),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn closing_stdin_stops_it_quietly() {
    let env = Env::start().await;
    let dir = project();
    let mut mcp = Mcp::start(&env, dir.path(), &[]);
    mcp.tool_names();
    // A client quitting closes the pipe.
    drop(mcp.stdin.take());
    let status = wait_for(&mut mcp.child, Duration::from_secs(10)).expect("exits after EOF");
    assert_eq!(status.code(), Some(0));
    let err = mcp.stderr();
    assert!(err.is_empty(), "{err:?}");
}
