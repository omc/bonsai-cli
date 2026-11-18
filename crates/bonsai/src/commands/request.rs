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
    EngineClient, EngineConfig, EngineError, EnginePath, EngineResponse, RetryPolicy,
};
use bonsai_core::{CoreError, human_duration, shell_quote};
use reqwest::Method;
use reqwest::header::{AUTHORIZATION, CONTENT_TYPE, HeaderMap, HeaderName, HeaderValue};
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

/// Whether the path's endpoint takes NDJSON: `_bulk`, `_msearch`, `_msearch/template`.
fn takes_ndjson(path: &EnginePath) -> bool {
    let segments = path.segments();
    match segments.as_slice() {
        [.., last] if last == "_bulk" || last == "_msearch" => true,
        [.., before, last] => before == "_msearch" && last == "template",
        _ => false,
    }
}

/// The body as it's sent, and its `Content-Type` unless `-H` set one. An NDJSON
/// endpoint refuses a body that doesn't end in a newline, so one is added.
fn prepare_body(mut body: Vec<u8>, path: &EnginePath, headers: &mut HeaderMap) -> Vec<u8> {
    let ndjson = takes_ndjson(path);
    if ndjson && !body.ends_with(b"\n") {
        body.push(b'\n');
    }
    if !headers.contains_key(CONTENT_TYPE) {
        let kind = if ndjson {
            "application/x-ndjson"
        } else {
            "application/json"
        };
        headers.insert(CONTENT_TYPE, HeaderValue::from_static(kind));
    }
    body
}

/// What a request removes wholesale, if it does: the target of `DELETE /{target}`
/// (an index, several, or `_all`), or of `/{target}/…/_delete_by_query`.
///
/// The path is read twice: as the engine routes it, and as a hop that decodes it
/// first would (`/plants/%2F` is `/plants` to such a hop). Either reading is
/// enough to ask.
///
/// When the two name different targets and the second could be real indices
/// (`/plants%2F` is `plants/` as written and `plants` decoded first), the request
/// is refused: whichever name was typed back, it might not be the index that goes.
/// A date-math name, whose slash has to be encoded (`<logs-{now/d}>`), isn't such
/// a case: decoded first it splits into pieces that name nothing, so the name as
/// written is the one asked for.
///
/// Nothing else is classified. Sorting the rest of two engines' APIs into safe
/// and unsafe would be a promise this CLI can't keep.
fn destroys(method: &Method, path: &EnginePath) -> Result<Option<String>> {
    let as_routed = target(method, &path.segments());
    let decoded_first = target(method, &path.segments_if_decoded_first());
    match (as_routed, decoded_first) {
        (Some(a), Some(b)) if a != b && could_be_indices(&b) => Err(usage(format!(
            "{method} {path} could remove {a} or {b}, depending on where the path is decoded; \
             write the path so that it names one of them plainly"
        ))),
        (Some(target), _) | (None, Some(target)) => Ok(Some(target)),
        (None, None) => Ok(None),
    }
}

/// Whether `target` could name real indices as a cluster would resolve it: every
/// name in the list is either a whole date-math expression (`<…>`), or has none
/// of these characters, which Elasticsearch and OpenSearch refuse in a name.
///
/// The pieces a date-math name splits into at its encoded slash (`<logs-{now` and
/// `d}>`) are neither, and a cluster refuses a list with such a piece in it whole.
/// The check errs towards "could": a name it wrongly takes for real only costs a
/// refusal.
fn could_be_indices(target: &str) -> bool {
    let mut names = target.split(',').filter(|name| !name.is_empty()).peekable();
    names.peek().is_some()
        && names.all(|name| {
            let date_math = name.len() > 2 && name.starts_with('<') && name.ends_with('>');
            date_math || !name.contains(['/', '\\', '"', '<', '>', '|', ' ', '#'])
        })
}

