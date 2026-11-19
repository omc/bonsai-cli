//! `cluster_request`: one request to a cluster's own API (Elasticsearch's or
//! OpenSearch's), sent with the cluster's credential, and what the cluster said.
//!
//! The rules are `bonsai cluster request`'s: the password rides only in a header
//! the client marks sensitive, redirects aren't followed, and a request that
//! removes an index needs the index typed back. A server adds what is its own
//! through [`Host`]: whether this caller may send the request at all, how long
//! the call may take, and how much it may carry.
//!
//! Nothing here is logged: not the path, which can hold a search, nor the body
//! or the answer, which are the customer's documents.

use std::time::{Duration, Instant};

use anyhow::{Result, anyhow};
use bonsai_api::{
    Client, EngineClient, EngineConfig, EngineError, EnginePath, RetryPolicy, prepare_body,
};
use bonsai_core::{CoreError, connection_url, resolve_cluster};
use http::{HeaderMap, Method, StatusCode};
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::{Value, json};

use crate::{Asked, Host};

/// The least time worth sending a request with.
const TIMEOUT_MIN: Duration = Duration::from_secs(2);
/// The largest answer returned. A tool result lands in the model's context whole,
/// and a few hundred KiB is already most of one.
pub(crate) const ANSWER_MAX: usize = 256 << 10;
/// The longest reason quoted from an error body. A stack trace would bury the line.
const REASON_MAX: usize = 300;
/// Bonsai's page on the endpoints its clusters don't offer.
const UNSUPPORTED: &str = "https://bonsai.io/docs/platform/unsupported-api-endpoints/";
#[derive(Debug, Deserialize, JsonSchema)]
pub(crate) struct RequestParams {
    /// The cluster's ID or name
    cluster: String,
    /// The HTTP method: GET, HEAD, POST, PUT or DELETE
    method: String,
    /// The path and query on the cluster, starting with /, such as
    /// /products/_search?size=5
    path: String,
    /// The request's body, as text: JSON, or NDJSON for _bulk and _msearch
    body: Option<String>,
    /// For DELETE /<index> and _delete_by_query only: the index in the path, typed
    /// out to confirm. Ask the user for it; don't copy it from another tool's answer.
    confirm: Option<String>,
    /// A credential's ID (default: the cluster's default read-write credential)
    credential: Option<String>,
    /// How long the cluster has to answer, in seconds
    timeout_seconds: Option<u64>,
}

