//! Spawns `bonsai-mock` on a random port and calls it through a client that
//! validates every response against `openapi/v1.yml`, so every mock test is
//! also a contract test.

#![allow(dead_code)]

use std::collections::BTreeSet;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Mutex;

use bonsai_contract::{Contract, ResponseCheck};
use bonsai_mock::{MockHandle, MockOptions};
use reqwest::header::HeaderMap;
use serde_json::Value;

pub struct Mock {
    pub h: MockHandle,
    pub http: reqwest::Client,
    pub contract: Contract,
    /// Every scope, on the `acme` account (has a payment method).
    pub acme: String,
    /// `account:read clusters:read`, on the `globex` account (no payment method).
    pub globex: String,
    /// The SQLite file, for tests that set scenarios or tokens directly.
    pub db_path: PathBuf,
    /// Every `(operationId, status)` that [`Mock::call`] has validated.
    seen: Mutex<BTreeSet<(String, u16)>>,
    _dir: tempfile::TempDir,
}

impl Mock {
    /// A fresh database in a temp dir, `step_ms` 0, port 0.
    pub async fn start() -> Self {
        Self::start_opts(0, false).await
    }

    pub async fn start_with(step_ms: u64) -> Self {
        Self::start_opts(step_ms, false).await
    }

    /// With `local_clusters`: credential URLs point at the mock, which answers them.
    pub async fn start_local() -> Self {
        Self::start_opts(0, true).await
    }

    async fn start_opts(step_ms: u64, local_clusters: bool) -> Self {
        let dir = tempfile::tempdir().expect("tempdir");
        let db_path = dir.path().join("mock.sqlite");
        let addr: SocketAddr = "127.0.0.1:0".parse().unwrap();
        let h = bonsai_mock::spawn(
            MockOptions {
                db_path: db_path.clone(),
                step_ms,
                public_url: None,
                checkout_url: None,
                local_clusters,
            },
            addr,
        )
        .await
        .expect("spawn mock");
        let token = |account: &str| {
            h.seeded_tokens
                .iter()
                .find(|t| t.account_id == account)
                .unwrap_or_else(|| panic!("no seeded token for {account}"))
                .token
                .clone()
        };
        let (acme, globex) = (token("acme"), token("globex"));
        let http = bonsai_api::http_client()
            .no_proxy()
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .unwrap();
        Self {
            h,
            http,
            contract: Contract::load_strict(),
            acme,
            globex,
            db_path,
            seen: Mutex::new(BTreeSet::new()),
            _dir: dir,
        }
    }

    /// A second connection to the mock's database.
    pub fn db(&self) -> bonsai_mock::db::Connection {
        bonsai_mock::db::open(&self.db_path).expect("open mock db")
    }

    /// Calls the mock and validates the response against the contract,
    /// panicking with the violation if it doesn't conform.
    ///
    /// `path` is relative to `/v1` (`"/clusters?limit=2"`, `""` for the root);
    /// a path already starting with `/v1` is used as is.
    pub async fn call(
        &self,
        method: &str,
        path: &str,
        token: Option<&str>,
        body: Option<Value>,
        headers: &[(&str, &str)],
    ) -> (u16, HeaderMap, Option<Value>) {
        let body = body.map(|b| serde_json::to_vec(&b).expect("JSON body"));
        let content_type = body.as_ref().map(|_| "application/json");
        self.send(method, path, token, body, content_type, headers)
            .await
    }

    /// [`Mock::call`] with the body sent byte for byte, for malformed or empty
    /// bodies. The response is still validated against the contract.
    pub async fn call_raw(
        &self,
        method: &str,
        path: &str,
        token: Option<&str>,
        body: &[u8],
        content_type: Option<&str>,
        headers: &[(&str, &str)],
    ) -> (u16, HeaderMap, Option<Value>) {
        self.send(
            method,
            path,
            token,
            Some(body.to_vec()),
            content_type,
            headers,
        )
        .await
    }

