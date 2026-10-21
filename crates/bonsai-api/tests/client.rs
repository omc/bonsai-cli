use bonsai_api::*;
use secrecy::SecretString;
use serde_json::{Value, json};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};
use wiremock::matchers::{
    body_json, header, header_regex, method, path, query_param, query_param_is_missing,
};
use wiremock::{Mock, MockServer, ResponseTemplate};

const T: &str = "2026-01-01T00:00:00Z";

/// A 503 the client retries. `metrics_unavailable` isn't one: it comes straight back.
const UNAVAILABLE: &str = "service_unavailable";

/// Fast retries: tests that need a real `Retry-After` set it on the response.
fn fast() -> RetryPolicy {
    RetryPolicy {
        max_retries: 4,
        max_wait: Duration::from_secs(5),
        deadline: Duration::from_secs(20),
        base: Duration::from_millis(10),
    }
}

fn config(base: &str) -> ClientConfig {
    ClientConfig {
        base_url: base.parse().unwrap(),
        token: Some(SecretString::from("bonsai_key_secret")),
        user_agent: "bonsai-cli/0.0.0-test (linux; x86_64)".into(),
        retry: fast(),
        ca_bundle: None,
    }
}

fn client(server: &MockServer) -> Client {
    Client::new(config(&format!("{}/v1", server.uri()))).unwrap()
}

fn client_with(server: &MockServer, retry: RetryPolicy) -> Client {
    let mut cfg = config(&format!("{}/v1", server.uri()));
    cfg.retry = retry;
    Client::new(cfg).unwrap()
}

fn problem(status: u16, code: &str) -> Value {
    json!({
        "type": format!("https://bonsai.io/docs/api/problems/{code}"),
        "title": "Something went wrong", "status": status, "code": code,
        "detail": "details here", "details": []
    })
}

fn problem_response(status: u16, code: &str) -> ResponseTemplate {
    ResponseTemplate::new(status).set_body_raw(
        problem(status, code).to_string(),
        "application/problem+json",
    )
}

fn action_json() -> Value {
    json!({
        "id": "act_1", "type": "cluster.provision", "state": "new",
        "subject": {"type": "cluster", "id": "c-1"}, "created_at": T, "finished_at": null,
        "error": null, "result": null, "steps": []
    })
}

fn account_json() -> Value {
    json!({"id": "acc_1", "name": "Acme", "contact_email_override": null, "created_at": T})
}

fn cluster_json(id: &str) -> Value {
    json!({
        "id": id, "name": id, "state": "ready", "plan": {"id": "standard", "name": "Standard"},
        "region": {"id": "aws-us-east-1", "name": "AWS US East (Virginia)"}, "space": null,
        "release": {"id": "opensearch-3.8.0", "name": "OpenSearch 3.8.0"},
        "single_tenant": false, "host": null, "port": 443,
        "stats": {"docs": null, "shards": null, "bytes": null, "polled_at": null, "limits": null},
        "latest_action": null, "created_at": T, "terminated_at": null
    })
}

fn create_body() -> ClusterCreate {
    ClusterCreate {
        name: "prod".into(),
        plan: "standard".into(),
        region: "aws-us-east-1".into(),
        space: None,
        release: None,
    }
}

async fn request_count(server: &MockServer) -> usize {
    server.received_requests().await.unwrap().len()
}

fn fixture(name: &str) -> std::path::PathBuf {
    std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures")
        .join(name)
}

/// Counts request attempts through the `attempt` field each one logs. Only sees
/// events on this thread, which is the test's (`#[tokio::test]` is current-thread).
struct Attempts {
    count: Arc<AtomicUsize>,
    _guard: tracing::subscriber::DefaultGuard,
}

impl Attempts {
    fn install() -> Self {
        use tracing_subscriber::layer::SubscriberExt;
        let count = Arc::new(AtomicUsize::new(0));
        let layer = AttemptLayer(count.clone());
        let guard = tracing::subscriber::set_default(tracing_subscriber::registry().with(layer));
        Self {
            count,
            _guard: guard,
        }
    }

    fn count(&self) -> usize {
        self.count.load(Ordering::SeqCst)
    }
}

struct AttemptLayer(Arc<AtomicUsize>);

impl<S: tracing::Subscriber> tracing_subscriber::Layer<S> for AttemptLayer {
    fn on_event(&self, event: &tracing::Event<'_>, _: tracing_subscriber::layer::Context<'_, S>) {
        if event.metadata().fields().field("attempt").is_some() {
            self.0.fetch_add(1, Ordering::SeqCst);
        }
    }
}

/// An HTTPS server on 127.0.0.1 whose certificate is signed by `fixtures/test-ca.pem`.
/// It answers every request with `account_json()`, and counts the connections it accepts.
/// The leaf expires 2028-12-29; macOS rejects TLS leaves valid for more than 825 days.
async fn tls_server() -> (String, Arc<AtomicUsize>) {
    use rustls::pki_types::pem::PemObject;
    use rustls::pki_types::{CertificateDer, PrivateKeyDer};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

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
    let accepted = Arc::new(AtomicUsize::new(0));
    let counter = accepted.clone();
    tokio::spawn(async move {
        while let Ok((tcp, _)) = listener.accept().await {
            counter.fetch_add(1, Ordering::SeqCst);
            let acceptor = acceptor.clone();
            tokio::spawn(async move {
                let Ok(mut stream) = acceptor.accept(tcp).await else {
                    return;
                };
                let mut buf = Vec::new();
                let mut chunk = [0u8; 1024];
                while !buf.windows(4).any(|w| w == b"\r\n\r\n") {
                    match stream.read(&mut chunk).await {
                        Ok(0) | Err(_) => return,
                        Ok(n) => buf.extend_from_slice(&chunk[..n]),
                    }
                }
                let body = account_json().to_string();
                let head = format!(
                    "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
                    body.len()
                );
                let _ = stream.write_all(head.as_bytes()).await;
                let _ = stream.write_all(body.as_bytes()).await;
                let _ = stream.shutdown().await;
            });
        }
    });
    (format!("https://127.0.0.1:{port}/v1"), accepted)
}

