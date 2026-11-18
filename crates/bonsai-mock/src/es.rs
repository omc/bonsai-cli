//! A small fake of a cluster's own HTTP API, for `local_clusters`: enough of
//! Elasticsearch's (OpenSearch answers the same) to create an index, load
//! documents, read them back, search, count and delete, so `bonsai cluster
//! request` can be tried and tested without a Bonsai cluster.
//!
//! A request reaches it by carrying `Authorization: Basic` and matching no other
//! route. The username names a credential, and so its cluster. Each cluster has
//! its own indices.
//!
//! It keeps what real clusters do that a client has to handle: a bulk body must
//! end in a newline and is refused whole when malformed, `_all` and wildcard
//! deletes are refused (as with `action.destructive_requires_name`), an unknown
//! endpoint is `no handler found`, and a read-only credential can't write.
//!
//! It doesn't model plan limits, shards, mappings, analysis or relevance: `match`
//! finds the documents whose field holds any of the words, `term` an equal value,
//! `q=` the text anywhere in the document, and nothing is scored. It never
//! changes a cluster's stats.

use std::collections::HashMap;

use axum::extract::{Request, State};
use axum::http::{HeaderMap, HeaderValue, Method, StatusCode, header};
use axum::response::{IntoResponse, Response};
use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use percent_encoding::percent_decode_str;
use rusqlite::{Connection, OptionalExtension, params};
use serde_json::{Map, Value, json};
use url::Url;

use crate::{AppState, db, ids, problem, scenario, seed};

/// The largest request body read: well over what Bonsai's own routing accepts.
const MAX_BODY: usize = 64 << 20;

/// `origin` with a credential's username and password: the URL a credential gets
/// when the mock serves its clusters.
pub fn credential_url(origin: &Url, username: &str, password: &str) -> String {
    let mut url = origin.clone();
    // Neither fails for http and https.
    let _ = url.set_username(username);
    let _ = url.set_password(Some(password));
    url.set_path("");
    url.as_str().trim_end_matches('/').to_owned()
}

/// The router's fallback. With `local_clusters`, a request with Basic credentials
/// goes to the fake; anything else is the API's 404, as before.
pub async fn fallback(State(state): State<AppState>, req: Request) -> Response {
    let credentials = basic(req.headers()).filter(|_| state.cluster_origin().is_some());
    let Some((user, pass)) = credentials else {
        return problem::not_found().into_response();
    };
    let (parts, body) = req.into_parts();
    let Ok(body) = axum::body::to_bytes(body, MAX_BODY).await else {
        return error(
            StatusCode::PAYLOAD_TOO_LARGE,
            "content_too_long_exception",
            "the request body is too large",
        );
    };
    let segments: Vec<String> = parts
        .uri
        .path()
        .split('/')
        .filter(|s| !s.is_empty())
        .map(|s| percent_decode_str(s).decode_utf8_lossy().into_owned())
        .collect();
    let query: HashMap<String, String> =
        url::form_urlencoded::parse(parts.uri.query().unwrap_or("").as_bytes())
            .into_owned()
            .collect();
    let req = Req {
        method: &parts.method,
        segments: segments.iter().map(String::as_str).collect(),
        query: &query,
        body: &body,
    };
    state
        .db(|c| answer(c, &user, &pass, &req))
        .unwrap_or_else(|e| {
            tracing::error!(error = %e, "es: database error");
            error(
                StatusCode::INTERNAL_SERVER_ERROR,
                "exception",
                "bonsai-mock: database error",
            )
        })
}

/// The username and password of `Authorization: Basic`, if that's the scheme.
fn basic(headers: &HeaderMap) -> Option<(String, String)> {
    let value = headers.get(header::AUTHORIZATION)?.to_str().ok()?;
    let (scheme, encoded) = value.split_once(' ')?;
    if !scheme.eq_ignore_ascii_case("basic") {
        return None;
    }
    let decoded = String::from_utf8(STANDARD.decode(encoded.trim()).ok()?).ok()?;
    let (user, pass) = decoded.split_once(':')?;
    Some((user.to_owned(), pass.to_owned()))
}

struct Req<'a> {
    method: &'a Method,
    /// The path's segments, percent-decoded.
    segments: Vec<&'a str>,
    query: &'a HashMap<String, String>,
    body: &'a [u8],
}

/// Who a request is from: its credential's cluster, and whether it may write.
struct Access {
    cluster_id: String,
    read_only: bool,
}

/// How a handler ends early: the database failed, or the request gets this answer
/// and whatever it wrote so far is rolled back.
enum Fail {
    Db(rusqlite::Error),
    // Boxed so `Result<_, Fail>` stays small.
    Reply(Box<Response>),
}

impl From<rusqlite::Error> for Fail {
    fn from(e: rusqlite::Error) -> Self {
        Fail::Db(e)
    }
}

type Out = Result<Response, Fail>;

