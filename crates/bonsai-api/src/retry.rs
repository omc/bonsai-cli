//! The one retry loop every request goes through.

use crate::enums::ProblemCode;
use crate::problem::{ApiError, Problem};
use jiff::Timestamp;
use reqwest::header::{CONTENT_TYPE, HeaderMap, RETRY_AFTER};
use reqwest::{Method, RequestBuilder, StatusCode};
use serde::Deserialize;
use std::time::{Duration, Instant};

/// How hard to retry. The defaults: 4 retries, waits capped at 60 s,
/// and a 120 s overall deadline.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RetryPolicy {
    pub max_retries: u32,
    /// The longest single wait, `Retry-After` included.
    pub max_wait: Duration,
    /// No retry starts if it would end after this much time since the first attempt.
    pub deadline: Duration,
    /// The first backoff step, doubled on each attempt, and the jitter range.
    pub base: Duration,
}

impl Default for RetryPolicy {
    fn default() -> Self {
        Self {
            max_retries: 4,
            max_wait: Duration::from_secs(60),
            deadline: Duration::from_secs(120),
            base: Duration::from_millis(500),
        }
    }
}

/// A fresh `Idempotency-Key`: a random UUID. Make one per run, outside any retry.
pub fn new_idempotency_key() -> String {
    uuid::Uuid::new_v4().to_string()
}

/// Parses `Retry-After` as delay-seconds or an HTTP-date. A date in the past is zero.
pub fn parse_retry_after(v: &str, now: Timestamp) -> Option<Duration> {
    let v = v.trim();
    if v.bytes().all(|b| b.is_ascii_digit()) {
        return v.parse().ok().map(Duration::from_secs);
    }
    let at = jiff::fmt::rfc2822::DateTimeParser::new()
        .parse_timestamp(v)
        .ok()?;
    Some(at.duration_since(now).try_into().unwrap_or(Duration::ZERO))
}

/// A response with a success status, its body read.
pub(crate) struct RawResponse {
    pub status: u16,
    pub headers: HeaderMap,
    pub body: Vec<u8>,
}

/// The least time left before the deadline worth starting another attempt with.
pub(crate) const MIN_ATTEMPT: Duration = Duration::from_millis(250);

/// Sends the request `build` makes, retrying per `policy`.
///
/// `build` is called once per attempt and must attach the same `Idempotency-Key` each
/// time; the key is never made here. `keyed` says whether it carries one.
///
/// Retried: connection, timeout and send errors, `429` (but not
/// `429 too_many_failed_authentications`), `502`, `503` (but not
/// `503 metrics_unavailable`), and `409 idempotency_key_in_use`. A `PATCH`, or an
/// unsafe method without a key, is retried only on a refused connection or a
/// `429`, because the request may have been processed otherwise. DNS and TLS
/// failures aren't retried: they don't heal in seconds.
pub(crate) async fn send_with_retry(
    policy: &RetryPolicy,
    method: &Method,
    keyed: bool,
    build: impl Fn() -> RequestBuilder,
) -> Result<RawResponse, ApiError> {
    let replayable = *method != Method::PATCH && (keyed || method.is_safe());
    let started = Instant::now();
    let mut attempt: u32 = 0;
    let mut previous: Option<ApiError> = None;
    loop {
        let sent_at = Instant::now();
        let timeout = policy.deadline.saturating_sub(started.elapsed());
        let (err, retry_after, retryable) = match exchange(build().timeout(timeout)).await {
            Ok((url, status, headers, body)) => {
                let retry_after = header_retry_after(&headers);
                tracing::debug!(
                    %method,
                    path = url.path(),
                    status = status.as_u16(),
                    duration_ms = sent_at.elapsed().as_millis() as u64,
                    retry_after_s = retry_after.map(|d| d.as_secs()),
                    attempt,
                    "request"
                );
                if status.is_success() {
                    let Some(body) = body else {
                        return Err(ApiError::Decode(format!(
                            "the response is larger than {} MiB",
                            MAX_BODY >> 20
                        )));
                    };
                    return Ok(RawResponse {
                        status: status.as_u16(),
                        headers,
                        body,
                    });
                }
                let err = error_from(status, &headers, body.unwrap_or_default(), retry_after);
                let retryable = is_retryable_status(&err, replayable);
                (err, retry_after, retryable)
            }
            Err(e) => {
                tracing::debug!(
                    %method,
                    duration_ms = sent_at.elapsed().as_millis() as u64,
                    connect = e.is_connect(),
                    timeout = e.is_timeout(),
                    attempt,
                    "request failed"
                );
                // Cut short by the deadline. For a safe method the error before it says
                // more. Any other request may have been applied, so its outcome is
                // unknown: returning an earlier refusal would invite a rerun that
                // creates a second cluster. It gets the timeout.
                if e.is_timeout()
                    && started.elapsed() >= policy.deadline
                    && let Some(previous) = previous.take()
                {
                    if method.is_safe() {
                        return Err(previous);
                    }
                    tracing::debug!(%method, previous = %previous, "deadline cut a retry short");
                }
                let retryable = is_retryable_transport(&e, replayable);
                (ApiError::Transport(e), None, retryable)
            }
        };
        if !retryable || attempt >= policy.max_retries {
            return Err(err);
        }
        let wait = match retry_after {
            Some(d) => d.min(policy.max_wait),
            None => backoff(policy, attempt),
        };
        if started.elapsed() + wait + MIN_ATTEMPT > policy.deadline {
            return Err(err);
        }
        previous = Some(err);
        tokio::time::sleep(wait).await;
        attempt += 1;
    }
}

