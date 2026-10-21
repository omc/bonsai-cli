//! The flows end to end against the stateful mock (`bonsai-mock`), and against a
//! wiremock fake where the mock can't produce the case (an unknown Action state,
//! RFC 8628's `slow_down` and `expired_token` on demand).

use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use bonsai_api::{
    AccessType, Action, ActionErrorCode, ActionState, ActionType, ApiError, CliLoginStart, Client,
    ClientConfig, Cluster, ClusterCreate, ClusterQuery, ClusterState, CredentialCreate,
    ProblemCode, Response, RetryPolicy, new_idempotency_key,
};
use bonsai_core::{
    ActionFailure, CleanupOutcome, CleanupPolicy, CoreError, CreateRequest, Event, EventSink,
    StopReason, WaitOptions, connection_url, create_cluster, destroy_cluster, device_login,
    resize_cluster, resolve_cluster, wait_for_action,
};
use bonsai_mock::{MockHandle, MockOptions};
use secrecy::{ExposeSecret, SecretString};
use serde_json::{Value, json};
use tokio::sync::mpsc::{UnboundedReceiver, unbounded_channel};
use tokio_util::sync::CancellationToken;
use url::Url;
use wiremock::matchers::{method, path, query_param};
use wiremock::{Mock, MockServer, ResponseTemplate};

const PROVISION_STEPS: [&str; 4] = [
    "Reserving capacity",
    "Provisioning cluster",
    "Configuring routes",
    "Running health check",
];

struct Env {
    h: MockHandle,
    acme: String,
    db_path: PathBuf,
    http: reqwest::Client,
    _dir: tempfile::TempDir,
}

impl Env {
    async fn start(step_ms: u64) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("mock.sqlite");
        let addr: SocketAddr = "127.0.0.1:0".parse().unwrap();
        let h = bonsai_mock::spawn(
            MockOptions {
                db_path: db_path.clone(),
                step_ms,
                public_url: None,
                checkout_url: None,
            },
            addr,
        )
        .await
        .unwrap();
        let acme = h
            .seeded_tokens
            .iter()
            .find(|t| t.account_id == "acme")
            .unwrap()
            .token
            .clone();
        let http = bonsai_api::http_client().no_proxy().build().unwrap();
        Self {
            h,
            acme,
            db_path,
            http,
            _dir: dir,
        }
    }

    fn acme(&self) -> Client {
        client(&self.h.base_url, Some(&self.acme))
    }

    fn anonymous(&self) -> Client {
        client(&self.h.base_url, None)
    }

    /// A token on `globex` (no card) that may write clusters; the seeded one can't.
    fn globex_writer(&self) -> Client {
        let conn = bonsai_mock::db::open(&self.db_path).unwrap();
        let token = bonsai_mock::seed::create_token(
            &conn,
            "globex",
            "flows test",
            bonsai_mock::seed::ALL_SCOPES,
            None,
        )
        .unwrap();
        client(&self.h.base_url, Some(&token))
    }

    async fn admin(&self, method: reqwest::Method, path: &str, body: Value) {
        let url = self.h.origin.join(path).unwrap();
        let res = self
            .http
            .request(method, url)
            .bearer_auth(&self.h.admin_token)
            .json(&body)
            .send()
            .await
            .unwrap();
        assert_eq!(res.status(), 204, "{path}: {}", res.text().await.unwrap());
    }

    async fn fail_next(&self, how: &str) {
        self.admin(
            reqwest::Method::PUT,
            "/_mock/scenario/fail_next",
            json!(how),
        )
        .await;
    }
}

fn client(base_url: &Url, token: Option<&str>) -> Client {
    Client::new(ClientConfig {
        base_url: base_url.clone(),
        token: token.map(|t| SecretString::from(t.to_owned())),
        user_agent: "bonsai-core-tests".into(),
        retry: RetryPolicy {
            max_retries: 2,
            max_wait: Duration::from_secs(1),
            deadline: Duration::from_secs(10),
            base: Duration::from_millis(20),
        },
        ca_bundle: None,
    })
    .unwrap()
}

fn opts() -> WaitOptions {
    WaitOptions {
        timeout: Duration::from_secs(30),
        cancel: CancellationToken::new(),
        default_poll: Duration::from_millis(50),
    }
}

fn sink() -> (EventSink, UnboundedReceiver<Event>) {
    unbounded_channel()
}

fn drain(rx: &mut UnboundedReceiver<Event>) -> Vec<Event> {
    let mut out = Vec::new();
    while let Ok(e) = rx.try_recv() {
        out.push(e);
    }
    out
}

fn steps(events: &[Event]) -> Vec<String> {
    events
        .iter()
        .filter_map(|e| match e {
            Event::StepCompleted { message, .. } => Some(message.clone()),
            _ => None,
        })
        .collect()
}

fn failure(err: CoreError) -> ActionFailure {
    match err {
        CoreError::ActionFailed(f) => *f,
        other => panic!("expected ActionFailed, got {other:?}"),
    }
}

fn body(name: &str, plan: &str) -> ClusterCreate {
    ClusterCreate {
        name: name.into(),
        plan: plan.into(),
        region: "aws-us-east-1".into(),
        space: None,
        release: None,
    }
}

fn request(name: &str, wait: bool, cleanup: CleanupPolicy) -> CreateRequest {
    CreateRequest {
        body: body(name, "standard-fir"),
        wait,
        cleanup,
        idempotency_key: None,
    }
}

async fn clusters(api: &Client) -> Vec<Cluster> {
    api.list_clusters(&ClusterQuery::default(), None)
        .await
        .unwrap()
        .value
        .data
}

/// Creates a cluster and waits for it to be ready.
async fn ready_cluster(api: &Client, name: &str) -> String {
    let (ev, _rx) = sink();
    let done = create_cluster(api, request(name, true, CleanupPolicy::Keep), &opts(), &ev)
        .await
        .unwrap();
    done.value.subject.id
}

// ---------------------------------------------------------------- resolve

#[tokio::test]
async fn resolve_by_id_name_ambiguous_missing() {
    let env = Env::start(0).await;
    let api = env.acme();
    let (ev, _rx) = sink();
    let mut dup_ids = Vec::new();
    for _ in 0..2 {
        let a = create_cluster(
            &api,
            request("dup", false, CleanupPolicy::Keep),
            &opts(),
            &ev,
        )
        .await
        .unwrap();
        dup_ids.push(a.value.subject.id);
    }
    let solo = create_cluster(
        &api,
        request("solo", false, CleanupPolicy::Keep),
        &opts(),
        &ev,
    )
    .await
    .unwrap()
    .value
    .subject
    .id;

    let by_name = resolve_cluster(&api, "solo").await.unwrap();
    assert_eq!(by_name.value.id, solo);
    assert_eq!(by_name.raw["id"], json!(solo));
    let by_id = resolve_cluster(&api, &solo).await.unwrap();
    assert_eq!(by_id.value.name, "solo");

    match resolve_cluster(&api, "DUP").await {
        Err(CoreError::Ambiguous { name, mut ids }) => {
            assert_eq!(name, "DUP");
            ids.sort();
            dup_ids.sort();
            assert_eq!(ids, dup_ids);
        }
        other => panic!("expected Ambiguous, got {other:?}"),
    }
    match resolve_cluster(&api, "nope").await {
        Err(CoreError::ClusterNotFound(name)) => assert_eq!(name, "nope"),
        other => panic!("expected ClusterNotFound, got {other:?}"),
    }
}

// ---------------------------------------------------------------- create