fn reply<T>(resp: Response) -> Result<T, Fail> {
    Err(Fail::Reply(Box::new(resp)))
}

fn answer(conn: &Connection, user: &str, pass: &str, req: &Req) -> rusqlite::Result<Response> {
    let Some(access) = authenticate(conn, user, pass)? else {
        return Ok(unauthorized());
    };
    if let Some(status) = scenario::take_status(conn, "cluster_status_next")? {
        let status = StatusCode::from_u16(status).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
        return Ok(json_response(
            status,
            json!({"error": "bonsai-mock: cluster_status_next", "status": status.as_u16()}),
        ));
    }
    if access.read_only && !is_read(req) {
        return Ok(error(
            StatusCode::FORBIDDEN,
            "security_exception",
            "this credential is read-only",
        ));
    }
    match db::atomically(conn, |c| route(c, &access, req)) {
        Ok(resp) => Ok(resp),
        Err(Fail::Reply(resp)) => Ok(*resp),
        Err(Fail::Db(e)) => Err(e),
    }
}

/// The credential `user` names, when `pass` is its password, it isn't revoked and
/// its cluster isn't terminated.
fn authenticate(conn: &Connection, user: &str, pass: &str) -> rusqlite::Result<Option<Access>> {
    let row = conn
        .query_row(
            "SELECT c.cluster_id, c.access_type, c.password FROM credentials c
             JOIN clusters k ON k.id = c.cluster_id
             WHERE c.username = ?1 AND c.revoked_at IS NULL AND k.terminated_at IS NULL",
            [user],
            |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, String>(2)?,
                ))
            },
        )
        .optional()?;
    // Compared by hash, as the admin token is.
    Ok(row
        .filter(|(_, _, stored)| seed::sha256_hex(stored) == seed::sha256_hex(pass))
        .map(|(cluster_id, access_type, _)| Access {
            cluster_id,
            read_only: access_type == "ro",
        }))
}

/// What an `ro` credential may send: any `GET` or `HEAD`, and a `POST` to
/// `_search` or `_count`.
fn is_read(req: &Req) -> bool {
    match req.method.as_str() {
        "GET" | "HEAD" => true,
        "POST" => matches!(req.segments.last(), Some(&"_search" | &"_count")),
        _ => false,
    }
}

fn route(conn: &Connection, access: &Access, req: &Req) -> Out {
    let cluster = access.cluster_id.as_str();
    match (req.method.as_str(), req.segments.as_slice()) {
        ("GET" | "HEAD", []) => info(conn, cluster),
        ("GET", ["_cluster", "health"]) => Ok(health(cluster)),
        ("GET", ["_cat", "indices"]) => cat_indices(conn, cluster, req),
        ("POST" | "PUT", ["_bulk"]) => bulk(conn, cluster, None, req.body),
        ("GET" | "POST", ["_search"]) => search(conn, cluster, None, req),
        ("GET" | "POST", ["_count"]) => count(conn, cluster, None, req),
        ("DELETE", [target]) => delete_indices(conn, cluster, target),
        (_, [name]) if name.starts_with('_') => Ok(no_handler(req)),
        ("PUT", [index]) => create_index(conn, cluster, index, req.body),
        ("GET", [index]) => get_index(conn, cluster, index),
        ("HEAD", [index]) => Ok(empty(if index_exists(conn, cluster, index)? {
            StatusCode::OK
        } else {
            StatusCode::NOT_FOUND
        })),
        ("POST" | "PUT", [index, "_bulk"]) => bulk(conn, cluster, Some(index), req.body),
        ("GET" | "POST", [index, "_search"]) => search(conn, cluster, Some(index), req),
        ("GET" | "POST", [index, "_count"]) => count(conn, cluster, Some(index), req),
        ("POST", [index, "_delete_by_query"]) => delete_by_query(conn, cluster, index, req),
        ("GET" | "POST", [index, "_refresh"]) => refresh(conn, cluster, index),
        ("POST", [index, "_doc"]) => put_doc(conn, cluster, index, None, req.body),
        ("PUT" | "POST", [index, "_doc", id]) => put_doc(conn, cluster, index, Some(id), req.body),
        ("GET" | "HEAD", [index, "_doc", id]) => get_doc(conn, cluster, index, id),
        ("DELETE", [index, "_doc", id]) => delete_doc(conn, cluster, index, id),
        _ => Ok(no_handler(req)),
    }
}

// ---------------------------------------------------------------- answers

fn json_response(status: StatusCode, body: Value) -> Response {
    let kind = HeaderValue::from_static("application/json; charset=UTF-8");
    (status, [(header::CONTENT_TYPE, kind)], body.to_string()).into_response()
}

fn ok(body: Value) -> Response {
    json_response(StatusCode::OK, body)
}

fn empty(status: StatusCode) -> Response {
    status.into_response()
}

