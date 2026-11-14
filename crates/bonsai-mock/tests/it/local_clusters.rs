//! `local_clusters`: the fake of a cluster's own API (`es`), reached with a
//! credential's username and password. None of it is in `v1.yml`, so these calls
//! skip the contract check.

use crate::common::{Mock, header};
use bonsai_mock::scenario;
use serde_json::{Value, json};

/// The URL of `cluster`'s default credential: where its own API answers.
async fn cluster_url(m: &Mock, cluster: &str) -> String {
    let (_, _, page) = m
        .call(
            "GET",
            &format!("/clusters/{cluster}/credentials"),
            Some(&m.acme),
            None,
            &[],
        )
        .await;
    let page = page.unwrap();
    let id = page["data"]
        .as_array()
        .unwrap()
        .iter()
        .find(|c| c["default"] == true)
        .unwrap()["id"]
        .as_str()
        .unwrap()
        .to_owned();
    let (status, _, cred) = m
        .call(
            "GET",
            &format!("/clusters/{cluster}/credentials/{id}"),
            Some(&m.acme),
            None,
            &[],
        )
        .await;
    assert_eq!(status, 200);
    cred.unwrap()["url"].as_str().unwrap().to_owned()
}

/// One request to a cluster's API, with the username and password of `base` (a
/// credential's URL). Returns the status, the headers and the body's text.
async fn es_full(
    m: &Mock,
    base: &str,
    method: &str,
    path: &str,
    body: Option<&str>,
) -> (u16, reqwest::header::HeaderMap, String) {
    let base = url::Url::parse(base).unwrap();
    let target = format!("{}{path}", m.h.origin.as_str().trim_end_matches('/'));
    let mut req = m
        .http
        .request(method.parse().unwrap(), target)
        .basic_auth(base.username(), base.password());
    if let Some(body) = body {
        req = req.body(body.to_owned());
    }
    let resp = req.send().await.unwrap();
    let (status, headers) = (resp.status().as_u16(), resp.headers().clone());
    (status, headers, resp.text().await.unwrap())
}

async fn es(m: &Mock, base: &str, method: &str, path: &str, body: Option<&str>) -> (u16, Value) {
    let (status, _, text) = es_full(m, base, method, path, body).await;
    let json = if text.is_empty() {
        Value::Null
    } else {
        serde_json::from_str(&text).unwrap_or_else(|e| panic!("not JSON ({e}): {text}"))
    };
    (status, json)
}

/// A ready cluster on a mock that serves it, and its URL.
async fn local(plan: &str) -> (Mock, String, String) {
    let m = Mock::start_local().await;
    let cluster = m.ready_cluster("Data", plan).await;
    let url = cluster_url(&m, &cluster).await;
    (m, cluster, url)
}

const THREE: &str = concat!(
    "{\"index\":{\"_id\":\"1\"}}\n{\"name\":\"Boston fern\",\"light\":\"shade\",\"price\":12}\n",
    "{\"index\":{\"_id\":\"2\"}}\n{\"name\":\"Jade plant\",\"light\":\"sun\",\"price\":9}\n",
    "{\"index\":{\"_id\":\"3\"}}\n{\"name\":\"Sword fern\",\"light\":\"shade\",\"price\":7}\n",
);

async fn three_plants(m: &Mock, url: &str) {
    let (status, body) = es(m, url, "POST", "/plants/_bulk", Some(THREE)).await;
    assert_eq!((status, &body["errors"]), (200, &json!(false)), "{body}");
}

#[tokio::test]
async fn without_local_clusters_nothing_changes() {
    let m = Mock::start().await;
    let cluster = m.ready_cluster("Plain", "sandbox").await;
    let url = cluster_url(&m, &cluster).await;
    assert!(url.starts_with("https://"), "{url}");
    assert!(url.ends_with(".bonsaisearch.net"), "{url}");
    // Basic credentials on a path that isn't the mock's: still the API's 404.
    let (status, body) = es(&m, &url, "GET", "/_cat/indices", None).await;
    assert_eq!(status, 404);
    assert_eq!(body["code"], "not_found");
}

