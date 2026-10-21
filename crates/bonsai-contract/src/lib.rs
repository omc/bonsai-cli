//! Loads the vendored `openapi/v1.yml` and validates HTTP responses and schema
//! values against it.
//!
//! Two modes. The default is lenient, as a client must be: extensible enums are
//! open and only the schema and the required headers are checked; it suits real
//! Bonsai API responses. [`Contract::load_strict`] is for the mock's own tests, where
//! a value outside the documented set can only be a bug:
//!
//! - every `x-extensible-enum` is closed;
//! - a Problem's `status` equals the HTTP status, and its `code` is one the
//!   response's description lists;
//! - `Retry-After`, where the response declares it and it is sent, is an integer
//!   of at least 1;
//! - `payment_method_required` and `payment_failed` (a Problem or an Action's
//!   `error`) carry `action_url`, `missing_scope` names its `scope` in `details`,
//!   and `503 metrics_unavailable` sends `Retry-After`.

mod convert;

use std::collections::HashMap;

use jsonschema::{Draft, Validator};
use serde_json::{Value, json};

const SPEC: &str = include_str!("../../../openapi/v1.yml");
const METHODS: &[&str] = &["get", "post", "put", "patch", "delete", "head", "options"];

#[derive(Debug, Clone)]
pub struct Operation {
    pub id: String,
    pub method: String,
    /// Path template, e.g. `/clusters/{cluster_id}`.
    pub path: String,
    pub statuses: Vec<u16>,
    pub action_types: Vec<String>,
}

pub struct ResponseCheck<'a> {
    pub method: &'a str,
    /// Concrete path without the `/v1` prefix.
    pub path: &'a str,
    pub status: u16,
    pub content_type: Option<&'a str>,
    pub headers: &'a [(String, String)],
    pub body: Option<&'a Value>,
}

#[derive(Debug, thiserror::Error)]
pub enum Violation {
    #[error("no operation matches {method} {path}")]
    UnknownOperation { method: String, path: String },
    #[error("{operation} documents no {status} response (documented: {documented:?})")]
    UndocumentedStatus {
        operation: String,
        status: u16,
        documented: Vec<u16>,
    },
    #[error("{operation} {status}: expected content type {expected:?}, got {actual:?}")]
    WrongContentType {
        operation: String,
        status: u16,
        expected: Option<String>,
        actual: Option<String>,
    },
    #[error("missing required response header {0}")]
    MissingHeader(String),
    #[error("schema violations: {}", .0.join("; "))]
    Schema(Vec<String>),
    /// A strict-mode rule the schema can't express.
    #[error("{0}")]
    Strict(String),
}

/// What the contract says about one `(operation, status)` pair.
struct ResponseSpec {
    content_type: Option<String>,
    /// Key into `Contract::validators` for the body schema.
    validator: Option<usize>,
    /// The Problem codes the response's description lists (in backticks).
    codes: Vec<String>,
    /// The response declares a `Retry-After` header.
    retry_after: bool,
}

pub struct Contract {
    version: String,
    operations: Vec<Operation>,
    responses: Vec<HashMap<u16, ResponseSpec>>,
    validators: Vec<Validator>,
    named: HashMap<String, usize>,
    strict: bool,
}

fn build(defs: &Value, schema: Value) -> Result<Validator, String> {
    let mut wrapped = match schema {
        Value::Object(m) => m,
        other => return Err(format!("schema is not an object: {other}")),
    };
    wrapped.insert("$defs".into(), defs.clone());
    jsonschema::options()
        .with_draft(Draft::Draft202012)
        .should_validate_formats(true)
        .build(&Value::Object(wrapped))
        .map_err(|e| e.to_string())
}

fn resolve<'a>(doc: &'a Value, v: &'a Value) -> Result<&'a Value, String> {
    match v.get("$ref").and_then(Value::as_str) {
        Some(r) => {
            let pointer = r
                .strip_prefix('#')
                .ok_or_else(|| format!("external $ref {r}"))?;
            doc.pointer(pointer)
                .ok_or_else(|| format!("dangling $ref {r}"))
        }
        None => Ok(v),
    }
}