/// An error as Elasticsearch shapes it.
fn error(status: StatusCode, kind: &str, reason: impl Into<String>) -> Response {
    let reason = reason.into();
    json_response(
        status,
        json!({
            "error": {
                "root_cause": [{"type": kind, "reason": reason}],
                "type": kind,
                "reason": reason,
            },
            "status": status.as_u16(),
        }),
    )
}

fn unauthorized() -> Response {
    let mut resp = error(
        StatusCode::UNAUTHORIZED,
        "security_exception",
        "missing or wrong credentials for this cluster",
    );
    resp.headers_mut().insert(
        header::WWW_AUTHENTICATE,
        HeaderValue::from_static("Basic realm=\"bonsai-mock\""),
    );
    resp
}

fn no_such_index(index: &str) -> Response {
    error(
        StatusCode::NOT_FOUND,
        "index_not_found_exception",
        format!("no such index [{index}]"),
    )
}

fn no_handler(req: &Req) -> Response {
    let message = format!(
        "no handler found for uri [/{}] and method [{}]",
        req.segments.join("/"),
        req.method
    );
    json_response(StatusCode::BAD_REQUEST, json!({ "error": message }))
}

fn shards() -> Value {
    json!({"total": 1, "successful": 1, "skipped": 0, "failed": 0})
}

/// What a write to one document answers with.
fn written(index: &str, id: &str, result: &str) -> Value {
    json!({
        "_index": index, "_id": id, "_version": 1, "result": result,
        "_shards": {"total": 1, "successful": 1, "failed": 0},
    })
}

// ---------------------------------------------------------------- the cluster

fn info(conn: &Connection, cluster: &str) -> Out {
    let (engine, number): (String, String) = conn.query_row(
        "SELECT r.engine, r.version FROM clusters c JOIN releases r ON r.id = c.release
         WHERE c.id = ?1",
        [cluster],
        |r| Ok((r.get(0)?, r.get(1)?)),
    )?;
    let (version, tagline) = if engine == "opensearch" {
        (
            json!({"distribution": "opensearch", "number": number}),
            "The OpenSearch Project: https://opensearch.org/",
        )
    } else {
        (json!({ "number": number }), "You Know, for Search")
    };
    Ok(ok(json!({
        "name": "bonsai-mock", "cluster_name": cluster, "version": version, "tagline": tagline,
    })))
}

fn health(cluster: &str) -> Response {
    ok(json!({
        "cluster_name": cluster, "status": "green", "timed_out": false,
        "number_of_nodes": 1, "number_of_data_nodes": 1,
    }))
}

const CAT_COLUMNS: [&str; 10] = [
    "health",
    "status",
    "index",
    "uuid",
    "pri",
    "rep",
    "docs.count",
    "docs.deleted",
    "store.size",
    "pri.store.size",
];

