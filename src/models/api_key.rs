//! API Key model and related types
//!
//! API keys are used for authentication and authorization of API requests.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sqlx::FromRow;
use validator::Validate;

/// Maximum number of queues per API key
pub const MAX_QUEUES_PER_KEY: usize = 50;

/// Deterministic, unsalted SHA-256 (hex) of an API key's plaintext.
///
/// Stored in `api_keys.lookup_hash` and used ONLY as a fast, unique index key so
/// authentication resolves a key with a single indexed row read plus one bcrypt
/// verify, instead of bcrypt-scanning every key that shares the non-selective
/// `sp_live_`/`sp_test_` prefix. This is NOT a security boundary: bcrypt still
/// verifies the credential after the lookup, so a (practically impossible) SHA-256
/// collision cannot authenticate as the wrong key.
pub fn api_key_lookup_hash(token: &str) -> String {
    use sha2::{Digest, Sha256};
    let mut hasher = Sha256::new();
    hasher.update(token.as_bytes());
    hex::encode(hasher.finalize())
}

/// Validate queue names in list
fn validate_queues(queues: &Vec<String>) -> Result<(), validator::ValidationError> {
    if queues.len() > MAX_QUEUES_PER_KEY {
        let mut err = validator::ValidationError::new("too_many_queues");
        err.message = Some(std::borrow::Cow::Owned(format!(
            "Maximum {} queues allowed per API key",
            MAX_QUEUES_PER_KEY
        )));
        return Err(err);
    }

    for queue in queues {
        if queue.is_empty() || queue.len() > 255 {
            let mut err = validator::ValidationError::new("invalid_queue_name");
            err.message = Some(std::borrow::Cow::Borrowed(
                "Queue name must be 1-255 characters",
            ));
            return Err(err);
        }
        if !queue
            .chars()
            .all(|c| c.is_alphanumeric() || c == '-' || c == '_' || c == '.' || c == '*')
        {
            let mut err = validator::ValidationError::new("invalid_queue_chars");
            err.message = Some(std::borrow::Cow::Borrowed(
                "Queue name can only contain alphanumeric characters, dashes, underscores, dots, or wildcard (*)"
            ));
            return Err(err);
        }
    }

    Ok(())
}

/// Validate expiration date is in the future
fn validate_expires_at(expires_at: &DateTime<Utc>) -> Result<(), validator::ValidationError> {
    if *expires_at <= Utc::now() {
        let mut err = validator::ValidationError::new("expires_in_past");
        err.message = Some(std::borrow::Cow::Borrowed(
            "Expiration date must be in the future",
        ));
        return Err(err);
    }
    Ok(())
}

/// API Key entity
///
#[derive(Debug, Clone, Serialize, FromRow)]
pub struct ApiKey {
    /// Unique identifier
    pub id: String,
    /// Organization that owns this key
    pub organization_id: String,
    /// Bcrypt hash of the key (never expose raw key)
    #[serde(skip_serializing)]
    pub key_hash: String,
    /// Key prefix for efficient lookup (first 8 chars)
    #[serde(skip_serializing)]
    pub key_prefix: Option<String>,
    /// Human-readable name
    pub name: String,
    /// Allowed queues (empty = all queues)
    pub queues: Vec<String>,
    /// Rate limit override (null = use org default)
    pub rate_limit: Option<i32>,
    /// Whether the key is active
    pub is_active: bool,
    /// Creation timestamp
    pub created_at: DateTime<Utc>,
    /// Last usage timestamp
    pub last_used: Option<DateTime<Utc>>,
    /// Expiration timestamp (null = never expires)
    pub expires_at: Option<DateTime<Utc>>,
}

/// Validate API key name for safe characters
fn validate_api_key_name(name: &str) -> Result<(), validator::ValidationError> {
    // Allow alphanumeric, spaces, hyphens, underscores, and common punctuation
    if !name.chars().all(|c| {
        c.is_alphanumeric() || c == ' ' || c == '-' || c == '_' || c == '.' || c == '(' || c == ')'
    }) {
        let mut err = validator::ValidationError::new("invalid_name_chars");
        err.message = Some(std::borrow::Cow::Borrowed(
            "API key name can only contain alphanumeric characters, spaces, hyphens, underscores, dots, and parentheses"
        ));
        return Err(err);
    }
    Ok(())
}

/// Request to create a new API key
///
#[derive(Debug, Deserialize, Validate)]
pub struct CreateApiKeyRequest {
    /// Human-readable name
    #[validate(length(min = 1, max = 100, message = "Name must be 1-100 characters"))]
    #[validate(custom(function = "validate_api_key_name"))]
    pub name: String,

    /// Allowed queues (empty = all queues)
    /// Now validated for safe characters and count
    #[validate(custom(function = "validate_queues"))]
    pub queues: Option<Vec<String>>,

