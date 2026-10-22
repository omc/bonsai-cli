//! The `/v1` HTTP client: one typed method per operation.

use crate::models::*;
use crate::problem::ApiError;
use crate::retry::{RawResponse, RetryPolicy, header_retry_after, send_with_retry};
use jiff::Timestamp;
use reqwest::header::{ACCEPT, AUTHORIZATION, HeaderMap, HeaderValue, USER_AGENT};
use reqwest::{Method, RequestBuilder};
use secrecy::{ExposeSecret, SecretString};
use serde::Serialize;
use serde::de::DeserializeOwned;
use serde_json::Value;
use std::fmt;
use std::path::PathBuf;
use std::time::Duration;
use url::Url;

const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);

#[derive(Debug, Clone)]
pub struct ClientConfig {
    /// The API root, ending in `/v1`, e.g. `https://api.bonsai.io/v1`.
    pub base_url: Url,
    pub token: Option<SecretString>,
    pub user_agent: String,
    pub retry: RetryPolicy,
    /// A PEM file of extra root certificates (`BONSAI_CA_BUNDLE`), trusted alongside
    /// the platform's.
    pub ca_bundle: Option<PathBuf>,
}

pub struct Client {
    http: reqwest::Client,
    /// `base_url` with a trailing slash removed.
    base: Url,
    auth: Option<HeaderValue>,
    user_agent: HeaderValue,
    retry: RetryPolicy,
}

impl fmt::Debug for Client {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Client")
            .field("base", &self.base.as_str())
            .field("auth", &self.auth.is_some())
            .field("retry", &self.retry)
            .finish_non_exhaustive()
    }
}

/// A decoded response, with the body as it came for `--json`.
#[derive(Debug, Clone)]
pub struct Response<T> {
    pub value: T,
    pub raw: Value,
    pub status: u16,
    /// `Retry-After`: on an Action, how long to wait before polling it.
    pub retry_after: Option<Duration>,
    pub location: Option<String>,
    /// `Idempotent-Replayed: true`.
    pub idempotent_replayed: bool,
    pub date: Option<Timestamp>,
}

/// `listClusters` filters.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ClusterQuery {
    pub name: Option<String>,
    pub region: Option<String>,
    pub space: Option<String>,
    pub single_tenant: Option<bool>,
    pub state: Option<String>,
    pub limit: Option<u32>,
}