#[tokio::test]
async fn create_waits_to_ready_and_emits_steps() {
    let env = Env::start(0).await;
    let api = env.acme();
    let (ev, mut rx) = sink();
    let done = create_cluster(
        &api,
        request("search-prod", true, CleanupPolicy::Remove),
        &opts(),
        &ev,
    )
    .await
    .unwrap();
    assert_eq!(done.value.state, ActionState::Success);
    assert_eq!(done.raw["state"], json!("success"));

    let events = drain(&mut rx);
    let started: Vec<_> = events
        .iter()
        .filter(|e| matches!(e, Event::ActionStarted { .. }))
        .collect();
    assert_eq!(started.len(), 1, "{events:?}");
    assert!(
        matches!(started[0], Event::ActionStarted { action_id, kind: ActionType::Provision, subject }
            if *action_id == done.value.id && *subject == done.value.subject.id),
        "{events:?}"
    );
    assert_eq!(steps(&events), PROVISION_STEPS, "{events:?}");

    let cluster = api.get_cluster(&done.value.subject.id).await.unwrap();
    assert_eq!(cluster.value.state, ClusterState::Ready);
}

#[tokio::test]
async fn create_without_wait_returns_the_started_action() {
    let env = Env::start(60_000).await;
    let api = env.acme();
    let (ev, mut rx) = sink();
    let started = create_cluster(
        &api,
        request("later", false, CleanupPolicy::Remove),
        &opts(),
        &ev,
    )
    .await
    .unwrap();
    assert_eq!(started.status, 202);
    assert!(!started.value.state.is_terminal());
    let events = drain(&mut rx);
    assert!(
        matches!(events.as_slice(), [Event::ActionStarted { .. }]),
        "{events:?}"
    );
}

#[tokio::test]
async fn create_payment_failed_is_refused_nothing_created() {
    let env = Env::start(0).await;
    let api = env.acme();
    env.fail_next("payment_failed").await;
    let (ev, mut rx) = sink();
    let err = create_cluster(
        &api,
        request("unpaid", true, CleanupPolicy::Remove),
        &opts(),
        &ev,
    )
    .await
    .unwrap_err();
    match &err {
        CoreError::Api(ApiError::Problem {
            status, problem, ..
        }) => {
            assert_eq!(*status, 422);
            assert_eq!(problem.code, ProblemCode::PaymentFailed);
            assert!(problem.action_url.is_some(), "{problem:?}");
        }
        other => panic!("expected a payment_failed problem, got {other:?}"),
    }
    assert!(clusters(&api).await.is_empty());
    assert!(drain(&mut rx).is_empty());
}

#[tokio::test]
async fn create_payment_method_required_creates_nothing() {
    let env = Env::start(0).await;
    let api = env.globex_writer();
    let (ev, mut rx) = sink();
    let err = create_cluster(
        &api,
        request("no-card", true, CleanupPolicy::Remove),
        &opts(),
        &ev,
    )
    .await
    .unwrap_err();
    match &err {
        CoreError::Api(ApiError::Problem {
            status, problem, ..
        }) => {
            assert_eq!(*status, 422);
            assert_eq!(problem.code, ProblemCode::PaymentMethodRequired);
            assert!(problem.action_url.is_some(), "{problem:?}");
        }
        other => panic!("expected payment_method_required, got {other:?}"),
    }
    assert!(clusters(&api).await.is_empty());
    assert!(drain(&mut rx).is_empty());
}

/// The `DELETE`s a fake received.
async fn deletes(server: &MockServer) -> Vec<wiremock::Request> {
    server
        .received_requests()
        .await
        .unwrap()
        .into_iter()
        .filter(|r| r.method.as_str() == "DELETE")
        .collect()
}

/// The API terminates a failed create itself: the CLI sees the cluster
/// `deprovisioned`, reports it removed by Bonsai, and sends no `DELETE`, whatever
/// the policy, without asking.
#[tokio::test]
async fn bonsai_removes_a_failed_create_itself() {
    let env = Env::start(0).await;
    let api = env.acme();
    let asked = Arc::new(Mutex::new(0));
    for (fail, code, policy) in [
        ("action_failed", ActionErrorCode::ActionFailed, "remove"),
        ("action_failed", ActionErrorCode::ActionFailed, "keep"),
        ("timed_out", ActionErrorCode::TimedOut, "ask"),
    ] {
        env.fail_next(fail).await;
        let seen = asked.clone();
        let cleanup = match policy {
            "remove" => CleanupPolicy::Remove,
            "keep" => CleanupPolicy::Keep,
            _ => CleanupPolicy::Ask(Box::new(move |_| {
                *seen.lock().unwrap() += 1;
                true
            })),
        };
        let (ev, mut rx) = sink();
        let err = create_cluster(&api, request("doomed", true, cleanup), &opts(), &ev)
            .await
            .unwrap_err();
        let f = failure(err);
        assert_eq!(f.code, code);
        assert_eq!(f.action_url, None);
        let cleanup = f.cleanup.expect("a cleanup");
        assert_eq!(cleanup.outcome, CleanupOutcome::RemovedByBonsai, "{policy}");
        assert_eq!(f.cluster_id.as_deref(), Some(cleanup.cluster_id.as_str()));
        let cluster = api.get_cluster(&cleanup.cluster_id).await.unwrap();
        assert_eq!(cluster.value.state, ClusterState::Deprovisioned);

        let events = drain(&mut rx);
        assert!(
            !events.iter().any(|e| matches!(
                e,
                Event::CleanupStarted { .. } | Event::CleanupFinished { .. }
            )),
            "{events:?}"
        );
        let kinds: Vec<_> = events
            .iter()
            .filter_map(|e| match e {
                Event::ActionStarted { kind, .. } => Some(kind.clone()),
                _ => None,
            })
            .collect();
        assert_eq!(kinds, [ActionType::Provision], "no deprovision: {policy}");
    }
    assert_eq!(*asked.lock().unwrap(), 0, "no one is asked");
    assert!(clusters(&api).await.is_empty());
}

// ---------------------------------------------------------------- wait

#[tokio::test]
async fn wait_timeout() {
    let env = Env::start(60_000).await;
    let api = env.acme();
    let (ev, _rx) = sink();
    let started = create_cluster(
        &api,
        request("slow", false, CleanupPolicy::Keep),
        &opts(),
        &ev,
    )
    .await
    .unwrap();
    let id = started.value.id.clone();
    let opts = WaitOptions {
        timeout: Duration::from_secs(1),
        ..opts()
    };
    let t0 = Instant::now();
    let err = wait_for_action(&api, started, &opts, &ev)
        .await
        .unwrap_err();
    assert!(t0.elapsed() < Duration::from_secs(5), "{:?}", t0.elapsed());
    match err {
        CoreError::Timeout { action_id, timeout } => {
            assert_eq!(action_id, id);
            assert_eq!(timeout, Duration::from_secs(1));
        }
        other => panic!("expected Timeout, got {other:?}"),
    }
}

#[tokio::test]
async fn wait_cancel() {
    let env = Env::start(60_000).await;
    let api = env.acme();
    let (ev, _rx) = sink();
    let started = create_cluster(
        &api,
        request("slow", false, CleanupPolicy::Keep),
        &opts(),
        &ev,
    )
    .await
    .unwrap();
    let id = started.value.id.clone();
    let opts = opts();
    let cancel = opts.cancel.clone();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(100)).await;
        cancel.cancel();
    });
    let t0 = Instant::now();
    let err = wait_for_action(&api, started, &opts, &ev)
        .await
        .unwrap_err();
    assert!(t0.elapsed() < Duration::from_secs(2), "{:?}", t0.elapsed());
    match err {
        CoreError::Cancelled { action_id } => assert_eq!(action_id, id),
        other => panic!("expected Cancelled, got {other:?}"),
    }
}

fn cluster_json(id: &str, name: &str) -> Value {
    json!({
        "id": id,
        "name": name,
        "state": "ready",
        "plan": {"id": "standard-fir", "name": "Standard Fir"},
        "region": {"id": "aws-us-east-1", "name": "AWS US East (Virginia)"},
        "space": null,
        "release": {"id": "opensearch-3.8.0", "name": "OpenSearch 3.8.0"},
        "single_tenant": false,
        "host": null,
        "port": 443,
        "stats": {"docs": null, "shards": null, "bytes": null, "polled_at": null, "limits": null},
        "latest_action": null,
        "created_at": "2026-10-20T00:00:00Z",
        "terminated_at": null,
    })
}

