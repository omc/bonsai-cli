//! `EngineClient` against a wiremock server standing in for a cluster.

use std::io::Read;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use bonsai_api::{EngineClient, EngineConfig, EngineError, EnginePath, RetryPolicy};
use reqwest::Method;
use reqwest::header::{AUTHORIZATION, CONTENT_TYPE, HeaderMap, HeaderValue};
use secrecy::SecretString;
use wiremock::matchers::{basic_auth, body_string, header, method, path, query_param};
use wiremock::{Mock, MockServer, ResponseTemplate};

/// Fast retries, so a test that rides out a 429 takes milliseconds.
fn fast() -> RetryPolicy {
    RetryPolicy {
        max_retries: 2,
        max_wait: Duration::from_secs(1),
        deadline: Duration::from_secs(30),
        base: Duration::from_millis(10),
    }
}

/// A client for `authority` whose URL carries a username and a password that both
/// need decoding: `user` and `p@ss`.
fn client_at(authority: &str, retry: RetryPolicy) -> EngineClient {
    EngineClient::new(EngineConfig {
        url: SecretString::from(format!("http://us%65r:p%40ss@{authority}")),
        user_agent: "bonsai-cli/0.0.0-test".into(),
        retry,
        ca_bundle: None,
    })
    .unwrap()
}

fn client(server: &MockServer) -> EngineClient {
    client_at(&server.address().to_string(), fast())
}

fn at(path: &str) -> EnginePath {
    EnginePath::parse(path).unwrap()
}

async fn requests(server: &MockServer) -> usize {
    server.received_requests().await.unwrap().len()
}

/// An error with its causes, as the CLI prints it.
fn chain(e: &dyn std::error::Error) -> String {
    let mut text = e.to_string();
    let mut source = e.source();
    while let Some(cause) = source {
        text.push_str(&format!(": {cause}"));
        source = cause.source();
    }
    text
}

#[tokio::test]
async fn the_password_goes_in_a_basic_header_decoded() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/_cat/indices"))
        .and(query_param("format", "json"))
        .and(basic_auth("user", "p@ss"))
        .and(header("user-agent", "bonsai-cli/0.0.0-test"))
        .respond_with(ResponseTemplate::new(200).set_body_string("[]"))
        .expect(1)
        .mount(&server)
        .await;
    let resp = client(&server)
        .send(
            &Method::GET,
            &at("/_cat/indices?format=json"),
            &HeaderMap::new(),
            None,
        )
        .await
        .unwrap();
    assert_eq!(resp.status, 200);
    assert_eq!(resp.body, b"[]");
    assert_eq!(resp.retries, 0);
}

#[tokio::test]
async fn the_body_and_headers_go_out_as_given() {
    let server = MockServer::start().await;
    let ndjson = "{\"index\":{}}\n{\"name\":\"Fern\"}\n";
    Mock::given(method("POST"))
        .and(path("/products/_bulk"))
        .and(header("content-type", "application/x-ndjson"))
        .and(body_string(ndjson))
        .respond_with(ResponseTemplate::new(200).set_body_string("{\"errors\":false}"))
        .expect(1)
        .mount(&server)
        .await;
    let mut headers = HeaderMap::new();
    headers.insert(
        CONTENT_TYPE,
        HeaderValue::from_static("application/x-ndjson"),
    );
    let resp = client(&server)
        .send(
            &Method::POST,
            &at("/products/_bulk"),
            &headers,
            Some(ndjson.as_bytes()),
        )
        .await
        .unwrap();
    assert_eq!(resp.status, 200);
}

#[tokio::test]
async fn an_error_status_is_an_answer_with_its_body() {
    let server = MockServer::start().await;
    let body = r#"{"error":{"reason":"no such index [nope]"},"status":404}"#;
    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(404).set_body_string(body))
        .mount(&server)
        .await;
    let resp = client(&server)
        .send(&Method::GET, &at("/nope"), &HeaderMap::new(), None)
        .await
        .unwrap();
    assert_eq!(resp.status, 404);
    assert_eq!(resp.body, body.as_bytes());
}

#[tokio::test]
async fn a_429_is_retried_until_the_cluster_answers() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(429).set_body_string("busy"))
        .up_to_n_times(2)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(200).set_body_string("ok"))
        .mount(&server)
        .await;
    let resp = client(&server)
        .send(
            &Method::POST,
            &at("/_bulk"),
            &HeaderMap::new(),
            Some(b"{}\n"),
        )
        .await
        .unwrap();
    assert_eq!((resp.status, resp.retries), (200, 2));
    assert_eq!(requests(&server).await, 3);
}