#[tokio::test]
async fn a_credential_url_points_at_the_mock_which_answers_it() {
    let (m, cluster, url) = local("sandbox").await;
    let parsed = url::Url::parse(&url).unwrap();
    assert_eq!(parsed.scheme(), "http");
    assert_eq!(parsed.host_str(), Some("127.0.0.1"));
    assert_eq!(parsed.port(), Some(m.h.local_addr.port()));
    assert_eq!(parsed.path(), "/");
    assert!(!parsed.username().is_empty() && parsed.password().is_some());
    // The cluster's own host field doesn't change.
    let host = m.cluster(&cluster).await["host"]
        .as_str()
        .unwrap()
        .to_owned();
    assert!(host.ends_with(".bonsaisearch.net"), "{host}");

    let (status, info) = es(&m, &url, "GET", "/", None).await;
    assert_eq!(status, 200);
    assert_eq!(info["cluster_name"], cluster.as_str());
    assert!(info["version"]["number"].is_string(), "{info}");
    assert!(info["tagline"].is_string(), "{info}");

    let (status, health) = es(&m, &url, "GET", "/_cluster/health", None).await;
    assert_eq!((status, &health["status"]), (200, &json!("green")));
}

#[tokio::test]
async fn an_index_is_created_listed_and_deleted() {
    let (m, _, url) = local("sandbox").await;
    let mapping = r#"{"mappings": {"properties": {"name": {"type": "text"}}}}"#;
    let (status, made) = es(&m, &url, "PUT", "/plants", Some(mapping)).await;
    assert_eq!(status, 200, "{made}");
    assert_eq!(
        made,
        json!({"acknowledged": true, "shards_acknowledged": true, "index": "plants"})
    );

    let (status, again) = es(&m, &url, "PUT", "/plants", None).await;
    assert_eq!(status, 400);
    assert_eq!(again["error"]["type"], "resource_already_exists_exception");
    assert_eq!(again["status"], 400);

    let (status, _) = es(&m, &url, "HEAD", "/plants", None).await;
    assert_eq!(status, 200);
    let (status, shown) = es(&m, &url, "GET", "/plants", None).await;
    assert_eq!(status, 200);
    assert_eq!(
        shown["plants"]["mappings"]["properties"]["name"]["type"],
        "text"
    );

    let (_, rows) = es(&m, &url, "GET", "/_cat/indices?format=json", None).await;
    assert_eq!(rows.as_array().unwrap().len(), 1);
    assert_eq!(rows[0]["index"], "plants");
    assert_eq!(rows[0]["docs.count"], "0");
    assert_eq!(rows[0]["health"], "green");

    let (status, h, text) = es_full(&m, &url, "GET", "/_cat/indices?v", None).await;
    assert_eq!(status, 200);
    assert!(
        header(&h, "content-type")
            .unwrap()
            .starts_with("text/plain")
    );
    let lines: Vec<&str> = text.lines().collect();
    assert_eq!(lines.len(), 2, "{text}");
    assert!(lines[0].starts_with("health status index"), "{text}");
    assert!(lines[1].starts_with("green  open   plants"), "{text}");
    let (_, _, bare) = es_full(&m, &url, "GET", "/_cat/indices", None).await;
    assert_eq!(bare.lines().count(), 1, "no header without ?v: {bare}");

    let (status, gone) = es(&m, &url, "DELETE", "/plants", None).await;
    assert_eq!((status, gone), (200, json!({"acknowledged": true})));
    let (status, _) = es(&m, &url, "HEAD", "/plants", None).await;
    assert_eq!(status, 404);
    let (status, missing) = es(&m, &url, "DELETE", "/plants", None).await;
    assert_eq!(status, 404);
    assert_eq!(missing["error"]["type"], "index_not_found_exception");
    assert_eq!(missing["error"]["reason"], "no such index [plants]");
}

#[tokio::test]
async fn an_index_name_is_checked() {
    let (m, _, url) = local("sandbox").await;
    for bad in ["/Plants", "/-plants", "/pl%20ants"] {
        let (status, body) = es(&m, &url, "PUT", bad, None).await;
        assert_eq!(status, 400, "{bad}: {body}");
        assert_eq!(
            body["error"]["type"], "invalid_index_name_exception",
            "{bad}"
        );
    }
}

