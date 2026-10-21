use crate::enums::ProblemCode;
use serde::{Deserialize, Serialize};
use std::time::Duration;

/// RFC 9457 problem details, extended with `code` and `details`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Problem {
    #[serde(rename = "type")]
    pub r#type: String,
    pub title: String,
    pub status: u16,
    pub code: ProblemCode,
    pub detail: String,
    #[serde(default)]
    pub instance: Option<String>,
    #[serde(default)]
    pub action_url: Option<String>,
    pub details: Vec<ProblemDetail>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ProblemDetail {
    /// A JSON pointer into the body, or a query parameter name; `None` when no
    /// single field is at fault.
    #[serde(deserialize_with = "crate::models::required_nullable")]
    pub field: Option<String>,
    pub message: String,
    /// Set on `missing_scope`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scope: Option<String>,
}

/// Everything a request can fail with.
#[derive(Debug, thiserror::Error)]
pub enum ApiError {
    /// An error response with an `application/problem+json` body.
    #[error("{} ({})", .problem.title, .problem.code)]
    Problem {
        status: u16,
        problem: Box<Problem>,
        /// The body as it came, for `--json` (fields this client doesn't know included).
        raw: Box<serde_json::Value>,
        retry_after: Option<Duration>,
    },
    /// An error response without a problem body.
    #[error("HTTP {status}")]
    Http { status: u16, body: String },
    #[error(transparent)]
    Transport(#[from] reqwest::Error),
    #[error("invalid response: {0}")]
    Decode(String),
    #[error("refusing non-https API URL {0}")]
    InsecureUrl(String),
    /// An ID or `send_raw` path that would leave the intended resource: `""`, `.` or
    /// `..` as an ID, or a path outside the API root.
    #[error("invalid request path: {0}")]
    InvalidPath(String),
    /// The client can't be built: an unreadable CA bundle, a token that isn't a valid header.
    #[error("invalid client configuration: {0}")]
    Config(String),
}

impl ApiError {
    pub fn code(&self) -> Option<&ProblemCode> {
        match self {
            Self::Problem { problem, .. } => Some(&problem.code),
            _ => None,
        }
    }

    pub fn status(&self) -> Option<u16> {
        match self {
            Self::Problem { status, .. } | Self::Http { status, .. } => Some(*status),
            Self::Transport(e) => e.status().map(|s| s.as_u16()),
            _ => None,
        }
    }
}