#[tokio::test]
async fn sends_bearer_and_user_agent() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/v1/account"))
        .and(header("authorization", "Bearer bonsai_key_secret"))
        .and(header(
            "user-agent",
            "bonsai-cli/0.0.0-test (linux; x86_64)",
        ))
        .and(header_regex(
            "accept",
            r"^application/json, application/problem\+json$",
        ))
        .respond_with(ResponseTemplate::new(200).set_body_json(account_json()))
        .expect(1)
        .mount(&server)
        .await;

    let res = client(&server).get_account().await.unwrap();
    assert_eq!(res.status, 200);
    assert_eq!(res.value.name, "Acme");
    assert_eq!(res.raw, account_json());
}

#[tokio::test]
async fn retries_503_honouring_retry_after_seconds() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/v1/account"))
        .respond_with(problem_response(503, UNAVAILABLE).insert_header("Retry-After", "1"))
        .up_to_n_times(1)
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/v1/account"))
        .respond_with(ResponseTemplate::new(200).set_body_json(account_json()))
        .mount(&server)
        .await;

    let started = Instant::now();
    let res = client(&server).get_account().await.unwrap();
    assert_eq!(res.value.id, "acc_1");
    assert_eq!(request_count(&server).await, 2);
    assert!(
        started.elapsed() >= Duration::from_secs(1),
        "{:?}",
        started.elapsed()
    );
}

#[test]
fn parses_retry_after_seconds_and_http_date() {
    let now: jiff::Timestamp = "2026-10-21T07:27:30Z".parse().unwrap();
    assert_eq!(
        parse_retry_after("Wed, 21 Oct 2026 07:28:00 GMT", now),
        Some(Duration::from_secs(30))
    );
    assert_eq!(parse_retry_after("5", now), Some(Duration::from_secs(5)));
    assert_eq!(parse_retry_after(" 7 ", now), Some(Duration::from_secs(7)));
    // A date in the past means "now".
    assert_eq!(
        parse_retry_after("Wed, 21 Oct 2026 07:00:00 GMT", now),
        Some(Duration::ZERO)
    );
    assert_eq!(parse_retry_after("-1", now), None);
    assert_eq!(parse_retry_after("soon", now), None);
}

#[tokio::test]
async fn retries_429_with_http_date() {
    let server = MockServer::start().await;
    let at = jiff::Timestamp::now() + jiff::SignedDuration::from_secs(2);
    let date = jiff::fmt::rfc2822::DateTimePrinter::new()
        .timestamp_to_rfc9110_string(&at)
        .unwrap();
    Mock::given(method("GET"))
        .and(path("/v1/account"))
        .respond_with(
            problem_response(429, "rate_limited").insert_header("Retry-After", date.as_str()),
        )
        .up_to_n_times(1)
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/v1/account"))
        .respond_with(ResponseTemplate::new(200).set_body_json(account_json()))
        .mount(&server)
        .await;

    let started = Instant::now();
    client(&server).get_account().await.unwrap();
    assert_eq!(request_count(&server).await, 2);
    // Second precision: the date is 1-2 s away when parsed.
    assert!(
        started.elapsed() >= Duration::from_millis(900),
        "{:?}",
        started.elapsed()
    );
}

#[tokio::test]
async fn caps_retry_after_at_max_wait() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/v1/account"))
        .respond_with(problem_response(503, UNAVAILABLE).insert_header("Retry-After", "3600"))
        .up_to_n_times(1)
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/v1/account"))
        .respond_with(ResponseTemplate::new(200).set_body_json(account_json()))
        .mount(&server)
        .await;

    let retry = RetryPolicy {
        max_wait: Duration::from_secs(1),
        ..fast()
    };
    let started = Instant::now();
    client_with(&server, retry).get_account().await.unwrap();
    assert_eq!(request_count(&server).await, 2);
    assert!(
        started.elapsed() < Duration::from_secs(3),
        "{:?}",
        started.elapsed()
    );
}

#[tokio::test]
async fn stops_at_the_deadline() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/v1/account"))
        .respond_with(problem_response(503, UNAVAILABLE).insert_header("Retry-After", "2"))
        .mount(&server)
        .await;

    let retry = RetryPolicy {
        deadline: Duration::from_secs(1),
        ..fast()
    };
    let started = Instant::now();
    let err = client_with(&server, retry).get_account().await.unwrap_err();
    assert_eq!(err.status(), Some(503));
    assert_eq!(request_count(&server).await, 1);
    assert!(
        started.elapsed() < Duration::from_secs(1),
        "{:?}",
        started.elapsed()
    );
}

#[tokio::test]
async fn reuses_idempotency_key_across_retries() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/clusters"))
        .respond_with(ResponseTemplate::new(502))
        .up_to_n_times(1)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/v1/clusters"))
        .respond_with(
            ResponseTemplate::new(202)
                .set_body_json(action_json())
                .insert_header("Location", "/v1/actions/act_1")
                .insert_header("Retry-After", "2")
                .insert_header("Idempotent-Replayed", "true"),
        )
        .mount(&server)
        .await;

    let key = new_idempotency_key();
    let res = client(&server)
        .create_cluster(&create_body(), &key)
        .await
        .unwrap();
    assert_eq!(res.status, 202);
    assert_eq!(res.value.id, "act_1");
    assert_eq!(res.location.as_deref(), Some("/v1/actions/act_1"));
    assert_eq!(res.retry_after, Some(Duration::from_secs(2)));
    assert!(res.idempotent_replayed);

    let requests = server.received_requests().await.unwrap();
    assert_eq!(requests.len(), 2);
    let keys: Vec<_> = requests
        .iter()
        .map(|r| {
            r.headers
                .get("idempotency-key")
                .unwrap()
                .to_str()
                .unwrap()
                .to_owned()
        })
        .collect();
    assert_eq!(keys, vec![key.clone(), key]);
}