impl Contract {
    /// The vendored contract, lenient (see the crate docs).
    pub fn load_default() -> Self {
        Self::from_yaml(SPEC).expect("vendored openapi/v1.yml must load")
    }

    /// The vendored contract, strict: for the mock's own responses.
    pub fn load_strict() -> Self {
        Self::from_yaml_with(SPEC, true).expect("vendored openapi/v1.yml must load")
    }

    pub fn from_yaml(src: &str) -> Result<Self, String> {
        Self::from_yaml_with(src, false)
    }

    pub fn from_yaml_with(src: &str, strict: bool) -> Result<Self, String> {
        let doc: Value = serde_yaml::from_str(src).map_err(|e| e.to_string())?;
        let known_codes: Vec<String> = doc
            .pointer("/components/schemas/Problem/properties/code/x-extensible-enum")
            .and_then(Value::as_array)
            .map(|a| {
                a.iter()
                    .filter_map(|v| v.as_str().map(String::from))
                    .collect()
            })
            .unwrap_or_default();
        let version = doc
            .pointer("/info/version")
            .and_then(Value::as_str)
            .ok_or("info.version missing")?
            .to_string();
        let defs = convert::convert_components(
            doc.pointer("/components/schemas").unwrap_or(&Value::Null),
            strict,
        );

        let mut validators = Vec::new();
        let mut named = HashMap::new();
        for name in defs.as_object().into_iter().flatten().map(|(k, _)| k) {
            let schema = json!({ "$ref": format!("#/$defs/{name}") });
            named.insert(name.clone(), validators.len());
            validators.push(build(&defs, schema).map_err(|e| format!("schema {name}: {e}"))?);
        }

        let mut operations = Vec::new();
        let mut responses = Vec::new();
        let paths = doc
            .get("paths")
            .and_then(Value::as_object)
            .ok_or("paths missing")?;
        for (path, item) in paths {
            for method in METHODS {
                let Some(op) = item.get(*method) else {
                    continue;
                };
                let id = op
                    .get("operationId")
                    .and_then(Value::as_str)
                    .ok_or_else(|| format!("{method} {path}: operationId missing"))?
                    .to_string();
                let action_types = op
                    .get("x-action-types")
                    .and_then(Value::as_array)
                    .map(|a| {
                        a.iter()
                            .filter_map(|v| v.as_str().map(String::from))
                            .collect()
                    })
                    .unwrap_or_default();
                let mut specs = HashMap::new();
                let resp = op
                    .get("responses")
                    .and_then(Value::as_object)
                    .ok_or_else(|| format!("{id}: responses missing"))?;
                for (code, r) in resp {
                    let status: u16 = code
                        .parse()
                        .map_err(|_| format!("{id}: unsupported response key {code}"))?;
                    let r = resolve(&doc, r).map_err(|e| format!("{id} {code}: {e}"))?;
                    let codes = documented_codes(r, &known_codes);
                    let retry_after = r
                        .get("headers")
                        .and_then(Value::as_object)
                        .is_some_and(|h| h.keys().any(|k| k.eq_ignore_ascii_case("retry-after")));
                    let spec = match r.get("content").and_then(Value::as_object) {
                        None => ResponseSpec {
                            content_type: None,
                            validator: None,
                            codes,
                            retry_after,
                        },
                        Some(content) => {
                            if content.len() != 1 {
                                return Err(format!(
                                    "{id} {code}: expected exactly one content type, found {}",
                                    content.len()
                                ));
                            }
                            let (ct, media) = content.iter().next().expect("len checked");
                            let validator = match media.get("schema") {
                                None => None,
                                Some(s) => {
                                    let converted = convert::convert_schema(s, strict);
                                    let idx = match converted
                                        .get("$ref")
                                        .and_then(Value::as_str)
                                        .and_then(|r| r.strip_prefix("#/$defs/"))
                                        .and_then(|n| named.get(n))
                                    {
                                        Some(i) => *i,
                                        None => {
                                            validators.push(
                                                build(&defs, converted)
                                                    .map_err(|e| format!("{id} {code}: {e}"))?,
                                            );
                                            validators.len() - 1
                                        }
                                    };
                                    Some(idx)
                                }
                            };
                            ResponseSpec {
                                content_type: Some(ct.clone()),
                                validator,
                                codes,
                                retry_after,
                            }
                        }
                    };
                    specs.insert(status, spec);
                }
                let mut statuses: Vec<u16> = specs.keys().copied().collect();
                statuses.sort_unstable();
                operations.push(Operation {
                    id,
                    method: method.to_uppercase(),
                    path: path.clone(),
                    statuses,
                    action_types,
                });
                responses.push(specs);
            }
        }
        Ok(Self {
            version,
            operations,
            responses,
            validators,
            named,
            strict,
        })
    }