/// `_cat/indices`: text as the engine prints it (a header with `?v`), or objects
/// of strings with `?format=json`.
fn cat_indices(conn: &Connection, cluster: &str, req: &Req) -> Out {
    let rows: Vec<(String, u64, u64)> = conn
        .prepare(
            "SELECT i.name, count(d.id), coalesce(sum(length(d.source)), 0) FROM es_indices i
             LEFT JOIN es_docs d ON d.cluster_id = i.cluster_id AND d.idx = i.name
             WHERE i.cluster_id = ?1 GROUP BY i.name ORDER BY i.name",
        )?
        .query_map([cluster], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?
        .collect::<rusqlite::Result<_>>()?;
    let table: Vec<[String; 10]> = rows
        .iter()
        .map(|(name, docs, bytes)| {
            let size = human_bytes(*bytes);
            let uuid = seed::sha256_hex(&format!("{cluster}/{name}"))[..22].to_owned();
            [
                "green".into(),
                "open".into(),
                name.clone(),
                uuid,
                "1".into(),
                "1".into(),
                docs.to_string(),
                "0".into(),
                size.clone(),
                size,
            ]
        })
        .collect();
    if req.query.get("format").map(String::as_str) == Some("json") {
        let items = table
            .iter()
            .map(|row| {
                let object: Map<String, Value> = CAT_COLUMNS
                    .iter()
                    .zip(row)
                    .map(|(k, v)| ((*k).to_owned(), Value::String(v.clone())))
                    .collect();
                Value::Object(object)
            })
            .collect();
        return Ok(ok(Value::Array(items)));
    }
    let mut lines = Vec::new();
    if req.query.contains_key("v") {
        lines.push(CAT_COLUMNS.map(str::to_owned));
    }
    lines.extend(table);
    let kind = HeaderValue::from_static("text/plain; charset=UTF-8");
    Ok((
        StatusCode::OK,
        [(header::CONTENT_TYPE, kind)],
        aligned(&lines),
    )
        .into_response())
}

fn human_bytes(n: u64) -> String {
    if n < 1024 {
        format!("{n}b")
    } else {
        format!("{:.1}kb", n as f64 / 1024.0)
    }
}

/// Rows as `_cat` prints them: each column padded to its widest cell, one space
/// apart, a newline after every row.
fn aligned(rows: &[[String; 10]]) -> String {
    let mut widths = [0usize; 10];
    for row in rows {
        for (width, cell) in widths.iter_mut().zip(row) {
            *width = (*width).max(cell.len());
        }
    }
    rows.iter()
        .map(|row| {
            let cells: Vec<String> = row
                .iter()
                .zip(widths)
                .map(|(cell, width)| format!("{cell:<width$}"))
                .collect();
            format!("{}\n", cells.join(" ").trim_end())
        })
        .collect()
}

// ---------------------------------------------------------------- indices

fn index_exists(conn: &Connection, cluster: &str, index: &str) -> rusqlite::Result<bool> {
    conn.query_row(
        "SELECT EXISTS (SELECT 1 FROM es_indices WHERE cluster_id = ?1 AND name = ?2)",
        [cluster, index],
        |r| r.get(0),
    )
}

fn require_index(conn: &Connection, cluster: &str, index: &str) -> Result<(), Fail> {
    if index_exists(conn, cluster, index)? {
        Ok(())
    } else {
        reply(no_such_index(index))
    }
}

/// Why `name` can't be an index's, by Elasticsearch's rules.
fn invalid_name(name: &str) -> Option<&'static str> {
    const FORBIDDEN: [char; 11] = ['\\', '/', '*', '?', '"', '<', '>', '|', ' ', ',', '#'];
    if name.is_empty() || name.len() > 255 {
        Some("must be 1 to 255 bytes long")
    } else if name.chars().any(|c| c.is_ascii_uppercase()) {
        Some("must be lowercase")
    } else if name.starts_with(['_', '-', '+']) {
        Some("must not start with '_', '-', or '+'")
    } else if name.contains(FORBIDDEN) {
        Some("must not contain spaces, commas or any of \\ / * ? \" < > | #")
    } else if matches!(name, "." | "..") {
        Some("must not be '.' or '..'")
    } else {
        None
    }
}

fn bad_name(name: &str, why: &str) -> Response {
    error(
        StatusCode::BAD_REQUEST,
        "invalid_index_name_exception",
        format!("Invalid index name [{name}], {why}"),
    )
}

fn insert_index(
    conn: &Connection,
    cluster: &str,
    index: &str,
    body: &Value,
) -> rusqlite::Result<()> {
    conn.execute(
        "INSERT INTO es_indices (cluster_id, name, body, created_at) VALUES (?1, ?2, ?3, ?4)",
        params![cluster, index, body.to_string(), db::now()],
    )?;
    Ok(())
}

/// The index a document goes to, created on first use as the engine does.
fn auto_create(conn: &Connection, cluster: &str, index: &str) -> Result<(), Fail> {
    if index_exists(conn, cluster, index)? {
        return Ok(());
    }
    if let Some(why) = invalid_name(index) {
        return reply(bad_name(index, why));
    }
    insert_index(conn, cluster, index, &json!({}))?;
    Ok(())
}

/// The body as a JSON object; an empty body is `{}`.
fn object(body: &[u8]) -> Result<Map<String, Value>, Fail> {
    if body.iter().all(u8::is_ascii_whitespace) {
        return Ok(Map::new());
    }
    match serde_json::from_slice(body) {
        Ok(Value::Object(map)) => Ok(map),
        _ => reply(error(
            StatusCode::BAD_REQUEST,
            "parse_exception",
            "the request body isn't a JSON object",
        )),
    }
}

fn create_index(conn: &Connection, cluster: &str, index: &str, body: &[u8]) -> Out {
    if let Some(why) = invalid_name(index) {
        return reply(bad_name(index, why));
    }
    let body = object(body)?;
    if index_exists(conn, cluster, index)? {
        return reply(error(
            StatusCode::BAD_REQUEST,
            "resource_already_exists_exception",
            format!("index [{index}] already exists"),
        ));
    }
    insert_index(conn, cluster, index, &Value::Object(body))?;
    Ok(ok(
        json!({"acknowledged": true, "shards_acknowledged": true, "index": index}),
    ))
}

fn get_index(conn: &Connection, cluster: &str, index: &str) -> Out {
    let body: Option<String> = conn
        .query_row(
            "SELECT body FROM es_indices WHERE cluster_id = ?1 AND name = ?2",
            [cluster, index],
            |r| r.get(0),
        )
        .optional()?;
    let Some(body) = body else {
        return reply(no_such_index(index));
    };
    let body: Value = serde_json::from_str(&body).unwrap_or_else(|_| json!({}));
    let mut out = Map::new();
    out.insert(
        index.to_owned(),
        json!({
            "aliases": {},
            "mappings": body.get("mappings").cloned().unwrap_or_else(|| json!({})),
            "settings": {"index": {
                "provided_name": index, "number_of_shards": "1", "number_of_replicas": "1",
            }},
        }),
    );
    Ok(ok(Value::Object(out)))
}

