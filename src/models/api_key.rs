//! API Key model and related types
//!
//! API keys are used for authentication and authorization of API requests.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sqlx::FromRow;
use validator::Validate;

/// Maximum number of queues per API key
pub const MAX_QUEUES_PER_KEY: usize = 50;

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

    #[test]
    fn test_api_key_summary_excludes_hash() {
        let key = ApiKey {
            id: "key-1".to_string(),
            organization_id: "org-1".to_string(),
            key_hash: "secret_hash".to_string(),
            key_prefix: Some("sk_test_".to_string()),
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
}