fn fake_api(server: &MockServer) -> Client {
    client(&Url::parse(&format!("{}/v1", server.uri())).unwrap(), None)
}

fn action_json(state: &str, steps: Value) -> Value {
    json!({
        "id": "act_fake",
        "type": "cluster.provision",
        "state": state,
        "subject": {"type": "cluster", "id": "fake-cluster"},
        "created_at": "2026-10-20T00:00:00Z",
        "finished_at": null,
        "error": null,
        "result": null,
        "steps": steps,
    })
}

fn response(raw: Value) -> Response<Action> {
    Response {
        value: serde_json::from_value(raw.clone()).unwrap(),
        raw,
        status: 200,
        retry_after: None,
        location: None,
        idempotent_replayed: false,
        date: None,
    }
}

#[tokio::test]
async fn wait_unknown_state_keeps_polling() {
    let server = MockServer::start().await;
    let step = |m: &str, s: &str| json!({"state": s, "message": m, "at": "2026-10-20T00:00:01Z"});
    Mock::given(method("GET"))
        .and(path("/v1/actions/act_fake"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(action_json("paused", json!([step("one", "running")]))),
        )
        .up_to_n_times(1)
        .with_priority(1)
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/v1/actions/act_fake"))
        .respond_with(ResponseTemplate::new(200).set_body_json(action_json(
            "success",
            json!([step("one", "running"), step("two", "success")]),
        )))
        .with_priority(2)
        .mount(&server)
        .await;
    let api = client(&Url::parse(&format!("{}/v1", server.uri())).unwrap(), None);
    let (ev, mut rx) = sink();
    let initial = response(action_json("paused", json!([])));
    let done = wait_for_action(&api, initial, &opts(), &ev).await.unwrap();
    assert_eq!(done.value.state, ActionState::Success);
    assert_eq!(server.received_requests().await.unwrap().len(), 2);
    assert_eq!(steps(&drain(&mut rx)), ["one", "two"]);
}

// ---------------------------------------------------------------- destroy and resize

#[tokio::test]
async fn destroy_name_mismatch_is_refused_by_the_api() {
    let env = Env::start(0).await;
    let api = env.acme();
    let id = ready_cluster(&api, "search-prod").await;
    let cluster = api.get_cluster(&id).await.unwrap().value;
    let (ev, mut rx) = sink();
    let err = destroy_cluster(&api, &cluster, "search-dev", None, true, &opts(), &ev)
        .await
        .unwrap_err();
    match &err {
        CoreError::Api(e) => assert_eq!(e.code(), Some(&ProblemCode::NameMismatch), "{e:?}"),
        other => panic!("expected name_mismatch, got {other:?}"),
    }
    assert_eq!(
        api.get_cluster(&id).await.unwrap().value.state,
        ClusterState::Ready
    );
    assert!(drain(&mut rx).is_empty());
}

#[tokio::test]
async fn destroy_waits_to_deprovisioned() {
    let env = Env::start(0).await;
    let api = env.acme();
    let id = ready_cluster(&api, "goner").await;
    let cluster = api.get_cluster(&id).await.unwrap().value;
    let (ev, mut rx) = sink();
    let done = destroy_cluster(&api, &cluster, "goner", None, true, &opts(), &ev)
        .await
        .unwrap();
    assert_eq!(done.value.r#type, ActionType::Deprovision);
    assert_eq!(done.value.state, ActionState::Success);
    assert_eq!(
        api.get_cluster(&id).await.unwrap().value.state,
        ClusterState::Deprovisioned
    );
    assert_eq!(steps(&drain(&mut rx)).len(), 3);
}

#[tokio::test]
async fn resize_waits_for_the_plan_change() {
    let env = Env::start(0).await;
    let api = env.acme();
    let id = ready_cluster(&api, "growing").await;
    let (ev, _rx) = sink();
    let done = resize_cluster(&api, &id, "standard-cork", None, true, &opts(), &ev)
        .await
        .unwrap();
    assert_eq!(done.value.r#type, ActionType::ChangePlan);
    assert_eq!(done.value.state, ActionState::Success);
    let cluster = api.get_cluster(&id).await.unwrap().value;
    assert_eq!(cluster.plan.unwrap().id, "standard-cork");
}

// ---------------------------------------------------------------- caller keys

#[tokio::test]
async fn create_with_a_caller_key_is_replayed_on_a_rerun() {
    let env = Env::start(60_000).await;
    let api = env.acme();
    let (ev, _rx) = sink();
    let req = || CreateRequest {
        idempotency_key: Some("ci-run-42-create".into()),
        ..request("keyed", false, CleanupPolicy::Keep)
    };
    let first = create_cluster(&api, req(), &opts(), &ev).await.unwrap();
    let again = create_cluster(&api, req(), &opts(), &ev).await.unwrap();
    assert!(!first.idempotent_replayed);
    assert!(again.idempotent_replayed, "the same key is sent again");
    assert_eq!(again.value.id, first.value.id);
    assert_eq!(clusters(&api).await.len(), 1);
}

#[tokio::test]
async fn create_without_a_caller_key_makes_a_new_one_per_run() {
    let env = Env::start(60_000).await;
    let api = env.acme();
    let (ev, _rx) = sink();
    let req = || request("unkeyed", false, CleanupPolicy::Keep);
    let first = create_cluster(&api, req(), &opts(), &ev).await.unwrap();
    let again = create_cluster(&api, req(), &opts(), &ev).await.unwrap();
    assert!(!again.idempotent_replayed);
    assert_ne!(again.value.id, first.value.id);
    assert_eq!(clusters(&api).await.len(), 2);
}

#[tokio::test]
async fn a_replayed_create_never_removes_the_cluster() {
    let env = Env::start(0).await;
    let api = env.acme();
    env.fail_next("action_failed").await;
    let (ev, mut rx) = sink();
    let keyed = |cleanup| CreateRequest {
        idempotency_key: Some("ci-run-7-create".into()),
        ..request("replayed", true, cleanup)
    };
    let first = failure(
        create_cluster(&api, keyed(CleanupPolicy::Keep), &opts(), &ev)
            .await
            .unwrap_err(),
    );
    assert!(!first.replayed);
    let id = first.cluster_id.clone().unwrap();
    drain(&mut rx);

    // The rerun gets the first run's Action back: this run created nothing, so
    // it removes nothing, whatever the policy. Bonsai already removed it.
    let asked = Arc::new(Mutex::new(false));
    let seen = asked.clone();
    for cleanup in [
        CleanupPolicy::Remove,
        CleanupPolicy::Ask(Box::new(move |_| {
            *seen.lock().unwrap() = true;
            true
        })),
    ] {
        let again = failure(
            create_cluster(&api, keyed(cleanup), &opts(), &ev)
                .await
                .unwrap_err(),
        );
        assert!(again.replayed);
        assert_eq!(again.action_id, first.action_id);
        assert_eq!(again.cluster_id.as_deref(), Some(id.as_str()));
        assert_eq!(
            again.cleanup.map(|c| c.outcome),
            Some(CleanupOutcome::RemovedByBonsai)
        );
    }
    assert!(
        !*asked.lock().unwrap(),
        "no one is asked about a cluster this run didn't create"
    );
    let events = drain(&mut rx);
    assert!(
        !events
            .iter()
            .any(|e| matches!(e, Event::CleanupStarted { .. })),
        "{events:?}"
    );
    assert_eq!(
        api.get_cluster(&id).await.unwrap().value.state,
        ClusterState::Deprovisioned
    );
}

#[tokio::test]
async fn destroy_with_a_caller_key_is_replayed_on_a_rerun() {
    let env = Env::start(0).await;
    let api = env.acme();
    let id = ready_cluster(&api, "keyed-goner").await;
    let cluster = api.get_cluster(&id).await.unwrap().value;
    let (ev, _rx) = sink();
    let key = Some("ci-run-42-destroy");
    let first = destroy_cluster(&api, &cluster, "keyed-goner", key, false, &opts(), &ev)
        .await
        .unwrap();
    let again = destroy_cluster(&api, &cluster, "keyed-goner", key, false, &opts(), &ev)
        .await
        .unwrap();
    assert!(again.idempotent_replayed);
    assert_eq!(again.value.id, first.value.id);
}

#[tokio::test]
async fn resize_with_a_caller_key_is_replayed_on_a_rerun() {
    let env = Env::start(0).await;
    let api = env.acme();
    let id = ready_cluster(&api, "keyed-grower").await;
    let (ev, _rx) = sink();
    let key = Some("ci-run-42-resize");
    let first = resize_cluster(&api, &id, "standard-cork", key, true, &opts(), &ev)
        .await
        .unwrap();
    let again = resize_cluster(&api, &id, "standard-cork", key, true, &opts(), &ev)
        .await
        .unwrap();
    assert!(again.idempotent_replayed);
    assert_eq!(again.value.id, first.value.id);
}

// ---------------------------------------------------------------- url

#[tokio::test]
async fn connection_url_single_rw() {
    let env = Env::start(0).await;
    let api = env.acme();
    let id = ready_cluster(&api, "connect-me").await;

    let url = connection_url(&api, &id, None).await.unwrap();
    assert!(url.expose_secret().starts_with("https://"), "url");
    assert!(url.expose_secret().contains('@'));

    let second = api
        .create_cluster_credential(
            &id,
            &CredentialCreate {
                name: "second".into(),
                access_type: AccessType::Rw,
                allowed_actions: None,
                allowed_index_globs: None,
                allowed_cidrs: None,
            },
            &new_idempotency_key(),
        )
        .await
        .unwrap()
        .value;
    match connection_url(&api, &id, None).await {
        Err(CoreError::SeveralCredentials { cluster, ids }) => {
            assert_eq!(cluster, id);
            assert_eq!(ids.len(), 2);
            assert!(ids.contains(&second.credential.id));
        }
        Err(other) => panic!("expected SeveralCredentials, got {other:?}"),
        Ok(_) => panic!("expected SeveralCredentials, got a URL"),
    }
    let picked = connection_url(&api, &id, Some(&second.credential.id))
        .await
        .unwrap();
    assert_eq!(picked.expose_secret(), second.url);
}

// ---------------------------------------------------------------- login

fn login_start() -> CliLoginStart {
    CliLoginStart {
        client_name: "bonsai-cli".into(),
        hostname: "test-host".into(),
        os: "linux".into(),
    }
}

/// Starts `device_login` and hands the user code to `decide` once it's granted.
async fn login_with<F, Fut>(env: &Env, decide: F) -> Result<bonsai_core::LoginOutcome, CoreError>
where
    F: FnOnce(String) -> Fut + Send + 'static,
    Fut: Future<Output = ()> + Send + 'static,
{
    let (tx, rx) = tokio::sync::oneshot::channel::<String>();
    let decider = tokio::spawn(async move {
        if let Ok(code) = rx.await {
            decide(code).await;
        }
    });
    let api = env.anonymous();
    let out = device_login(
        &api,
        login_start(),
        move |grant| {
            let _ = tx.send(grant.user_code.clone());
        },
        CancellationToken::new(),
    )
    .await;
    decider.await.unwrap();
    out
}

#[tokio::test]
async fn device_login_happy_path() {
    let env = Env::start(0).await;
    let (origin, admin, http) = (
        env.h.origin.clone(),
        env.h.admin_token.clone(),
        env.http.clone(),
    );
    let out = login_with(&env, move |code| async move {
        let url = origin
            .join(&format!("/_mock/cli/logins/{code}/approve"))
            .unwrap();
        let res = http
            .post(url)
            .bearer_auth(admin)
            .json(&json!({"account": "acme", "scopes": ["account:read"], "expires_in_days": 30}))
            .send()
            .await
            .unwrap();
        assert_eq!(res.status(), 204);
    })
    .await
    .unwrap();
    assert_eq!(out.account.id, "acme");
    assert_eq!(out.scopes, ["account:read"]);
    assert!(out.expires_at.is_some());

    let api = client(&env.h.base_url, Some(out.token.expose_secret()));
    assert_eq!(api.get_account().await.unwrap().value.id, "acme");
}

#[tokio::test]
async fn device_login_denied() {
    let env = Env::start(0).await;
    let (origin, admin, http) = (
        env.h.origin.clone(),
        env.h.admin_token.clone(),
        env.http.clone(),
    );
    let err = login_with(&env, move |code| async move {
        let url = origin
            .join(&format!("/_mock/cli/logins/{code}/deny"))
            .unwrap();
        let res = http.post(url).bearer_auth(admin).send().await.unwrap();
        assert_eq!(res.status(), 204);
    })
    .await
    .err()
    .unwrap();
    assert!(matches!(err, CoreError::LoginDenied), "{err:?}");
}

#[tokio::test]
async fn device_login_cancel() {
    let env = Env::start(0).await;
    let api = env.anonymous();
    let cancel = CancellationToken::new();
    let trigger = cancel.clone();
    let t0 = Instant::now();
    let err = device_login(&api, login_start(), move |_| trigger.cancel(), cancel)
        .await
        .err()
        .unwrap();
    assert!(matches!(err, CoreError::LoginCancelled), "{err:?}");
    assert!(
        t0.elapsed() < Duration::from_millis(900),
        "{:?}",
        t0.elapsed()
    );
}

async fn fake_login(server: &MockServer, interval: u64, expires_in: u64) {
    Mock::given(method("POST"))
        .and(path("/v1/cli/logins"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "device_code": "dev",
            "user_code": "BCDF-GHJK",
            "verification_uri": "https://example.test/approve",
            "verification_uri_complete": "https://example.test/approve?user_code=BCDF-GHJK",
            "expires_in": expires_in,
            "interval": interval,
        })))
        .mount(server)
        .await;
}

fn login_problem(code: &str) -> ResponseTemplate {
    ResponseTemplate::new(400).set_body_raw(
        json!({
            "type": "about:blank",
            "title": "Bad Request",
            "status": 400,
            "code": code,
            "detail": code,
            "details": [],
        })
        .to_string(),
        "application/problem+json",
    )
}

#[tokio::test]
async fn device_login_expired_token() {
    let server = MockServer::start().await;
    fake_login(&server, 1, 600).await;
    Mock::given(method("POST"))
        .and(path("/v1/cli/logins/token"))
        .respond_with(login_problem("expired_token"))
        .mount(&server)
        .await;
    let api = client(&Url::parse(&format!("{}/v1", server.uri())).unwrap(), None);
    let err = device_login(&api, login_start(), |_| {}, CancellationToken::new())
        .await
        .err()
        .unwrap();
    assert!(matches!(err, CoreError::LoginExpired), "{err:?}");
}

/// An `interval` longer than the time left doesn't sleep past `expires_in`.
#[tokio::test]
async fn device_login_never_sleeps_past_expires_in() {
    let server = MockServer::start().await;
    fake_login(&server, 3600, 1).await;
    let api = client(&Url::parse(&format!("{}/v1", server.uri())).unwrap(), None);
    let t0 = Instant::now();
    let err = device_login(&api, login_start(), |_| {}, CancellationToken::new())
        .await
        .err()
        .unwrap();
    assert!(matches!(err, CoreError::LoginExpired), "{err:?}");
    assert!(t0.elapsed() < Duration::from_secs(3), "{:?}", t0.elapsed());
}

#[tokio::test]
async fn device_login_gives_up_at_expires_in() {
    let server = MockServer::start().await;
    fake_login(&server, 1, 2).await;
    Mock::given(method("POST"))
        .and(path("/v1/cli/logins/token"))
        .respond_with(login_problem("authorization_pending"))
        .mount(&server)
        .await;
    let api = client(&Url::parse(&format!("{}/v1", server.uri())).unwrap(), None);
    let t0 = Instant::now();
    let err = device_login(&api, login_start(), |_| {}, CancellationToken::new())
        .await
        .err()
        .unwrap();
    assert!(matches!(err, CoreError::LoginExpired), "{err:?}");
    assert!(t0.elapsed() < Duration::from_secs(4), "{:?}", t0.elapsed());
}

/// The slow test: `slow_down` adds RFC 8628's 5 s to the 1 s interval.
#[tokio::test]
async fn device_login_slow_down_adds_five_seconds() {
    let server = MockServer::start().await;
    fake_login(&server, 1, 600).await;
    Mock::given(method("POST"))
        .and(path("/v1/cli/logins/token"))
        .respond_with(login_problem("slow_down"))
        .up_to_n_times(1)
        .with_priority(1)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/v1/cli/logins/token"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "token": "bonsai_key_secret",
            "account": {"id": "acme", "name": "Acme"},
            "scopes": ["account:read"],
            "expires_at": null,
        })))
        .with_priority(2)
        .mount(&server)
        .await;
    let api = client(&Url::parse(&format!("{}/v1", server.uri())).unwrap(), None);
    let t0 = Instant::now();
    let out = device_login(&api, login_start(), |_| {}, CancellationToken::new())
        .await
        .unwrap();
    // One 1 s wait before the first poll, then 1 + 5 s before the second.
    assert!(t0.elapsed() >= Duration::from_secs(7), "{:?}", t0.elapsed());
    assert_eq!(out.token.expose_secret(), "bonsai_key_secret");
    assert_eq!(out.account.id, "acme");
}