/// `DELETE /{target}`: one index, or several separated by commas, all or none.
/// `_all` and wildcards are refused, as a cluster with
/// `action.destructive_requires_name` refuses them.
fn delete_indices(conn: &Connection, cluster: &str, target: &str) -> Out {
    let names: Vec<&str> = target.split(',').collect();
    if names.iter().any(|n| *n == "_all" || n.contains('*')) {
        return reply(error(
            StatusCode::BAD_REQUEST,
            "illegal_argument_exception",
            "Wildcard expressions or all indices are not allowed",
        ));
    }
    for name in &names {
        require_index(conn, cluster, name)?;
    }
    for name in &names {
        // The documents go with it (`ON DELETE CASCADE`).
        conn.execute(
            "DELETE FROM es_indices WHERE cluster_id = ?1 AND name = ?2",
            params![cluster, name],
        )?;
    }
    Ok(ok(json!({ "acknowledged": true })))
}

fn refresh(conn: &Connection, cluster: &str, index: &str) -> Out {
    require_index(conn, cluster, index)?;
    Ok(ok(json!({ "_shards": shards() })))
}

// ---------------------------------------------------------------- documents

fn read_doc(
    conn: &Connection,
    cluster: &str,
    index: &str,
    id: &str,
) -> rusqlite::Result<Option<Value>> {
    conn.query_row(
        "SELECT source FROM es_docs WHERE cluster_id = ?1 AND idx = ?2 AND id = ?3",
        [cluster, index, id],
        |r| db::json(r, 0),
    )
    .optional()
}

/// Stores a document, replacing the one with its ID.
fn write_doc(
    conn: &Connection,
    cluster: &str,
    index: &str,
    id: &str,
    source: &Value,
) -> rusqlite::Result<()> {
    conn.execute(
        "INSERT INTO es_docs (cluster_id, idx, id, source) VALUES (?1, ?2, ?3, ?4)
         ON CONFLICT (cluster_id, idx, id) DO UPDATE SET source = excluded.source",
        params![cluster, index, id, source.to_string()],
    )?;
    Ok(())
}

fn remove_doc(conn: &Connection, cluster: &str, index: &str, id: &str) -> rusqlite::Result<bool> {
    let n = conn.execute(
        "DELETE FROM es_docs WHERE cluster_id = ?1 AND idx = ?2 AND id = ?3",
        [cluster, index, id],
    )?;
    Ok(n == 1)
}

/// Every document of `index`, or of every index, oldest first: `(index, id, source)`.
fn docs(
    conn: &Connection,
    cluster: &str,
    index: Option<&str>,
) -> rusqlite::Result<Vec<(String, String, Value)>> {
    conn.prepare(
        "SELECT idx, id, source FROM es_docs
         WHERE cluster_id = ?1 AND (?2 IS NULL OR idx = ?2) ORDER BY rowid",
    )?
    .query_map(params![cluster, index], |r| {
        Ok((r.get(0)?, r.get(1)?, db::json(r, 2)?))
    })?
    .collect()
}

fn put_doc(conn: &Connection, cluster: &str, index: &str, id: Option<&str>, body: &[u8]) -> Out {
    let Ok(source @ Value::Object(_)) = serde_json::from_slice::<Value>(body) else {
        return reply(error(
            StatusCode::BAD_REQUEST,
            "mapper_parsing_exception",
            "failed to parse: a document must be a JSON object",
        ));
    };
    auto_create(conn, cluster, index)?;
    let id = id.map_or_else(|| ids::hex(20), str::to_owned);
    let existed = read_doc(conn, cluster, index, &id)?.is_some();
    write_doc(conn, cluster, index, &id, &source)?;
    Ok(if existed {
        ok(written(index, &id, "updated"))
    } else {
        json_response(StatusCode::CREATED, written(index, &id, "created"))
    })
}

fn get_doc(conn: &Connection, cluster: &str, index: &str, id: &str) -> Out {
    require_index(conn, cluster, index)?;
    Ok(match read_doc(conn, cluster, index, id)? {
        Some(source) => ok(json!({
            "_index": index, "_id": id, "_version": 1, "found": true, "_source": source,
        })),
        None => json_response(
            StatusCode::NOT_FOUND,
            json!({"_index": index, "_id": id, "found": false}),
        ),
    })
}

fn delete_doc(conn: &Connection, cluster: &str, index: &str, id: &str) -> Out {
    require_index(conn, cluster, index)?;
    Ok(if remove_doc(conn, cluster, index, id)? {
        ok(written(index, id, "deleted"))
    } else {
        json_response(StatusCode::NOT_FOUND, written(index, id, "not_found"))
    })
}