#[test]
fn idempotency_keys_are_fresh_uuids() {
    let (a, b) = (new_idempotency_key(), new_idempotency_key());
    assert_ne!(a, b);
    assert_eq!(uuid::Uuid::parse_str(&a).unwrap().get_version_num(), 4);
}

#[tokio::test]
async fn does_not_retry_patch_after_send() {
    let server = MockServer::start().await;
    Mock::given(method("PATCH"))
        .and(path("/v1/clusters/c-1"))
        .respond_with(ResponseTemplate::new(503))
        .mount(&server)
        .await;

    let err = client(&server)
        .update_cluster("c-1", &ClusterUpdate { name: "new".into() })
        .await
        .unwrap_err();
    assert_eq!(err.status(), Some(503));
    assert_eq!(request_count(&server).await, 1);
}

#[tokio::test]
async fn does_not_retry_unkeyed_post_after_send() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/cli/logins/token"))
        .respond_with(ResponseTemplate::new(503))
        .mount(&server)
        .await;

    let err = client(&server).poll_cli_login("dev_1").await.unwrap_err();
    assert_eq!(err.status(), Some(503));
    assert_eq!(request_count(&server).await, 1);
}

#[tokio::test]
async fn retries_idempotency_key_in_use_409() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/clusters"))
        .respond_with(
            problem_response(409, "idempotency_key_in_use").insert_header("Retry-After", "1"),
        )
        .up_to_n_times(1)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/v1/clusters"))
        .respond_with(ResponseTemplate::new(202).set_body_json(action_json()))
        .mount(&server)
        .await;

    let res = client(&server)
        .create_cluster(&create_body(), &new_idempotency_key())
        .await
        .unwrap();
    assert_eq!(res.value.id, "act_1");
    assert_eq!(request_count(&server).await, 2);
}

#[tokio::test]
async fn does_not_retry_other_409s() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/clusters"))
        .respond_with(problem_response(409, "action_in_progress").insert_header("Retry-After", "1"))
        .mount(&server)
        .await;

    let err = client(&server)
        .create_cluster(&create_body(), &new_idempotency_key())
        .await
        .unwrap_err();
    assert_eq!(err.code(), Some(&ProblemCode::ActionInProgress));
    assert_eq!(request_count(&server).await, 1);
}

#[tokio::test]
async fn gives_up_after_max_retries() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/v1/account"))
        .respond_with(ResponseTemplate::new(503).set_body_string("upstream down"))
        .mount(&server)
        .await;

    let err = client(&server).get_account().await.unwrap_err();
    assert_eq!(request_count(&server).await, 5);
    match err {
        ApiError::Http { status, body } => {
            assert_eq!(status, 503);
            assert_eq!(body, "upstream down");
        }
        other => panic!("expected Http, got {other:?}"),
    }
}

#[tokio::test]
async fn does_not_retry_metrics_unavailable() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/v1/clusters/c-1/metrics"))
        .respond_with(
            problem_response(503, "metrics_unavailable").insert_header("Retry-After", "30"),
        )
        .mount(&server)
        .await;

    let started = Instant::now();
    let err = client(&server)
        .get_cluster_metrics("c-1", None, None, None)
        .await
        .unwrap_err();
    assert_eq!(request_count(&server).await, 1);
    assert!(
        started.elapsed() < Duration::from_secs(1),
        "{:?}",
        started.elapsed()
    );
    match err {
        ApiError::Problem {
            status: 503,
            problem,
            retry_after,
            ..
        } => {
            assert_eq!(problem.code, ProblemCode::MetricsUnavailable);
            assert_eq!(retry_after, Some(Duration::from_secs(30)));
        }
        other => panic!("expected a Problem, got {other:?}"),
    }
}

#[tokio::test]
async fn retries_connect_errors() {
    // Nothing listens on port 1. (A dropped MockServer goes back to wiremock's pool,
    // so its port may still answer.)
    let attempts = Attempts::install();
    let mut cfg = config("http://127.0.0.1:1/v1");
    cfg.retry = RetryPolicy {
        max_retries: 2,
        ..fast()
    };
    let err = Client::new(cfg).unwrap().get_account().await.unwrap_err();
    match err {
        ApiError::Transport(e) => assert!(e.is_connect(), "{e:?}"),
        other => panic!("expected Transport, got {other:?}"),
    }
    assert_eq!(attempts.count(), 3);
}

#[tokio::test]
async fn retries_patch_on_connect_errors() {
    let attempts = Attempts::install();
    let mut cfg = config("http://127.0.0.1:1/v1");
    cfg.retry = RetryPolicy {
        max_retries: 2,
        ..fast()
    };
    let err = Client::new(cfg)
        .unwrap()
        .update_cluster("c-1", &ClusterUpdate { name: "new".into() })
        .await
        .unwrap_err();
    assert!(
        matches!(&err, ApiError::Transport(e) if e.is_connect()),
        "{err:?}"
    );
    assert_eq!(attempts.count(), 3);
}