    /// Rate limit override
    #[validate(range(
        min = 1,
        max = 10000,
        message = "Rate limit must be between 1 and 10000"
    ))]
    pub rate_limit: Option<i32>,

    /// Expiration timestamp
    /// Now validated to be in the future
    #[validate(custom(function = "validate_expires_at"))]
    pub expires_at: Option<DateTime<Utc>>,
}

/// Response after creating an API key
#[derive(Debug, Serialize)]
pub struct CreateApiKeyResponse {
    /// API key ID
    pub id: String,
    /// The raw API key (only shown once!)
    pub key: String,
    /// Human-readable name
    pub name: String,
    /// Allowed queues (empty = all queues)
    pub queues: Vec<String>,
    /// Creation timestamp
    pub created_at: DateTime<Utc>,
    /// Expiration timestamp
    pub expires_at: Option<DateTime<Utc>>,
}

/// API key summary for list responses (without sensitive data)
#[derive(Debug, Serialize)]
pub struct ApiKeySummary {
    pub id: String,
    pub name: String,
    pub queues: Vec<String>,
    pub rate_limit: Option<i32>,
    pub is_active: bool,
    pub created_at: DateTime<Utc>,
    pub last_used: Option<DateTime<Utc>>,
    pub expires_at: Option<DateTime<Utc>>,
}

impl From<ApiKey> for ApiKeySummary {
    fn from(key: ApiKey) -> Self {
        Self {
            id: key.id,
            name: key.name,
            queues: key.queues,
            rate_limit: key.rate_limit,
            is_active: key.is_active,
            created_at: key.created_at,
            last_used: key.last_used,
            expires_at: key.expires_at,
        }
    }
}

/// API key context extracted from authentication
#[derive(Debug, Clone)]
pub struct ApiKeyContext {
    /// API key ID
    pub api_key_id: String,
    /// Organization ID
    pub organization_id: String,
    /// Allowed queues (empty = all)
    pub queues: Vec<String>,
    /// Rate limit for this key
    pub rate_limit: Option<i32>,
}

impl ApiKeyContext {
    /// Whether this key is allowed to operate on the given queue.
    ///
    /// A key with an empty `queues` list (or one containing the `*` wildcard)
    /// may access every queue. Otherwise the queue must be explicitly listed.
    /// Used to enforce queue scoping consistently across enqueue, claim,
    /// bulk enqueue, worker registration, and the gRPC equivalents.
    pub fn can_access_queue(&self, queue_name: &str) -> bool {
        self.queues.is_empty() || self.queues.iter().any(|q| q == "*" || q == queue_name)
    }

    /// Whether this key may operate on a worker that can process the supplied
    /// queue set. Worker lifecycle actions affect every queue the worker can
    /// process, so a scoped key must be authorized for all of them.
    pub fn can_access_all_queues(&self, queues: &[String]) -> bool {
        if self.is_unrestricted() {
            return true;
        }
        !queues.is_empty() && queues.iter().all(|queue| self.can_access_queue(queue))
    }

    /// Whether this key has unrestricted (all-queue) access — an empty list or a
    /// `*` entry. Restricted keys must have every operation constrained to their
    /// listed queues.
    pub fn is_unrestricted(&self) -> bool {
        self.queues.is_empty() || self.queues.iter().any(|q| q == "*")
    }

    /// Whether this key may GRANT the `requested` queue set to a new/updated API
    /// key. An unrestricted key may grant anything; a restricted key may only
    /// grant a subset of its own queues and may never grant the `*` wildcard.
    /// Prevents a queue-scoped key from minting a broader key (privilege
    /// escalation).
    pub fn can_grant_queues(&self, requested: &[String]) -> bool {
        if self.is_unrestricted() {
            return true;
        }
        !requested.is_empty()
            && !requested.iter().any(|q| q == "*")
            && requested.iter().all(|q| self.can_access_queue(q))
    }

    /// The allowed-queue list to use as a SQL filter, or `None` when the key is
    /// unrestricted (no queue predicate needed). Lets read/list/delete queries
    /// restrict rows to the key's queues with `queue_name = ANY($list)`.
    pub fn queue_scope_filter(&self) -> Option<&[String]> {
        if self.is_unrestricted() {
            None
        } else {
            Some(&self.queues)
        }
    }
}

/// Validate optional API key name
fn validate_optional_api_key_name(name: &str) -> Result<(), validator::ValidationError> {
    validate_api_key_name(name)
}

/// Request to update an API key
///
#[derive(Debug, Deserialize, Validate)]
pub struct UpdateApiKeyRequest {
    /// Human-readable name
    #[validate(length(min = 1, max = 100, message = "Name must be 1-100 characters"))]
    #[validate(custom(function = "validate_optional_api_key_name"))]
    pub name: Option<String>,

