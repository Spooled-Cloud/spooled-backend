//! Organization model and related types

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sqlx::FromRow;
use validator::Validate;

/// Organization entity representing a tenant in the system
#[derive(Debug, Clone, Serialize, Deserialize, FromRow)]
pub struct Organization {
    /// Unique identifier
    pub id: String,
    /// Organization name
    pub name: String,
    /// URL-friendly slug (unique)
    pub slug: String,
    /// Billing plan tier (free, starter, pro, enterprise)
    pub plan_tier: String,
    /// Email for billing notifications
    pub billing_email: Option<String>,
    /// Additional settings as JSON
    pub settings: serde_json::Value,
    /// Creation timestamp
    pub created_at: DateTime<Utc>,
    /// Last update timestamp
    pub updated_at: DateTime<Utc>,
}

/// Request to create a new organization
///
#[derive(Debug, Deserialize, Validate)]
pub struct CreateOrganizationRequest {
    /// Organization name (3-100 characters)
    #[validate(length(min = 3, max = 100, message = "Name must be 3-100 characters"))]
    pub name: String,
    /// URL-friendly slug (3-50 characters, lowercase alphanumeric and hyphens)
    #[validate(
        length(min = 3, max = 50, message = "Slug must be 3-50 characters"),
        regex(
            path = "*SLUG_REGEX",
            message = "Slug must be lowercase alphanumeric with hyphens"
        ),
        custom(function = "validate_slug_not_reserved")
    )]
    pub slug: String,
    /// Optional billing email
    #[validate(email(message = "Invalid email format"))]
    pub billing_email: Option<String>,
}

/// Maximum organization settings size (64KB)
const MAX_ORG_SETTINGS_SIZE: usize = 64 * 1024;

/// Validate organization settings size
fn validate_org_settings_size(
    settings: &serde_json::Value,
) -> Result<(), validator::ValidationError> {
    let json_str = serde_json::to_string(settings).unwrap_or_default();
    if json_str.len() > MAX_ORG_SETTINGS_SIZE {
        let mut err = validator::ValidationError::new("settings_too_large");
        err.message = Some(std::borrow::Cow::Owned(format!(
            "Settings too large: {} bytes (max: {} bytes)",
            json_str.len(),
            MAX_ORG_SETTINGS_SIZE
        )));
        return Err(err);
    }
    Ok(())
}

/// Request to update an organization
///
#[derive(Debug, Deserialize, Validate)]
pub struct UpdateOrganizationRequest {
    /// Organization name (3-100 characters)
    #[validate(length(min = 3, max = 100, message = "Name must be 3-100 characters"))]
    pub name: Option<String>,
    /// Optional billing email
    #[validate(email(message = "Invalid email format"))]
    pub billing_email: Option<String>,
    /// Additional settings (max 64KB)
    /// Now validated for size
    #[validate(custom(function = "validate_org_settings_size"))]
    pub settings: Option<serde_json::Value>,
}

/// Organization summary for API responses
#[derive(Debug, Serialize)]
pub struct OrganizationSummary {
    pub id: String,
    pub name: String,
    pub slug: String,
    pub plan_tier: String,
    pub created_at: DateTime<Utc>,
}

impl From<Organization> for OrganizationSummary {
    fn from(org: Organization) -> Self {
        Self {
            id: org.id,
            name: org.name,
            slug: org.slug,
            plan_tier: org.plan_tier,
            created_at: org.created_at,
        }
    }
}

/// Plan tier enumeration
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum PlanTier {
    #[default]
    Free,
    Starter,
    Pro,
    Enterprise,
}

impl std::fmt::Display for PlanTier {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            PlanTier::Free => write!(f, "free"),
            PlanTier::Starter => write!(f, "starter"),
            PlanTier::Pro => write!(f, "pro"),
            PlanTier::Enterprise => write!(f, "enterprise"),
        }
    }
}

lazy_static::lazy_static! {
    /// Regex for validating organization slugs
    /// Made regex more restrictive - requires at least 3 chars and no consecutive hyphens
    static ref SLUG_REGEX: regex::Regex = regex::Regex::new(r"^[a-z][a-z0-9]*(?:-[a-z0-9]+)*$").unwrap();
}

/// List of reserved slugs that cannot be used for organizations
/// These are reserved to prevent confusion with system endpoints
pub const RESERVED_SLUGS: &[&str] = &[
    "admin",
    "api",
    "app",
    "auth",
    "dashboard",
    "docs",
    "help",
    "internal",
    "login",
    "logout",
    "metrics",
    "root",
    "settings",
    "signup",
    "status",
    "support",
    "system",
    "test",
    "www",
];

/// Validate slug is not reserved
pub fn validate_slug_not_reserved(slug: &str) -> Result<(), validator::ValidationError> {
    if RESERVED_SLUGS.contains(&slug) {
        let mut err = validator::ValidationError::new("reserved_slug");
        err.message = Some(std::borrow::Cow::Borrowed(
            "This slug is reserved and cannot be used",
        ));
        return Err(err);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_slug_regex() {
        assert!(SLUG_REGEX.is_match("my-org"));
        assert!(SLUG_REGEX.is_match("my-cool-org-123"));
        assert!(SLUG_REGEX.is_match("org"));
        assert!(!SLUG_REGEX.is_match("-invalid"));
        assert!(!SLUG_REGEX.is_match("invalid-"));
        assert!(!SLUG_REGEX.is_match("UPPERCASE"));
        assert!(!SLUG_REGEX.is_match("with spaces"));
    }

    #[test]
    fn test_plan_tier_display() {
        assert_eq!(PlanTier::Free.to_string(), "free");
        assert_eq!(PlanTier::Enterprise.to_string(), "enterprise");
    }
}