#[tokio::test]
async fn does_not_retry_certificate_errors() {
    let (base, accepted) = tls_server().await;
    let attempts = Attempts::install();
    let err = Client::new(config(&base))
        .unwrap()
        .get_account()
        .await
        .unwrap_err();
    assert!(matches!(err, ApiError::Transport(_)), "{err:?}");
    assert_eq!(attempts.count(), 1);
    assert_eq!(accepted.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn ca_bundle_adds_a_trusted_root() {
    let (base, _) = tls_server().await;
    let mut cfg = config(&base);
    cfg.ca_bundle = Some(fixture("test-ca.pem"));
    let res = Client::new(cfg).unwrap().get_account().await.unwrap();
    assert_eq!(res.value.id, "acc_1");
}

#[tokio::test]
async fn deadline_cuts_a_slow_attempt_and_returns_the_previous_error() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/v1/account"))
        .respond_with(problem_response(503, UNAVAILABLE).insert_header("Retry-After", "1"))
        .up_to_n_times(1)
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/v1/account"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(account_json())
                .set_delay(Duration::from_secs(10)),
        )
        .mount(&server)
        .await;

    let retry = RetryPolicy {
        deadline: Duration::from_secs(2),
        ..fast()
    };
    let started = Instant::now();
    let err = client_with(&server, retry).get_account().await.unwrap_err();
    assert!(
        matches!(
            err,
            ApiError::Problem {
                status: 503,
                retry_after: Some(_),
                ..
            }
        ),
        "{err:?}"
    );
    assert_eq!(request_count(&server).await, 2);
    let elapsed = started.elapsed();
    assert!(
        elapsed >= Duration::from_secs(2) && elapsed < Duration::from_secs(3),
        "{elapsed:?}"
    );
}

#[tokio::test]
async fn deadline_cut_on_a_write_is_a_timeout_not_the_earlier_refusal() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/clusters"))
        .respond_with(problem_response(503, UNAVAILABLE).insert_header("Retry-After", "1"))
        .up_to_n_times(1)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/v1/clusters"))
        .respond_with(
            ResponseTemplate::new(202)
                .set_body_json(action_json())
                .set_delay(Duration::from_secs(10)),
        )
        .mount(&server)
        .await;

    let retry = RetryPolicy {
        deadline: Duration::from_secs(2),
        ..fast()
    };
    let err = client_with(&server, retry)
        .create_cluster(&create_body(), &new_idempotency_key())
        .await
        .unwrap_err();
    // The second attempt reached the server and may have created the cluster.
    assert!(
        matches!(&err, ApiError::Transport(e) if e.is_timeout()),
        "{err:?}"
    );
    assert_eq!(request_count(&server).await, 2);
}

#[tokio::test]
async fn send_raw_works_with_a_root_base_url() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/clusters"))
        .and(query_param("limit", "2"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({})))
        .expect(1)
        .mount(&server)
        .await;

    let c = Client::new(config(&format!("{}/", server.uri()))).unwrap();
    c.send_raw(reqwest::Method::GET, "/clusters?limit=2", None, None)
        .await
        .unwrap();
}

#[tokio::test]
async fn skips_an_attempt_with_too_little_time_left() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/v1/account"))
        .respond_with(problem_response(503, UNAVAILABLE).insert_header("Retry-After", "1"))
        .mount(&server)
        .await;

    // Waiting 1 s would leave 100 ms of a 1.1 s deadline: not worth an attempt.
    let retry = RetryPolicy {
        deadline: Duration::from_millis(1100),
        ..fast()
    };
    let started = Instant::now();
    let err = client_with(&server, retry).get_account().await.unwrap_err();
    assert_eq!(err.status(), Some(503));
    assert_eq!(request_count(&server).await, 1);
    assert!(
        started.elapsed() < Duration::from_millis(500),
        "{:?}",
        started.elapsed()
    );
}

#[tokio::test]
async fn retries_429_on_unkeyed_post() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/cli/logins"))
        .respond_with(problem_response(429, "rate_limited"))
        .up_to_n_times(1)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/v1/cli/logins"))
        .respond_with(ResponseTemplate::new(201).set_body_json(json!({
            "device_code": "dev_1", "user_code": "WDJB-MJHT",
            "verification_uri": "https://app.bonsai.io/cli",
            "verification_uri_complete": "https://app.bonsai.io/cli?code=WDJB-MJHT",
            "expires_in": 600, "interval": 5
        })))
        .mount(&server)
        .await;

    let start = CliLoginStart {
        client_name: "bonsai-cli".into(),
        hostname: "laptop".into(),
        os: "linux".into(),
    };
    client(&server).start_cli_login(&start).await.unwrap();
    assert_eq!(request_count(&server).await, 2);
}

#[tokio::test]
async fn does_not_follow_redirects() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/v1/account"))
        .respond_with(ResponseTemplate::new(302).insert_header("Location", "/v1/elsewhere"))
        .mount(&server)
        .await;

    let err = client(&server).get_account().await.unwrap_err();
    assert!(matches!(err, ApiError::Http { status: 302, .. }), "{err:?}");
    assert_eq!(request_count(&server).await, 1);
}

#[tokio::test]
async fn decodes_problem() {
    let server = MockServer::start().await;
    let mut body = problem(422, "payment_method_required");
    body["action_url"] = json!("https://checkout.stripe.com/c/pay/cs_1");
    Mock::given(method("POST"))
        .and(path("/v1/clusters"))
        .respond_with(
            ResponseTemplate::new(422).set_body_raw(body.to_string(), "application/problem+json"),
        )
        .mount(&server)
        .await;

    let err = client(&server)
        .create_cluster(&create_body(), &new_idempotency_key())
        .await
        .unwrap_err();
    assert_eq!(err.status(), Some(422));
    assert_eq!(err.code(), Some(&ProblemCode::PaymentMethodRequired));
    match err {
        ApiError::Problem {
            status,
            problem,
            retry_after,
            ..
        } => {
            assert_eq!(status, 422);
            assert_eq!(
                problem.action_url.as_deref(),
                Some("https://checkout.stripe.com/c/pay/cs_1")
            );
            assert_eq!(retry_after, None);
        }
        other => panic!("expected Problem, got {other:?}"),
    }
    assert_eq!(request_count(&server).await, 1);
}

#[tokio::test]
async fn non_problem_errors_are_http() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/v1/clusters/c-1"))
        .respond_with(ResponseTemplate::new(404).set_body_string("<html>nope</html>"))
        .mount(&server)
        .await;

    let err = client(&server).get_cluster("c-1").await.unwrap_err();
    assert!(matches!(err, ApiError::Http { status: 404, .. }), "{err:?}");
    assert_eq!(err.code(), None);
}