    pub fn info_version(&self) -> &str {
        &self.version
    }

    pub fn operations(&self) -> &[Operation] {
        &self.operations
    }

    pub fn match_path(&self, method: &str, path: &str) -> Option<&Operation> {
        self.find(method, path).map(|i| &self.operations[i])
    }

    fn find(&self, method: &str, path: &str) -> Option<usize> {
        let actual: Vec<&str> = path.split('/').collect();
        self.operations.iter().position(|op| {
            op.method.eq_ignore_ascii_case(method) && {
                let template: Vec<&str> = op.path.split('/').collect();
                template.len() == actual.len()
                    && template.iter().zip(&actual).all(|(t, a)| {
                        if t.starts_with('{') && t.ends_with('}') {
                            !a.is_empty()
                        } else {
                            t == a
                        }
                    })
            }
        })
    }

    /// `r.path` must be the bare path: no query string, no trailing slash.
    pub fn validate_response(&self, r: &ResponseCheck) -> Result<&Operation, Violation> {
        let idx = self
            .find(r.method, r.path)
            .ok_or_else(|| Violation::UnknownOperation {
                method: r.method.to_string(),
                path: r.path.to_string(),
            })?;
        let op = &self.operations[idx];
        let spec =
            self.responses[idx]
                .get(&r.status)
                .ok_or_else(|| Violation::UndocumentedStatus {
                    operation: op.id.clone(),
                    status: r.status,
                    documented: op.statuses.clone(),
                })?;

        let has_header = |name: &str| r.headers.iter().any(|(k, _)| k.eq_ignore_ascii_case(name));
        if (200..300).contains(&r.status) {
            for name in ["RateLimit", "RateLimit-Policy"] {
                if !has_header(name) {
                    return Err(Violation::MissingHeader(name.into()));
                }
            }
        }
        if matches!(r.status, 201 | 202) && !has_header("Location") {
            return Err(Violation::MissingHeader("Location".into()));
        }

        let actual_ct = r.content_type.map(|c| {
            c.split(';')
                .next()
                .unwrap_or("")
                .trim()
                .to_ascii_lowercase()
        });
        let wrong_ct = |expected: Option<&String>| Violation::WrongContentType {
            operation: op.id.clone(),
            status: r.status,
            expected: expected.cloned(),
            actual: r.content_type.map(String::from),
        };
        match (&spec.content_type, r.body) {
            (None, None) => Ok(op),
            (None, Some(_)) => Err(Violation::Schema(vec![format!(
                "{} {} documents no body",
                op.id, r.status
            )])),
            (Some(expected), body) => {
                if (body.is_some() || actual_ct.is_some())
                    && actual_ct.as_deref() != Some(expected.as_str())
                {
                    return Err(wrong_ct(Some(expected)));
                }
                let body = body.ok_or_else(|| {
                    Violation::Schema(vec![format!("{} {} requires a body", op.id, r.status)])
                })?;
                if let Some(v) = spec.validator {
                    self.check(v, body)?;
                }
                if self.strict {
                    strict_checks(r, spec, expected == "application/problem+json", body)?;
                }
                Ok(op)
            }
        }
    }