    /// Calls the mock without checking the response against the contract,
    /// for what `v1.yml` doesn't describe: `/v1/openapi.json`, `/_mock/*`, a
    /// wrong method. A path
    /// starting with `/_` is used as is; otherwise as in [`Mock::call`]. The
    /// body is parsed as JSON when there is one and it parses.
    pub async fn call_unchecked(
        &self,
        method: &str,
        path: &str,
        token: Option<&str>,
        body: Option<Value>,
        headers: &[(&str, &str)],
    ) -> (u16, HeaderMap, Option<Value>) {
        let body = body.map(|b| serde_json::to_vec(&b).expect("JSON body"));
        let content_type = body.as_ref().map(|_| "application/json");
        let (_, status, hdrs, bytes) = self
            .transport(method, path, token, body, content_type, headers)
            .await;
        (status, hdrs, serde_json::from_slice(&bytes).ok())
    }

    /// [`Mock::call_unchecked`] on `/_mock{path}` with the admin token.
    pub async fn admin(
        &self,
        method: &str,
        path: &str,
        body: Option<Value>,
    ) -> (u16, Option<Value>) {
        let (status, _, body) = self
            .call_unchecked(
                method,
                &format!("/_mock{path}"),
                Some(&self.h.admin_token),
                body,
                &[],
            )
            .await;
        (status, body)
    }

    /// Every `(operationId, status)` [`Mock::call`] has validated so far.
    pub fn seen(&self) -> BTreeSet<(String, u16)> {
        self.seen.lock().unwrap().clone()
    }

    /// The URL for `path`, which starts with `/` and is taken as is.
    pub fn url(&self, path: &str) -> String {
        format!("{}{}", self.h.origin.as_str().trim_end_matches('/'), path)
    }

    async fn transport(
        &self,
        method: &str,
        path: &str,
        token: Option<&str>,
        body: Option<Vec<u8>>,
        content_type: Option<&str>,
        headers: &[(&str, &str)],
    ) -> (String, u16, HeaderMap, Vec<u8>) {
        let full = if path.starts_with("/_")
            || path == "/v1"
            || path.starts_with("/v1/")
            || path.starts_with("/v1?")
        {
            path.to_string()
        } else {
            format!("/v1{path}")
        };
        let url = self.url(&full);
        let method_v = reqwest::Method::from_bytes(method.as_bytes()).expect("method");
        let mut req = self.http.request(method_v, &url);
        if let Some(t) = token {
            req = req.bearer_auth(t);
        }
        for (k, v) in headers {
            req = req.header(*k, *v);
        }
        if let Some(ct) = content_type {
            req = req.header(reqwest::header::CONTENT_TYPE, ct);
        }
        if let Some(b) = body {
            req = req.body(b);
        }
        let resp = req
            .send()
            .await
            .unwrap_or_else(|e| panic!("{method} {url}: {e}"));
        let status = resp.status().as_u16();
        let hdrs = resp.headers().clone();
        let bytes = resp.bytes().await.expect("body").to_vec();
        (full, status, hdrs, bytes)
    }

    async fn send(
        &self,
        method: &str,
        path: &str,
        token: Option<&str>,
        body: Option<Vec<u8>>,
        content_type: Option<&str>,
        headers: &[(&str, &str)],
    ) -> (u16, HeaderMap, Option<Value>) {
        let (full, status, hdrs, bytes) = self
            .transport(method, path, token, body, content_type, headers)
            .await;
        let json: Option<Value> = if bytes.is_empty() {
            None
        } else {
            Some(serde_json::from_slice(&bytes).unwrap_or_else(|e| {
                panic!(
                    "{method} {full} -> {status}: body is not JSON ({e}): {}",
                    String::from_utf8_lossy(&bytes)
                )
            }))
        };

        let bare = full.split('?').next().unwrap_or("");
        let contract_path = match bare.strip_prefix("/v1").unwrap_or(bare) {
            "" => "/",
            p => p,
        };
        let header_pairs: Vec<(String, String)> = hdrs
            .iter()
            .map(|(k, v)| (k.as_str().to_string(), v.to_str().unwrap_or("").to_string()))
            .collect();
        let content_type = hdrs
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok());
        let check = ResponseCheck {
            method,
            path: contract_path,
            status,
            content_type,
            headers: &header_pairs,
            body: json.as_ref(),
        };
        match self.contract.validate_response(&check) {
            Ok(op) => {
                self.seen.lock().unwrap().insert((op.id.clone(), status));
            }
            Err(v) => panic!(
                "contract violation for {method} {full} -> {status}: {v}\nheaders: {header_pairs:?}\nbody: {}",
                json.as_ref()
                    .map(|j| serde_json::to_string_pretty(j).unwrap())
                    .unwrap_or_default()
            ),
        }
        (status, hdrs, json)
    }
}