// ---------------------------------------------------------------- edge cases

#[tokio::test]
async fn wait_timeout_bounds_a_poll_in_flight() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/v1/actions/act_fake"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(action_json("running", json!([])))
                .set_delay(Duration::from_secs(5)),
        )
        .mount(&server)
        .await;
    let api = fake_api(&server);
    let (ev, _rx) = sink();
    let opts = WaitOptions {
        timeout: Duration::from_millis(500),
        ..opts()
    };
    let t0 = Instant::now();
    let err = wait_for_action(
        &api,
        response(action_json("running", json!([]))),
        &opts,
        &ev,
    )
    .await
    .unwrap_err();
    assert!(
        t0.elapsed() < Duration::from_millis(1500),
        "{:?}",
        t0.elapsed()
    );
    assert!(
        matches!(&err, CoreError::Timeout { action_id, .. } if action_id == "act_fake"),
        "{err:?}"
    );
}

#[tokio::test]
async fn wait_with_an_enormous_timeout_does_not_overflow() {
    let server = MockServer::start().await;
    let api = fake_api(&server);
    let (ev, _rx) = sink();
    let opts = WaitOptions {
        timeout: Duration::MAX,
        ..opts()
    };
    let done = wait_for_action(
        &api,
        response(action_json("success", json!([]))),
        &opts,
        &ev,
    )
    .await
    .unwrap();
    assert_eq!(done.value.state, ActionState::Success);
}