    pub fn validate_schema(&self, schema_name: &str, value: &Value) -> Result<(), Violation> {
        let idx = self
            .named
            .get(schema_name)
            .ok_or_else(|| Violation::Schema(vec![format!("unknown schema {schema_name}")]))?;
        self.check(*idx, value)
    }

    fn check(&self, validator: usize, value: &Value) -> Result<(), Violation> {
        let errors: Vec<String> = self.validators[validator]
            .iter_errors(value)
            .map(|e| format!("{}: {e}", e.instance_path()))
            .collect();
        if errors.is_empty() {
            Ok(())
        } else {
            Err(Violation::Schema(errors))
        }
    }
}

/// The Problem codes `response`'s description names in backticks.
fn documented_codes(response: &Value, known: &[String]) -> Vec<String> {
    let description = response
        .get("description")
        .and_then(Value::as_str)
        .unwrap_or_default();
    description
        .split('`')
        .skip(1)
        .step_by(2)
        .filter(|w| known.iter().any(|k| k == w))
        .map(String::from)
        .collect()
}

fn header<'a>(r: &'a ResponseCheck, name: &str) -> Option<&'a str> {
    r.headers
        .iter()
        .find(|(k, _)| k.eq_ignore_ascii_case(name))
        .map(|(_, v)| v.as_str())
}

/// The strict-mode rules beyond the schema (see the crate docs).
fn strict_checks(
    r: &ResponseCheck,
    spec: &ResponseSpec,
    is_problem: bool,
    body: &Value,
) -> Result<(), Violation> {
    let fail = |m: String| Err(Violation::Strict(m));
    let retry_after = header(r, "Retry-After");
    if spec.retry_after
        && let Some(v) = retry_after
        && !v.trim().parse::<u64>().is_ok_and(|n| n >= 1)
    {
        return fail(format!("Retry-After {v:?} isn't an integer of at least 1"));
    }
    if is_problem {
        if body.get("status").and_then(Value::as_u64) != Some(u64::from(r.status)) {
            return fail(format!(
                "the Problem's status {} isn't the HTTP status {}",
                body.get("status").unwrap_or(&Value::Null),
                r.status
            ));
        }
        let code = body.get("code").and_then(Value::as_str).unwrap_or_default();
        if !spec.codes.is_empty() && !spec.codes.iter().any(|c| c == code) {
            return fail(format!(
                "code {code:?} isn't one this {} response documents ({})",
                r.status,
                spec.codes.join(", ")
            ));
        }
        match code {
            "payment_method_required" | "payment_failed" if !has_string(body, "action_url") => {
                return fail(format!("{code} without action_url"));
            }
            "missing_scope" => {
                let named = body
                    .get("details")
                    .and_then(Value::as_array)
                    .is_some_and(|d| d.iter().any(|d| has_string(d, "scope")));
                if !named {
                    return fail("missing_scope without details[].scope".into());
                }
            }
            "metrics_unavailable" if retry_after.is_none() => {
                return fail("metrics_unavailable without Retry-After".into());
            }
            _ => {}
        }
    }
    action_errors_have_action_urls(body)
}

fn has_string(v: &Value, key: &str) -> bool {
    v.get(key).is_some_and(Value::is_string)
}

/// Every Action `error` (anywhere in the body, e.g. a list's `data`) whose code is
/// `payment_failed` carries `action_url`.
fn action_errors_have_action_urls(v: &Value) -> Result<(), Violation> {
    match v {
        Value::Object(m) => {
            if let Some(err) = m.get("error")
                && err.get("code").and_then(Value::as_str) == Some("payment_failed")
                && !has_string(err, "action_url")
            {
                return Err(Violation::Strict(
                    "an Action's payment_failed error without action_url".into(),
                ));
            }
            m.values().try_for_each(action_errors_have_action_urls)
        }
        Value::Array(a) => a.iter().try_for_each(action_errors_have_action_urls),
        _ => Ok(()),
    }
}