// ---------------------------------------------------------------- bulk

/// `_bulk`: NDJSON, an action line then (but for `delete`) a document line. A
/// document that fails is its item's error and the request still answers 200; a
/// body that can't be read is a 400 and applies nothing.
fn bulk(conn: &Connection, cluster: &str, default_index: Option<&str>, body: &[u8]) -> Out {
    let bad = |reason: String| -> Out {
        reply(error(
            StatusCode::BAD_REQUEST,
            "illegal_argument_exception",
            reason,
        ))
    };
    if !body.ends_with(b"\n") {
        return bad("The bulk request must be terminated by a newline [\\n]".into());
    }
    let Ok(text) = std::str::from_utf8(body) else {
        return bad("the bulk body isn't UTF-8".into());
    };
    let mut lines = text
        .lines()
        .enumerate()
        .filter(|(_, line)| !line.trim().is_empty());
    let mut items = Vec::new();
    let mut errors = false;
    while let Some((n, line)) = lines.next() {
        let malformed = || {
            format!(
                "Malformed action/metadata line [{}], expected one of index, create, delete or update",
                n + 1
            )
        };
        let Ok(Value::Object(action)) = serde_json::from_str::<Value>(line) else {
            return bad(malformed());
        };
        let Some((op, meta)) = action.iter().next().filter(|_| action.len() == 1) else {
            return bad(malformed());
        };
        if !matches!(op.as_str(), "index" | "create" | "delete" | "update") {
            return bad(malformed());
        }
        let Some(index) = meta.get("_index").and_then(Value::as_str).or(default_index) else {
            return reply(error(
                StatusCode::BAD_REQUEST,
                "action_request_validation_exception",
                "Validation Failed: 1: index is missing;",
            ));
        };
        let id = meta
            .get("_id")
            .and_then(Value::as_str)
            .map_or_else(|| ids::hex(20), str::to_owned);
        let doc = if op == "delete" {
            None
        } else {
            match lines.next().map(|(_, l)| serde_json::from_str::<Value>(l)) {
                Some(Ok(doc @ Value::Object(_))) => Some(doc),
                _ => return bad(format!("Malformed document after line [{}]", n + 1)),
            }
        };
        let (status, extra) = bulk_item(conn, cluster, op, index, &id, doc)?;
        errors |= status >= 400;
        let mut item = Map::new();
        item.insert("_index".into(), index.into());
        item.insert("_id".into(), id.into());
        item.insert("status".into(), status.into());
        item.extend(extra);
        let mut wrapper = Map::new();
        wrapper.insert(op.clone(), Value::Object(item));
        items.push(Value::Object(wrapper));
    }
    Ok(ok(json!({"took": 1, "errors": errors, "items": items})))
}

/// One bulk action: its status, and what its item adds (`result`, or `error`).
fn bulk_item(
    conn: &Connection,
    cluster: &str,
    op: &str,
    index: &str,
    id: &str,
    doc: Option<Value>,
) -> Result<(u16, Map<String, Value>), Fail> {
    let result = |r: &str| -> Map<String, Value> {
        let mut m = Map::new();
        m.insert("result".into(), r.into());
        m
    };
    let failure = |kind: &str, reason: String| -> Map<String, Value> {
        let mut m = Map::new();
        m.insert("error".into(), json!({"type": kind, "reason": reason}));
        m
    };
    let Some(doc) = doc else {
        // `delete`, the one action without a document line.
        if !index_exists(conn, cluster, index)? {
            let reason = format!("no such index [{index}]");
            return Ok((404, failure("index_not_found_exception", reason)));
        }
        return Ok(if remove_doc(conn, cluster, index, id)? {
            (200, result("deleted"))
        } else {
            (404, result("not_found"))
        });
    };
    if !index_exists(conn, cluster, index)? {
        if let Some(why) = invalid_name(index) {
            let reason = format!("Invalid index name [{index}], {why}");
            return Ok((400, failure("invalid_index_name_exception", reason)));
        }
        insert_index(conn, cluster, index, &json!({}))?;
    }
    match (op, read_doc(conn, cluster, index, id)?) {
        ("create", Some(_)) => Ok((
            409,
            failure(
                "version_conflict_engine_exception",
                format!("[{id}]: version conflict, document already exists"),
            ),
        )),
        ("update", None) => Ok((
            404,
            failure(
                "document_missing_exception",
                format!("[{id}]: document missing"),
            ),
        )),
        ("update", Some(mut current)) => {
            let (Some(patch), Some(fields)) = (
                doc.get("doc").and_then(Value::as_object),
                current.as_object_mut(),
            ) else {
                let reason = "Validation Failed: 1: script or doc is missing;".to_owned();
                return Ok((400, failure("action_request_validation_exception", reason)));
            };
            fields.extend(patch.clone());
            write_doc(conn, cluster, index, id, &current)?;
            Ok((200, result("updated")))
        }
        (_, existing) => {
            write_doc(conn, cluster, index, id, &doc)?;
            Ok(if existing.is_some() {
                (200, result("updated"))
            } else {
                (201, result("created"))
            })
        }
    }
}