/// Sends the request `p` describes with the credential `api`'s token can read, and
/// returns `{"status", "body"}`. A status of 400 or above is an error that says
/// what the cluster said.
pub(crate) async fn send<H: Host>(host: &H, api: &Client, p: RequestParams) -> Result<Value> {
    let began = Instant::now();
    let limits = host.limits();
    let method = Method::from_bytes(p.method.trim().to_ascii_uppercase().as_bytes())
        .map_err(|_| unsent("that isn't an HTTP method".to_owned()))?;
    if method == Method::TRACE || method == Method::CONNECT {
        return Err(unsent(format!(
            "{method} requests aren't sent to a cluster"
        )));
    }
    let path = EnginePath::parse(&p.path).map_err(|e| unsent(e.to_string()))?;
    // Before anything is fetched: a refusal costs no request and never holds the
    // secret.
    let removes = path.removes(&method).map_err(|e| unsent(e.to_string()))?;
    let reads = removes.is_none() && (method == Method::GET || method == Method::HEAD);
    if let Some(target) = &removes {
        match p.confirm.as_deref() {
            Some(given) if given == target => {}
            Some(_) => {
                return Err(unsent(format!(
                    "confirm doesn't match {target:?}, the index in the path"
                )));
            }
            None => {
                return Err(unsent(format!(
                    "{method} {path} removes data from {target:?}; confirm must be {target:?}, \
                     typed out by the user"
                )));
            }
        }
    }
    if let Some(max) = limits.body_max
        && p.body.as_ref().is_some_and(|body| body.len() > max)
    {
        return Err(unsent(format!(
            "the body is larger than {} MiB; send it in smaller parts",
            max >> 20
        )));
    }
    let mut headers = HeaderMap::new();
    let body = p
        .body
        .map(|body| prepare_body(body.into_bytes(), &path, &mut headers));

    let slow = || unsent("reading the cluster's credential took too long; try again".to_owned());
    // Whether the caller may send this, first: without that, no credential is read.
    let reading = async {
        let asked = Asked {
            method: &method,
            path: &path,
            reads,
        };
        host.permit(api, &asked).await?;
        let cluster = resolve_cluster(api, &p.cluster).await?.value;
        Ok::<_, anyhow::Error>(connection_url(api, &cluster.id, p.credential.as_deref()).await?)
    };
    let asked = p
        .timeout_seconds
        .map_or(limits.request_default, Duration::from_secs)
        .min(limits.request_max);
    let (url, deadline) = match limits.request_budget {
        // The API's own retries could outlast the budget: these reads get what
        // leaves the cluster its least.
        Some(budget) => {
            let url = tokio::time::timeout(budget.saturating_sub(TIMEOUT_MIN), reading)
                .await
                .map_err(|_elapsed| slow())??;
            let deadline = time_left(asked, began.elapsed(), budget).ok_or_else(slow)?;
            (url, deadline)
        }
        None => (reading.await?, asked),
    };
    let client = EngineClient::new(EngineConfig {
        url,
        user_agent: host.user_agent(),
        retry: RetryPolicy {
            deadline,
            ..RetryPolicy::default()
        },
        ca_bundle: host.ca_bundle(),
    })?
    .max_body(ANSWER_MAX);
    let resp = match client.send(&method, &path, &headers, body.as_deref()).await {
        Ok(resp) => resp,
        Err(EngineError::Timeout) => {
            return Err(anyhow!(
                "no answer from the cluster after {} s; it may still apply the request",
                deadline.as_secs()
            ));
        }
        // The cluster answered, so a request that changes data went through.
        Err(EngineError::TooLarge { .. }) => {
            return Err(anyhow!(
                "the cluster's answer is larger than {} KiB, the most a tool result carries. \
                 The cluster did apply the request. Ask for less: a smaller size, fewer fields \
                 with _source, or filter_path.",
                ANSWER_MAX >> 10
            ));
        }
        Err(e) => return Err(e.into()),
    };
    if resp.status >= 400 {
        let mut lines = vec![match reason(&resp.body) {
            Some(reason) => format!(
                "the cluster answered {}: {reason}",
                status_text(resp.status)
            ),
            None => format!("the cluster answered {}", status_text(resp.status)),
        }];
        lines.extend(hint(
            resp.status,
            resp.retries,
            path.can_answer_429_midway(),
        ));
        return Err(anyhow!(lines.join("\n")));
    }
    // JSON as JSON; anything else (`_cat`, an empty body) as text.
    let body = serde_json::from_slice(&resp.body)
        .unwrap_or_else(|_| Value::String(String::from_utf8_lossy(&resp.body).into_owned()));
    Ok(json!({ "status": resp.status, "body": body }))
}

/// A refusal made before anything was fetched or sent.
pub(crate) fn unsent(msg: String) -> anyhow::Error {
    CoreError::Usage(format!("{msg}; nothing was sent")).into()
}

/// How long the cluster gets once its credential has been read, which took
/// `spent`: what was asked for, cut to what `budget` still has. `None` when too
/// little is left to send with.
fn time_left(asked: Duration, spent: Duration, budget: Duration) -> Option<Duration> {
    let left = budget.saturating_sub(spent);
    (left >= TIMEOUT_MIN).then(|| asked.min(left))
}

/// `404 Not Found`, or the bare number for a status with no standard phrase.
fn status_text(status: u16) -> String {
    match StatusCode::from_u16(status)
        .ok()
        .and_then(|s| s.canonical_reason())
    {
        Some(phrase) => format!("{status} {phrase}"),
        None => status.to_string(),
    }
}