#[tokio::test]
async fn undecodable_success_is_decode_error() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/v1/account"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"id": "acc_1"})))
        .mount(&server)
        .await;

    let err = client(&server).get_account().await.unwrap_err();
    assert!(matches!(err, ApiError::Decode(_)), "{err:?}");
}

#[test]
fn refuses_http_non_localhost() {
    for bad in [
        "http://api.example.com/v1",
        "http://localhost.evil.com/v1",
        "http://127.0.0.1.nip.io/v1",
        "ftp://127.0.0.1/v1",
    ] {
        let err = Client::new(config(bad)).unwrap_err();
        assert!(matches!(err, ApiError::InsecureUrl(_)), "{bad}: {err:?}");
    }
    for ok in [
        "http://127.0.0.1:1/v1",
        "http://localhost:3000/v1",
        "http://[::1]:3000/v1",
        "https://api.bonsai.io/v1",
    ] {
        Client::new(config(ok)).unwrap_or_else(|e| panic!("{ok}: {e}"));
    }
}

#[test]
fn ca_bundle_is_checked_at_new() {
    let mut cfg = config("https://api.bonsai.io/v1");
    cfg.ca_bundle = Some("/nonexistent/bonsai-ca.pem".into());
    assert!(matches!(Client::new(cfg), Err(ApiError::Config(_))));

    let file = tempfile::NamedTempFile::new().unwrap();
    std::fs::write(file.path(), "not a certificate").unwrap();
    let mut cfg = config("https://api.bonsai.io/v1");
    cfg.ca_bundle = Some(file.path().to_owned());
    assert!(matches!(Client::new(cfg), Err(ApiError::Config(_))));

    let mut cfg = config("https://api.bonsai.io/v1");
    cfg.ca_bundle = Some(fixture("test-ca.pem"));
    Client::new(cfg).unwrap();
}

#[tokio::test]
async fn send_raw_joins_path_and_query_under_v1() {
    let server = MockServer::start().await;
    let page = json!({"data": [cluster_json("a"), cluster_json("b")], "next_cursor": null});
    Mock::given(method("GET"))
        .and(path("/v1/clusters"))
        .and(query_param("limit", "2"))
        .respond_with(ResponseTemplate::new(200).set_body_json(page.clone()))
        .expect(1)
        .mount(&server)
        .await;

    let res = client(&server)
        .send_raw(reqwest::Method::GET, "/clusters?limit=2", None, None)
        .await
        .unwrap();
    assert_eq!(res.value, page);
    assert_eq!(res.raw, page);
}

#[tokio::test]
async fn send_raw_stays_under_v1_on_the_api_host() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({})))
        .mount(&server)
        .await;

    let c = client(&server);
    c.send_raw(
        reqwest::Method::GET,
        "https://evil.example.com/x",
        None,
        None,
    )
    .await
    .unwrap();
    let requests = server.received_requests().await.unwrap();
    assert_eq!(requests.len(), 1);
    assert!(
        requests[0].url.path().starts_with("/v1/https:"),
        "{}",
        requests[0].url
    );

    for escape in ["/../account", "/%2e%2e/account", "/clusters/../../x", ".."] {
        let err = c
            .send_raw(reqwest::Method::GET, escape, None, None)
            .await
            .unwrap_err();
        assert!(matches!(err, ApiError::InvalidPath(_)), "{escape}: {err:?}");
    }
    assert_eq!(request_count(&server).await, 1);
}

#[tokio::test]
async fn path_segments_are_encoded() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/v1/clusters/a%2Fb%3Fc"))
        .respond_with(ResponseTemplate::new(200).set_body_json(cluster_json("x")))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/v1/clusters/%252e%252e"))
        .respond_with(ResponseTemplate::new(200).set_body_json(cluster_json("y")))
        .expect(1)
        .mount(&server)
        .await;

    let c = client(&server);
    c.get_cluster("a/b?c").await.unwrap();
    c.get_cluster("%2e%2e").await.unwrap();

    // `url` would drop these or turn them into a trailing slash.
    for id in ["", ".", ".."] {
        let err = c.get_cluster(id).await.unwrap_err();
        assert!(matches!(err, ApiError::InvalidPath(_)), "{id:?}: {err:?}");
        let err = c.revoke_cluster_credential("c-1", id).await.unwrap_err();
        assert!(matches!(err, ApiError::InvalidPath(_)), "{id:?}: {err:?}");
    }
    assert_eq!(request_count(&server).await, 2);
}

#[tokio::test]
async fn list_clusters_sends_filters() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/v1/clusters"))
        .and(query_param("name", "prod"))
        .and(query_param("single_tenant", "true"))
        .and(query_param("limit", "10"))
        .and(query_param("cursor", "abc"))
        .and(query_param_is_missing("region"))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(json!({"data": [], "next_cursor": null})),
        )
        .expect(1)
        .mount(&server)
        .await;

    let q = ClusterQuery {
        name: Some("prod".into()),
        single_tenant: Some(true),
        limit: Some(10),
        ..Default::default()
    };
    let res = client(&server)
        .list_clusters(&q, Some("abc"))
        .await
        .unwrap();
    assert!(res.value.data.is_empty());
}

#[tokio::test]
async fn delete_cluster_confirms_name_with_key() {
    let server = MockServer::start().await;
    Mock::given(method("DELETE"))
        .and(path("/v1/clusters/c-1"))
        .and(query_param("name", "prod"))
        .and(header("idempotency-key", "key-1"))
        .respond_with(ResponseTemplate::new(202).set_body_json(action_json()))
        .expect(1)
        .mount(&server)
        .await;

    client(&server)
        .delete_cluster("c-1", "prod", "key-1")
        .await
        .unwrap();
}

