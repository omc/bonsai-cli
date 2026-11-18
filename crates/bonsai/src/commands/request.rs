//! `bonsai cluster request`: one request to the cluster's own API (Elasticsearch's
//! or OpenSearch's), sent with the cluster's credential, and the cluster's answer
//! as it came.
//!
//! The credential's URL is fetched for the run and never printed, logged or
//! written: this is how a person or an agent creates indices, loads documents and
//! searches without handling the password.

use std::io::{IsTerminal, Read};

use anyhow::{Context, Result};
use bonsai_api::{
    EngineClient, EngineConfig, EngineError, EnginePath, EngineResponse, RetryPolicy, prepare_body,
};
use bonsai_core::{CoreError, human_duration, shell_quote};
use reqwest::Method;
use reqwest::header::{AUTHORIZATION, HeaderMap, HeaderName, HeaderValue};
use serde_json::Value;

use super::cluster::prompt_error;
use super::{Ctx, ca_bundle, user_agent};
use crate::cli::RequestArgs;
use crate::exit::{EngineStatus, Hint, TimedOut, status_text};
use crate::output::truncate;
use crate::sanitize::sanitize;

/// Bonsai's page on the endpoints its clusters don't offer.
const UNSUPPORTED: &str = "https://bonsai.io/docs/platform/unsupported-api-endpoints/";

/// The longest reason quoted from an error body. A stack trace would bury the line.
const REASON_MAX: usize = 300;

pub async fn request(ctx: &Ctx, args: RequestArgs) -> Result<()> {
    let method = Method::from_bytes(args.method.to_ascii_uppercase().as_bytes())
        .map_err(|_| usage(format!("{:?} isn't an HTTP method", args.method)))?;
    if method == Method::TRACE || method == Method::CONNECT {
        return Err(usage(format!("{method} requests aren't sent to a cluster")));
    }
    let path = EnginePath::parse(&args.path)?;
    let mut headers = parse_headers(&args.headers)?;
    // Before the body is read or anything is fetched: a refusal costs no request
    // and never holds the secret.
    confirm(ctx, &args.cluster, &method, &path, args.confirm.as_deref())?;
    let body = match &args.data {
        Some(data) => Some(prepare_body(
            read_body(ctx, data).await?,
            &path,
            &mut headers,
        )),
        None => None,
    };

    let a = ctx.authed().await?;
    let url = super::url::fetch(ctx, &a.client, &args.cluster, args.credential.as_deref()).await?;
    let client = EngineClient::new(EngineConfig {
        url,
        user_agent: user_agent(),
        retry: RetryPolicy {
            deadline: args.timeout,
            ..RetryPolicy::default()
        },
        ca_bundle: ca_bundle(),
    })?;
    let sent = ctx
        .abortable(async { Ok(client.send(&method, &path, &headers, body.as_deref()).await) })
        .await?;
    let resp = match sent {
        Ok(resp) => resp,
        Err(EngineError::Timeout) => {
            return Err(TimedOut(format!(
                "no answer from the cluster after {}; it may still apply the request",
                human_duration(args.timeout)
            ))
            .into());
        }
        Err(e) => return Err(e.into()),
    };
    print_body(ctx, &resp)?;
    if resp.status < 400 {
        return Ok(());
    }
    Err(failure(&resp, &path))
}

fn usage(msg: impl Into<String>) -> anyhow::Error {
    CoreError::Usage(msg.into()).into()
}

/// `-H 'Name: value'` into a header map. `Authorization` is the credential's.
fn parse_headers(raw: &[String]) -> Result<HeaderMap> {
    let mut headers = HeaderMap::new();
    for h in raw {
        let (name, value) = h
            .split_once(':')
            .ok_or_else(|| usage(format!("-H {h:?} isn't 'Name: value'")))?;
        let name = HeaderName::from_bytes(name.trim().as_bytes())
            .map_err(|_| usage(format!("-H {h:?}: invalid header name")))?;
        if name == AUTHORIZATION {
            return Err(usage(
                "the Authorization header is the cluster's credential; it can't be set with -H",
            ));
        }
        let value = HeaderValue::from_str(value.trim())
            .map_err(|_| usage(format!("-H {h:?}: invalid header value")))?;
        headers.insert(name, value);
    }
    Ok(headers)
}