#[tokio::test]
async fn device_login_with_an_enormous_expiry_does_not_overflow() {
    let server = MockServer::start().await;
    fake_login(&server, u64::MAX, u64::MAX).await;
    let api = fake_api(&server);
    let cancel = CancellationToken::new();
    let trigger = cancel.clone();
    let err = device_login(&api, login_start(), move |_| trigger.cancel(), cancel)
        .await
        .err()
        .unwrap();
    assert!(matches!(err, CoreError::LoginCancelled), "{err:?}");
}

#[test]
fn core_error_stays_small() {
    assert!(
        std::mem::size_of::<CoreError>() <= 64,
        "{} bytes",
        std::mem::size_of::<CoreError>()
    );
}

#[tokio::test]
async fn wait_emits_steps_from_the_initial_response_once() {
    let server = MockServer::start().await;
    let step = |m: &str, s: &str| json!({"state": s, "message": m, "at": "2026-10-20T00:00:01Z"});
    Mock::given(method("GET"))
        .and(path("/v1/actions/act_fake"))
        .respond_with(ResponseTemplate::new(200).set_body_json(action_json(
            "success",
            json!([step("one", "running"), step("two", "success")]),
        )))
        .mount(&server)
        .await;
    let api = fake_api(&server);
    let (ev, mut rx) = sink();
    let initial = response(action_json("running", json!([step("one", "running")])));
    wait_for_action(&api, initial, &opts(), &ev).await.unwrap();
    assert_eq!(steps(&drain(&mut rx)), ["one", "two"]);
}

#[tokio::test]
async fn resolve_prefers_an_id_over_a_name_equal_to_it() {
    let env = Env::start(0).await;
    let api = env.acme();
    let (ev, _rx) = sink();
    let first = create_cluster(
        &api,
        request("first", false, CleanupPolicy::Keep),
        &opts(),
        &ev,
    )
    .await
    .unwrap()
    .value
    .subject
    .id;
    // A second cluster whose name is the first one's ID.
    create_cluster(
        &api,
        request(&first, false, CleanupPolicy::Keep),
        &opts(),
        &ev,
    )
    .await
    .unwrap();
    let found = resolve_cluster(&api, &first).await.unwrap();
    assert_eq!(found.value.id, first);
    assert_eq!(found.value.name, "first");
}

#[tokio::test]
async fn resolve_searches_names_across_pages() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/v1/clusters/dup"))
        .respond_with(
            ResponseTemplate::new(404).set_body_raw(
                json!({"type": "about:blank", "title": "Not Found", "status": 404,
                   "code": "not_found", "detail": "No such cluster.", "details": []})
                .to_string(),
                "application/problem+json",
            ),
        )
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/v1/clusters"))
        .and(query_param("cursor", "page2"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "data": [cluster_json("dup-2", "Dup"), cluster_json("dupe-3", "dupe")],
            "next_cursor": null,
        })))
        .with_priority(1)
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/v1/clusters"))
        .and(query_param("name", "dup"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "data": [cluster_json("dup-1", "dup")],
            "next_cursor": "page2",
        })))
        .with_priority(2)
        .mount(&server)
        .await;
    match resolve_cluster(&fake_api(&server), "dup").await {
        Err(CoreError::Ambiguous { ids, .. }) => assert_eq!(ids, ["dup-1", "dup-2"]),
        other => panic!("expected Ambiguous, got {other:?}"),
    }
}

#[tokio::test]
async fn connection_url_without_an_rw_credential() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/v1/clusters/c1/credentials"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "data": [{
                "id": "cred-ro", "name": "reader", "access_type": "ro",
                "allowed_actions": [], "allowed_index_globs": [], "allowed_cidrs": [],
                "created_at": "2026-10-20T00:00:00Z",
            }],
            "next_cursor": null,
        })))
        .mount(&server)
        .await;
    match connection_url(&fake_api(&server), "c1", None).await {
        Err(CoreError::NoCredential(cluster)) => assert_eq!(cluster, "c1"),
        Err(other) => panic!("expected NoCredential, got {other:?}"),
        Ok(_) => panic!("expected NoCredential, got a URL"),
    }
}