    /// Allowed queues
    /// Now validated for safe characters and count
    #[validate(custom(function = "validate_queues"))]
    pub queues: Option<Vec<String>>,

    /// Rate limit override
    #[validate(range(
        min = 1,
        max = 10000,
        message = "Rate limit must be between 1 and 10000"
    ))]
    pub rate_limit: Option<i32>,

    /// Whether the key is active
    pub is_active: Option<bool>,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ctx(queues: &[&str]) -> ApiKeyContext {
        ApiKeyContext {
            api_key_id: "k".into(),
            organization_id: "o".into(),
            queues: queues.iter().map(|s| s.to_string()).collect(),
            rate_limit: None,
        }
    }

    #[test]
    fn test_queue_scope_access_and_grant() {
        let unrestricted_empty = ctx(&[]);
        let unrestricted_star = ctx(&["*"]);
        let scoped = ctx(&["emails", "billing"]);

        // Access.
        assert!(unrestricted_empty.can_access_queue("anything"));
        assert!(unrestricted_star.can_access_queue("anything"));
        assert!(scoped.can_access_queue("emails"));
        assert!(!scoped.can_access_queue("payroll"));
        assert!(scoped.can_access_all_queues(&["emails".to_string()]));
        assert!(scoped.can_access_all_queues(&["emails".to_string(), "billing".to_string()]));
        assert!(!scoped.can_access_all_queues(&[]));
        assert!(!scoped.can_access_all_queues(&["emails".to_string(), "payroll".to_string()]));
        assert!(unrestricted_empty.is_unrestricted());
        assert!(unrestricted_star.is_unrestricted());
        assert!(!scoped.is_unrestricted());

        // Grant: unrestricted keys may grant anything (incl. `*`).
        assert!(unrestricted_empty.can_grant_queues(&["*".into()]));
        assert!(unrestricted_star.can_grant_queues(&["payroll".into()]));

        // Grant: omitted and explicit empty request scopes both reach this helper as
        // an empty slice and must be rejected for scoped create/update callers.
        assert!(!scoped.can_grant_queues(&[]));
        assert!(scoped.can_grant_queues(&["emails".into()]));
        assert!(scoped.can_grant_queues(&["emails".into(), "billing".into()]));
        assert!(!scoped.can_grant_queues(&["*".into()]));
        assert!(!scoped.can_grant_queues(&["payroll".into()]));
        assert!(!scoped.can_grant_queues(&["emails".into(), "payroll".into()]));

        // Scope filter drives list restriction: None for unrestricted, Some for scoped.
        assert!(unrestricted_empty.queue_scope_filter().is_none());
        assert!(unrestricted_star.queue_scope_filter().is_none());
        assert_eq!(
            scoped.queue_scope_filter(),
            Some(&["emails".to_string(), "billing".to_string()][..])
        );
    }

    #[test]
    fn test_api_key_summary_excludes_hash() {
        let key = ApiKey {
            id: "key-1".to_string(),
            organization_id: "org-1".to_string(),
            key_hash: "secret_hash".to_string(),
            key_prefix: Some("sp_test_".to_string()),
            name: "Test Key".to_string(),
            queues: vec!["default".to_string()],
            rate_limit: Some(100),
            is_active: true,
            created_at: Utc::now(),
            last_used: None,
            expires_at: None,
        };

        let summary = ApiKeySummary::from(key);
        // key_hash should not be in summary
        let json = serde_json::to_string(&summary).unwrap();
        assert!(!json.contains("secret_hash"));
        assert!(json.contains("Test Key"));
    }

    #[test]
    fn test_api_key_lookup_hash() {
        let a = api_key_lookup_hash("sp_live_abcdefgh12345678");
        let b = api_key_lookup_hash("sp_live_abcdefgh12345678");
        let c = api_key_lookup_hash("sp_live_zzzzzzzz99999999");
        // Deterministic for the same key, distinct for different keys.
        assert_eq!(a, b);
        assert_ne!(a, c);
        // Full SHA-256 rendered as lowercase hex (64 chars) so the column is unique.
        assert_eq!(a.len(), 64);
        assert!(a.chars().all(|ch| ch.is_ascii_hexdigit()));
        // Differs from the 8-char key_prefix that every sp_live_ key shares.
        assert_ne!(&a[..8], "sp_live_");
    }

    #[test]
    fn test_validate_queues_valid() {
        let queues = vec!["default".to_string(), "emails".to_string()];
        assert!(validate_queues(&queues).is_ok());
    }

    #[test]
    fn test_validate_queues_wildcard() {
        let queues = vec!["*".to_string()];
        assert!(validate_queues(&queues).is_ok());
    }

    #[test]
    fn test_validate_queues_too_many() {
        let queues: Vec<String> = (0..=MAX_QUEUES_PER_KEY)
            .map(|i| format!("queue-{}", i))
            .collect();
        assert!(validate_queues(&queues).is_err());
    }

    #[test]
    fn test_validate_queues_empty_name() {
        let queues = vec!["".to_string()];
        assert!(validate_queues(&queues).is_err());
    }

    #[test]
    fn test_validate_queues_invalid_chars() {
        let queues = vec!["invalid@queue".to_string()];
        assert!(validate_queues(&queues).is_err());
    }

    #[test]
    fn test_validate_expires_at_future() {
        let future = Utc::now() + chrono::Duration::hours(1);
        assert!(validate_expires_at(&future).is_ok());
    }

    #[test]
    fn test_validate_expires_at_past() {
        let past = Utc::now() - chrono::Duration::hours(1);
        assert!(validate_expires_at(&past).is_err());
    }

    #[test]
    fn test_create_api_key_request_validation() {
        // Valid request
        let valid = CreateApiKeyRequest {
            name: "Production Key".to_string(),
            queues: Some(vec!["default".to_string()]),
            rate_limit: Some(1000),
            expires_at: None,
        };
        assert!(valid.validate().is_ok());

        // Name too short
        let short_name = CreateApiKeyRequest {
            name: "".to_string(),
            queues: None,
            rate_limit: None,
            expires_at: None,
        };
        assert!(short_name.validate().is_err());
    }

    #[test]
    fn test_create_api_key_request_rate_limit_bounds() {
        // Rate limit too low
        let low_rate = CreateApiKeyRequest {
            name: "Test Key".to_string(),
            queues: None,
            rate_limit: Some(0),
            expires_at: None,
        };
        assert!(low_rate.validate().is_err());

        // Rate limit at max (10000)
        let max_rate = CreateApiKeyRequest {
            name: "Test Key".to_string(),
            queues: None,
            rate_limit: Some(10000),
            expires_at: None,
        };
        assert!(max_rate.validate().is_ok());

        // Rate limit over max
        let over_max = CreateApiKeyRequest {
            name: "Test Key".to_string(),
            queues: None,
            rate_limit: Some(10001),
            expires_at: None,
        };
        assert!(over_max.validate().is_err());
    }

    #[test]
    fn test_create_api_key_response_serialization() {
        let response = CreateApiKeyResponse {
            id: "key-123".to_string(),
            key: "sp_live_abcdefgh12345678".to_string(),
            name: "My Key".to_string(),
            queues: vec!["billing".to_string()],
            created_at: Utc::now(),
            expires_at: None,
        };

        let json = serde_json::to_string(&response).unwrap();
        assert!(json.contains("key-123"));
        assert!(json.contains("sp_live_abcdefgh12345678"));
        assert!(json.contains("My Key"));
        assert!(json.contains("billing"));
    }

    #[test]
    fn test_can_access_queue_scoping() {
        let ctx = |queues: Vec<&str>| ApiKeyContext {
            api_key_id: "k".to_string(),
            organization_id: "o".to_string(),
            queues: queues.into_iter().map(String::from).collect(),
            rate_limit: None,
        };

        // Empty list = all queues allowed
        assert!(ctx(vec![]).can_access_queue("anything"));
        // Wildcard = all queues allowed
        assert!(ctx(vec!["*"]).can_access_queue("anything"));
        // Scoped key: only listed queues
        let scoped = ctx(vec!["billing", "emails"]);
        assert!(scoped.can_access_queue("billing"));
        assert!(scoped.can_access_queue("emails"));
        assert!(!scoped.can_access_queue("default"));
        assert!(!scoped.can_access_queue("Billing")); // case-sensitive
    }

    #[test]
    fn test_update_api_key_request_validation() {
        // Valid partial update
        let valid = UpdateApiKeyRequest {
            name: Some("Updated Name".to_string()),
            queues: None,
            rate_limit: None,
            is_active: Some(false),
        };
        assert!(valid.validate().is_ok());
    }

    #[test]
    fn test_api_key_summary_serialization() {
        let summary = ApiKeySummary {
            id: "key-456".to_string(),
            name: "Summary Key".to_string(),
            queues: vec!["default".to_string(), "emails".to_string()],
            rate_limit: Some(500),
            is_active: true,
            created_at: Utc::now(),
            last_used: None,
            expires_at: None,
        };

        let json = serde_json::to_string(&summary).unwrap();
        assert!(json.contains("key-456"));
        assert!(json.contains("Summary Key"));
        assert!(json.contains("default"));
    }

    #[test]
    fn test_max_queues_per_key_constant() {
        assert_eq!(MAX_QUEUES_PER_KEY, 50);
    }
}
