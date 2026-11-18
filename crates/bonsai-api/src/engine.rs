//! The client for a cluster's own HTTP API: Elasticsearch's or OpenSearch's, at the
//! URL of one of the cluster's credentials. `bonsai cluster request` sends through it.
//!
//! It isn't the `/v1` client. There are no models and no Problems: a request goes
//! out as given, and the answer comes back as it is, whatever its status.
//!
//! The password never rides in a URL. [`EngineClient::new`] takes it out of the
//! credential's URL, and each request carries it as a sensitive `Authorization`
//! header, so it can't show up in an error's text or in a log line.

use std::fmt;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use percent_encoding::percent_decode_str;
use reqwest::header::{AUTHORIZATION, HeaderMap, HeaderValue, USER_AGENT};
use reqwest::{Method, RequestBuilder};
use secrecy::{ExposeSecret, SecretString};
use url::Url;

use crate::client::{CONNECT_TIMEOUT, http_client, is_loopback_host, read_ca_bundle};
use crate::retry::{MIN_ATTEMPT, RetryPolicy, backoff, header_retry_after, is_tls, read_up_to};

/// The largest answer read by default. Past it the rest isn't read and the request
/// fails with [`EngineError::TooLarge`].
pub const MAX_ENGINE_BODY: usize = 64 << 20;

/// Why a request to a cluster couldn't be sent or answered. A status of 400 or
/// above isn't one of these: it's an [`EngineResponse`].
#[derive(Debug, thiserror::Error)]
pub enum EngineError {
    /// The credential's URL can't address a cluster. The text never quotes the URL,
    /// which holds the password.
    #[error("the cluster's URL can't be used: {0}")]
    Url(String),
    #[error("the cluster's URL isn't https; its password won't be sent in clear text")]
    InsecureUrl,
    /// A path that doesn't start with `/`.
    #[error("{0}")]
    InvalidPath(String),
    /// A header the caller may not set, a CA bundle that can't be read, a method that
    /// isn't sent, or a request reqwest couldn't build.
    #[error("{0}")]
    Config(String),
    /// The deadline passed while the request was under way. The cluster may still
    /// apply it.
    #[error("the cluster didn't answer in time")]
    Timeout,
    #[error("the cluster's answer is larger than {}", size(.max))]
    TooLarge { max: usize },
    /// The cluster couldn't be reached, so nothing was sent: a name that doesn't
    /// resolve, a TLS failure, a connection refused, or one that timed out while
    /// connecting. The URL is stripped from the cause, so it can't be printed with it.
    #[error("couldn't reach the cluster")]
    Unreachable(#[source] reqwest::Error),
    /// The connection broke once the request was on its way, so the cluster may have
    /// applied it. Never retried. The URL is stripped from the cause.
    #[error(
        "the connection to the cluster broke after the request was sent; the cluster may have applied it"
    )]
    Interrupted(#[source] reqwest::Error),
}

/// A size as whole MiB from 1 MiB up, otherwise as bytes.
fn size(bytes: &usize) -> String {
    if *bytes >= 1 << 20 {
        format!("{} MiB", bytes >> 20)
    } else {
        format!("{bytes} bytes")
    }
}

/// A request's path and query, normalized as they'll be sent.
///
/// `url` resolves `.` and `..` segments (and their `%2e` spellings) here, once, so
/// what a caller inspects through [`EnginePath::segments`] is what the cluster
/// receives: `DELETE /logs/../products` is seen as `DELETE /products`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EnginePath {
    /// A placeholder origin with the path and query; only those two are read.
    url: Url,
}

impl EnginePath {
    /// `path_and_query` must start with `/`, e.g. `/_cat/indices?v`. It can't name a
    /// host: the path is set on a URL whose origin is already fixed.
    pub fn parse(path_and_query: &str) -> Result<Self, EngineError> {
        if !path_and_query.starts_with('/') {
            return Err(EngineError::InvalidPath(format!(
                "the path {path_and_query:?} must start with /"
            )));
        }
        let (path, query) = match path_and_query.split_once('?') {
            Some((path, query)) => (path, Some(query)),
            None => (path_and_query, None),
        };
        let mut url = Url::parse("http://cluster.invalid/").expect("a valid URL");
        url.set_path(path);
        url.set_query(query.filter(|q| !q.is_empty()));
        Ok(Self { url })
    }

