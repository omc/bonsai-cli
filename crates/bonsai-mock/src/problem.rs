//! `application/problem+json` error responses.

use axum::http::{HeaderName, HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use bonsai_api::{Problem, ProblemCode, ProblemDetail};

pub const PROBLEM_CONTENT_TYPE: &str = "application/problem+json";
pub const PROBLEM_TYPE_BASE: &str = "https://bonsai.io/docs/api/errors/";
pub const WWW_AUTHENTICATE: &str = "Bearer realm=\"Bonsai API\"";

/// A problem response under construction. Handlers return it as their error
/// type (`Result<_, ProblemBuilder>`); it renders as RFC 9457 JSON.
#[derive(Debug, Clone)]
#[must_use]
pub struct ProblemBuilder {
    // Boxed so `Result<_, ProblemBuilder>` stays small.
    problem: Box<Problem>,
    headers: Vec<(HeaderName, HeaderValue)>,
}

/// A problem with `type` derived from `code`, no `details`, and `instance: null`.
pub fn problem(
    status: StatusCode,
    code: ProblemCode,
    title: impl Into<String>,
    detail: impl Into<String>,
) -> ProblemBuilder {
    ProblemBuilder {
        problem: Box::new(Problem {
            r#type: format!("{PROBLEM_TYPE_BASE}{}", code.as_str()),
            title: title.into(),
            status: status.as_u16(),
            code,
            detail: detail.into(),
            instance: None,
            action_url: None,
            details: Vec::new(),
        }),
        headers: Vec::new(),
    }
}

/// [`problem`] with the status's reason phrase as `title`.
pub fn status_problem(
    status: StatusCode,
    code: ProblemCode,
    detail: impl Into<String>,
) -> ProblemBuilder {
    problem(
        status,
        code,
        status.canonical_reason().unwrap_or("Error"),
        detail,
    )
}

/// 400 `invalid_parameter` naming one query parameter or body pointer.
pub fn invalid_parameter(field: &str, message: impl Into<String>) -> ProblemBuilder {
    let message = message.into();
    status_problem(
        StatusCode::BAD_REQUEST,
        ProblemCode::InvalidParameter,
        message.clone(),
    )
    .field(field, message)
}

/// 404 `not_found`.
pub fn not_found() -> ProblemBuilder {
    status_problem(
        StatusCode::NOT_FOUND,
        ProblemCode::NotFound,
        "No such resource in this account.",
    )
}

/// 401 `unauthorized` with `WWW-Authenticate`.
pub fn unauthorized() -> ProblemBuilder {
    status_problem(
        StatusCode::UNAUTHORIZED,
        ProblemCode::Unauthorized,
        "A valid API token is required, sent as `Authorization: Bearer <token>`.",
    )
    .header(header::WWW_AUTHENTICATE, WWW_AUTHENTICATE)
}

/// 500 `internal_error`. The contract documents no 500, so a test that hits
/// this fails its contract check, as it should.
pub fn internal_error() -> ProblemBuilder {
    status_problem(
        StatusCode::INTERNAL_SERVER_ERROR,
        ProblemCode::InternalError,
        "Something went wrong in the mock.",
    )
}

impl ProblemBuilder {
    /// Adds a `details` entry naming a field: a JSON pointer into the body
    /// (`/plan`) or a query parameter's name (`limit`).
    pub fn field(self, ptr: &str, message: impl Into<String>) -> Self {
        self.push_detail(ProblemDetail {
            field: Some(ptr.to_string()),
            message: message.into(),
            scope: None,
        })
    }

    /// Adds a `details` entry as given.
    pub fn push_detail(mut self, detail: ProblemDetail) -> Self {
        self.problem.details.push(detail);
        self
    }

    pub fn action_url(mut self, url: impl Into<String>) -> Self {
        self.problem.action_url = Some(url.into());
        self
    }

    /// Sets `Retry-After` in seconds.
    pub fn retry_after(self, secs: u64) -> Self {
        self.header(header::RETRY_AFTER, &secs.to_string())
    }

    pub fn header(mut self, name: HeaderName, value: &str) -> Self {
        let value = HeaderValue::from_str(value).expect("header values are ASCII");
        self.headers.retain(|(n, _)| n != name);
        self.headers.push((name, value));
        self
    }

    pub fn problem(&self) -> &Problem {
        &self.problem
    }
}

impl From<rusqlite::Error> for ProblemBuilder {
    fn from(e: rusqlite::Error) -> Self {
        tracing::error!(error = %e, "database error");
        internal_error()
    }
}

impl IntoResponse for ProblemBuilder {
    fn into_response(self) -> Response {
        let status =
            StatusCode::from_u16(self.problem.status).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
        let mut body = serde_json::to_value(&self.problem).expect("Problem serializes");
        // `action_url` is absent, not null, when there's nowhere to send a person.
        if let Some(obj) = body.as_object_mut()
            && obj.get("action_url").is_some_and(|v| v.is_null())
        {
            obj.remove("action_url");
        }
        let mut resp = (
            status,
            [(header::CONTENT_TYPE, PROBLEM_CONTENT_TYPE)],
            body.to_string(),
        )
            .into_response();
        for (name, value) in self.headers {
            resp.headers_mut().insert(name, value);
        }
        resp
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builds_the_problem_shape() {
        let p = problem(
            StatusCode::UNPROCESSABLE_ENTITY,
            ProblemCode::PaymentMethodRequired,
            "Payment method required",
            "Add a card.",
        )
        .field("/plan", "needs a card")
        .action_url("http://localhost/_mock/checkout/globex");
        let v = serde_json::to_value(p.problem()).unwrap();
        assert_eq!(
            v["type"],
            "https://bonsai.io/docs/api/errors/payment_method_required"
        );
        assert_eq!(v["status"], 422);
        assert_eq!(v["details"][0]["field"], "/plan");
        assert_eq!(v["action_url"], "http://localhost/_mock/checkout/globex");
    }
}