// ---------------------------------------------------------------- search

enum Query {
    All,
    /// The field holds any of these lowercase words.
    Match {
        field: String,
        words: Vec<String>,
    },
    /// The field equals this value.
    Term {
        field: String,
        value: Value,
    },
    /// `q=`: this lowercase text is somewhere in the document.
    Text(String),
}

struct Search {
    query: Query,
    from: usize,
    size: usize,
    /// Whether the request named a query at all.
    explicit: bool,
}

/// `from`, `size` and the query, from the query string (`q`) or the body.
fn parse_search(req: &Req) -> Result<Search, Fail> {
    let body = object(req.body)?;
    let number = |name: &str, default: usize| -> usize {
        req.query
            .get(name)
            .and_then(|v| v.parse().ok())
            .or_else(|| {
                body.get(name)
                    .and_then(Value::as_u64)
                    .and_then(|n| usize::try_from(n).ok())
            })
            .unwrap_or(default)
    };
    let (query, explicit) = match (req.query.get("q"), body.get("query")) {
        (Some(q), _) => (Query::Text(q.to_lowercase()), true),
        (None, Some(q)) => (parse_query(q)?, true),
        (None, None) => (Query::All, false),
    };
    Ok(Search {
        query,
        from: number("from", 0),
        size: number("size", 10),
        explicit,
    })
}

fn parse_query(q: &Value) -> Result<Query, Fail> {
    let unsupported = || -> Result<Query, Fail> {
        reply(error(
            StatusCode::BAD_REQUEST,
            "parsing_exception",
            "bonsai-mock understands match_all, match and term queries only",
        ))
    };
    let Some((kind, arg)) = q
        .as_object()
        .filter(|o| o.len() == 1)
        .and_then(|o| o.iter().next())
    else {
        return unsupported();
    };
    // `{"field": value}` or `{"field": {inner: value}}`.
    let field_and = |inner: &str| -> Option<(String, Value)> {
        let (field, v) = arg.as_object().filter(|o| o.len() == 1)?.iter().next()?;
        let v = match v {
            Value::Object(o) => o.get(inner)?.clone(),
            v => v.clone(),
        };
        Some((field.clone(), v))
    };
    match kind.as_str() {
        "match_all" => Ok(Query::All),
        "match" => match field_and("query") {
            Some((field, v)) => Ok(Query::Match {
                field,
                words: words(&text_of(&v)),
            }),
            None => unsupported(),
        },
        "term" => match field_and("value") {
            Some((field, value)) => Ok(Query::Term { field, value }),
            None => unsupported(),
        },
        _ => unsupported(),
    }
}

fn text_of(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        other => other.to_string(),
    }
}

/// `s` as lowercase words, split at anything that isn't a letter or a digit.
fn words(s: &str) -> Vec<String> {
    s.to_lowercase()
        .split(|c: char| !c.is_alphanumeric())
        .filter(|w| !w.is_empty())
        .map(str::to_owned)
        .collect()
}

/// `source`'s value at `path`, which may be dotted (`a.b`).
fn field<'a>(source: &'a Value, path: &str) -> Option<&'a Value> {
    path.split('.').try_fold(source, |v, key| v.get(key))
}

fn matches(query: &Query, source: &Value) -> bool {
    match query {
        Query::All => true,
        Query::Match {
            field: f,
            words: wanted,
        } => field(source, f).is_some_and(|v| {
            let have = words(&text_of(v));
            wanted.iter().any(|w| have.contains(w))
        }),
        Query::Term { field: f, value } => field(source, f) == Some(value),
        Query::Text(q) => source.to_string().to_lowercase().contains(q.as_str()),
    }
}

/// The documents `req` selects in `index` (or every index): `(index, id, source)`.
fn selected(
    conn: &Connection,
    cluster: &str,
    index: Option<&str>,
    search: &Search,
) -> Result<Vec<(String, String, Value)>, Fail> {
    if let Some(index) = index {
        require_index(conn, cluster, index)?;
    }
    Ok(docs(conn, cluster, index)?
        .into_iter()
        .filter(|(_, _, source)| matches(&search.query, source))
        .collect())
}

fn search(conn: &Connection, cluster: &str, index: Option<&str>, req: &Req) -> Out {
    let s = parse_search(req)?;
    let found = selected(conn, cluster, index, &s)?;
    let total = found.len();
    let hits: Vec<Value> = found
        .into_iter()
        .skip(s.from)
        .take(s.size)
        .map(
            |(idx, id, source)| json!({"_index": idx, "_id": id, "_score": 1.0, "_source": source}),
        )
        .collect();
    let max_score = if total == 0 { Value::Null } else { json!(1.0) };
    Ok(ok(json!({
        "took": 1, "timed_out": false, "_shards": shards(),
        "hits": {
            "total": {"value": total, "relation": "eq"},
            "max_score": max_score,
            "hits": hits,
        },
    })))
}