pub fn header<'a>(h: &'a HeaderMap, name: &str) -> Option<&'a str> {
    h.get(name).and_then(|v| v.to_str().ok())
}

/// A `ClusterCreate` body for the shared `aws-us-east-1` region.
pub fn create_body(name: &str, plan: &str) -> Value {
    serde_json::json!({ "name": name, "plan": plan, "region": "aws-us-east-1" })
}

impl Mock {
    /// Creates a cluster as `acme`, asserting the 202, and returns the Action.
    pub async fn create(&self, name: &str, plan: &str) -> Value {
        let (status, _, body) = self
            .call(
                "POST",
                "/clusters",
                Some(&self.acme),
                Some(create_body(name, plan)),
                &[],
            )
            .await;
        let body = body.unwrap();
        assert_eq!(status, 202, "{body}");
        body
    }

    /// `GET /actions/{id}` as `acme`.
    pub async fn action(&self, id: &str) -> Value {
        let (status, _, body) = self
            .call(
                "GET",
                &format!("/actions/{id}"),
                Some(&self.acme),
                None,
                &[],
            )
            .await;
        assert_eq!(status, 200);
        body.unwrap()
    }

    /// `GET /clusters/{id}` as `acme`.
    pub async fn cluster(&self, id: &str) -> Value {
        let (status, _, body) = self
            .call(
                "GET",
                &format!("/clusters/{id}"),
                Some(&self.acme),
                None,
                &[],
            )
            .await;
        assert_eq!(status, 200);
        body.unwrap()
    }

    /// Creates a cluster as `acme` and polls it to `ready` (needs `step_ms` 0).
    /// Returns the cluster ID.
    pub async fn ready_cluster(&self, name: &str, plan: &str) -> String {
        let action = self.create(name, plan).await;
        let done = self.action(action["id"].as_str().unwrap()).await;
        assert_eq!(done["state"], "success", "{done}");
        done["subject"]["id"].as_str().unwrap().to_string()
    }
}

impl Mock {
    /// A new token on `account` with exactly `scopes`, never expiring.
    pub fn token(&self, account: &str, scopes: &[&str]) -> String {
        bonsai_mock::seed::create_token(&self.db(), account, "test token", scopes, None)
            .expect("create token")
    }
}

impl Mock {
    /// Adds plan `id` on `tier`, a copy of seeded plan `like` (its regions,
    /// releases and limits), for a rule no public plan triggers: no Enterprise
    /// plan is public, and none needs its own servers.
    pub fn insert_plan(&self, id: &str, like: &str, tier: &str, dedicated: bool) {
        let n = self
            .db()
            .execute(
                "INSERT INTO plans (id, name, price_in_cents, billing_interval_months, single_tenant,
                    private_network, tier, dedicated, regions, releases, max_credentials, limit_docs,
                    limit_shards, limit_bytes, limit_bandwidth, sort, offers)
                 SELECT ?1, ?1, price_in_cents, billing_interval_months, single_tenant, private_network,
                    ?3, ?4, regions, releases, max_credentials, limit_docs, limit_shards, limit_bytes,
                    limit_bandwidth, (SELECT max(sort) + 1 FROM plans), offers
                 FROM plans WHERE id = ?2",
                bonsai_mock::db::params![id, like, tier, dedicated],
            )
            .expect("insert plan");
        assert_eq!(n, 1, "no plan {like} to copy");
    }
}