type Query = Vec<(&'static str, String)>;
type Result<T> = std::result::Result<Response<T>, ApiError>;

impl Client {
    /// Builds a client. Refuses a non-`https` URL unless its host is the loopback.
    pub fn new(cfg: ClientConfig) -> std::result::Result<Self, ApiError> {
        check_url(&cfg.base_url)?;
        let mut base = cfg.base_url;
        if let Ok(mut segments) = base.path_segments_mut() {
            segments.pop_if_empty();
        }
        base.set_query(None);
        base.set_fragment(None);

        let auth = cfg
            .token
            .map(|t| {
                let mut v = HeaderValue::from_str(&format!("Bearer {}", t.expose_secret()))
                    .map_err(|_| ApiError::Config("the token isn't a valid header value".into()))?;
                v.set_sensitive(true);
                Ok::<_, ApiError>(v)
            })
            .transpose()?;
        let user_agent = HeaderValue::from_str(&cfg.user_agent)
            .map_err(|_| ApiError::Config("the user agent isn't a valid header value".into()))?;

        let mut builder = http_client()
            .connect_timeout(CONNECT_TIMEOUT)
            .redirect(reqwest::redirect::Policy::none());
        if let Some(path) = &cfg.ca_bundle {
            builder = builder.tls_certs_merge(read_ca_bundle(path)?);
        }
        // Plain http is allowed only on loopback; a proxy would carry the token off
        // the machine in clear text (reqwest doesn't exempt loopback itself).
        if is_loopback_host(&base) {
            builder = builder.no_proxy();
        }
        Ok(Self {
            http: builder.build()?,
            base,
            auth,
            user_agent,
            retry: cfg.retry,
        })
    }

    /// Sends any request under the base URL, for `bonsai api`. `path_and_query` is
    /// relative to `/v1`, e.g. `/clusters?limit=2`; it can't leave `/v1` on the API's host.
    pub async fn send_raw(
        &self,
        method: Method,
        path_and_query: &str,
        body: Option<&Value>,
        idempotency_key: Option<&str>,
    ) -> Result<Value> {
        self.send_raw_with_headers(
            method,
            path_and_query,
            body,
            idempotency_key,
            &HeaderMap::new(),
        )
        .await
    }

    /// [`Client::send_raw`] with extra request headers (`bonsai api -H`), which replace
    /// the client's own `Accept` and `User-Agent` when they name them. `Authorization`
    /// and `Idempotency-Key` are refused: the token comes only from the client's config,
    /// and the key from `idempotency_key`.
    pub async fn send_raw_with_headers(
        &self,
        method: Method,
        path_and_query: &str,
        body: Option<&Value>,
        idempotency_key: Option<&str>,
        headers: &HeaderMap,
    ) -> Result<Value> {
        if let Some(name) = headers
            .keys()
            .find(|n| *n == AUTHORIZATION || n.as_str() == "idempotency-key")
        {
            return Err(ApiError::Config(format!(
                "the {name} header can't be set by hand"
            )));
        }
        // Trimmed, so a base of `https://host/` works as well as `https://host/v1`.
        let mut relative = path_and_query.trim_start_matches('/');
        // A path copied from the docs or a log (`/v1/plans`) when the base already
        // ends in `/v1`: taken as the path under it, not as `/v1/v1/plans`.
        if self.base.path().trim_end_matches('/').ends_with("/v1")
            && let Some(rest) = relative.strip_prefix("v1")
            && (rest.is_empty() || rest.starts_with(['/', '?']))
        {
            relative = rest.trim_start_matches('/');
        }
        let joined = format!("{}/{}", self.base.as_str().trim_end_matches('/'), relative);
        let url = Url::parse(&joined)
            .map_err(|e| ApiError::InvalidPath(format!("{path_and_query}: {e}")))?;
        let root = self.base.path().trim_end_matches('/');
        let under_root = url.path() == root
            || url
                .path()
                .strip_prefix(root)
                .is_some_and(|rest| rest.starts_with('/'));
        if url.origin() != self.base.origin() || !under_root {
            return Err(ApiError::InvalidPath(format!(
                "{path_and_query} leaves {}",
                self.base
            )));
        }
        self.send(method, url, &[], body, idempotency_key, headers)
            .await
    }

    pub async fn get_api_root(&self) -> Result<ApiRoot> {
        self.get(&[], &[]).await
    }

    pub async fn get_account(&self) -> Result<Account> {
        self.get(&["account"], &[]).await
    }

    pub async fn list_clusters(
        &self,
        q: &ClusterQuery,
        cursor: Option<&str>,
    ) -> Result<Page<Cluster>> {
        let mut query = Query::new();
        push(&mut query, "name", q.name.as_ref());
        push(&mut query, "region", q.region.as_ref());
        push(&mut query, "space", q.space.as_ref());
        push(&mut query, "single_tenant", q.single_tenant);
        push(&mut query, "state", q.state.as_ref());
        push(&mut query, "limit", q.limit);
        push(&mut query, "cursor", cursor);
        self.get(&["clusters"], &query).await
    }

    pub async fn create_cluster(&self, body: &ClusterCreate, key: &str) -> Result<Action> {
        self.write(Method::POST, &["clusters"], &[], Some(body), Some(key))
            .await
    }

    pub async fn get_cluster(&self, id: &str) -> Result<Cluster> {
        self.get(&["clusters", id], &[]).await
    }

    /// Not retried once sent: a rename takes no `Idempotency-Key`.
    pub async fn update_cluster(&self, id: &str, body: &ClusterUpdate) -> Result<Cluster> {
        self.write(Method::PATCH, &["clusters", id], &[], Some(body), None)
            .await
    }

    /// `name` must match the cluster's name; the API refuses the delete otherwise.
    pub async fn delete_cluster(&self, id: &str, name: &str, key: &str) -> Result<Action> {
        let query = vec![("name", name.to_owned())];
        self.write::<(), _>(Method::DELETE, &["clusters", id], &query, None, Some(key))
            .await
    }

    pub async fn change_cluster_plan(
        &self,
        id: &str,
        body: &PlanChange,
        key: &str,
    ) -> Result<Action> {
        self.write(
            Method::POST,
            &["clusters", id, "plan_changes"],
            &[],
            Some(body),
            Some(key),
        )
        .await
    }

    pub async fn list_cluster_actions(
        &self,
        id: &str,
        limit: Option<u32>,
        cursor: Option<&str>,
    ) -> Result<Page<Action>> {
        let mut query = Query::new();
        push(&mut query, "limit", limit);
        push(&mut query, "cursor", cursor);
        self.get(&["clusters", id, "actions"], &query).await
    }

    pub async fn get_action(&self, id: &str) -> Result<Action> {
        self.get(&["actions", id], &[]).await
    }

    pub async fn get_cluster_usage(
        &self,
        id: &str,
        from: Option<Timestamp>,
        to: Option<Timestamp>,
    ) -> Result<Usage> {
        let mut query = Query::new();
        push(&mut query, "from", from);
        push(&mut query, "to", to);
        self.get(&["clusters", id, "usage"], &query).await
    }

    pub async fn get_cluster_metrics(
        &self,
        id: &str,
        from: Option<Timestamp>,
        to: Option<Timestamp>,
        resolution: Option<&str>,
    ) -> Result<Metrics> {
        let mut query = Query::new();
        push(&mut query, "from", from);
        push(&mut query, "to", to);
        push(&mut query, "resolution", resolution);
        self.get(&["clusters", id, "metrics"], &query).await
    }

    pub async fn get_cluster_top_queries(
        &self,
        id: &str,
        window: Option<&str>,
    ) -> Result<TopQueries> {
        let mut query = Query::new();
        push(&mut query, "window", window);
        self.get(&["clusters", id, "queries"], &query).await
    }

    pub async fn list_cluster_credentials(
        &self,
        id: &str,
        cursor: Option<&str>,
    ) -> Result<Page<Credential>> {
        let mut query = Query::new();
        push(&mut query, "cursor", cursor);
        self.get(&["clusters", id, "credentials"], &query).await
    }

    pub async fn create_cluster_credential(
        &self,
        id: &str,
        body: &CredentialCreate,
        key: &str,
    ) -> Result<CredentialWithSecret> {
        self.write(
            Method::POST,
            &["clusters", id, "credentials"],
            &[],
            Some(body),
            Some(key),
        )
        .await
    }

    pub async fn get_cluster_credential(
        &self,
        id: &str,
        cred: &str,
    ) -> Result<CredentialWithSecret> {
        self.get(&["clusters", id, "credentials", cred], &[]).await
    }

    /// Not retried once sent: it takes no `Idempotency-Key`.
    pub async fn revoke_cluster_credential(&self, id: &str, cred: &str) -> Result<()> {
        self.write::<(), _>(
            Method::DELETE,
            &["clusters", id, "credentials", cred],
            &[],
            None,
            None,
        )
        .await
    }

    pub async fn list_plans(&self, cursor: Option<&str>) -> Result<Page<Plan>> {
        self.get(&["plans"], &cursor_query(cursor)).await
    }

    pub async fn get_plan(&self, id: &str) -> Result<Plan> {
        self.get(&["plans", id], &[]).await
    }

    pub async fn list_regions(&self, cursor: Option<&str>) -> Result<Page<Region>> {
        self.get(&["regions"], &cursor_query(cursor)).await
    }

    pub async fn list_spaces(&self, cursor: Option<&str>) -> Result<Page<Space>> {
        self.get(&["spaces"], &cursor_query(cursor)).await
    }

    pub async fn list_releases(&self, cursor: Option<&str>) -> Result<Page<Release>> {
        self.get(&["releases"], &cursor_query(cursor)).await
    }

    // Proposed, not in v1.yml. A 404 means the server doesn't have the endpoint yet.

    /// `GET /token`. `None` when the server doesn't offer it.
    pub async fn get_token(&self) -> std::result::Result<Option<Response<TokenInfo>>, ApiError> {
        none_on_404(self.get(&["token"], &[]).await)
    }

    /// `DELETE /token`: revokes the calling token. `None` when the server doesn't offer it.
    pub async fn revoke_token(&self) -> std::result::Result<Option<()>, ApiError> {
        let res = self
            .write::<(), ()>(Method::DELETE, &["token"], &[], None, None)
            .await;
        Ok(none_on_404(res)?.map(|_| ()))
    }

    // The browser login. Neither operation takes a token.

    /// `POST /cli/logins`: starts a browser login. The device code is left out of the response's `raw`
    /// (it stays in `value`, for the poll).
    pub async fn start_cli_login(&self, body: &CliLoginStart) -> Result<CliLoginGrant> {
        let mut res: Response<CliLoginGrant> = self
            .write(Method::POST, &["cli", "logins"], &[], Some(body), None)
            .await?;
        if let Some(code) = res.raw.get_mut("device_code") {
            *code = Value::String("<redacted>".into());
        }
        Ok(res)
    }

    /// `POST /cli/logins/token`. Until the login is approved this fails with a 400
    /// Problem whose code is `authorization_pending`, `slow_down`, `access_denied` or
    /// `expired_token`. The token is left out of the response's `raw`.
    pub async fn poll_cli_login(&self, device_code: &str) -> Result<CliLoginToken> {
        let body = serde_json::json!({ "device_code": device_code });
        let mut res: Response<CliLoginToken> = self
            .write(
                Method::POST,
                &["cli", "logins", "token"],
                &[],
                Some(&body),
                None,
            )
            .await?;
        if let Some(token) = res.raw.get_mut("token") {
            *token = Value::String("<redacted>".into());
        }
        Ok(res)
    }

    async fn get<T: DeserializeOwned>(
        &self,
        segments: &[&str],
        query: &[(&str, String)],
    ) -> Result<T> {
        self.send(
            Method::GET,
            self.url(segments)?,
            query,
            None::<&()>,
            None,
            &HeaderMap::new(),
        )
        .await
    }

    async fn write<B: Serialize, T: DeserializeOwned>(
        &self,
        method: Method,
        segments: &[&str],
        query: &[(&str, String)],
        body: Option<&B>,
        key: Option<&str>,
    ) -> Result<T> {
        self.send(
            method,
            self.url(segments)?,
            query,
            body,
            key,
            &HeaderMap::new(),
        )
        .await
    }

    /// The base URL with each segment appended, percent-encoded.
    ///
    /// `url` would drop a `.` or `..` segment and turn `""` into a trailing slash,
    /// so `get_cluster("..")` would list clusters: those are refused. `url` encodes `/`
    /// and `%` itself, so `a/b` and `%2e%2e` stay one literal segment.
    fn url(&self, segments: &[&str]) -> std::result::Result<Url, ApiError> {
        if let Some(bad) = segments.iter().find(|s| matches!(**s, "" | "." | "..")) {
            return Err(ApiError::InvalidPath(format!("{bad:?} isn't a valid ID")));
        }
        let mut url = self.base.clone();
        url.path_segments_mut()
            .map_err(|()| ApiError::InvalidPath(self.base.to_string()))?
            .extend(segments);
        Ok(url)
    }

    async fn send<B: Serialize + ?Sized, T: DeserializeOwned>(
        &self,
        method: Method,
        url: Url,
        query: &[(&str, String)],
        body: Option<&B>,
        key: Option<&str>,
        extra: &HeaderMap,
    ) -> Result<T> {
        let build = || self.request(method.clone(), url.clone(), query, body, key, extra);
        let raw = send_with_retry(&self.retry, &method, key.is_some(), build).await?;
        decode(raw)
    }

    fn request<B: Serialize + ?Sized>(
        &self,
        method: Method,
        url: Url,
        query: &[(&str, String)],
        body: Option<&B>,
        key: Option<&str>,
        extra: &HeaderMap,
    ) -> RequestBuilder {
        let mut req = self
            .http
            .request(method, url)
            .header(USER_AGENT, self.user_agent.clone())
            .header(ACCEPT, "application/json, application/problem+json");
        if !query.is_empty() {
            req = req.query(query);
        }
        if let Some(auth) = &self.auth {
            req = req.header(AUTHORIZATION, auth.clone());
        }
        if let Some(key) = key {
            req = req.header("Idempotency-Key", key);
        }
        if let Some(body) = body {
            req = req.json(body);
        }
        if !extra.is_empty() {
            // Replaces a header of the same name set above (`Accept`, `User-Agent`).
            req = req.headers(extra.clone());
        }
        req
    }
}

/// A reqwest builder with rustls' ring provider installed. reqwest is built with
/// `rustls-no-provider` and panics on `build()` without one, so every client starts here.
pub fn http_client() -> reqwest::ClientBuilder {
    // Fails only when a provider is already installed, which is the goal.
    let _ = rustls::crypto::ring::default_provider().install_default();
    reqwest::Client::builder()
}

/// True for `localhost`, `127.0.0.1` and `[::1]` (matched on the parsed host,
/// so `localhost.evil.com` and `127.0.0.2` are not loopback).
pub fn is_loopback_host(url: &Url) -> bool {
    matches!(
        url.host(),
        Some(url::Host::Domain("localhost"))
            | Some(url::Host::Ipv4(std::net::Ipv4Addr::LOCALHOST))
            | Some(url::Host::Ipv6(std::net::Ipv6Addr::LOCALHOST))
    )
}

fn check_url(url: &Url) -> std::result::Result<(), ApiError> {
    // reqwest would send `user:pass@` as a second, Basic, Authorization header.
    if !url.username().is_empty() || url.password().is_some() {
        return Err(ApiError::Config(
            "the API URL can't carry a username or password".into(),
        ));
    }
    match url.scheme() {
        "https" => Ok(()),
        "http" if is_loopback_host(url) => Ok(()),
        _ => Err(ApiError::InsecureUrl(url.to_string())),
    }
}

fn read_ca_bundle(
    path: &std::path::Path,
) -> std::result::Result<Vec<reqwest::Certificate>, ApiError> {
    let pem = std::fs::read(path)
        .map_err(|e| ApiError::Config(format!("can't read CA bundle {}: {e}", path.display())))?;
    let certs = reqwest::Certificate::from_pem_bundle(&pem)
        .map_err(|e| ApiError::Config(format!("invalid CA bundle {}: {e}", path.display())))?;
    if certs.is_empty() {
        return Err(ApiError::Config(format!(
            "CA bundle {} has no certificates",
            path.display()
        )));
    }
    Ok(certs)
}

fn decode<T: DeserializeOwned>(raw: RawResponse) -> Result<T> {
    let json: Value = if raw.body.iter().all(u8::is_ascii_whitespace) {
        Value::Null
    } else {
        serde_json::from_slice(&raw.body)
            .map_err(|e| ApiError::Decode(format!("HTTP {} body isn't JSON: {e}", raw.status)))?
    };
    let value = T::deserialize(&json)
        .map_err(|e| ApiError::Decode(format!("HTTP {} body: {e}", raw.status)))?;
    let header = |name: &str| raw.headers.get(name).and_then(|v| v.to_str().ok());
    Ok(Response {
        value,
        status: raw.status,
        retry_after: header_retry_after(&raw.headers),
        location: header("location").map(str::to_owned),
        idempotent_replayed: header("idempotent-replayed")
            .is_some_and(|v| v.eq_ignore_ascii_case("true")),
        date: header("date").and_then(|v| {
            jiff::fmt::rfc2822::DateTimeParser::new()
                .parse_timestamp(v)
                .ok()
        }),
        raw: json,
    })
}

fn none_on_404<T>(
    res: std::result::Result<T, ApiError>,
) -> std::result::Result<Option<T>, ApiError> {
    match res {
        Ok(v) => Ok(Some(v)),
        Err(e) if e.status() == Some(404) => Ok(None),
        Err(e) => Err(e),
    }
}

fn push(query: &mut Query, name: &'static str, value: Option<impl ToString>) {
    if let Some(v) = value {
        query.push((name, v.to_string()));
    }
}

fn cursor_query(cursor: Option<&str>) -> Query {
    let mut query = Query::new();
    push(&mut query, "cursor", cursor);
    query
}
