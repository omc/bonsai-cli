//! A stateful mock of the Bonsai `/v1` API, backed by SQLite.
//!
//! `openapi/v1.yml` is the source of truth for every response shape; responses
//! are built from `bonsai-api`'s models so the shapes come from one place.

pub mod db;
pub mod engine;
pub mod html;
pub mod http;
pub mod ids;
pub mod problem;
pub mod routes;
pub mod scenario;
pub mod seed;

use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use anyhow::Context;
use axum::Router;
use axum::middleware::from_fn_with_state;
use rusqlite::Connection;
use url::Url;

use crate::http::RateLimiter;
use crate::seed::SeedToken;

pub struct MockOptions {
    pub db_path: PathBuf,
    /// How long each Action step takes; 0 completes an Action on its next read.
    pub step_ms: u64,
    /// The origin clients reach the mock at; defaults to the bound address.
    pub public_url: Option<Url>,
    /// When set, a `payment_method_required` or `payment_failed` `action_url`
    /// is this prefix and a fake Stripe session ID, not the mock's own
    /// checkout page.
    pub checkout_url: Option<String>,
}

pub struct MockHandle {
    /// `http://127.0.0.1:PORT/v1`
    pub base_url: Url,
    /// `http://127.0.0.1:PORT/`
    pub origin: Url,
    /// The loopback address the mock is bound to, whatever `public_url` says.
    pub local_addr: SocketAddr,
    /// Bearer token for the `/_mock` admin API. New on every start.
    pub admin_token: String,
    /// One token per seeded account, when this start seeded the database;
    /// empty when it already held data (tokens are stored hashed).
    pub seeded_tokens: Vec<SeedToken>,
    /// Send `()` (or drop) to stop the server gracefully.
    pub shutdown: tokio::sync::oneshot::Sender<()>,
}

/// Shared state for every handler. Database work is short and synchronous:
/// take the lock with [`AppState::db`] and never hold it across an `.await`.
#[derive(Clone)]
pub struct AppState {
    inner: Arc<Inner>,
}

struct Inner {
    db: Mutex<Connection>,
    step_ms: u64,
    origin: Url,
    admin_token: String,
    checkout_url: Option<String>,
    limiter: RateLimiter,
}

impl AppState {
    pub fn new(
        conn: Connection,
        step_ms: u64,
        origin: Url,
        admin_token: String,
        checkout_url: Option<String>,
    ) -> Self {
        Self {
            inner: Arc::new(Inner {
                db: Mutex::new(conn),
                step_ms,
                origin,
                admin_token,
                checkout_url,
                limiter: RateLimiter::default(),
            }),
        }
    }

    /// Runs `f` with the database connection locked.
    pub fn db<T>(&self, f: impl FnOnce(&Connection) -> T) -> T {
        let conn = self.inner.db.lock().unwrap_or_else(|e| e.into_inner());
        f(&conn)
    }

    pub fn step_ms(&self) -> u64 {
        self.inner.step_ms
    }

    pub fn origin(&self) -> &Url {
        &self.inner.origin
    }

    pub fn admin_token(&self) -> &str {
        &self.inner.admin_token
    }

    /// The prefix for checkout links, when `--checkout-url` set one.
    pub fn checkout_url(&self) -> Option<&str> {
        self.inner.checkout_url.as_deref()
    }

    /// An absolute URL on the mock for `path` (`"/v1/actions/act_..."`).
    pub fn url(&self, path: &str) -> String {
        format!(
            "{}{}",
            self.inner.origin.as_str().trim_end_matches('/'),
            path
        )
    }

    pub(crate) fn limiter(&self) -> &RateLimiter {
        &self.inner.limiter
    }
}

/// The mock's routes:
///
/// - `GET /v1` and the login endpoints are rate-limited per IP, every other
///   `/v1` route per token. A wrong method on a `/v1` route is a 404
///   `not_found` problem, after the token is checked where the route takes
///   one, as the Bonsai API answers an unknown route.
/// - `/v1/openapi.json`, `/_mock/*` and unknown paths (404 `not_found`) are
///   outside the rate limits, so they never take from `rate_limit_next`.
pub fn app(state: AppState) -> Router {
    let by_ip = routes::root::router()
        .merge(routes::login::router())
        .method_not_allowed_fallback(|| async { problem::not_found() })
        .route_layer(from_fn_with_state(state.clone(), http::limit_by_ip));
    let by_token = Router::new()
        .merge(routes::account::router())
        .merge(routes::catalogue::router())
        .merge(routes::clusters::router())
        .merge(routes::actions::router())
        .merge(routes::credentials::router())
        .merge(routes::observability::router())
        .merge(routes::token::router())
        .method_not_allowed_fallback(wrong_method)
        .route_layer(from_fn_with_state(state.clone(), http::limit_by_token));
    let unlimited = routes::root::openapi_router()
        .merge(routes::admin::router())
        .merge(routes::login::page_router());
    Router::new()
        .merge(by_ip)
        .merge(by_token)
        .merge(unlimited)
        .fallback(|| async { problem::not_found() })
        .layer(from_fn_with_state(state.clone(), only_known_hosts))
        .with_state(state)
}