#[tokio::test]
async fn revoke_credential_accepts_no_content() {
    let server = MockServer::start().await;
    Mock::given(method("DELETE"))
        .and(path("/v1/clusters/c-1/credentials/cred_1"))
        .respond_with(ResponseTemplate::new(204))
        .expect(1)
        .mount(&server)
        .await;

    let res = client(&server)
        .revoke_cluster_credential("c-1", "cred_1")
        .await
        .unwrap();
    assert_eq!(res.status, 204);
}

#[tokio::test]
async fn collect_pages_follows_cursor() {
    let server = MockServer::start().await;
    let page = |range: std::ops::Range<usize>, next: Option<&str>| {
        json!({
            "data": range.map(|i| cluster_json(&format!("c-{i}"))).collect::<Vec<_>>(),
            "next_cursor": next,
        })
    };
    Mock::given(method("GET"))
        .and(path("/v1/clusters"))
        .and(query_param_is_missing("cursor"))
        .respond_with(ResponseTemplate::new(200).set_body_json(page(0..25, Some("p2"))))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/v1/clusters"))
        .and(query_param("cursor", "p2"))
        .respond_with(ResponseTemplate::new(200).set_body_json(page(25..50, Some("p3"))))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/v1/clusters"))
        .and(query_param("cursor", "p3"))
        .respond_with(ResponseTemplate::new(200).set_body_json(page(50..60, None)))
        .mount(&server)
        .await;

    let c = client(&server);
    let q = ClusterQuery::default();
    let (items, raw) = collect_pages(None, |cursor| {
        let (c, q) = (&c, &q);
        async move { c.list_clusters(q, cursor.as_deref()).await }
    })
    .await
    .unwrap();
    assert_eq!(items.len(), 60);
    assert_eq!(raw.len(), 60);
    assert_eq!(items[59].id, "c-59");
    assert_eq!(raw[59], cluster_json("c-59"));
    assert_eq!(request_count(&server).await, 3);

    server.reset().await;
    Mock::given(method("GET"))
        .and(path("/v1/clusters"))
        .and(query_param_is_missing("cursor"))
        .respond_with(ResponseTemplate::new(200).set_body_json(page(0..25, Some("p2"))))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/v1/clusters"))
        .and(query_param("cursor", "p2"))
        .respond_with(ResponseTemplate::new(200).set_body_json(page(25..50, Some("p3"))))
        .mount(&server)
        .await;

    let (items, raw) = collect_pages(Some(30), |cursor| {
        let (c, q) = (&c, &q);
        async move { c.list_clusters(q, cursor.as_deref()).await }
    })
    .await
    .unwrap();
    assert_eq!(items.len(), 30);
    assert_eq!(raw.len(), 30);
    assert_eq!(items[29].id, "c-29");
    assert_eq!(request_count(&server).await, 2);
}

#[tokio::test]
async fn collect_pages_refuses_a_repeated_cursor() {
    let server = MockServer::start().await;
    let page = |next: &str| json!({"data": [cluster_json("c-1")], "next_cursor": next});
    Mock::given(method("GET"))
        .and(path("/v1/clusters"))
        .and(query_param_is_missing("cursor"))
        .respond_with(ResponseTemplate::new(200).set_body_json(page("p2")))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/v1/clusters"))
        .and(query_param("cursor", "p2"))
        .respond_with(ResponseTemplate::new(200).set_body_json(page("p2")))
        .mount(&server)
        .await;

    let c = client(&server);
    let q = ClusterQuery::default();
    let err = collect_pages(None, |cursor| {
        let (c, q) = (&c, &q);
        async move { c.list_clusters(q, cursor.as_deref()).await }
    })
    .await
    .unwrap_err();
    assert!(matches!(err, ApiError::Decode(_)), "{err:?}");
    assert_eq!(request_count(&server).await, 2);
}

#[tokio::test]
async fn collect_pages_refuses_a_cursor_cycle() {
    // A -> B -> A would fetch the same two pages forever.
    let server = MockServer::start().await;
    let page = |next: &str| json!({"data": [cluster_json("c-1")], "next_cursor": next});
    Mock::given(method("GET"))
        .and(path("/v1/clusters"))
        .and(query_param_is_missing("cursor"))
        .respond_with(ResponseTemplate::new(200).set_body_json(page("a")))
        .mount(&server)
        .await;
    for (sent, next) in [("a", "b"), ("b", "a")] {
        Mock::given(method("GET"))
            .and(path("/v1/clusters"))
            .and(query_param("cursor", sent))
            .respond_with(ResponseTemplate::new(200).set_body_json(page(next)))
            .mount(&server)
            .await;
    }

    let c = client(&server);
    let q = ClusterQuery::default();
    let err = collect_pages(None, |cursor| {
        let (c, q) = (&c, &q);
        async move { c.list_clusters(q, cursor.as_deref()).await }
    })
    .await
    .unwrap_err();
    assert!(matches!(err, ApiError::Decode(_)), "{err:?}");
    assert_eq!(request_count(&server).await, 3);
}

#[tokio::test]
async fn proposed_endpoints_404_is_none() {
    let server = MockServer::start().await;
    Mock::given(path("/v1/token"))
        .respond_with(problem_response(404, "not_found"))
        .mount(&server)
        .await;

    let c = client(&server);
    assert!(c.get_token().await.unwrap().is_none());
    assert!(c.revoke_token().await.unwrap().is_none());
}

#[tokio::test]
async fn token_endpoints_when_present() {
    let server = MockServer::start().await;
    let info = json!({
        "name": "Bonsai CLI on laptop", "scopes": ["account:read", "clusters:read"],
        "expires_at": "2027-01-01T00:00:00Z", "key_prefix": "abcd1234"
    });
    Mock::given(method("GET"))
        .and(path("/v1/token"))
        .respond_with(ResponseTemplate::new(200).set_body_json(info))
        .mount(&server)
        .await;
    Mock::given(method("DELETE"))
        .and(path("/v1/token"))
        .respond_with(ResponseTemplate::new(204))
        .mount(&server)
        .await;

    let c = client(&server);
    let info = c.get_token().await.unwrap().unwrap().value;
    assert_eq!(info.key_prefix, "abcd1234");
    assert_eq!(info.scopes, vec!["account:read", "clusters:read"]);
    assert_eq!(c.revoke_token().await.unwrap(), Some(()));
}

