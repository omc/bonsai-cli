//! `GET /v1` (`getApiRoot`) and `GET /v1/openapi.json`, the contract as
//! JSON: no token needed for either.

use std::sync::LazyLock;

use axum::extract::State;
use axum::http::header;
use axum::response::IntoResponse;
use axum::routing::get;
use axum::{Json, Router};
use bonsai_api::ApiRoot;

use crate::AppState;

const CONTRACT: &str = include_str!("../../../../openapi/v1.yml");

/// The contract's `info.version`.
pub static CONTRACT_VERSION: LazyLock<String> = LazyLock::new(|| info_version(CONTRACT));

/// The vendored `openapi/v1.yml`, converted to JSON, as the Bonsai API
/// serves it.
static CONTRACT_JSON: LazyLock<String> = LazyLock::new(|| {
    let spec: serde_json::Value =
        serde_yaml::from_str(CONTRACT).expect("openapi/v1.yml converts to JSON");
    spec.to_string()
});

/// Reads `info.version` from the YAML without a YAML parser: the first
/// `version:` key indented under the top-level `info:` block.
fn info_version(yaml: &str) -> String {
    yaml.lines()
        .skip_while(|l| l.trim_end() != "info:")
        .skip(1)
        .take_while(|l| l.is_empty() || l.starts_with(' '))
        .find_map(|l| l.trim().strip_prefix("version:"))
        .map(|v| v.trim().trim_matches(|c| c == '"' || c == '\'').to_string())
        .expect("openapi/v1.yml has info.version")
}

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/v1", get(show))
        .route("/v1/", get(show))
}

/// `GET /v1/openapi.json`, which the Bonsai API serves outside `/v1`'s rate limits.
pub fn openapi_router() -> Router<AppState> {
    Router::new().route("/v1/openapi.json", get(openapi_json))
}

async fn openapi_json() -> impl IntoResponse {
    (
        [(header::CONTENT_TYPE, "application/json")],
        CONTRACT_JSON.as_str(),
    )
}

async fn show(State(state): State<AppState>) -> Json<ApiRoot> {
    Json(ApiRoot {
        name: "Bonsai API".into(),
        version: CONTRACT_VERSION.clone(),
        docs_url: state.url("/docs"),
        openapi_url: state.url("/v1/openapi.json"),
        tokens_url: state.url("/_mock/api_tokens"),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_info_version() {
        let yaml =
            "openapi: 3.0.3\ninfo:\n  title: X\n  version: \"1.2.3\"\nservers:\n  version: no\n";
        assert_eq!(info_version(yaml), "1.2.3");
        assert!(!CONTRACT_VERSION.is_empty());
    }
}