fn count(conn: &Connection, cluster: &str, index: Option<&str>, req: &Req) -> Out {
    let s = parse_search(req)?;
    let n = selected(conn, cluster, index, &s)?.len();
    Ok(ok(json!({"count": n, "_shards": shards()})))
}

fn delete_by_query(conn: &Connection, cluster: &str, index: &str, req: &Req) -> Out {
    let s = parse_search(req)?;
    if !s.explicit {
        return reply(error(
            StatusCode::BAD_REQUEST,
            "action_request_validation_exception",
            "Validation Failed: 1: query is missing;",
        ));
    }
    let mut deleted = 0;
    for (_, id, _) in selected(conn, cluster, Some(index), &s)? {
        deleted += u64::from(remove_doc(conn, cluster, index, &id)?);
    }
    Ok(ok(json!({
        "took": 1, "timed_out": false, "total": deleted, "deleted": deleted, "batches": 1,
        "version_conflicts": 0, "noops": 0, "failures": [],
    })))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_credential_url_is_the_origin_with_the_userinfo() {
        let origin = Url::parse("http://127.0.0.1:4010/").unwrap();
        assert_eq!(
            credential_url(&origin, "user", "p@ss"),
            "http://user:p%40ss@127.0.0.1:4010"
        );
        let public = Url::parse("https://mock.example.test/").unwrap();
        assert_eq!(
            credential_url(&public, "u", "p"),
            "https://u:p@mock.example.test"
        );
    }

    #[test]
    fn basic_credentials_are_read_from_the_header() {
        let with = |value: &str| {
            let mut h = HeaderMap::new();
            h.insert(header::AUTHORIZATION, HeaderValue::from_str(value).unwrap());
            basic(&h)
        };
        let encoded = STANDARD.encode("user:p@ss:word");
        assert_eq!(
            with(&format!("Basic {encoded}")),
            Some(("user".into(), "p@ss:word".into()))
        );
        assert_eq!(with(&format!("basic {encoded}")).unwrap().0, "user");
        assert_eq!(with("Bearer bonsai_key_secret"), None);
        assert_eq!(with("Basic !!!"), None);
        assert_eq!(with(&format!("Basic {}", STANDARD.encode("nocolon"))), None);
        assert_eq!(basic(&HeaderMap::new()), None);
    }

    #[test]
    fn index_names_follow_the_engines_rules() {
        for ok in ["plants", "logs-2026.11", "a_b", "9lives"] {
            assert_eq!(invalid_name(ok), None, "{ok}");
        }
        for bad in [
            "", "Plants", "_all", "-x", "+x", "a b", "a,b", "a*", "a/b", ".", "..",
        ] {
            assert!(invalid_name(bad).is_some(), "{bad}");
        }
        assert!(invalid_name(&"x".repeat(256)).is_some());
    }

    #[test]
    fn queries_match_documents() {
        let doc = json!({"name": "Boston Fern", "price": 12, "care": {"light": "shade"}});
        let matching = |field: &str, text: &str| Query::Match {
            field: field.into(),
            words: words(text),
        };
        assert!(matches(&Query::All, &doc));
        assert!(matches(&matching("name", "fern"), &doc));
        assert!(matches(&matching("name", "jade FERN"), &doc), "any word");
        assert!(!matches(&matching("name", "fer"), &doc), "whole words");
        assert!(
            matches(&matching("care.light", "shade"), &doc),
            "dotted path"
        );
        assert!(!matches(&matching("missing", "fern"), &doc));
        let term = |field: &str, value: Value| Query::Term {
            field: field.into(),
            value,
        };
        assert!(matches(&term("price", json!(12)), &doc));
        assert!(!matches(&term("price", json!("12")), &doc));
        assert!(!matches(&term("name", json!("boston fern")), &doc), "exact");
        assert!(matches(&Query::Text("boston".into()), &doc));
        assert!(!matches(&Query::Text("jade".into()), &doc));
    }

    #[test]
    fn cat_rows_line_up() {
        let row = |index: &str, docs: &str| -> [String; 10] {
            ["green", "open", index, "u", "1", "1", docs, "0", "1b", "1b"].map(str::to_owned)
        };
        let text = aligned(&[row("a", "1"), row("plants", "120")]);
        assert_eq!(
            text,
            "green open a      u 1 1 1   0 1b 1b\ngreen open plants u 1 1 120 0 1b 1b\n"
        );
        assert_eq!(aligned(&[]), "");
        assert_eq!(human_bytes(1023), "1023b");
        assert_eq!(human_bytes(1536), "1.5kb");
    }
}