    pub fn path(&self) -> &str {
        self.url.path()
    }

    pub fn query(&self) -> Option<&str> {
        self.url.query()
    }

    /// The path's segments, percent-decoded, without the empty ones a doubled or
    /// trailing slash leaves.
    pub fn segments(&self) -> Vec<String> {
        self.url
            .path_segments()
            .into_iter()
            .flatten()
            .filter(|s| !s.is_empty())
            .map(|s| percent_decode_str(s).decode_utf8_lossy().into_owned())
            .collect()
    }

    /// Whether this endpoint can answer 429 after doing part of its work.
    /// `_update_by_query`, `_delete_by_query` and `_reindex` answer with the
    /// highest status among their own bulk and search failures, so a 429 from them
    /// says nothing was refused up front: sending the request again would apply it
    /// twice to what it already reached.
    ///
    /// Either reading of the path counts, as the engine routes it or as
    /// [`EnginePath::segments_if_decoded_first`] has it: not sending again is the
    /// safe side. The price is a 429 left unretried for a path that merely ends
    /// like one of the three, such as a document whose ID is `_reindex`.
    pub fn can_answer_429_midway(&self) -> bool {
        let midway = |segments: Vec<String>| {
            matches!(
                segments.last().map(String::as_str),
                Some("_update_by_query" | "_delete_by_query" | "_reindex")
            )
        };
        midway(self.segments()) || midway(self.segments_if_decoded_first())
    }

    /// The segments as a hop that decodes the path before it routes would see them:
    /// the whole path percent-decoded, then split and normalized again, so
    /// `/plants/%2F` reads as `["plants"]` and `/x/..%2Fplants` as `["plants"]`.
    /// The engine itself splits first and decodes after, as [`EnginePath::segments`]
    /// does; a rule that must hold whichever way a request is routed checks both.
    pub fn segments_if_decoded_first(&self) -> Vec<String> {
        let decoded = percent_decode_str(self.url.path()).decode_utf8_lossy();
        match Self::parse(&decoded) {
            Ok(path) => path.segments(),
            Err(_) => self.segments(),
        }
    }

    /// This path and query on `origin`.
    fn on(&self, origin: &Url) -> Url {
        let mut url = origin.clone();
        url.set_path(self.url.path());
        url.set_query(self.url.query());
        url
    }
}

impl fmt::Display for EnginePath {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.url.path())?;
        if let Some(query) = self.url.query() {
            write!(f, "?{query}")?;
        }
        Ok(())
    }
}

#[derive(Debug, Clone)]
pub struct EngineConfig {
    /// A credential's `https://user:pass@host` URL.
    pub url: SecretString,
    pub user_agent: String,
    /// `deadline` bounds the whole exchange, retries and the body included.
    pub retry: RetryPolicy,
    /// A PEM file of extra root certificates (`BONSAI_CA_BUNDLE`).
    pub ca_bundle: Option<PathBuf>,
}

pub struct EngineClient {
    http: reqwest::Client,
    /// The credential's URL without its username and password.
    origin: Url,
    username: String,
    password: SecretString,
    user_agent: HeaderValue,
    retry: RetryPolicy,
    /// How long a connection may take: see [`connect_timeout_within`].
    connect_timeout: Duration,
    max_body: usize,
}

impl fmt::Debug for EngineClient {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("EngineClient")
            .field("origin", &self.origin.as_str())
            .field("retry", &self.retry)
            .finish_non_exhaustive()
    }
}

/// A cluster's answer, whatever its status.
#[derive(Debug)]
pub struct EngineResponse {
    pub status: u16,
    pub headers: HeaderMap,
    pub body: Vec<u8>,
    /// How many times the request was sent again, after a 429 or a connection
    /// that couldn't be made.
    pub retries: u32,
}