#[tokio::test]
async fn cli_login_flow() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/cli/logins"))
        .and(body_json(
            json!({"client_name": "bonsai-cli", "hostname": "laptop", "os": "linux"}),
        ))
        .respond_with(ResponseTemplate::new(201).set_body_json(json!({
            "device_code": "dev_1", "user_code": "WDJB-MJHT",
            "verification_uri": "https://app.bonsai.io/cli",
            "verification_uri_complete": "https://app.bonsai.io/cli?code=WDJB-MJHT",
            "expires_in": 600, "interval": 5
        })))
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/v1/cli/logins/token"))
        .and(body_json(json!({"device_code": "dev_1"})))
        .respond_with(problem_response(400, "authorization_pending"))
        .up_to_n_times(1)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/v1/cli/logins/token"))
        .and(body_json(json!({"device_code": "dev_1"})))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "token": "bonsai_key_newsecret", "account": {"id": "acc_1", "name": "Acme"},
            "scopes": ["account:read"], "expires_at": null
        })))
        .mount(&server)
        .await;

    let c = Client::new(ClientConfig {
        token: None,
        ..config(&format!("{}/v1", server.uri()))
    })
    .unwrap();
    let start = CliLoginStart {
        client_name: "bonsai-cli".into(),
        hostname: "laptop".into(),
        os: "linux".into(),
    };
    let grant = c.start_cli_login(&start).await.unwrap().value;
    assert_eq!(grant.user_code, "WDJB-MJHT");
    assert_eq!(grant.interval, 5);

    let err = c.poll_cli_login(&grant.device_code).await.unwrap_err();
    assert_eq!(
        err.code().map(|c| c.as_str()),
        Some("authorization_pending")
    );
    assert_eq!(err.status(), Some(400));

    let res = c.poll_cli_login(&grant.device_code).await.unwrap();
    assert_eq!(
        secrecy::ExposeSecret::expose_secret(&res.value.token),
        "bonsai_key_newsecret"
    );
    assert_eq!(res.value.account.name, "Acme");
    let debug = format!("{res:?}");
    assert!(!debug.contains("newsecret"), "{debug}");

    let requests = server.received_requests().await.unwrap();
    assert!(
        requests
            .iter()
            .all(|r| r.headers.get("authorization").is_none())
    );
}

#[tokio::test]
async fn send_raw_with_headers_adds_them_and_keeps_the_token() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/v1/clusters"))
        .and(header("x-trace", "abc"))
        .and(header("accept", "application/problem+json"))
        .and(header("authorization", "Bearer bonsai_key_secret"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({})))
        .expect(1)
        .mount(&server)
        .await;

    let mut headers = reqwest::header::HeaderMap::new();
    headers.insert("x-trace", "abc".parse().unwrap());
    headers.insert("accept", "application/problem+json".parse().unwrap());
    client(&server)
        .send_raw_with_headers(reqwest::Method::GET, "/clusters", None, None, &headers)
        .await
        .unwrap();
}

#[tokio::test]
async fn send_raw_with_headers_refuses_authorization_and_idempotency_key() {
    let server = MockServer::start().await;
    let c = client(&server);
    for name in ["authorization", "idempotency-key"] {
        let mut headers = reqwest::header::HeaderMap::new();
        headers.insert(name, "x".parse().unwrap());
        let err = c
            .send_raw_with_headers(reqwest::Method::GET, "/clusters", None, None, &headers)
            .await
            .unwrap_err();
        assert!(matches!(err, ApiError::Config(_)), "{name}: {err:?}");
    }
    assert_eq!(request_count(&server).await, 0);
}

#[tokio::test]
async fn a_problem_keeps_the_body_as_it_came() {
    let server = MockServer::start().await;
    let body = json!({
        "type": "about:blank", "title": "Not Found", "status": 404, "code": "not_found",
        "detail": "No such cluster.", "details": [], "from_the_future": {"x": 1}
    });
    Mock::given(method("GET"))
        .respond_with(
            ResponseTemplate::new(404).set_body_raw(body.to_string(), "application/problem+json"),
        )
        .mount(&server)
        .await;
    match client(&server).get_account().await.unwrap_err() {
        ApiError::Problem { raw, problem, .. } => {
            assert_eq!(*raw, body);
            assert_eq!(problem.detail, "No such cluster.");
        }
        other => panic!("expected a Problem, got {other:?}"),
    }
}

// ---------------------------------------------------------------- edge cases

#[test]
fn refuses_userinfo_in_the_base_url() {
    // reqwest would turn it into a second, Basic Authorization header.
    for url in [
        "http://u:pw@127.0.0.1:4000/v1",
        "https://user@api.example/v1",
    ] {
        let err = Client::new(config(url)).expect_err("refused");
        assert!(matches!(err, ApiError::Config(_)), "{url}: {err:?}");
        assert!(!err.to_string().contains("pw"), "{err}");
    }
}

#[tokio::test]
async fn accepts_problem_json_and_detects_it_in_any_case() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/v1/account"))
        .and(header_regex("accept", r"application/problem\+json"))
        .respond_with(ResponseTemplate::new(404).set_body_raw(
            problem(404, "not_found").to_string(),
            "Application/Problem+JSON",
        ))
        .mount(&server)
        .await;
    let err = client(&server).get_account().await.unwrap_err();
    assert_eq!(err.code(), Some(&ProblemCode::NotFound), "{err:?}");
}