#[tokio::test]
async fn wait_poll_failure_keeps_the_action_id() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/v1/actions/act_fake"))
        .respond_with(ResponseTemplate::new(503))
        .mount(&server)
        .await;
    let (ev, _rx) = sink();
    let err = wait_for_action(
        &fake_api(&server),
        response(action_json("running", json!([]))),
        &opts(),
        &ev,
    )
    .await
    .unwrap_err();
    // The source is reported by `source()`, not repeated in the message.
    let shown = err.to_string();
    assert!(!shown.contains("503"), "{shown}");
    assert!(std::error::Error::source(&err).is_some());
    match err {
        CoreError::WaitFailed { action_id, source } => {
            assert_eq!(action_id, "act_fake");
            assert_eq!(source.status(), Some(503));
        }
        other => panic!("expected WaitFailed, got {other:?}"),
    }
}

/// Mounts `GET /clusters/c1`, named `c1-name`, in `state` (terminated when it's
/// `deprovisioned`).
async fn mount_c1(server: &MockServer, state: &str) {
    let mut c1 = cluster_json("c1", "c1-name");
    c1["state"] = json!(state);
    if state == "deprovisioned" {
        c1["terminated_at"] = json!("2026-10-20T00:00:01Z");
    }
    Mock::given(method("GET"))
        .and(path("/v1/clusters/c1"))
        .respond_with(ResponseTemplate::new(200).set_body_json(c1))
        .mount(server)
        .await;
}

/// The deprovision Action `act_dep` on `c1`, in `state`.
fn deprovision(state: &str) -> Value {
    let mut dep = action_json(state, json!([]));
    dep["id"] = json!("act_dep");
    dep["type"] = json!("cluster.deprovision");
    dep["subject"]["id"] = json!("c1");
    if state == "error" {
        dep["error"] = json!({"code": "action_failed", "message": "It broke."});
    }
    dep
}

/// A fake `/v1` where `POST /clusters` starts `act_prov` on `c1`, and polling it
/// gives `prov_final` after `prov_delay`. The cluster still exists, `failed`, as
/// when Bonsai hasn't removed it.
async fn fake_failed_create(prov_final: Value, prov_delay: Duration) -> MockServer {
    fake_create(prov_final, prov_delay, "failed").await
}

async fn fake_create(prov_final: Value, prov_delay: Duration, cluster_state: &str) -> MockServer {
    let server = MockServer::start().await;
    mount_c1(&server, cluster_state).await;
    let mut started = action_json("new", json!([]));
    started["id"] = json!("act_prov");
    started["subject"]["id"] = json!("c1");
    Mock::given(method("POST"))
        .and(path("/v1/clusters"))
        .respond_with(ResponseTemplate::new(202).set_body_json(started))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/v1/actions/act_prov"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(prov_final)
                .set_delay(prov_delay),
        )
        .mount(&server)
        .await;
    server
}

/// The provision `act_prov` on `c1`, still running.
fn provisioning() -> Value {
    let mut a = action_json("running", json!([]));
    a["id"] = json!("act_prov");
    a["subject"]["id"] = json!("c1");
    a
}

fn errored(code: &str) -> Value {
    let mut a = action_json("error", json!([]));
    a["id"] = json!("act_prov");
    a["subject"]["id"] = json!("c1");
    a["error"] = json!({"code": code, "message": "It broke."});
    a
}

#[tokio::test]
async fn failed_cleanup_keeps_the_original_error() {
    // Any Action error after this run created the cluster is cleaned up, even an
    // Action-level payment_failed.
    let server = fake_failed_create(errored("payment_failed"), Duration::ZERO).await;
    Mock::given(method("DELETE"))
        .and(path("/v1/clusters/c1"))
        .respond_with(ResponseTemplate::new(500))
        .mount(&server)
        .await;
    let (ev, mut rx) = sink();
    let err = create_cluster(
        &fake_api(&server),
        request("c1-name", true, CleanupPolicy::Remove),
        &opts(),
        &ev,
    )
    .await
    .unwrap_err();
    let f = failure(err);
    assert_eq!(f.action_id, "act_prov");
    assert_eq!(f.code, ActionErrorCode::PaymentFailed);
    assert_eq!(f.message, "It broke.");
    assert_eq!(f.cluster_id.as_deref(), Some("c1"));
    let cleanup = f.cleanup.expect("a cleanup");
    assert_eq!(cleanup.cluster_id, "c1");
    assert_eq!(cleanup.outcome, CleanupOutcome::DeleteFailed);
    assert_eq!(
        drain(&mut rx).last(),
        Some(&Event::CleanupFinished { ok: false })
    );
}

#[tokio::test]
async fn cleanup_cut_short_reports_the_started_deprovision() {
    // The provision takes 1.2 s of a 2 s timeout; the cleanup gets what's left.
    let server = fake_failed_create(errored("action_failed"), Duration::from_millis(1200)).await;
    let mut dep = action_json("running", json!([]));
    dep["id"] = json!("act_dep");
    dep["type"] = json!("cluster.deprovision");
    dep["subject"]["id"] = json!("c1");
    Mock::given(method("DELETE"))
        .and(path("/v1/clusters/c1"))
        .respond_with(ResponseTemplate::new(202).set_body_json(dep.clone()))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/v1/actions/act_dep"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(dep)
                .insert_header("Retry-After", "60"),
        )
        .mount(&server)
        .await;
    let opts = WaitOptions {
        timeout: Duration::from_secs(2),
        ..opts()
    };
    let (ev, _rx) = sink();
    let t0 = Instant::now();
    let err = create_cluster(
        &fake_api(&server),
        request("c1-name", true, CleanupPolicy::Remove),
        &opts,
        &ev,
    )
    .await
    .unwrap_err();
    assert!(
        t0.elapsed() < Duration::from_millis(2800),
        "{:?}",
        t0.elapsed()
    );
    let cleanup = failure(err).cleanup.expect("a cleanup");
    assert_eq!(
        cleanup.outcome,
        CleanupOutcome::Pending {
            deprovision_action_id: "act_dep".into()
        }
    );
}

/// A create whose POST is answered `Idempotent-Replayed: true` (as this run's own
/// retry after a lost response would be), then fails; the DELETE succeeds.
async fn fake_replayed_failed_create() -> MockServer {
    let server = MockServer::start().await;
    mount_c1(&server, "failed").await;
    let mut started = action_json("new", json!([]));
    started["id"] = json!("act_prov");
    started["subject"]["id"] = json!("c1");
    Mock::given(method("POST"))
        .and(path("/v1/clusters"))
        .respond_with(
            ResponseTemplate::new(202)
                .insert_header("Idempotent-Replayed", "true")
                .set_body_json(started),
        )
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/v1/actions/act_prov"))
        .respond_with(ResponseTemplate::new(200).set_body_json(errored("action_failed")))
        .mount(&server)
        .await;
    let mut dep = action_json("success", json!([]));
    dep["id"] = json!("act_dep");
    dep["type"] = json!("cluster.deprovision");
    dep["subject"]["id"] = json!("c1");
    Mock::given(method("DELETE"))
        .and(path("/v1/clusters/c1"))
        .respond_with(ResponseTemplate::new(202).set_body_json(dep))
        .mount(&server)
        .await;
    server
}

#[tokio::test]
async fn a_replay_of_this_runs_own_key_is_still_cleaned_up() {
    // No caller key: the key was made in this run, so a replay can only be this
    // run's own retry, and the cluster is this run's.
    let server = fake_replayed_failed_create().await;
    let (ev, _rx) = sink();
    let err = create_cluster(
        &fake_api(&server),
        request("c1-name", true, CleanupPolicy::Remove),
        &opts(),
        &ev,
    )
    .await
    .unwrap_err();
    let f = failure(err);
    assert!(!f.replayed);
    let cleanup = f.cleanup.expect("a cleanup");
    assert_eq!(
        cleanup.outcome,
        CleanupOutcome::Removed {
            deprovision_action_id: "act_dep".into()
        }
    );
}