/// `-d`: the text itself, `@FILE`, or `-` for stdin, byte for byte. Not parsed:
/// `_bulk` takes NDJSON, which isn't JSON.
async fn read_body(ctx: &Ctx, data: &str) -> Result<Vec<u8>> {
    if data == "-" {
        return ctx
            .blocking(|| {
                let mut body = Vec::new();
                std::io::stdin().lock().read_to_end(&mut body)?;
                Ok(body)
            })
            .await;
    }
    if let Some(path) = data.strip_prefix('@') {
        // A fifo or slow disk can block: off the runtime thread.
        let owned = path.to_owned();
        return ctx
            .blocking(move || {
                std::fs::read(&owned).with_context(|| format!("couldn't read {owned}"))
            })
            .await;
    }
    Ok(data.as_bytes().to_vec())
}

/// Has the target of a destructive request typed back: `--confirm`, or a prompt
/// on a terminal. As with `cluster destroy`, prompts off and no flag is a refusal.
fn confirm(
    ctx: &Ctx,
    cluster: &str,
    method: &Method,
    path: &EnginePath,
    given: Option<&str>,
) -> Result<()> {
    let Some(target) = path.removes(method).map_err(|e| usage(e.to_string()))? else {
        return Ok(());
    };
    match given {
        Some(given) if given == target => Ok(()),
        Some(_) => Err(usage(format!(
            "--confirm doesn't match {target}, the index in the path; nothing was sent"
        ))),
        None if !ctx.interactive => Err(usage(format!(
            "{method} {path} removes data from {target}; it needs --confirm {} when prompts are off",
            shell_quote(&target)
        ))),
        None => {
            let shown = sanitize(&target, false);
            let cluster = sanitize(cluster, false);
            let typed = inquire::Text::new(&format!(
                "Type {shown} to send {method} {path} to {cluster}:"
            ))
            .prompt()
            .map_err(prompt_error)?;
            if typed != target {
                return Err(usage(format!("that isn't {shown}; nothing was sent")));
            }
            Ok(())
        }
    }
}

/// What stdout gets for a body.
#[derive(Debug, PartialEq)]
enum Shown<'a> {
    Nothing,
    /// Stdout is a pipe or a file: the bytes as they came. Re-serializing would
    /// sort keys and round the numbers `serde_json` can't hold.
    Raw(&'a [u8]),
    /// On a terminal: JSON, re-indented, with unsafe characters escaped.
    Json(Value),
    /// On a terminal: anything else, as sanitized text.
    Text(String),
}

fn shown(body: &[u8], terminal: bool) -> Shown<'_> {
    if body.is_empty() {
        return Shown::Nothing;
    }
    if !terminal {
        return Shown::Raw(body);
    }
    match serde_json::from_slice(body) {
        Ok(v) => Shown::Json(v),
        Err(_) => {
            // CRLF line ends are line ends; a lone `\r` is still replaced.
            let text = String::from_utf8_lossy(body).replace("\r\n", "\n");
            Shown::Text(sanitize(text.trim_end_matches('\n'), true).into_owned())
        }
    }
}

/// The body to stdout, for every status: an error's body is the cluster's answer.
fn print_body(ctx: &Ctx, resp: &EngineResponse) -> Result<()> {
    match shown(&resp.body, std::io::stdout().is_terminal()) {
        Shown::Nothing => {
            // `HEAD /index` has nothing to print: say how it went.
            if resp.status < 400 {
                ctx.out.info(&status_text(resp.status));
            }
            Ok(())
        }
        Shown::Raw(bytes) => ctx.out.data_bytes(bytes),
        Shown::Json(v) => ctx.out.json(&v),
        Shown::Text(text) => ctx.out.data_line(&text),
    }
}

/// The error for a status of 400 or above: the status, what the body says, and
/// what to do about it.
fn failure(resp: &EngineResponse, path: &EnginePath) -> anyhow::Error {
    let e = anyhow::Error::new(EngineStatus {
        status: resp.status,
        reason: reason(&resp.body),
    });
    match hint(resp.status, resp.retries, path.can_answer_429_midway()) {
        Some(lines) => e.context(Hint(lines)),
        None => e,
    }
}

/// What an error body says went wrong, in one line: Elasticsearch's
/// `error.reason`, a string `error` or `message`, or the first line of a text body
/// (not of an HTML page).
pub(super) fn reason(body: &[u8]) -> Option<String> {
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
    Some(truncate(line, REASON_MAX).into_owned())
}

/// What to do about a status, from the status alone: the CLI keeps no list of the
/// endpoints Bonsai blocks.
pub(super) fn hint(status: u16, retries: u32, midway: bool) -> Option<Vec<String>> {
    let lines = match status {
        401 => vec![
            "The cluster refused the credential. It may have been revoked.".to_owned(),
            format!("Or this is an endpoint Bonsai doesn't offer: {UNSUPPORTED}"),
        ],
        413 => {
            vec!["The body is over the cluster's size limit. Send it in smaller parts.".to_owned()]
        }
        429 if midway => vec![
            "The cluster is at its plan's limit of concurrent requests.".to_owned(),
            "This endpoint can answer 429 after doing part of its work, so it wasn't sent again: check what changed before you send it again.".to_owned(),
        ],
        429 => vec![format!(
            "The cluster is at its plan's limit of concurrent requests; this request was sent again {}. \
             Send fewer at once, or try again shortly.",
            times(retries)
        )],
        501 => vec![format!("Bonsai doesn't offer this endpoint: {UNSUPPORTED}")],
        _ => return None,
    };
    Some(lines)
}