fn target(method: &Method, segments: &[String]) -> Option<String> {
    match segments {
        [target] if *method == Method::DELETE => Some(target.clone()),
        [target, .., last] if last == "_delete_by_query" => Some(target.clone()),
        _ => None,
    }
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
    let Some(target) = destroys(method, path)? else {
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
    Some(truncate(line, REASON_MAX).into_owned())
}

/// What to do about a status, from the status alone: the CLI keeps no list of the
/// endpoints Bonsai blocks.
fn hint(status: u16, retries: u32, midway: bool) -> Option<Vec<String>> {
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

    fn path(s: &str) -> EnginePath {
        EnginePath::parse(s).unwrap()
    }

    #[test]
    fn only_whole_index_deletes_and_delete_by_query_need_confirming() {
        let asks = |m: Method, p: &str| destroys(&m, &path(p)).unwrap();
        assert_eq!(asks(Method::DELETE, "/products"), Some("products".into()));
        assert_eq!(asks(Method::DELETE, "/products/"), Some("products".into()));
        assert_eq!(asks(Method::DELETE, "/a,b?timeout=1m"), Some("a,b".into()));
        assert_eq!(asks(Method::DELETE, "/_all"), Some("_all".into()));
        assert_eq!(asks(Method::DELETE, "/*"), Some("*".into()));
        // What's confirmed is what's sent: dots are resolved first.
        assert_eq!(
            asks(Method::DELETE, "/logs/../products"),
            Some("products".into())
        );
        assert_eq!(
            asks(Method::DELETE, "/x/%2e%2e/products"),
            Some("products".into())
        );
        assert_eq!(
            asks(Method::POST, "/products/_delete_by_query"),
            Some("products".into())
        );
        assert_eq!(
            asks(Method::GET, "/a,b/_doc/_delete_by_query?conflicts=proceed"),
            Some("a,b".into())
        );
        // A hop that decodes before it routes would read these as the same requests.
        assert_eq!(asks(Method::DELETE, "/plants/%2F"), Some("plants".into()));
        assert_eq!(
            asks(Method::DELETE, "/x/..%2Fplants"),
            Some("plants".into())
        );
        assert_eq!(
            asks(Method::POST, "/plants/_delete_by_query%2F"),
            Some("plants".into())
        );
        // Two readings, two targets: refused, whatever --confirm says.
        for ambiguous in [
            "/logs%2F..%2Fplants",
            "/plants%2F",
            "/x%2F..%2Fplants/_delete_by_query",
        ] {
            let method = if ambiguous.ends_with("_delete_by_query") {
                Method::POST
            } else {
                Method::DELETE
            };
            assert!(destroys(&method, &path(ambiguous)).is_err(), "{ambiguous}");
        }
        // An encoded character that isn't a slash or a dot reads the same both ways.
        assert_eq!(asks(Method::DELETE, "/my%2Dindex"), Some("my-index".into()));
        assert_eq!(asks(Method::DELETE, "/a%2Cb"), Some("a,b".into()));
        // A date-math name needs its slash encoded. Decoded first it splits into
        // pieces that could name no index, so the name as written is what's asked for.
        let date_math = "/%3Clogs-%7Bnow%2Fd%7D%3E";
        assert_eq!(
            asks(Method::DELETE, date_math),
            Some("<logs-{now/d}>".into())
        );
        assert_eq!(
            asks(Method::POST, &format!("{date_math}/_delete_by_query")),
            Some("<logs-{now/d}>".into())
        );
        assert_eq!(
            asks(
                Method::POST,
                &format!("{date_math},{}/_delete_by_query", &date_math[1..])
            ),
            Some("<logs-{now/d}>,<logs-{now/d}>".into())
        );
        // So does a name with a literal percent sign in it.
        assert_eq!(asks(Method::DELETE, "/a%252Fb"), Some("a%2Fb".into()));
        // A list of a plain name and a date-math one is asked for as written too.
        assert_eq!(
            asks(
                Method::POST,
                &format!("/plants,{}/_delete_by_query", &date_math[1..])
            ),
            Some("plants,<logs-{now/d}>".into())
        );
        // But a whole date-math name with no slash in it is a real index to a hop
        // that decodes first: with a second reading that differs, refused.
        for (method, ambiguous) in [
            (Method::DELETE, "/%3Clogs-%7Bnow%7D%3E%2F"),
            (Method::DELETE, "/%3Cplants%3E%2F"),
            (Method::DELETE, "/%2F%3Clogs-%7Bnow%7D%3E"),
            (Method::DELETE, "/plants,%2F"),
            (Method::POST, "/%3Clogs-%7Bnow%7D%3E%2F/_delete_by_query"),
        ] {
            assert!(destroys(&method, &path(ambiguous)).is_err(), "{ambiguous}");
        }
        // What makes a second reading worth refusing over: it could be real indices.
        for real in [
            "plants",
            "a,b",
            "pla*",
            "_all",
            "logs-2026.11.13",
            ".kibana",
            "<logs-{now}>",
            "<plants>",
            "plants,<logs-{now}>",
            "plants,",
        ] {
            assert!(could_be_indices(real), "{real}");
        }
        for not in [
            "",
            ",",
            "<logs-{now",
            "d}>",
            "plants,<logs-{now",
            "<>",
            "a/b",
            "a b",
            "a#b",
            "a\\b",
            "a|b",
            "a\"b",
        ] {
            assert!(!could_be_indices(not), "{not}");
        }
        for (m, p) in [
            (Method::DELETE, "/products/_doc/1"),
            (Method::DELETE, "/_search/scroll"),
            (Method::DELETE, "/"),
            (Method::GET, "/products"),
            (Method::HEAD, "/products"),
            (Method::PUT, "/products"),
            (Method::POST, "/products/_bulk"),
            (Method::POST, "/products/_close"),
            (Method::POST, "/_delete_by_query"),
        ] {
            assert_eq!(asks(m.clone(), p), None, "{m} {p}");
        }
    }

    #[test]
    fn ndjson_endpoints_get_their_content_type_and_final_newline() {
        let prepared = |p: &str, body: &str, given: Option<&'static str>| {
            let mut headers = HeaderMap::new();
            if let Some(kind) = given {
                headers.insert(CONTENT_TYPE, HeaderValue::from_static(kind));
            }
            let body = prepare_body(body.as_bytes().to_vec(), &path(p), &mut headers);
            (
                String::from_utf8(body).unwrap(),
                headers[CONTENT_TYPE].to_str().unwrap().to_owned(),
            )
        };
        let ndjson = "application/x-ndjson".to_owned();
        assert_eq!(
            prepared("/_bulk", "{}\n{}", None),
            ("{}\n{}\n".into(), ndjson.clone())
        );
        assert_eq!(
            prepared("/p/_bulk?refresh=true", "{}\n", None),
            ("{}\n".into(), ndjson.clone())
        );
        assert_eq!(prepared("/_msearch", "{}", None).1, ndjson);
        assert_eq!(prepared("/p/_msearch/template", "{}", None).1, ndjson);
        // Everything else is JSON and goes out untouched.
        assert_eq!(
            prepared("/p/_search", "{}", None),
            ("{}".into(), "application/json".into())
        );
        assert_eq!(prepared("/p/_doc/1", "", None).0, "");
        // -H wins, and the newline is still the endpoint's rule.
        assert_eq!(
            prepared("/_bulk", "{}", Some("text/plain")),
            ("{}\n".into(), "text/plain".into())
        );
    }

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
