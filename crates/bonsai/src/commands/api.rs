//! `bonsai api`: an authenticated request to `/v1`, printing the JSON as it came.

use anyhow::{Context, Result};
use bonsai_api::{ApiError, Client, new_idempotency_key};
use bonsai_core::CoreError;
use reqwest::Method;
use reqwest::header::{HeaderMap, HeaderName, HeaderValue};
use serde_json::Value;

use super::{Ctx, read_stdin};
use crate::cli::ApiArgs;

pub async fn api(ctx: &Ctx, args: ApiArgs) -> Result<()> {
    let method = Method::from_bytes(args.method.to_ascii_uppercase().as_bytes())
        .map_err(|_| usage(format!("{:?} isn't an HTTP method", args.method)))?;
    let (headers, key) = parse_headers(&args.headers)?;
    let body = match &args.data {
        None => None,
        Some(data) => Some(parse_body(ctx, data).await?),
    };
    // One key per run, made outside the retry loop, so retries reuse it.
    let key = key
        .or_else(|| (method == Method::POST || method == Method::DELETE).then(new_idempotency_key));

    let settings = ctx.settings()?;
    // The root and the login endpoints take no token, so none is fine here.
    let token = ctx.find_token(&settings).await?;
    let client = ctx.client(&settings, token.map(|(t, _)| t))?;

    let out = if args.paginate {
        Value::Array(
            paginate(
                &client,
                method,
                &args.path,
                body.as_ref(),
                key.as_deref(),
                &headers,
            )
            .await?,
        )
    } else {
        client
            .send_raw_with_headers(method, &args.path, body.as_ref(), key.as_deref(), &headers)
            .await?
            .raw
    };
    // A 204 has no body: print nothing.
    if !out.is_null() {
        ctx.out.json(&out)?;
    }
    Ok(())
}

fn usage(msg: impl Into<String>) -> anyhow::Error {
    CoreError::Usage(msg.into()).into()
}

/// `-H 'Name: value'` into a header map, with `Idempotency-Key` taken out as the key.
fn parse_headers(raw: &[String]) -> Result<(HeaderMap, Option<String>)> {
    let mut headers = HeaderMap::new();
    let mut key = None;
    for h in raw {
        let (name, value) = h
            .split_once(':')
            .ok_or_else(|| usage(format!("-H {h:?} isn't 'Name: value'")))?;
        let name = HeaderName::from_bytes(name.trim().as_bytes())
            .map_err(|_| usage(format!("-H {h:?}: invalid header name")))?;
        let value = value.trim();
        if name == reqwest::header::AUTHORIZATION {
            return Err(usage(
                "the token can't be passed with -H; use `bonsai login` or BONSAI_TOKEN",
            ));
        }
        if name.as_str() == "idempotency-key" {
            key = Some(value.to_owned());
            continue;
        }
        let value = HeaderValue::from_str(value)
            .map_err(|_| usage(format!("-H {h:?}: invalid header value")))?;
        headers.insert(name, value);
    }
    Ok((headers, key))
}

/// `-d`: JSON, `@file` or `-` for stdin.
async fn parse_body(ctx: &Ctx, data: &str) -> Result<Value> {
    let (text, from) = if data == "-" {
        (read_stdin(ctx).await?, "stdin".to_owned())
    } else if let Some(path) = data.strip_prefix('@') {
        // A fifo or slow disk can block: off the runtime thread.
        let owned = path.to_owned();
        let text = ctx
            .blocking(move || {
                std::fs::read_to_string(&owned).with_context(|| format!("couldn't read {owned}"))
            })
            .await?;
        (text, path.to_owned())
    } else {
        (data.to_owned(), "-d".to_owned())
    };
    serde_json::from_str(&text).map_err(|e| usage(format!("{from} isn't JSON: {e}")))
}

/// Fetches every page and returns their `data` arrays as one.
async fn paginate(
    client: &Client,
    method: Method,
    path: &str,
    body: Option<&Value>,
    key: Option<&str>,
    headers: &HeaderMap,
) -> Result<Vec<Value>> {
    let mut all = Vec::new();
    let mut path = path.to_owned();
    // Every cursor sent so far: A -> B -> A is as endless as A -> A.
    let mut seen = std::collections::HashSet::new();
    loop {
        let page = client
            .send_raw_with_headers(method.clone(), &path, body, key, headers)
            .await?
            .raw;
        match page.get("data") {
            Some(Value::Array(data)) => all.extend(data.iter().cloned()),
            _ => {
                return Err(ApiError::Decode(format!(
                    "--paginate: {path} didn't answer with a page (no `data` array)"
                ))
                .into());
            }
        }
        match page.get("next_cursor").and_then(Value::as_str) {
            None => return Ok(all),
            Some(next) if !seen.insert(next.to_owned()) => {
                return Err(ApiError::Decode(format!(
                    "--paginate: the server returned cursor {next:?} twice"
                ))
                .into());
            }
            Some(next) => path = with_cursor(&path, next),
        }
    }
}

/// `path` with its `cursor` query parameter set to `cursor`.
fn with_cursor(path: &str, cursor: &str) -> String {
    let (base, query) = path.split_once('?').unwrap_or((path, ""));
    let mut out = url::form_urlencoded::Serializer::new(String::new());
    for (k, v) in url::form_urlencoded::parse(query.as_bytes()) {
        if k != "cursor" {
            out.append_pair(&k, &v);
        }
    }
    out.append_pair("cursor", cursor);
    format!("{base}?{}", out.finish())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cursor_replaces_the_old_one() {
        assert_eq!(with_cursor("/clusters", "a b"), "/clusters?cursor=a+b");
        assert_eq!(
            with_cursor("/clusters?limit=1&cursor=x", "y"),
            "/clusters?limit=1&cursor=y"
        );
    }

    #[test]
    fn headers_take_out_the_key_and_refuse_the_token() {
        let (h, key) = parse_headers(&["X-A: 1".into(), "Idempotency-Key: k".into()]).unwrap();
        assert_eq!(h.get("x-a").unwrap(), "1");
        assert_eq!(h.len(), 1);
        assert_eq!(key.as_deref(), Some("k"));
        assert!(parse_headers(&["authorization: Bearer x".into()]).is_err());
        assert!(parse_headers(&["no colon".into()]).is_err());
    }
}