#[tokio::test]
async fn a_429_that_outlasts_the_retries_comes_back_as_it_is() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(429).set_body_string("busy"))
        .mount(&server)
        .await;
    let resp = client(&server)
        .send(&Method::GET, &at("/"), &HeaderMap::new(), None)
        .await
        .unwrap();
    assert_eq!((resp.status, resp.retries), (429, 2));
    assert_eq!(resp.body, b"busy");
    assert_eq!(requests(&server).await, 3);
}

/// The engine has no idempotency keys: a request that reached it may have been
/// applied, whatever its method.
#[tokio::test]
async fn a_5xx_is_never_retried() {
    for verb in ["GET", "POST"] {
        let server = MockServer::start().await;
        Mock::given(method(verb))
            .respond_with(ResponseTemplate::new(503))
            .mount(&server)
            .await;
        let resp = client(&server)
            .send(
                &verb.parse().unwrap(),
                &at("/_search"),
                &HeaderMap::new(),
                None,
            )
            .await
            .unwrap();
        assert_eq!((resp.status, resp.retries), (503, 0), "{verb}");
        assert_eq!(requests(&server).await, 1, "{verb}");
    }
}

#[tokio::test]
async fn a_redirect_is_an_answer_not_a_second_request() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/old"))
        .respond_with(ResponseTemplate::new(302).insert_header("location", "/new"))
        .mount(&server)
        .await;
    let resp = client(&server)
        .send(&Method::GET, &at("/old"), &HeaderMap::new(), None)
        .await
        .unwrap();
    assert_eq!(resp.status, 302);
    assert_eq!(requests(&server).await, 1);
}

#[tokio::test]
async fn the_deadline_is_a_timeout() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(200).set_delay(Duration::from_secs(5)))
        .mount(&server)
        .await;
    let retry = RetryPolicy {
        deadline: Duration::from_millis(300),
        ..fast()
    };
    let started = Instant::now();
    let err = client_at(&server.address().to_string(), retry)
        .send(&Method::GET, &at("/"), &HeaderMap::new(), None)
        .await
        .unwrap_err();
    assert!(matches!(err, EngineError::Timeout), "{err:?}");
    assert!(started.elapsed() < Duration::from_secs(3));
}

#[tokio::test]
async fn an_answer_over_the_cap_is_refused() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(200).set_body_string("x".repeat(64)))
        .mount(&server)
        .await;
    let err = client(&server)
        .max_body(16)
        .send(&Method::GET, &at("/"), &HeaderMap::new(), None)
        .await
        .unwrap_err();
    assert!(matches!(err, EngineError::TooLarge { max: 16 }), "{err:?}");
}

#[tokio::test]
async fn authorization_cant_be_set_by_hand() {
    let server = MockServer::start().await;
    let mut headers = HeaderMap::new();
    headers.insert(AUTHORIZATION, HeaderValue::from_static("Basic eDp5"));
    let err = client(&server)
        .send(&Method::GET, &at("/"), &headers, None)
        .await
        .unwrap_err();
    assert!(matches!(err, EngineError::Config(_)), "{err:?}");
    assert_eq!(requests(&server).await, 0);
}

/// Nothing listens on the port: the request was never sent, so it's retried, and the
/// error that comes back names neither the password nor the URL it rode in.
#[tokio::test]
async fn a_refused_connection_says_nothing_of_the_password() {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    drop(listener);
    let started = Instant::now();
    let err = client_at(&addr.to_string(), fast())
        .send(&Method::GET, &at("/"), &HeaderMap::new(), None)
        .await
        .unwrap_err();
    // With `fast()` (base 10 ms, 2 retries) the two backoff waits are at least 10 ms
    // and 20 ms, and a refused loopback connection itself returns in well under a
    // millisecond, so 30 ms elapsed shows both retries happened.
    assert!(started.elapsed() >= Duration::from_millis(30));
    assert!(matches!(err, EngineError::Unreachable(_)), "{err:?}");
    for text in [chain(&err), format!("{err:?}")] {
        for secret in ["p@ss", "p%40ss", "us%65r"] {
            assert!(!text.contains(secret), "{secret} in {text}");
        }
    }
}

#[tokio::test]
async fn retry_after_is_followed_up_to_the_longest_wait() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(429).insert_header("Retry-After", "30"))
        .up_to_n_times(1)
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(200))
        .mount(&server)
        .await;
    let retry = RetryPolicy {
        max_wait: Duration::from_millis(50),
        ..fast()
    };
    let started = Instant::now();
    let resp = client_at(&server.address().to_string(), retry)
        .send(&Method::GET, &at("/"), &HeaderMap::new(), None)
        .await
        .unwrap();
    let took = started.elapsed();
    assert_eq!((resp.status, resp.retries), (200, 1));
    assert!(took >= Duration::from_millis(50), "{took:?}");
    assert!(took < Duration::from_secs(5), "{took:?}");
}