impl EngineClient {
    /// Refuses a URL that isn't `https`, unless its host is the loopback (the mock).
    pub fn new(cfg: EngineConfig) -> Result<Self, EngineError> {
        let (origin, username, password) = split_credentials(cfg.url.expose_secret())?;
        let user_agent = HeaderValue::from_str(&cfg.user_agent)
            .map_err(|_| EngineError::Config("the user agent isn't a valid header value".into()))?;
        // No redirects: the password would follow one to wherever it points.
        let connect_timeout = connect_timeout_within(cfg.retry.deadline);
        let mut builder = http_client()
            .connect_timeout(connect_timeout)
            .redirect(reqwest::redirect::Policy::none());
        if let Some(path) = &cfg.ca_bundle {
            let certs = read_ca_bundle(path).map_err(|e| EngineError::Config(e.to_string()))?;
            builder = builder.tls_certs_merge(certs);
        }
        // As for the API: plain http is loopback only, and a proxy would carry the
        // password off the machine in clear text.
        if is_loopback_host(&origin) {
            builder = builder.no_proxy();
        }
        let http = builder
            .build()
            .map_err(|e| EngineError::Config(e.to_string()))?;
        Ok(Self {
            http,
            origin,
            username,
            password,
            user_agent,
            retry: cfg.retry,
            connect_timeout,
            max_body: MAX_ENGINE_BODY,
        })
    }

    /// Reads at most `bytes` of an answer instead of [`MAX_ENGINE_BODY`].
    pub fn max_body(mut self, bytes: usize) -> Self {
        self.max_body = bytes;
        self
    }

    /// Sends one request and returns the answer, whatever its status.
    ///
    /// Retried, within the policy's deadline: a connection that was never made
    /// (refused, or timed out while connecting), and a 429, except from an endpoint
    /// that can answer 429 midway ([`EnginePath::can_answer_429_midway`]). Nothing
    /// else is retried, because the engine has no idempotency keys: a 5xx, a broken
    /// connection or a timeout after sending may follow a request that went
    /// through. A 429 that outlasts the retries comes back as the answer.
    ///
    /// Another attempt starts only while a whole connect timeout (and a moment for
    /// the answer) still fits before the deadline: under a short deadline a 429 is
    /// sent again fewer times, or not at all.
    ///
    /// `headers` replace the client's own `User-Agent` when they name it.
    /// `Authorization` is refused: it's the credential's.
    pub async fn send(
        &self,
        method: &Method,
        path: &EnginePath,
        headers: &HeaderMap,
        body: Option<&[u8]>,
    ) -> Result<EngineResponse, EngineError> {
        if headers.contains_key(AUTHORIZATION) {
            return Err(EngineError::Config(
                "the Authorization header is the cluster's credential; it can't be set by hand"
                    .into(),
            ));
        }
        if *method == Method::TRACE || *method == Method::CONNECT {
            return Err(EngineError::Config(format!(
                "{method} requests aren't sent to a cluster"
            )));
        }
        let url = path.on(&self.origin);
        let midway = path.can_answer_429_midway();
        let started = Instant::now();
        let mut attempt: u32 = 0;
        loop {
            let left = self.retry.deadline.saturating_sub(started.elapsed());
            let sent_at = Instant::now();
            let req = self.request(method, &url, headers, body).timeout(left);
            let (retry_after, last) = match exchange(req, self.max_body).await {
                Ok(answer) => {
                    // The path only: a query can carry a search term.
                    tracing::debug!(
                        %method,
                        path = path.path(),
                        status = answer.status,
                        duration_ms = sent_at.elapsed().as_millis() as u64,
                        attempt,
                        "cluster request"
                    );
                    if answer.status != 429 || midway {
                        return answer.finish(attempt, self.max_body);
                    }
                    (header_retry_after(&answer.headers), Last::Answer(answer))
                }
                Err(e) => {
                    tracing::debug!(
                        %method,
                        path = path.path(),
                        duration_ms = sent_at.elapsed().as_millis() as u64,
                        connect = e.is_connect(),
                        timeout = e.is_timeout(),
                        attempt,
                        "cluster request failed"
                    );
                    match failure(&e) {
                        Failure::Unsent { heals: true } => (None, Last::Unsent(e)),
                        Failure::Unsent { heals: false } => {
                            return Err(EngineError::Unreachable(e.without_url()));
                        }
                        Failure::TimedOut => return Err(EngineError::Timeout),
                        Failure::NotBuilt => {
                            return Err(EngineError::Config(e.without_url().to_string()));
                        }
                        Failure::Interrupted => {
                            return Err(EngineError::Interrupted(e.without_url()));
                        }
                    }
                }
            };
            let wait = match retry_after {
                Some(d) => d.min(self.retry.max_wait),
                None => backoff(&self.retry, attempt),
            };
            // Another attempt may have to connect again, even after an answer (the
            // server closed the connection, or it went stale). It starts only if a
            // whole connect timeout and a moment for the answer still fit: without
            // that room the deadline could pass mid-connect and read as a timeout
            // after sending.
            let room = self.connect_timeout + MIN_ATTEMPT;
            let out_of_time = started.elapsed() + wait + room > self.retry.deadline;
            if attempt >= self.retry.max_retries || out_of_time {
                return match last {
                    Last::Answer(answer) => answer.finish(attempt, self.max_body),
                    Last::Unsent(e) => Err(EngineError::Unreachable(e.without_url())),
                };
            }
            tokio::time::sleep(wait).await;
            attempt += 1;
        }
    }