fn times(n: u32) -> String {
    match n {
        0 => "0 times".to_owned(),
        1 => "once".to_owned(),
        n => format!("{n} times"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn headers_are_parsed_and_authorization_refused() {
        let h = parse_headers(&["X-Opaque-Id: run-1".into(), "Accept:text/plain".into()]).unwrap();
        assert_eq!(h.get("x-opaque-id").unwrap(), "run-1");
        assert_eq!(h.get("accept").unwrap(), "text/plain");
        assert_eq!(h.len(), 2);
        for bad in [
            "authorization: Basic eDp5",
            "no colon",
            "bad name: x",
            "X: a\nb",
        ] {
            assert!(parse_headers(&[bad.into()]).is_err(), "{bad}");
        }
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
            r(r#"{"error":"no handler found for uri [/x] and method [GET]"}"#).as_deref(),
            Some("no handler found for uri [/x] and method [GET]")
        );
        assert_eq!(
            r(r#"{"message":"Unauthorized"}"#).as_deref(),
            Some("Unauthorized")
        );
        assert_eq!(
            r("\n  Too many requests \nmore\n").as_deref(),
            Some("Too many requests")
        );
        // An answer that isn't an error message says nothing.
        assert_eq!(r(r#"{"_index":"p","_id":"1","found":false}"#), None);
        assert_eq!(r(r#"{"error":{"type":"x"}}"#), None);
        assert_eq!(r("<html><body>502 Bad Gateway</body></html>"), None);
        assert_eq!(r(""), None);
        assert_eq!(reason(&[0xff, 0xfe]), None);
        let long = r(&format!(r#"{{"error":"{}"}}"#, "x".repeat(1000))).unwrap();
        assert_eq!(long.chars().count(), REASON_MAX);
        assert!(long.ends_with('…'), "{long}");
    }

    #[test]
    fn hints_follow_the_status_alone() {
        let unauthorized = hint(401, 0, false).unwrap();
        assert!(unauthorized[0].contains("revoked"));
        assert!(unauthorized[1].contains(UNSUPPORTED));
        assert!(hint(501, 0, false).unwrap()[0].contains(UNSUPPORTED));
        assert!(hint(413, 0, false).unwrap()[0].contains("smaller parts"));
        assert!(hint(429, 1, false).unwrap()[0].contains("sent again once"));
        assert!(hint(429, 4, false).unwrap()[0].contains("sent again 4 times"));
        // An endpoint that can answer 429 midway wasn't sent again.
        let midway = hint(429, 0, true).unwrap().join(" ");
        assert!(midway.contains("part of its work"), "{midway}");
        assert!(midway.contains("wasn't sent again"), "{midway}");
        for none in [400, 403, 404, 409, 500, 503] {
            assert!(hint(none, 0, false).is_none(), "{none}");
        }
    }

    #[test]
    fn a_pipe_gets_the_bytes_and_a_terminal_gets_them_safe() {
        // Key order and a number serde_json would round: a pipe keeps both.
        let body: &[u8] = br#"{"b":1,"a":12345678901234567890123,"c":"x\u001b[2J"}"#;
        assert_eq!(shown(body, false), Shown::Raw(body));
        assert!(matches!(shown(body, true), Shown::Json(_)));
        // Text on a terminal can't drive it.
        assert_eq!(
            shown(b"green open p\x1b[2J\tq\n", true),
            Shown::Text("green open p\u{fffd}[2J\tq".into())
        );
        assert_eq!(
            shown(b"green open p\x1b[2J\n", false),
            Shown::Raw(b"green open p\x1b[2J\n")
        );
        assert_eq!(shown(b"", true), Shown::Nothing);
        assert_eq!(shown(b"", false), Shown::Nothing);
        // CRLF line ends are line ends; a lone carriage return still can't move the cursor.
        assert_eq!(
            shown(b"one\r\ntwo\rEVIL\r\n", true),
            Shown::Text("one\ntwo\u{fffd}EVIL".into())
        );
        // Not JSON, not UTF-8: still printed, never a panic.
        assert!(matches!(shown(&[0xff, 0xfe], true), Shown::Text(_)));
    }
}