#[tokio::test]
async fn caps_an_error_body() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/v1/account"))
        .respond_with(ResponseTemplate::new(500).set_body_string("x".repeat(3 << 20)))
        .mount(&server)
        .await;
    match client(&server).get_account().await.unwrap_err() {
        ApiError::Http { status, body } => {
            assert_eq!(status, 500);
            assert!(body.len() <= 1 << 20, "{} bytes kept", body.len());
        }
        other => panic!("expected Http, got {other:?}"),
    }
}

#[tokio::test]
async fn send_raw_takes_a_path_that_repeats_v1() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/v1/plans"))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(json!({"data": [], "next_cursor": null})),
        )
        .mount(&server)
        .await;
    let c = client(&server);
    for p in ["/plans", "/v1/plans", "v1/plans"] {
        c.send_raw(reqwest::Method::GET, p, None, None)
            .await
            .unwrap_or_else(|e| panic!("{p}: {e:?}"));
    }
    assert_eq!(request_count(&server).await, 3);
}

#[tokio::test]
async fn a_cli_login_grant_hides_its_device_code() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/cli/logins"))
        .respond_with(ResponseTemplate::new(201).set_body_json(json!({
            "device_code": "dev_secret_1", "user_code": "WDJB-MJHT",
            "verification_uri": "https://app.bonsai.io/cli",
            "verification_uri_complete": "https://app.bonsai.io/cli?code=WDJB-MJHT",
            "expires_in": 600, "interval": 5
        })))
        .mount(&server)
        .await;
    let start = CliLoginStart {
        client_name: "bonsai-cli".into(),
        hostname: "laptop".into(),
        os: "linux".into(),
    };
    let res = client(&server).start_cli_login(&start).await.unwrap();
    let debug = format!("{res:?}");
    assert!(!debug.contains("dev_secret_1"), "{debug}");
    assert_eq!(res.raw["device_code"], "<redacted>");
    // Still usable for the poll.
    assert_eq!(res.value.device_code, "dev_secret_1");
}

/// A server that reads each request and hangs up without answering on the first
/// `drops` connections, then answers `body` with 200. Returns its address and the
/// number of connections it took.
async fn hang_up_server(drops: usize, body: Value) -> (std::net::SocketAddr, Arc<AtomicUsize>) {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let served = Arc::new(AtomicUsize::new(0));
    let count = served.clone();
    tokio::spawn(async move {
        loop {
            let (mut sock, _) = listener.accept().await.unwrap();
            let n = count.fetch_add(1, Ordering::SeqCst);
            let mut buf = [0u8; 4096];
            let _ = sock.read(&mut buf).await;
            if n < drops {
                drop(sock);
                continue;
            }
            let body = body.to_string();
            let resp = format!(
                "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                body.len()
            );
            let _ = sock.write_all(resp.as_bytes()).await;
        }
    });
    (addr, served)
}

/// An unkeyed write whose connection drops after the request was sent may have
/// been applied, so it isn't sent again; a GET to the same server is (see
/// `a_get_is_retried_after_a_dropped_connection`). A timeout isn't tested here: a
/// request only times out at the overall deadline, when no retry is left anyway.
#[tokio::test]
async fn an_unkeyed_write_is_not_retried_after_a_dropped_connection() {
    let (addr, served) = hang_up_server(1, json!({})).await;
    let c = Client::new(config(&format!("http://{addr}/v1"))).unwrap();
    let start = CliLoginStart {
        client_name: "bonsai-cli".into(),
        hostname: "laptop".into(),
        os: "linux".into(),
    };
    assert!(c.start_cli_login(&start).await.is_err());
    assert_eq!(served.load(Ordering::SeqCst), 1, "sent again");
}

#[tokio::test]
async fn server_errors_other_than_502_and_503_are_not_retried() {
    for status in [500, 504] {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/v1/account"))
            .respond_with(ResponseTemplate::new(status))
            .mount(&server)
            .await;
        let err = client(&server).get_account().await.unwrap_err();
        assert_eq!(err.status(), Some(status));
        assert_eq!(request_count(&server).await, 1, "{status}");
    }
}

#[tokio::test]
async fn get_token_passes_a_401_through() {
    let server = MockServer::start().await;
    Mock::given(path("/v1/token"))
        .respond_with(problem_response(401, "unauthorized"))
        .mount(&server)
        .await;
    let err = client(&server).get_token().await.unwrap_err();
    assert_eq!(err.status(), Some(401));
}

/// A `GET` whose connection is dropped after the request was sent is retried: a
/// read can be repeated.
#[tokio::test]
async fn a_get_is_retried_after_a_dropped_connection() {
    let (addr, served) = hang_up_server(1, account_json()).await;
    let c = Client::new(config(&format!("http://{addr}/v1"))).unwrap();
    let account = c.get_account().await.unwrap();
    assert_eq!(account.value.name, "Acme");
    assert_eq!(served.load(Ordering::SeqCst), 2);
}

/// An IP locked out for failed logins stays locked out for the window (60 s on
/// the Bonsai API): retrying only waits minutes for the same answer.
#[tokio::test]
async fn does_not_retry_too_many_failed_authentications() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/v1/account"))
        .respond_with(
            problem_response(429, "too_many_failed_authentications")
                .insert_header("Retry-After", "60"),
        )
        .mount(&server)
        .await;
    let started = Instant::now();
    let err = client(&server).get_account().await.unwrap_err();
    assert!(
        started.elapsed() < Duration::from_secs(1),
        "{:?}",
        started.elapsed()
    );
    assert_eq!(request_count(&server).await, 1);
    match err {
        ApiError::Problem {
            status: 429,
            problem,
            retry_after,
            ..
        } => {
            assert_eq!(problem.code, ProblemCode::TooManyFailedAuthentications);
            assert_eq!(retry_after, Some(Duration::from_secs(60)));
        }
        other => panic!("expected a Problem, got {other:?}"),
    }
}