    fn request(
        &self,
        method: &Method,
        url: &Url,
        headers: &HeaderMap,
        body: Option<&[u8]>,
    ) -> RequestBuilder {
        // `basic_auth` marks the header sensitive, so reqwest never prints it.
        let mut req = self
            .http
            .request(method.clone(), url.clone())
            .header(USER_AGENT, self.user_agent.clone())
            .basic_auth(&self.username, Some(self.password.expose_secret()))
            .headers(headers.clone());
        if let Some(body) = body {
            req = req.body(body.to_vec());
        }
        req
    }
}

/// One exchange's answer, before it's known to be the last. The body is `None`
/// when it was larger than the cap.
struct Answer {
    status: u16,
    headers: HeaderMap,
    body: Option<Vec<u8>>,
}

impl Answer {
    fn finish(self, retries: u32, max: usize) -> Result<EngineResponse, EngineError> {
        Ok(EngineResponse {
            status: self.status,
            headers: self.headers,
            body: self.body.ok_or(EngineError::TooLarge { max })?,
            retries,
        })
    }
}

/// What the latest attempt ended with, kept in case it's the last.
enum Last {
    Answer(Answer),
    Unsent(reqwest::Error),
}

/// What a failed exchange means for the request.
enum Failure {
    /// Nothing was sent. `heals` when trying again in a moment can help: a
    /// connection refused or timed out, but not a name that doesn't resolve or a
    /// TLS failure.
    Unsent { heals: bool },
    /// The deadline passed.
    TimedOut,
    /// reqwest couldn't build the request; nothing was sent.
    NotBuilt,
    /// Anything else happened after the request was on its way.
    Interrupted,
}

fn failure(e: &reqwest::Error) -> Failure {
    // A name is resolved, and TLS is negotiated, inside the connect: only there do
    // they mean nothing was sent. A TLS error on a connection already in use comes
    // after the request left.
    if e.is_connect() || e.is_dns() {
        Failure::Unsent {
            heals: !(e.is_dns() || is_tls(e)),
        }
    } else if e.is_timeout() {
        Failure::TimedOut
    } else if e.is_builder() {
        Failure::NotBuilt
    } else {
        Failure::Interrupted
    }
}