#[tokio::test]
async fn a_replay_of_a_caller_key_is_kept() {
    let server = fake_replayed_failed_create().await;
    let (ev, _rx) = sink();
    let req = CreateRequest {
        idempotency_key: Some("ci-run-8".into()),
        ..request("c1-name", true, CleanupPolicy::Remove)
    };
    let f = failure(
        create_cluster(&fake_api(&server), req, &opts(), &ev)
            .await
            .unwrap_err(),
    );
    assert!(f.replayed);
    assert_eq!(f.cleanup, None);
    assert!(deletes(&server).await.is_empty());
}

#[tokio::test]
async fn a_cluster_bonsai_already_removed_gets_no_delete() {
    let server = fake_create(errored("action_failed"), Duration::ZERO, "deprovisioned").await;
    let (ev, mut rx) = sink();
    let ask = CleanupPolicy::Ask(Box::new(|_| panic!("asked about a removed cluster")));
    let f = failure(
        create_cluster(
            &fake_api(&server),
            request("c1-name", true, ask),
            &opts(),
            &ev,
        )
        .await
        .unwrap_err(),
    );
    assert_eq!(f.code, ActionErrorCode::ActionFailed);
    assert_eq!(
        f.cleanup,
        Some(bonsai_core::Cleanup {
            cluster_id: "c1".into(),
            outcome: CleanupOutcome::RemovedByBonsai,
        })
    );
    assert!(deletes(&server).await.is_empty());
    assert!(
        !drain(&mut rx)
            .iter()
            .any(|e| matches!(e, Event::CleanupStarted { .. }))
    );
}

#[tokio::test]
async fn a_delete_answered_cluster_terminated_is_removed_by_bonsai() {
    // The re-read raced Bonsai's own cleanup: the cluster looked `failed`, but the
    // DELETE finds it terminated.
    let server = fake_failed_create(errored("action_failed"), Duration::ZERO).await;
    Mock::given(method("DELETE"))
        .and(path("/v1/clusters/c1"))
        .respond_with(
            ResponseTemplate::new(422).set_body_raw(
                json!({
                    "type": "about:blank", "title": "Unprocessable Content", "status": 422,
                    "code": "cluster_terminated", "detail": "Cluster c1 is deprovisioned.",
                    "details": [],
                })
                .to_string(),
                "application/problem+json",
            ),
        )
        .mount(&server)
        .await;
    let (ev, mut rx) = sink();
    let f = failure(
        create_cluster(
            &fake_api(&server),
            request("c1-name", true, CleanupPolicy::Remove),
            &opts(),
            &ev,
        )
        .await
        .unwrap_err(),
    );
    assert_eq!(
        f.cleanup.map(|c| c.outcome),
        Some(CleanupOutcome::RemovedByBonsai)
    );
    assert_eq!(
        drain(&mut rx).last(),
        Some(&Event::CleanupFinished { ok: true })
    );
}

#[tokio::test]
async fn a_failed_deprovision_is_reported_as_failed() {
    let server = fake_failed_create(errored("action_failed"), Duration::ZERO).await;
    Mock::given(method("DELETE"))
        .and(path("/v1/clusters/c1"))
        .respond_with(ResponseTemplate::new(202).set_body_json(deprovision("running")))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/v1/actions/act_dep"))
        .respond_with(ResponseTemplate::new(200).set_body_json(deprovision("error")))
        .mount(&server)
        .await;
    let (ev, mut rx) = sink();
    let f = failure(
        create_cluster(
            &fake_api(&server),
            request("c1-name", true, CleanupPolicy::Remove),
            &opts(),
            &ev,
        )
        .await
        .unwrap_err(),
    );
    // The original error, not the deprovision's.
    assert_eq!(f.action_id, "act_prov");
    assert_eq!(
        f.cleanup.map(|c| c.outcome),
        Some(CleanupOutcome::DeprovisionFailed {
            deprovision_action_id: "act_dep".into()
        })
    );
    assert_eq!(
        drain(&mut rx).last(),
        Some(&Event::CleanupFinished { ok: false })
    );
}

#[tokio::test]
async fn no_delete_unless_the_provision_ended_in_error() {
    // Timeout: the provision is still running at the deadline.
    let server = fake_failed_create(provisioning(), Duration::ZERO).await;
    let short = WaitOptions {
        timeout: Duration::from_millis(300),
        ..opts()
    };
    let (ev, _rx) = sink();
    let err = create_cluster(
        &fake_api(&server),
        request("c1-name", true, CleanupPolicy::Remove),
        &short,
        &ev,
    )
    .await
    .unwrap_err();
    assert!(matches!(err, CoreError::Timeout { .. }), "{err:?}");
    assert!(deletes(&server).await.is_empty());

    // Cancelled while it runs.
    let server = fake_failed_create(provisioning(), Duration::ZERO).await;
    let cancel = CancellationToken::new();
    let o = WaitOptions {
        cancel: cancel.clone(),
        ..opts()
    };
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(200)).await;
        cancel.cancel();
    });
    let err = create_cluster(
        &fake_api(&server),
        request("c1-name", true, CleanupPolicy::Remove),
        &o,
        &ev,
    )
    .await
    .unwrap_err();
    assert!(matches!(err, CoreError::Cancelled { .. }), "{err:?}");
    assert!(deletes(&server).await.is_empty());

    // A poll that fails after the client's retries.
    let server = MockServer::start().await;
    mount_c1(&server, "failed").await;
    let mut started = action_json("new", json!([]));
    started["id"] = json!("act_prov");
    started["subject"]["id"] = json!("c1");
    Mock::given(method("POST"))
        .and(path("/v1/clusters"))
        .respond_with(ResponseTemplate::new(202).set_body_json(started))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/v1/actions/act_prov"))
        .respond_with(ResponseTemplate::new(503))
        .mount(&server)
        .await;
    let err = create_cluster(
        &fake_api(&server),
        request("c1-name", true, CleanupPolicy::Remove),
        &opts(),
        &ev,
    )
    .await
    .unwrap_err();
    assert!(matches!(err, CoreError::WaitFailed { .. }), "{err:?}");
    assert!(deletes(&server).await.is_empty());
}

#[tokio::test]
async fn the_cleanup_delete_has_its_own_key_and_the_requested_name() {
    // A caller key that isn't replayed: this run created the cluster, so Remove
    // removes it, with a key of its own and the name the create asked for.
    let server = fake_failed_create(errored("action_failed"), Duration::ZERO).await;
    Mock::given(method("DELETE"))
        .and(path("/v1/clusters/c1"))
        .and(query_param("name", "c1-name"))
        .respond_with(ResponseTemplate::new(202).set_body_json(deprovision("success")))
        .mount(&server)
        .await;
    let req = CreateRequest {
        idempotency_key: Some("ci-run-9".into()),
        ..request("c1-name", true, CleanupPolicy::Remove)
    };
    let (ev, _rx) = sink();
    let f = failure(
        create_cluster(&fake_api(&server), req, &opts(), &ev)
            .await
            .unwrap_err(),
    );
    assert!(!f.replayed);
    assert_eq!(
        f.cleanup.map(|c| c.outcome),
        Some(CleanupOutcome::Removed {
            deprovision_action_id: "act_dep".into()
        })
    );
    let received = server.received_requests().await.unwrap();
    let key = |m: &str| {
        received
            .iter()
            .find(|r| r.method.as_str() == m)
            .and_then(|r| r.headers.get("idempotency-key"))
            .map(|v| v.to_str().unwrap().to_owned())
    };
    assert_eq!(key("POST").as_deref(), Some("ci-run-9"));
    let delete_key = key("DELETE").expect("the DELETE carries a key");
    assert_ne!(delete_key, "ci-run-9");
    assert_eq!(deletes(&server).await.len(), 1);
}