#[test]
fn debug_output_never_holds_the_password() {
    let cfg = EngineConfig {
        url: SecretString::from("http://us%65r:p%40ss@127.0.0.1:9".to_owned()),
        user_agent: "t".into(),
        retry: fast(),
        ca_bundle: None,
    };
    let config_debug = format!("{cfg:?}");
    let client_debug = format!("{:?}", EngineClient::new(cfg).unwrap());
    for text in [config_debug, client_debug] {
        for secret in ["p@ss", "p%40ss"] {
            assert!(!text.contains(secret), "{secret} in {text}");
        }
    }
}

#[tokio::test]
async fn a_429_over_the_cap_is_too_large_once_retries_run_out() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(429).set_body_string("x".repeat(64)))
        .mount(&server)
        .await;
    let err = client(&server)
        .max_body(16)
        .send(&Method::GET, &at("/"), &HeaderMap::new(), None)
        .await
        .unwrap_err();
    assert!(matches!(err, EngineError::TooLarge { max: 16 }), "{err:?}");
    assert_eq!(
        err.to_string(),
        "the cluster's answer is larger than 16 bytes"
    );
}

#[test]
fn only_https_or_loopback_http() {
    let new = |url: &str| {
        EngineClient::new(EngineConfig {
            url: SecretString::from(url.to_owned()),
            user_agent: "t".into(),
            retry: fast(),
            ca_bundle: None,
        })
    };
    assert!(new("https://u:p@search.example.com").is_ok());
    assert!(new("http://u:p@127.0.0.1:9200").is_ok());
    assert!(new("http://u:p@localhost:9200").is_ok());
    for insecure in [
        "http://u:p@search.example.com",
        "http://u:p@127.0.0.2:9200",
        "ftp://u:p@127.0.0.1",
    ] {
        let err = new(insecure).unwrap_err();
        assert!(matches!(err, EngineError::InsecureUrl), "{insecure}");
    }
}

/// A server that reads each request's headers and then drops the connection
/// without a word, counting the connections it took.
fn hang_up_after_headers() -> (std::net::SocketAddr, Arc<AtomicUsize>) {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let accepted = Arc::new(AtomicUsize::new(0));
    let count = accepted.clone();
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { return };
            count.fetch_add(1, Ordering::SeqCst);
            let mut seen = Vec::new();
            let mut buf = [0u8; 1024];
            while !seen.windows(4).any(|w| w == b"\r\n\r\n") {
                match stream.read(&mut buf) {
                    Ok(0) | Err(_) => break,
                    Ok(n) => seen.extend_from_slice(&buf[..n]),
                }
            }
            drop(stream);
        }
    });
    (addr, accepted)
}

/// The request reached the cluster, so it may have been applied: not the "couldn't
/// reach" of a request that was never sent, and not sent again.
#[tokio::test]
async fn a_connection_that_breaks_after_the_request_is_sent_isnt_retried() {
    let (addr, accepted) = hang_up_after_headers();
    let err = client_at(&addr.to_string(), fast())
        .send(
            &Method::POST,
            &at("/plants/_doc"),
            &HeaderMap::new(),
            Some(b"{\"name\":\"Fern\"}"),
        )
        .await
        .unwrap_err();
    let text = chain(&err);
    assert!(text.contains("may have applied"), "{text}");
    assert!(matches!(err, EngineError::Interrupted(_)), "{err:?}");
    for text in [text, format!("{err:?}")] {
        for secret in ["p@ss", "p%40ss", "us%65r"] {
            assert!(!text.contains(secret), "{secret} in {text}");
        }
    }
    assert_eq!(accepted.load(Ordering::SeqCst), 1);
}

/// These answer with the highest status among their own bulk and search failures,
/// so a 429 can follow work already done: sending again would do it twice.
#[tokio::test]
async fn a_429_from_a_by_query_endpoint_comes_back_unretried() {
    for endpoint in [
        "/plants/_update_by_query",
        "/plants/_delete_by_query",
        "/_reindex",
    ] {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(429).set_body_string("busy"))
            .mount(&server)
            .await;
        let resp = client(&server)
            .send(&Method::POST, &at(endpoint), &HeaderMap::new(), Some(b"{}"))
            .await
            .unwrap();
        assert_eq!((resp.status, resp.retries), (429, 0), "{endpoint}");
        assert_eq!(requests(&server).await, 1, "{endpoint}");
    }
}