/// Refuses (`421 Misdirected Request`) a `Host` other than a loopback name or
/// the origin's host, whatever the port. A page on another origin that DNS
/// rebinding points at 127.0.0.1 sends its own name, so it can't start a login,
/// approve it on the unauthenticated approval form, or use the checkout page.
async fn only_known_hosts(
    axum::extract::State(state): axum::extract::State<AppState>,
    req: axum::extract::Request,
    next: axum::middleware::Next,
) -> axum::response::Response {
    use axum::response::IntoResponse;
    let host = req
        .headers()
        .get(axum::http::header::HOST)
        .and_then(|h| h.to_str().ok())
        .or_else(|| req.uri().authority().map(|a| a.as_str()))
        .unwrap_or_default();
    let name = match host.strip_prefix('[') {
        // [::1]:port
        Some(rest) => rest.split(']').next().map(|h| format!("[{h}]")),
        None => host.split(':').next().map(str::to_owned),
    }
    .unwrap_or_default()
    .to_ascii_lowercase();
    let origin = state
        .origin()
        .host_str()
        .unwrap_or_default()
        .to_ascii_lowercase();
    if matches!(name.as_str(), "localhost" | "127.0.0.1" | "[::1]") || name == origin {
        return next.run(req).await;
    }
    // Not in the contract (no operation documents a 421), so the code is the
    // nearest general one.
    problem::status_problem(
        axum::http::StatusCode::MISDIRECTED_REQUEST,
        bonsai_api::ProblemCode::InvalidParameter,
        "bonsai-mock answers only on localhost, 127.0.0.1, [::1] and its public URL's host.",
    )
    .field(
        "Host",
        "Host must be a loopback name or the mock's public URL's host.",
    )
    .into_response()
}

/// A route that exists, with a method it doesn't take: 401 without a valid
/// token, otherwise 404 `not_found`.
async fn wrong_method(
    auth: Result<http::Auth, problem::ProblemBuilder>,
) -> problem::ProblemBuilder {
    match auth {
        Err(p) => p,
        Ok(_) => problem::not_found(),
    }
}

/// Opens (and if empty, seeds) the database, binds `addr` (port 0 picks a free
/// port) and serves in the background until `MockHandle::shutdown` fires.
/// `addr` must be a loopback address: the mock's login approval and checkout
/// pages need no token.
pub async fn spawn(opts: MockOptions, addr: SocketAddr) -> anyhow::Result<MockHandle> {
    anyhow::ensure!(
        addr.ip().is_loopback(),
        "bonsai-mock binds only a loopback address, not {addr}"
    );
    let conn =
        db::open(&opts.db_path).with_context(|| format!("opening {}", opts.db_path.display()))?;
    let fresh = seed::is_empty(&conn)?;
    seed::seed_if_empty(&conn)?;
    let seeded_tokens = if fresh {
        seed::create_seed_tokens(&conn)?
    } else {
        Vec::new()
    };

    let listener = tokio::net::TcpListener::bind(addr)
        .await
        .with_context(|| format!("binding {addr}"))?;
    let local = listener.local_addr()?;
    let origin = match opts.public_url {
        Some(u) => u,
        // A loopback address (checked above), so it is reachable as it is.
        None => Url::parse(&format!("http://{local}/"))?,
    };
    let base_url = origin.join("v1")?;
    let admin_token = format!("mock_admin_{}", ids::hex(32));

    let state = AppState::new(
        conn,
        opts.step_ms,
        origin.clone(),
        admin_token.clone(),
        opts.checkout_url,
    );
    let router = app(state);
    let (shutdown, rx) = tokio::sync::oneshot::channel::<()>();
    tokio::spawn(async move {
        let served = axum::serve(
            listener,
            router.into_make_service_with_connect_info::<SocketAddr>(),
        )
        .with_graceful_shutdown(async {
            let _ = rx.await;
        })
        .await;
        if let Err(e) = served {
            tracing::error!(error = %e, "bonsai-mock server stopped");
        }
    });

    Ok(MockHandle {
        base_url,
        origin,
        local_addr: local,
        admin_token,
        seeded_tokens,
        shutdown,
    })
}