#[tokio::test]
async fn cancel_during_the_cleanup_wait_reports_it_pending() {
    let server = fake_failed_create(errored("action_failed"), Duration::ZERO).await;
    Mock::given(method("DELETE"))
        .and(path("/v1/clusters/c1"))
        .respond_with(ResponseTemplate::new(202).set_body_json(deprovision("running")))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/v1/actions/act_dep"))
        .respond_with(ResponseTemplate::new(200).set_body_json(deprovision("running")))
        .mount(&server)
        .await;
    let cancel = CancellationToken::new();
    let o = WaitOptions {
        cancel: cancel.clone(),
        ..opts()
    };
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(400)).await;
        cancel.cancel();
    });
    let (ev, _rx) = sink();
    let t0 = Instant::now();
    let f = failure(
        create_cluster(
            &fake_api(&server),
            request("c1-name", true, CleanupPolicy::Remove),
            &o,
            &ev,
        )
        .await
        .unwrap_err(),
    );
    assert!(t0.elapsed() < Duration::from_secs(5), "{:?}", t0.elapsed());
    assert_eq!(f.action_id, "act_prov");
    assert_eq!(
        f.cleanup.map(|c| c.outcome),
        Some(CleanupOutcome::Pending {
            deprovision_action_id: "act_dep".into()
        })
    );
}

#[tokio::test]
async fn a_declined_cleanup_keeps_the_cluster() {
    let server = fake_failed_create(errored("action_failed"), Duration::ZERO).await;
    let asked = Arc::new(Mutex::new(Vec::new()));
    let seen = asked.clone();
    let ask = CleanupPolicy::Ask(Box::new(move |name: &str| {
        seen.lock().unwrap().push(name.to_owned());
        false
    }));
    let (ev, _rx) = sink();
    let f = failure(
        create_cluster(
            &fake_api(&server),
            request("c1-name", true, ask),
            &opts(),
            &ev,
        )
        .await
        .unwrap_err(),
    );
    assert_eq!(f.cleanup, None);
    assert_eq!(f.cluster_id.as_deref(), Some("c1"));
    assert_eq!(*asked.lock().unwrap(), ["c1-name"]);
    assert!(deletes(&server).await.is_empty());
}

#[test]
fn a_timeout_reads_as_a_duration() {
    let e = CoreError::Timeout {
        action_id: "act_1".into(),
        timeout: Duration::from_secs(1800),
    };
    assert_eq!(
        e.to_string(),
        "still running after 30m; resume with `bonsai wait act_1`"
    );
}

/// A fake failed create (`act_prov` on `c1`) whose cluster reads come from
/// `cluster_reads`, mounted in order (each `Some(n)` answers `n` times).
async fn fake_create_reads(cluster_reads: Vec<(ResponseTemplate, Option<u64>)>) -> MockServer {
    let server = MockServer::start().await;
    for (template, times) in cluster_reads {
        let mock = Mock::given(method("GET"))
            .and(path("/v1/clusters/c1"))
            .respond_with(template);
        match times {
            Some(n) => mock.up_to_n_times(n).mount(&server).await,
            None => mock.mount(&server).await,
        }
    }
    let mut started = action_json("new", json!([]));
    started["id"] = json!("act_prov");
    started["subject"]["id"] = json!("c1");
    Mock::given(method("POST"))
        .and(path("/v1/clusters"))
        .respond_with(ResponseTemplate::new(202).set_body_json(started))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/v1/actions/act_prov"))
        .respond_with(ResponseTemplate::new(200).set_body_json(errored("action_failed")))
        .mount(&server)
        .await;
    Mock::given(method("DELETE"))
        .and(path("/v1/clusters/c1"))
        .respond_with(ResponseTemplate::new(202).set_body_json(deprovision("success")))
        .mount(&server)
        .await;
    server
}

fn c1_in(state: &str) -> ResponseTemplate {
    let mut c1 = cluster_json("c1", "c1-name");
    c1["state"] = json!(state);
    if state == "deprovisioned" {
        c1["terminated_at"] = json!("2026-10-20T00:00:01Z");
    }
    ResponseTemplate::new(200).set_body_json(c1)
}

/// The API saves the Action's `error` before it terminates the failed cluster,
/// so the first read can still see it `failed`: one more read settles it.
#[tokio::test]
async fn a_failed_cluster_is_read_again_before_the_policy_applies() {
    let server = fake_create_reads(vec![
        (c1_in("failed"), Some(1)),
        (c1_in("deprovisioned"), None),
    ])
    .await;
    let ask = CleanupPolicy::Ask(Box::new(|_| panic!("asked about a removed cluster")));
    let (ev, _rx) = sink();
    let f = failure(
        create_cluster(
            &fake_api(&server),
            request("c1-name", true, ask),
            &opts(),
            &ev,
        )
        .await
        .unwrap_err(),
    );
    assert_eq!(
        f.cleanup.map(|c| c.outcome),
        Some(CleanupOutcome::RemovedByBonsai)
    );
    assert!(deletes(&server).await.is_empty());
}

#[tokio::test]
async fn a_failed_re_read_falls_back_to_the_policy() {
    // No cluster read succeeds (a bare 404): the policy decides, and removes it.
    let server = fake_create_reads(vec![(ResponseTemplate::new(404), None)]).await;
    let asked = Arc::new(Mutex::new(0));
    let seen = asked.clone();
    let ask = CleanupPolicy::Ask(Box::new(move |_| {
        *seen.lock().unwrap() += 1;
        true
    }));
    let (ev, _rx) = sink();
    let f = failure(
        create_cluster(
            &fake_api(&server),
            request("c1-name", true, ask),
            &opts(),
            &ev,
        )
        .await
        .unwrap_err(),
    );
    assert_eq!(*asked.lock().unwrap(), 1);
    assert_eq!(
        f.cleanup.map(|c| c.outcome),
        Some(CleanupOutcome::Removed {
            deprovision_action_id: "act_dep".into()
        })
    );
    assert_eq!(deletes(&server).await.len(), 1);
}

/// Ctrl-C while the cluster is re-read removes nothing and returns at once.
#[tokio::test]
async fn cancel_during_the_re_read_sends_no_delete() {
    let server = fake_create_reads(vec![(
        c1_in("failed").set_delay(Duration::from_secs(10)),
        None,
    )])
    .await;
    let cancel = CancellationToken::new();
    let o = WaitOptions {
        cancel: cancel.clone(),
        ..opts()
    };
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(300)).await;
        cancel.cancel();
    });
    let (ev, _rx) = sink();
    let t0 = Instant::now();
    let f = failure(
        create_cluster(
            &fake_api(&server),
            request("c1-name", true, CleanupPolicy::Remove),
            &o,
            &ev,
        )
        .await
        .unwrap_err(),
    );
    assert!(t0.elapsed() < Duration::from_secs(3), "{:?}", t0.elapsed());
    assert_eq!(
        f.cleanup.map(|c| c.outcome),
        Some(CleanupOutcome::Unchecked {
            reason: StopReason::Cancelled
        })
    );
    assert_eq!(f.cluster_id.as_deref(), Some("c1"));
    assert!(deletes(&server).await.is_empty());
}

/// The timeout runs out while the cluster is re-read: the read is cut there,
/// nothing is removed, and the outcome says the cluster wasn't checked.
#[tokio::test]
async fn the_deadline_during_the_re_read_sends_no_delete() {
    let server = fake_create_reads(vec![(
        c1_in("failed").set_delay(Duration::from_secs(8)),
        None,
    )])
    .await;
    let o = WaitOptions {
        timeout: Duration::from_millis(800),
        ..opts()
    };
    let (ev, _rx) = sink();
    let t0 = Instant::now();
    let f = failure(
        create_cluster(
            &fake_api(&server),
            request("c1-name", true, CleanupPolicy::Remove),
            &o,
            &ev,
        )
        .await
        .unwrap_err(),
    );
    assert!(t0.elapsed() < Duration::from_secs(3), "{:?}", t0.elapsed());
    assert_eq!(
        f.cleanup.map(|c| c.outcome),
        Some(CleanupOutcome::Unchecked {
            reason: StopReason::Timeout
        })
    );
    assert!(deletes(&server).await.is_empty());
}