/// How long a connection may take within `deadline`: the usual connect timeout, or
/// most of a deadline shorter than that. Always less than the deadline, so a host
/// that can't be reached fails as a connect timeout, with nothing sent, before the
/// deadline can pass mid-connect and be taken for a timeout after sending.
fn connect_timeout_within(deadline: Duration) -> Duration {
    CONNECT_TIMEOUT.min(deadline.saturating_sub(MIN_ATTEMPT).max(deadline / 2))
}

async fn exchange(req: RequestBuilder, max: usize) -> Result<Answer, reqwest::Error> {
    let mut resp = req.send().await?;
    let status = resp.status().as_u16();
    let headers = resp.headers().clone();
    let body = read_up_to(&mut resp, max).await?;
    Ok(Answer {
        status,
        headers,
        body,
    })
}

/// The origin, username and password of a credential's URL. No message quotes the
/// URL: it holds the password.
fn split_credentials(raw: &str) -> Result<(Url, String, SecretString), EngineError> {
    let mut url = Url::parse(raw).map_err(|_| EngineError::Url("it isn't a URL".into()))?;
    match url.scheme() {
        "https" => {}
        "http" if is_loopback_host(&url) => {}
        _ => return Err(EngineError::InsecureUrl),
    }
    if url.username().is_empty() {
        return Err(EngineError::Url("it has no username".into()));
    }
    if !matches!(url.path(), "" | "/") || url.query().is_some() || url.fragment().is_some() {
        return Err(EngineError::Url(
            "it has a path, a query or a fragment, which this CLI can't address".into(),
        ));
    }
    let decode = |s: &str| {
        percent_decode_str(s)
            .decode_utf8()
            .map(|s| s.into_owned())
            .map_err(|_| EngineError::Url("its username or password isn't UTF-8".into()))
    };
    let username = decode(url.username())?;
    let password = SecretString::from(decode(url.password().unwrap_or(""))?);
    // Neither fails for http and https, which always have a host.
    let _ = url.set_username("");
    let _ = url.set_password(None);
    Ok((url, username, password))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_path_keeps_its_query_and_gives_its_segments() {
        let p = EnginePath::parse("/products/_search?q=fern&size=1").unwrap();
        assert_eq!(p.path(), "/products/_search");
        assert_eq!(p.query(), Some("q=fern&size=1"));
        assert_eq!(p.segments(), ["products", "_search"]);
        assert_eq!(p.to_string(), "/products/_search?q=fern&size=1");

        let root = EnginePath::parse("/").unwrap();
        assert!(root.segments().is_empty());
        assert_eq!(root.query(), None);
        assert_eq!(EnginePath::parse("/a//b/?").unwrap().segments(), ["a", "b"]);
        assert_eq!(
            EnginePath::parse("/my%20index").unwrap().segments(),
            ["my index"]
        );
    }

    #[test]
    fn the_connect_timeout_always_fits_inside_the_deadline() {
        let within = |ms| connect_timeout_within(Duration::from_millis(ms));
        // The usual one, when the deadline has room for it.
        assert_eq!(within(300_000), CONNECT_TIMEOUT);
        assert_eq!(within(10_250), CONNECT_TIMEOUT);
        // Under a short deadline, most of it, and always less than it.
        assert_eq!(within(3_000), Duration::from_millis(2_750));
        assert_eq!(within(300), Duration::from_millis(150));
        for ms in [1, 50, 300, 499, 500, 3_000, 9_999, 10_249] {
            assert!(within(ms) < Duration::from_millis(ms), "{ms}");
        }
    }

    #[test]
    fn by_query_and_reindex_can_answer_429_midway() {
        let midway = |p: &str| EnginePath::parse(p).unwrap().can_answer_429_midway();
        for yes in [
            "/plants/_update_by_query",
            "/plants/_delete_by_query?conflicts=proceed",
            "/_reindex",
            "/a,b/_update_by_query/",
            // As a hop that decodes the path first would route it.
            "/plants/_update_by_query%2F",
            "/x/..%2F_reindex",
        ] {
            assert!(midway(yes), "{yes}");
        }
        for no in [
            "/_bulk",
            "/plants/_bulk",
            "/plants/_search",
            "/plants",
            "/",
            "/_reindex/x",
        ] {
            assert!(!midway(no), "{no}");
        }
    }

    #[test]
    fn a_path_decoded_first_reads_as_a_decoding_hop_would_route_it() {
        let both = |p: &str| {
            let path = EnginePath::parse(p).unwrap();
            (path.segments(), path.segments_if_decoded_first())
        };
        // As the engine routes it, and as a hop that decodes first would.
        assert_eq!(
            both("/plants/%2F"),
            (vec!["plants".into(), "/".into()], vec!["plants".to_owned()])
        );
        assert_eq!(both("/x/..%2Fplants").1, ["plants"]);
        assert_eq!(
            both("/plants/_delete_by_query%2F").1,
            ["plants", "_delete_by_query"]
        );
        // A path with nothing encoded reads the same both ways.
        let (raw, decoded) = both("/plants/_doc/1?refresh=true");
        assert_eq!(raw, decoded);
        // Decoded once, not twice: %252F is a literal %2F inside one segment.
        assert_eq!(both("/plants%252Fx").1, ["plants/x"]);
    }

    #[test]
    fn a_path_must_start_with_a_slash() {
        for bad in ["", "products", "http://evil.test/x", "?q=1"] {
            let err = EnginePath::parse(bad).unwrap_err();
            assert!(matches!(err, EngineError::InvalidPath(_)), "{bad}");
        }
    }

    #[test]
    fn dot_segments_are_resolved_before_anything_reads_the_path() {
        let p = EnginePath::parse("/logs/../products").unwrap();
        assert_eq!(p.path(), "/products");
        assert_eq!(p.segments(), ["products"]);
        assert_eq!(EnginePath::parse("/a/%2e%2e/b").unwrap().segments(), ["b"]);
        assert_eq!(EnginePath::parse("/a/./b").unwrap().segments(), ["a", "b"]);
        assert!(EnginePath::parse("/..").unwrap().segments().is_empty());
    }

    #[test]
    fn a_path_cant_change_the_host() {
        let origin = Url::parse("https://search.example.com/").unwrap();
        for path in [
            "//evil.test/x",
            "/\\evil.test",
            "/@evil.test",
            "/x@evil.test:80/",
        ] {
            let url = EnginePath::parse(path).unwrap().on(&origin);
            assert_eq!(url.host_str(), Some("search.example.com"), "{path}");
            assert_eq!(url.scheme(), "https", "{path}");
            assert!(url.username().is_empty(), "{path}");
        }
    }

    #[test]
    fn a_path_lands_on_the_origin_with_its_query() {
        let origin = Url::parse("http://127.0.0.1:9200/").unwrap();
        let url = EnginePath::parse("/_cat/indices?v&format=json")
            .unwrap()
            .on(&origin);
        assert_eq!(
            url.as_str(),
            "http://127.0.0.1:9200/_cat/indices?v&format=json"
        );
    }

    #[test]
    fn a_credential_url_gives_its_origin_and_decoded_userinfo() {
        let (origin, user, pass) =
            split_credentials("https://us%65r:p%40ss%3Aword@search.example.com").unwrap();
        assert_eq!(origin.as_str(), "https://search.example.com/");
        assert_eq!(user, "user");
        assert_eq!(pass.expose_secret(), "p@ss:word");
    }

    #[test]
    fn a_url_that_cant_address_a_cluster_is_refused_without_quoting_it() {
        for bad in [
            "not a url",
            "https://search.example.com",
            "https://u:p@search.example.com/prefix",
            "https://u:p@search.example.com/?x=1",
        ] {
            let err = split_credentials(bad).unwrap_err();
            assert!(matches!(err, EngineError::Url(_)), "{bad}: {err:?}");
            assert!(!err.to_string().contains("example.com"), "{err}");
        }
    }
}