#[tokio::test]
async fn documents_round_trip() {
    let (m, _, url) = local("sandbox").await;
    let (status, made) = es(
        &m,
        &url,
        "PUT",
        "/plants/_doc/1",
        Some(r#"{"name":"Fern"}"#),
    )
    .await;
    assert_eq!(status, 201, "{made}");
    assert_eq!(
        (&made["result"], &made["_id"], &made["_index"]),
        (&json!("created"), &json!("1"), &json!("plants"))
    );
    let (status, again) = es(
        &m,
        &url,
        "PUT",
        "/plants/_doc/1",
        Some(r#"{"name":"Jade"}"#),
    )
    .await;
    assert_eq!((status, &again["result"]), (200, &json!("updated")));

    let (status, got) = es(&m, &url, "GET", "/plants/_doc/1", None).await;
    assert_eq!(status, 200);
    assert_eq!(got["found"], true);
    assert_eq!(got["_source"], json!({"name": "Jade"}));
    let (status, none) = es(&m, &url, "GET", "/plants/_doc/9", None).await;
    assert_eq!(status, 404);
    assert_eq!(
        none,
        json!({"_index": "plants", "_id": "9", "found": false})
    );

    let (status, auto) = es(&m, &url, "POST", "/plants/_doc", Some(r#"{"name":"Ivy"}"#)).await;
    assert_eq!(status, 201);
    assert!(auto["_id"].as_str().unwrap().len() >= 16, "{auto}");

    let (status, not_json) = es(&m, &url, "PUT", "/plants/_doc/2", Some("nope")).await;
    assert_eq!(status, 400, "{not_json}");

    let (status, gone) = es(&m, &url, "DELETE", "/plants/_doc/1", None).await;
    assert_eq!((status, &gone["result"]), (200, &json!("deleted")));
    let (status, gone) = es(&m, &url, "DELETE", "/plants/_doc/1", None).await;
    assert_eq!((status, &gone["result"]), (404, &json!("not_found")));
    let (status, _) = es(&m, &url, "GET", "/nowhere/_doc/1", None).await;
    assert_eq!(status, 404);
}

#[tokio::test]
async fn bulk_loads_and_reports_each_items_failure() {
    let (m, _, url) = local("sandbox").await;
    three_plants(&m, &url).await;
    let mixed = concat!(
        "{\"create\":{\"_index\":\"plants\",\"_id\":\"1\"}}\n{\"name\":\"Twice\"}\n",
        "{\"delete\":{\"_index\":\"plants\",\"_id\":\"99\"}}\n",
        "{\"update\":{\"_index\":\"plants\",\"_id\":\"2\"}}\n{\"doc\":{\"price\":10}}\n",
        "{\"index\":{\"_index\":\"herbs\"}}\n{\"name\":\"Basil\"}\n",
    );
    let (status, body) = es(&m, &url, "POST", "/_bulk", Some(mixed)).await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["errors"], true);
    let items = body["items"].as_array().unwrap();
    assert_eq!(items.len(), 4);
    assert_eq!(items[0]["create"]["status"], 409);
    assert_eq!(
        items[0]["create"]["error"]["type"],
        "version_conflict_engine_exception"
    );
    assert_eq!(items[1]["delete"]["status"], 404);
    assert_eq!(items[2]["update"]["status"], 200);
    assert_eq!(items[3]["index"]["status"], 201);
    assert_eq!(items[3]["index"]["_index"], "herbs");

    let (_, updated) = es(&m, &url, "GET", "/plants/_doc/2", None).await;
    assert_eq!(
        updated["_source"],
        json!({"name": "Jade plant", "light": "sun", "price": 10})
    );
    let (_, counted) = es(&m, &url, "GET", "/plants/_count", None).await;
    assert_eq!(counted["count"], 3);
}

/// As the engine does: the whole body is refused, and nothing in it is applied.
#[tokio::test]
async fn a_malformed_bulk_body_is_refused_whole() {
    let (m, _, url) = local("sandbox").await;
    let no_newline = "{\"index\":{\"_id\":\"1\"}}\n{\"name\":\"Fern\"}";
    let (status, body) = es(&m, &url, "POST", "/plants/_bulk", Some(no_newline)).await;
    assert_eq!(status, 400);
    assert_eq!(body["error"]["type"], "illegal_argument_exception");
    assert!(
        body["error"]["reason"]
            .as_str()
            .unwrap()
            .contains("terminated by a newline"),
        "{body}"
    );

    let bad_second = "{\"index\":{\"_id\":\"1\"}}\n{\"name\":\"Fern\"}\n{\"frobnicate\":{}}\n{}\n";
    let (status, body) = es(&m, &url, "POST", "/plants/_bulk", Some(bad_second)).await;
    assert_eq!(status, 400, "{body}");
    let (status, _) = es(&m, &url, "HEAD", "/plants", None).await;
    assert_eq!(status, 404, "the first action was rolled back");

    let no_index = "{\"index\":{}}\n{\"name\":\"Fern\"}\n";
    let (status, body) = es(&m, &url, "POST", "/_bulk", Some(no_index)).await;
    assert_eq!(status, 400);
    assert_eq!(body["error"]["type"], "action_request_validation_exception");
}

#[tokio::test]
async fn search_and_count() {
    let (m, _, url) = local("sandbox").await;
    three_plants(&m, &url).await;
    let total = |v: &Value| v["hits"]["total"]["value"].as_u64().unwrap();
    let ids = |v: &Value| -> Vec<String> {
        v["hits"]["hits"]
            .as_array()
            .unwrap()
            .iter()
            .map(|h| h["_id"].as_str().unwrap().to_owned())
            .collect()
    };

    let (status, all) = es(&m, &url, "GET", "/plants/_search", None).await;
    assert_eq!(status, 200);
    assert_eq!(
        (total(&all), ids(&all)),
        (3, vec!["1".into(), "2".into(), "3".into()])
    );
    assert_eq!(all["hits"]["hits"][0]["_source"]["name"], "Boston fern");

    let (_, paged) = es(&m, &url, "GET", "/plants/_search?size=1&from=1", None).await;
    assert_eq!((total(&paged), ids(&paged)), (3, vec!["2".into()]));
    let (_, sized) = es(&m, &url, "POST", "/plants/_search", Some(r#"{"size": 2}"#)).await;
    assert_eq!(ids(&sized).len(), 2);

    let matched = r#"{"query": {"match": {"name": "FERN"}}}"#;
    let (_, ferns) = es(&m, &url, "POST", "/plants/_search", Some(matched)).await;
    assert_eq!(ids(&ferns), ["1", "3"]);
    let long_form = r#"{"query": {"match": {"name": {"query": "jade"}}}}"#;
    let (_, jade) = es(&m, &url, "POST", "/plants/_search", Some(long_form)).await;
    assert_eq!(ids(&jade), ["2"]);

    let term = r#"{"query": {"term": {"price": 9}}}"#;
    let (_, nine) = es(&m, &url, "POST", "/plants/_search", Some(term)).await;
    assert_eq!(ids(&nine), ["2"]);
    let (_, by_q) = es(&m, &url, "GET", "/plants/_search?q=sword", None).await;
    assert_eq!(ids(&by_q), ["3"]);
    let (_, everywhere) = es(
        &m,
        &url,
        "POST",
        "/_search",
        Some(r#"{"query": {"match_all": {}}}"#),
    )
    .await;
    assert_eq!(total(&everywhere), 3);

    let shade = r#"{"query": {"term": {"light": "shade"}}}"#;
    let (_, counted) = es(&m, &url, "POST", "/plants/_count", Some(shade)).await;
    assert_eq!(counted["count"], 2);

    let (status, unknown) = es(
        &m,
        &url,
        "POST",
        "/plants/_search",
        Some(r#"{"query": {"range": {"price": {"gt": 1}}}}"#),
    )
    .await;
    assert_eq!(status, 400);
    assert_eq!(unknown["error"]["type"], "parsing_exception");
    let (status, missing) = es(&m, &url, "GET", "/nowhere/_search", None).await;
    assert_eq!(status, 404);
    assert_eq!(missing["error"]["type"], "index_not_found_exception");
}

#[tokio::test]
async fn delete_by_query_needs_a_query_and_deletes_what_matches() {
    let (m, _, url) = local("sandbox").await;
    three_plants(&m, &url).await;
    let (status, body) = es(&m, &url, "POST", "/plants/_delete_by_query", None).await;
    assert_eq!(status, 400, "{body}");
    let shade = r#"{"query": {"term": {"light": "shade"}}}"#;
    let (status, body) = es(&m, &url, "POST", "/plants/_delete_by_query", Some(shade)).await;
    assert_eq!(status, 200);
    assert_eq!((&body["deleted"], &body["total"]), (&json!(2), &json!(2)));
    let (_, counted) = es(&m, &url, "GET", "/plants/_count", None).await;
    assert_eq!(counted["count"], 1);
    let (status, _) = es(&m, &url, "POST", "/plants/_refresh", None).await;
    assert_eq!(status, 200);
}

#[tokio::test]
async fn all_and_wildcard_deletes_are_refused() {
    let (m, _, url) = local("sandbox").await;
    three_plants(&m, &url).await;
    for target in ["/_all", "/*", "/pla*", "/plants,_all"] {
        let (status, body) = es(&m, &url, "DELETE", target, None).await;
        assert_eq!(status, 400, "{target}: {body}");
        assert_eq!(
            body["error"]["type"], "illegal_argument_exception",
            "{target}"
        );
    }
    let (status, _) = es(&m, &url, "HEAD", "/plants", None).await;
    assert_eq!(status, 200);

    // Several named indices go together, or not at all.
    let (status, _) = es(&m, &url, "PUT", "/herbs", None).await;
    assert_eq!(status, 200);
    let (status, _) = es(&m, &url, "DELETE", "/plants,nowhere", None).await;
    assert_eq!(status, 404);
    let (status, _) = es(&m, &url, "DELETE", "/plants,herbs", None).await;
    assert_eq!(status, 200);
    let (_, rows) = es(&m, &url, "GET", "/_cat/indices?format=json", None).await;
    assert_eq!(rows, json!([]));
}

#[tokio::test]
async fn an_unknown_endpoint_has_no_handler() {
    let (m, _, url) = local("sandbox").await;
    for (method, path) in [
        ("GET", "/_nodes/hot_threads"),
        ("POST", "/_reindex"),
        ("PATCH", "/plants"),
    ] {
        let (status, body) = es(&m, &url, method, path, None).await;
        assert_eq!(status, 400, "{method} {path}");
        assert_eq!(
            body["error"],
            format!("no handler found for uri [{path}] and method [{method}]")
        );
    }
}

#[tokio::test]
async fn only_a_live_credential_of_the_cluster_gets_in() {
    let m = Mock::start_local().await;
    let a = m.ready_cluster("A", "standard-fir").await;
    let b = m.ready_cluster("B", "sandbox").await;
    let (url_a, url_b) = (cluster_url(&m, &a).await, cluster_url(&m, &b).await);

    // Each credential reaches only its own cluster's indices.
    let (status, _) = es(&m, &url_a, "PUT", "/only-a", None).await;
    assert_eq!(status, 200);
    let (_, rows) = es(&m, &url_b, "GET", "/_cat/indices?format=json", None).await;
    assert_eq!(rows, json!([]));

    // A wrong password, and a username nobody has.
    let mut wrong = url::Url::parse(&url_a).unwrap();
    wrong.set_password(Some("nope")).unwrap();
    let (status, h, _) = es_full(&m, wrong.as_str(), "GET", "/", None).await;
    assert_eq!(status, 401);
    assert_eq!(
        header(&h, "www-authenticate"),
        Some("Basic realm=\"bonsai-mock\"")
    );
    let (status, _) = es(&m, "http://ghost:nope@127.0.0.1", "GET", "/", None).await;
    assert_eq!(status, 401);

    // A revoked credential stops working at once.
    let (status, _, made) = m
        .call(
            "POST",
            &format!("/clusters/{a}/credentials"),
            Some(&m.acme),
            Some(json!({"name": "second", "access_type": "rw"})),
            &[],
        )
        .await;
    assert_eq!(status, 201);
    let made = made.unwrap();
    let second = made["url"].as_str().unwrap().to_owned();
    let (status, _) = es(&m, &second, "GET", "/", None).await;
    assert_eq!(status, 200);
    let (status, _, _) = m
        .call(
            "DELETE",
            &format!("/clusters/{a}/credentials/{}", made["id"].as_str().unwrap()),
            Some(&m.acme),
            None,
            &[],
        )
        .await;
    assert_eq!(status, 204);
    let (status, _) = es(&m, &second, "GET", "/", None).await;
    assert_eq!(status, 401);

    // And so does every credential of a destroyed cluster.
    let (status, _, action) = m
        .call(
            "DELETE",
            &format!("/clusters/{b}?name=B"),
            Some(&m.acme),
            None,
            &[],
        )
        .await;
    assert_eq!(status, 202);
    let done = m.action(action.unwrap()["id"].as_str().unwrap()).await;
    assert_eq!(done["state"], "success", "{done}");
    let (status, _) = es(&m, &url_b, "GET", "/", None).await;
    assert_eq!(status, 401);
}

#[tokio::test]
async fn a_read_only_credential_reads_but_cant_write() {
    let (m, cluster, url) = local("standard-fir").await;
    three_plants(&m, &url).await;
    let (status, _, made) = m
        .call(
            "POST",
            &format!("/clusters/{cluster}/credentials"),
            Some(&m.acme),
            Some(json!({"name": "reader", "access_type": "ro"})),
            &[],
        )
        .await;
    assert_eq!(status, 201);
    let reader = made.unwrap()["url"].as_str().unwrap().to_owned();

    let (status, _) = es(&m, &reader, "GET", "/_cat/indices?format=json", None).await;
    assert_eq!(status, 200);
    let (status, hits) = es(
        &m,
        &reader,
        "POST",
        "/plants/_search",
        Some(r#"{"query": {"match_all": {}}}"#),
    )
    .await;
    assert_eq!((status, &hits["hits"]["total"]["value"]), (200, &json!(3)));
    let (status, _) = es(&m, &reader, "POST", "/plants/_count", None).await;
    assert_eq!(status, 200);

    for (method, path, body) in [
        ("PUT", "/herbs", None),
        ("POST", "/plants/_bulk", Some(THREE)),
        ("PUT", "/plants/_doc/9", Some("{}")),
        ("DELETE", "/plants", None),
        (
            "POST",
            "/plants/_delete_by_query",
            Some(r#"{"query": {"match_all": {}}}"#),
        ),
    ] {
        let (status, refused) = es(&m, &reader, method, path, body).await;
        assert_eq!(status, 403, "{method} {path}: {refused}");
        assert_eq!(refused["error"]["type"], "security_exception");
    }
    let (_, counted) = es(&m, &url, "GET", "/plants/_count", None).await;
    assert_eq!(counted["count"], 3);
}

#[tokio::test]
async fn cluster_status_next_answers_the_next_request_once() {
    let (m, _, url) = local("sandbox").await;
    for bad in [
        json!(200),
        json!(399),
        json!(600),
        json!("429"),
        json!(true),
    ] {
        let (status, _) = m
            .admin("PUT", "/scenario/cluster_status_next", Some(bad.clone()))
            .await;
        assert_eq!(status, 400, "{bad}");
    }
    let (status, _) = m
        .admin("PUT", "/scenario/cluster_status_next", Some(json!(429)))
        .await;
    assert_eq!(status, 204);
    let (status, body) = es(&m, &url, "GET", "/", None).await;
    assert_eq!(status, 429, "{body}");
    assert_eq!(body["status"], 429);
    assert_eq!(scenario::get(&m.db(), "cluster_status_next").unwrap(), None);
    let (status, _) = es(&m, &url, "GET", "/", None).await;
    assert_eq!(status, 200);

    // A request that doesn't get past the credential check doesn't take it.
    scenario::set(&m.db(), "cluster_status_next", &json!(503)).unwrap();
    let (status, _) = es(&m, "http://ghost:nope@127.0.0.1", "GET", "/", None).await;
    assert_eq!(status, 401);
    let (status, _) = es(&m, &url, "GET", "/", None).await;
    assert_eq!(status, 503);
}