fn is_retryable_transport(e: &reqwest::Error, replayable: bool) -> bool {
    if e.is_dns() || is_tls(e) {
        return false;
    }
    e.is_connect() || (replayable && (e.is_timeout() || e.is_request() || e.is_body()))
}

/// Whether a TLS error (a certificate that doesn't verify, a failed handshake) is in
/// the chain. rustls errors arrive wrapped in `io::Error`s, sometimes nested, and an
/// `io::Error`'s `source()` skips the error it wraps, so those are unwrapped by hand.
pub(crate) fn is_tls(e: &reqwest::Error) -> bool {
    let mut next: Option<&(dyn std::error::Error + 'static)> = Some(e);
    while let Some(err) = next {
        if err.is::<rustls::Error>() {
            return true;
        }
        next = match err.downcast_ref::<std::io::Error>() {
            Some(io) => io
                .get_ref()
                .map(|inner| inner as &(dyn std::error::Error + 'static)),
            None => err.source(),
        };
    }
    false
}

/// The body is `None` when a success's was larger than [`MAX_BODY`].
type Exchange = (url::Url, StatusCode, HeaderMap, Option<Vec<u8>>);

/// The most of an error body kept; the rest isn't read.
const MAX_ERROR_BODY: usize = 1 << 20;

/// The largest success body read. The API's pages are a few hundred KiB at most;
/// past this the answer isn't the API's, and reading on would only fill memory.
const MAX_BODY: usize = 64 << 20;

/// Sends one request and reads the body: all of a success up to [`MAX_BODY`], at
/// most [`MAX_ERROR_BODY`] of an error.
async fn exchange(req: RequestBuilder) -> Result<Exchange, reqwest::Error> {
    let mut resp = req.send().await?;
    let url = resp.url().clone();
    let status = resp.status();
    let headers = resp.headers().clone();
    let body = if status.is_success() {
        read_up_to(&mut resp, MAX_BODY).await?
    } else {
        let mut body = Vec::new();
        while body.len() < MAX_ERROR_BODY
            && let Some(chunk) = resp.chunk().await?
        {
            body.extend_from_slice(&chunk);
        }
        body.truncate(MAX_ERROR_BODY);
        Some(body)
    };
    Ok((url, status, headers, body))
}

/// The whole body, or `None` once it's longer than `max`; the rest isn't read.
pub(crate) async fn read_up_to(
    resp: &mut reqwest::Response,
    max: usize,
) -> Result<Option<Vec<u8>>, reqwest::Error> {
    let mut body = Vec::new();
    while let Some(chunk) = resp.chunk().await? {
        if body.len() + chunk.len() > max {
            return Ok(None);
        }
        body.extend_from_slice(&chunk);
    }
    Ok(Some(body))
}

/// Decodes an error response: a problem body becomes `Problem`, anything else `Http`.
fn error_from(
    status: StatusCode,
    headers: &HeaderMap,
    body: Vec<u8>,
    retry_after: Option<Duration>,
) -> ApiError {
    let is_problem = headers
        .get(CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|ct| {
            ct.trim_start()
                .get(..24)
                .is_some_and(|t| t.eq_ignore_ascii_case("application/problem+json"))
        });
    if is_problem
        && let Ok(raw) = serde_json::from_slice::<serde_json::Value>(&body)
        && let Ok(problem) = Problem::deserialize(&raw)
    {
        return ApiError::Problem {
            status: status.as_u16(),
            problem: Box::new(problem),
            raw: Box::new(raw),
            retry_after,
        };
    }
    ApiError::Http {
        status: status.as_u16(),
        body: String::from_utf8_lossy(&body).into_owned(),
    }
}

pub(crate) fn header_retry_after(headers: &HeaderMap) -> Option<Duration> {
    let v = headers.get(RETRY_AFTER)?.to_str().ok()?;
    parse_retry_after(v, Timestamp::now())
}

/// `429` means the request wasn't processed, so it is retried for every method.
/// `503 metrics_unavailable` isn't retried: its `Retry-After` (30 s) is for the
/// user, who is told to try again then, not for a loop to sit through.
fn is_retryable_status(err: &ApiError, replayable: bool) -> bool {
    match err.status() {
        // An IP locked out for failed logins stays locked out for the whole window;
        // waiting it out in a retry would hang for minutes.
        Some(429) => err.code() != Some(&ProblemCode::TooManyFailedAuthentications),
        Some(503) if err.code() == Some(&ProblemCode::MetricsUnavailable) => false,
        Some(502 | 503) => replayable,
        Some(409) => replayable && err.code() == Some(&ProblemCode::IdempotencyKeyInUse),
        _ => false,
    }
}

/// `min(max_wait, base * 2^attempt)` plus up to `base` of jitter.
pub(crate) fn backoff(policy: &RetryPolicy, attempt: u32) -> Duration {
    let exp = policy
        .base
        .checked_mul(2u32.saturating_pow(attempt))
        .unwrap_or(policy.max_wait)
        .min(policy.max_wait);
    let jitter = policy.base.mul_f64(rand::random::<f64>());
    exp + jitter
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// Here rather than in `tests/client.rs` so the client can skip any proxy in the
    /// environment, which would otherwise do the DNS lookup itself.
    #[tokio::test]
    async fn does_not_retry_dns_failures() {
        let http = crate::http_client().no_proxy().build().unwrap();
        let attempts = AtomicUsize::new(0);
        let policy = RetryPolicy {
            base: Duration::from_millis(10),
            ..RetryPolicy::default()
        };
        let err = send_with_retry(&policy, &Method::GET, false, || {
            attempts.fetch_add(1, Ordering::SeqCst);
            http.get("https://bonsai-cli-test.invalid/v1/account")
        })
        .await
        .err()
        .unwrap();
        assert!(
            matches!(&err, ApiError::Transport(e) if e.is_dns()),
            "{err:?}"
        );
        assert_eq!(attempts.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn a_body_over_the_cap_is_not_kept() {
        use wiremock::{Mock, MockServer, ResponseTemplate, matchers::any};
        let server = MockServer::start().await;
        Mock::given(any())
            .respond_with(ResponseTemplate::new(200).set_body_bytes(vec![b'x'; 100]))
            .mount(&server)
            .await;
        let http = crate::http_client().no_proxy().build().unwrap();
        let get = || async { http.get(server.uri()).send().await.unwrap() };
        assert_eq!(
            read_up_to(&mut get().await, 100).await.unwrap(),
            Some(vec![b'x'; 100])
        );
        assert_eq!(read_up_to(&mut get().await, 99).await.unwrap(), None);
    }
}