/// RFC 9110 9.3.8: no credentials in a TRACE, whose answer reflects the request.
#[tokio::test]
async fn trace_and_connect_are_never_sent() {
    for verb in [Method::TRACE, Method::CONNECT] {
        let server = MockServer::start().await;
        let err = client(&server)
            .send(&verb, &at("/"), &HeaderMap::new(), None)
            .await
            .unwrap_err();
        assert!(matches!(err, EngineError::Config(_)), "{verb}: {err:?}");
        assert_eq!(requests(&server).await, 0, "{verb}");
    }
}

/// A TLS server on loopback (the certificate of `tests/fixtures`, for 127.0.0.1) that
/// completes the handshake, reads a request up to the end of its headers, then writes
/// garbage straight onto the TCP stream beneath TLS and closes. Returns its port.
async fn tls_server_that_breaks_after_the_request() -> u16 {
    use rustls::pki_types::pem::PemObject;
    use rustls::pki_types::{CertificateDer, PrivateKeyDer};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    let fixture = |name: &str| fixtures().join(name);
    let certs = CertificateDer::pem_file_iter(fixture("test-server.pem"))
        .unwrap()
        .collect::<Result<Vec<_>, _>>()
        .unwrap();
    let key = PrivateKeyDer::from_pem_file(fixture("test-server.key")).unwrap();
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let tls = rustls::ServerConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .unwrap()
        .with_no_client_auth()
        .with_single_cert(certs, key)
        .unwrap();
    let acceptor = tokio_rustls::TlsAcceptor::from(Arc::new(tls));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(async move {
        while let Ok((tcp, _)) = listener.accept().await {
            let acceptor = acceptor.clone();
            tokio::spawn(async move {
                let Ok(mut stream) = acceptor.accept(tcp).await else {
                    return;
                };
                let mut seen = Vec::new();
                let mut chunk = [0u8; 1024];
                while !seen.windows(4).any(|w| w == b"\r\n\r\n") {
                    match stream.read(&mut chunk).await {
                        Ok(0) | Err(_) => return,
                        Ok(n) => seen.extend_from_slice(&chunk[..n]),
                    }
                }
                let (mut tcp, _) = stream.into_inner();
                let _ = tcp.write_all(b"\x17\x03\x03\xff\xffnot a tls record").await;
                let _ = tcp.shutdown().await;
            });
        }
    });
    port
}

fn fixtures() -> std::path::PathBuf {
    std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures")
}

/// A TLS failure means "nothing was sent" only during the handshake. One on a
/// connection already in use came after the request left, so it may have applied.
#[tokio::test]
async fn a_tls_error_after_the_request_is_sent_is_an_interruption_not_unreachable() {
    let port = tls_server_that_breaks_after_the_request().await;
    let at_port = |ca_bundle| {
        EngineClient::new(EngineConfig {
            url: SecretString::from(format!("https://us%65r:p%40ss@127.0.0.1:{port}")),
            user_agent: "bonsai-cli/0.0.0-test".into(),
            retry: fast(),
            ca_bundle,
        })
        .unwrap()
    };
    let send = |client: EngineClient| async move {
        client
            .send(
                &Method::POST,
                &at("/plants/_doc"),
                &HeaderMap::new(),
                Some(b"{\"name\":\"Fern\"}"),
            )
            .await
            .unwrap_err()
    };

    let after = send(at_port(Some(fixtures().join("test-ca.pem")))).await;
    assert!(matches!(after, EngineError::Interrupted(_)), "{after:?}");

    // The handshake itself failing (the certificate isn't trusted) is the other half:
    // nothing was sent.
    let during = send(at_port(None)).await;
    assert!(matches!(during, EngineError::Unreachable(_)), "{during:?}");

    for err in [after, during] {
        for text in [chain(&err), format!("{err:?}")] {
            for secret in ["p@ss", "p%40ss", "us%65r"] {
                assert!(!text.contains(secret), "{secret} in {text}");
            }
        }
    }
}

/// An answer's room for another attempt includes a whole connect: without it the
/// deadline could pass mid-connect and read as a timeout after sending.
#[tokio::test]
async fn under_a_short_deadline_a_429_comes_back_without_another_attempt() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(429).set_body_string("busy"))
        .mount(&server)
        .await;
    let retry = RetryPolicy {
        deadline: Duration::from_secs(2),
        ..fast()
    };
    let resp = client_at(&server.address().to_string(), retry)
        .send(&Method::GET, &at("/"), &HeaderMap::new(), None)
        .await
        .unwrap();
    assert_eq!((resp.status, resp.retries), (429, 0));
    assert_eq!(requests(&server).await, 1);
}