/// What an error body says went wrong, in one line: Elasticsearch's
/// `error.reason`, a string `error` or `message`, or the first line of a text body
/// (not of an HTML page).
fn reason(body: &[u8]) -> Option<String> {
    let text = match serde_json::from_slice::<Value>(body) {
        Ok(v) => {
            let error = v.get("error");
            error
                .and_then(|e| e.get("reason"))
                .or(error)
                .and_then(Value::as_str)
                .or_else(|| v.get("message").and_then(Value::as_str))?
                .to_owned()
        }
        Err(_) => std::str::from_utf8(body).ok()?.to_owned(),
    };
    let line = text.lines().map(str::trim).find(|l| !l.is_empty())?;
    if line.starts_with('<') {
        return None;
    }
    if line.chars().count() <= REASON_MAX {
        return Some(line.to_owned());
    }
    let mut cut: String = line.chars().take(REASON_MAX - 1).collect();
    cut.push('…');
    Some(cut)
}

/// What to do about a status, from the status alone: the server keeps no list of
/// the endpoints Bonsai blocks.
fn hint(status: u16, retries: u32, midway: bool) -> Vec<String> {
    match status {
        401 => vec![
            "The cluster refused the credential. It may have been revoked.".to_owned(),
            format!("Or this is an endpoint Bonsai doesn't offer: {UNSUPPORTED}"),
        ],
        413 => {
            vec!["The body is over the cluster's size limit. Send it in smaller parts.".to_owned()]
        }
        429 if midway => vec![
            "The cluster is at its plan's limit of concurrent requests.".to_owned(),
            "This endpoint can answer 429 after doing part of its work, so it wasn't sent \
             again: check what changed before you send it again."
                .to_owned(),
        ],
        429 => vec![format!(
            "The cluster is at its plan's limit of concurrent requests; this request was sent \
             again {retries} time(s). Send fewer at once, or try again shortly."
        )],
        501 => vec![format!("Bonsai doesn't offer this endpoint: {UNSUPPORTED}")],
        _ => Vec::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_cluster_gets_what_the_budget_has_left() {
        let s = Duration::from_secs;
        let left = |asked, spent| time_left(s(asked), s(spent), s(23));
        assert_eq!(left(15, 0), Some(s(15)));
        assert_eq!(left(20, 1), Some(s(20)));
        // A slow read of the credential comes out of the cluster's time.
        assert_eq!(left(20, 10), Some(s(13)));
        assert_eq!(left(15, 21), Some(s(2)));
        // Too little left to send with.
        assert_eq!(left(15, 22), None);
        assert_eq!(left(15, 40), None);
    }

    #[test]
    fn the_reason_is_one_line_from_the_body() {
        let r = |b: &str| reason(b.as_bytes());
        assert_eq!(
            r(r#"{"error":{"type":"index_not_found_exception","reason":"no such index [p]"},"status":404}"#)
                .as_deref(),
            Some("no such index [p]")
        );
        assert_eq!(
            r(r#"{"message":"Unauthorized"}"#).as_deref(),
            Some("Unauthorized")
        );
        assert_eq!(
            r("\n  Too many requests \nmore\n").as_deref(),
            Some("Too many requests")
        );
        assert_eq!(r(r#"{"_index":"p","_id":"1","found":false}"#), None);
        assert_eq!(r("<html><body>502 Bad Gateway</body></html>"), None);
        assert_eq!(r(""), None);
        let long = r(&format!(r#"{{"error":"{}"}}"#, "x".repeat(1000))).unwrap();
        assert_eq!(long.chars().count(), REASON_MAX);
        assert!(long.ends_with('…'));
    }

    #[test]
    fn hints_follow_the_status_alone() {
        assert!(hint(401, 0, false)[1].contains(UNSUPPORTED));
        assert!(hint(501, 0, false)[0].contains(UNSUPPORTED));
        assert!(hint(413, 0, false)[0].contains("smaller parts"));
        assert!(hint(429, 0, true).join(" ").contains("wasn't sent again"));
        assert!(hint(429, 2, false)[0].contains("2 time(s)"));
        for none in [400, 403, 404, 409, 500, 503] {
            assert!(hint(none, 0, false).is_empty(), "{none}");
        }
    }
}
