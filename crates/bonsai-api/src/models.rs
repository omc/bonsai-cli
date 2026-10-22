//! One struct per schema in `openapi/v1.yml`.

use crate::enums::*;
use jiff::Timestamp;
use serde::{Deserialize, Serialize};
use std::fmt;

/// Deserializes a field that must be present but may be `null`. Plain `Option<T>`
/// would silently accept a missing key.
pub(crate) fn required_nullable<'de, D, T>(d: D) -> Result<Option<T>, D::Error>
where
    D: serde::Deserializer<'de>,
    T: Deserialize<'de>,
{
    Option::<T>::deserialize(d)
}

/// A page of a list response (`ClusterList`, `PlanList`, ...).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Page<T> {
    pub data: Vec<T>,
    #[serde(deserialize_with = "required_nullable")]
    pub next_cursor: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PageInfo {
    #[serde(deserialize_with = "required_nullable")]
    pub next_cursor: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ApiRoot {
    pub name: String,
    pub version: String,
    pub docs_url: String,
    pub openapi_url: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Account {
    pub id: String,
    pub name: String,
    #[serde(deserialize_with = "required_nullable")]
    pub contact_email_override: Option<String>,
    pub created_at: Timestamp,
}

/// A pointer to another resource.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Ref {
    pub id: String,
    pub name: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Cluster {
    pub id: String,
    pub name: String,
    pub state: ClusterState,
    #[serde(deserialize_with = "required_nullable")]
    pub plan: Option<Ref>,
    pub region: Ref,
    #[serde(deserialize_with = "required_nullable")]
    pub space: Option<Ref>,
    pub release: Ref,
    pub single_tenant: bool,
    #[serde(deserialize_with = "required_nullable")]
    pub host: Option<String>,
    pub port: u16,
    pub stats: ClusterStats,
    #[serde(deserialize_with = "required_nullable")]
    pub latest_action: Option<ActionRef>,
    pub created_at: Timestamp,
    #[serde(deserialize_with = "required_nullable")]
    pub terminated_at: Option<Timestamp>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ClusterStats {
    #[serde(deserialize_with = "required_nullable")]
    pub docs: Option<u64>,
    #[serde(deserialize_with = "required_nullable")]
    pub shards: Option<u64>,
    #[serde(deserialize_with = "required_nullable")]
    pub bytes: Option<u64>,
    #[serde(deserialize_with = "required_nullable")]
    pub polled_at: Option<Timestamp>,
    #[serde(deserialize_with = "required_nullable")]
    pub limits: Option<ClusterLimits>,
}

/// A `None` limit means unlimited.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ClusterLimits {
    #[serde(deserialize_with = "required_nullable")]
    pub docs: Option<u64>,
    #[serde(deserialize_with = "required_nullable")]
    pub shards: Option<u64>,
    #[serde(deserialize_with = "required_nullable")]
    pub bytes: Option<u64>,
    #[serde(deserialize_with = "required_nullable")]
    pub daily_bandwidth_bytes: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ClusterCreate {
    pub name: String,
    pub plan: String,
    pub region: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub space: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub release: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ClusterUpdate {
    pub name: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlanChange {
    pub plan: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ActionRef {
    pub id: String,
    #[serde(rename = "type")]
    pub r#type: ActionType,
    pub state: ActionState,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Action {
    pub id: String,
    #[serde(rename = "type")]
    pub r#type: ActionType,
    pub state: ActionState,
    pub subject: ActionSubject,
    pub created_at: Timestamp,
    #[serde(deserialize_with = "required_nullable")]
    pub finished_at: Option<Timestamp>,
    #[serde(deserialize_with = "required_nullable")]
    pub error: Option<ActionError>,
    #[serde(deserialize_with = "required_nullable")]
    pub result: Option<ActionResult>,
    pub steps: Vec<ActionStep>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ActionError {
    pub code: ActionErrorCode,
    pub message: String,
    /// Set on `payment_failed`; absent otherwise.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub action_url: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ActionResult {
    #[serde(deserialize_with = "required_nullable")]
    pub cluster: Option<String>,
    #[serde(deserialize_with = "required_nullable")]
    pub credential: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ActionSubject {
    #[serde(rename = "type")]
    pub r#type: SubjectType,
    pub id: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ActionStep {
    pub state: ActionState,
    pub message: String,
    pub at: Timestamp,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Usage {
    pub from: Timestamp,
    pub to: Timestamp,
    pub samples: Vec<UsageSample>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UsageSample {
    pub at: Timestamp,
    pub bytes: u64,
    pub docs: u64,
    pub docs_deleted: u64,
    pub shards: u64,
    pub primary_shards: u64,
    pub health: Health,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Metrics {
    pub from: Timestamp,
    pub to: Timestamp,
    pub resolution: String,
    pub buckets: Vec<MetricBucket>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MetricBucket {
    pub at: Timestamp,
    pub requests: u64,
    pub writes: u64,
    pub responses: MetricResponses,
    pub duration_ms: DurationPercentiles,
    /// Always 17 buckets, in order.
    pub duration_histogram: Vec<HistogramBucket>,
    pub bytes_sent: u64,
    pub bytes_received: u64,
    pub queue_time_ms: f64,
    pub concurrency: f64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MetricResponses {
    pub status_2xx_3xx: u64,
    pub status_4xx: u64,
    pub status_5xx: u64,
}

/// All `None` when the bucket had no requests.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DurationPercentiles {
    #[serde(deserialize_with = "required_nullable")]
    pub p50: Option<f64>,
    #[serde(deserialize_with = "required_nullable")]
    pub p95: Option<f64>,
    #[serde(deserialize_with = "required_nullable")]
    pub p99: Option<f64>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HistogramBucket {
    pub min_ms: u64,
    pub count: u64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TopQueries {
    pub window: String,
    pub window_start_at: Timestamp,
    pub window_end_at: Timestamp,
    pub computed_at: Timestamp,
    pub queries: Vec<TopQuery>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TopQuery {
    pub query: String,
    pub volume: u64,
    pub avg_duration_ms: f64,
    pub avg_hits: f64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Credential {
    pub id: String,
    #[serde(deserialize_with = "required_nullable")]
    pub name: Option<String>,
    pub access_type: AccessType,
    pub allowed_actions: Vec<String>,
    pub allowed_index_globs: Vec<String>,
    pub allowed_cidrs: Vec<String>,
    pub created_at: Timestamp,
}

/// A credential as returned on creation, with its secret. `Debug` masks the password and URL.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CredentialWithSecret {
    #[serde(flatten)]
    pub credential: Credential,
    pub username: String,
    pub password: String,
    pub url: String,
}

impl fmt::Debug for CredentialWithSecret {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("CredentialWithSecret")
            .field("credential", &self.credential)
            .field("username", &self.username)
            .field("password", &"<redacted>")
            .field("url", &"<redacted>")
            .finish()
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CredentialCreate {
    pub name: String,
    pub access_type: AccessType,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub allowed_actions: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub allowed_index_globs: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub allowed_cidrs: Option<Vec<String>>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Plan {
    pub id: String,
    pub name: String,
    pub price_in_cents: u64,
    pub billing_interval_months: u32,
    pub single_tenant: bool,
    pub private_network: bool,
    /// The regions of `offers`.
    pub regions: Vec<String>,
    /// Every release of `offers`, in offer order.
    pub releases: Vec<String>,
    /// Where the plan can be created, and with which releases in each region.
    pub offers: Vec<PlanOffer>,
}

impl Plan {
    /// The releases this plan offers in `region`, in offer order (the first is
    /// a create's default), or `None` when it isn't offered there.
    pub fn releases_in(&self, region: &str) -> Option<&[String]> {
        self.offers
            .iter()
            .find(|o| o.region == region)
            .map(|o| o.releases.as_slice())
    }
}

/// A region a plan can be created in, and the releases offered there.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlanOffer {
    pub region: String,
    /// OpenSearch first, then newest first.
    pub releases: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Region {
    pub id: String,
    pub name: String,
    pub cloud: Cloud,
    pub cloud_region: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Space {
    pub id: String,
    pub name: String,
    pub region: String,
    pub private_network: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Release {
    pub id: String,
    pub name: String,
    pub engine: Engine,
    pub version: String,
}

/// `GET /token`: the calling token's own details.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Token {
    /// The token's public half: `<key>` in `bonsai_<key>_<secret>`.
    pub key: String,
    /// `Bonsai CLI on <hostname>` for a token a browser login made; `None` for one
    /// made in the dashboard.
    #[serde(deserialize_with = "required_nullable")]
    pub name: Option<String>,
    pub account: Ref,
    pub scopes: Vec<String>,
    /// `None` for a token that never expires.
    #[serde(deserialize_with = "required_nullable")]
    pub expires_at: Option<Timestamp>,
    pub created_at: Timestamp,
}

// The browser login.

/// `POST /cli/logins` body.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CliLoginStart {
    pub client_name: String,
    pub hostname: String,
    pub os: String,
}

/// `POST /cli/logins` response (RFC 8628's device authorization response). The
/// device code can be exchanged for the token until it's used, so `Debug` hides it.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CliLoginGrant {
    pub device_code: String,
    pub user_code: String,
    pub verification_uri: String,
    pub verification_uri_complete: String,
    pub expires_in: u64,
    pub interval: u64,
}

impl fmt::Debug for CliLoginGrant {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("CliLoginGrant")
            .field("device_code", &"<redacted>")
            .field("user_code", &self.user_code)
            .field("verification_uri", &self.verification_uri)
            .field("verification_uri_complete", &self.verification_uri_complete)
            .field("expires_in", &self.expires_in)
            .field("interval", &self.interval)
            .finish()
    }
}

/// `POST /cli/logins/token` response once the login is approved. `Debug` hides the token.
#[derive(Clone, Deserialize)]
pub struct CliLoginToken {
    pub token: secrecy::SecretString,
    pub account: Ref,
    pub scopes: Vec<String>,
    #[serde(default)]
    pub expires_at: Option<Timestamp>,
}

impl fmt::Debug for CliLoginToken {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("CliLoginToken")
            .field("token", &"<redacted>")
            .field("account", &self.account)
            .field("scopes", &self.scopes)
            .field("expires_at", &self.expires_at)
            .finish()
    }
}
