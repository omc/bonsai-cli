//! Open-set string enums. Every one keeps values it doesn't know as `Unknown(String)`.

/// Declares a string enum that tolerates values added by a newer server.
#[macro_export]
macro_rules! extensible_enum {
    ($(#[$m:meta])* $name:ident { $($variant:ident => $wire:literal),+ $(,)? }) => {
        $(#[$m])*
        #[derive(Debug, Clone, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
        #[non_exhaustive]
        pub enum $name {
            $(#[serde(rename = $wire)] $variant,)+
            #[serde(untagged)]
            Unknown(String),
        }
        impl $name {
            pub fn as_str(&self) -> &str {
                match self {
                    $(Self::$variant => $wire,)+
                    Self::Unknown(s) => s,
                }
            }
        }
        impl std::fmt::Display for $name {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.write_str(self.as_str())
            }
        }
    };
}

extensible_enum!(ClusterState { Draft => "draft", Provisioning => "provisioning", Ready => "ready", Failed => "failed", ReadOnly => "read_only", Disabled => "disabled", Deprovisioning => "deprovisioning", Deprovisioned => "deprovisioned" });
extensible_enum!(ActionType { Provision => "cluster.provision", Deprovision => "cluster.deprovision", ChangePlan => "cluster.change_plan", ReadOnly => "cluster.read_only", Disable => "cluster.disable", Enable => "cluster.enable" });
extensible_enum!(ActionState { New => "new", Running => "running", Success => "success", Error => "error" });
extensible_enum!(ActionErrorCode { PaymentFailed => "payment_failed", TimedOut => "timed_out", ActionFailed => "action_failed" });
extensible_enum!(SubjectType { Cluster => "cluster" });
extensible_enum!(Health { Green => "green", Yellow => "yellow", Red => "red" });
extensible_enum!(AccessType { Rw => "rw", Ro => "ro", Rwm => "rwm", Acl => "acl" });
extensible_enum!(Cloud { Aws => "aws", Gcp => "gcp" });
extensible_enum!(Engine { Elasticsearch => "elasticsearch", Opensearch => "opensearch" });
extensible_enum!(ProblemCode { Unauthorized => "unauthorized", MissingScope => "missing_scope", FeatureNotEnabled => "feature_not_enabled", RateLimited => "rate_limited", TooManyFailedAuthentications => "too_many_failed_authentications", InvalidJson => "invalid_json", BodyRequired => "body_required", InvalidParameter => "invalid_parameter", NameConfirmationRequired => "name_confirmation_required", NameMismatch => "name_mismatch", NotFound => "not_found", ValidationFailed => "validation_failed", PaymentMethodRequired => "payment_method_required", PaymentFailed => "payment_failed", ClusterTerminated => "cluster_terminated", PlanChangeRefused => "plan_change_refused", ActionInProgress => "action_in_progress", IdempotencyKeyReused => "idempotency_key_reused", IdempotencyKeyInUse => "idempotency_key_in_use", IdempotencyReplayUnavailable => "idempotency_replay_unavailable", MetricsUnavailable => "metrics_unavailable", InternalError => "internal_error", AuthorizationPending => "authorization_pending", SlowDown => "slow_down", AccessDenied => "access_denied", ExpiredToken => "expired_token" });

impl ActionState {
    /// Only `Success` and `Error` end an action; an unknown state keeps polling.
    pub fn is_terminal(&self) -> bool {
        matches!(self, Self::Success | Self::Error)
    }
}
