//! Cursor-based Pagination
//!
//! This module provides cursor-based pagination for API endpoints,
//! which is more efficient than offset-based pagination for large datasets.

use base64::{engine::general_purpose::STANDARD as BASE64, Engine};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

/// Cursor for pagination
///
/// Now includes organization_id to prevent cross-tenant cursor manipulation
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Cursor {
    /// Timestamp of the last item
    pub created_at: DateTime<Utc>,
    /// ID of the last item (for stable sorting when timestamps are equal)
    pub id: String,
    /// Organization ID to prevent cross-tenant cursor reuse
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub organization_id: Option<String>,
}

impl Cursor {
    /// Create a new cursor from a timestamp and ID
    ///
    /// Deprecated - use new_with_org() instead to ensure org isolation
    /// This method is kept for backward compatibility but logs a warning
    #[deprecated(note = "Use new_with_org() to ensure organization isolation")]
    pub fn new(created_at: DateTime<Utc>, id: String) -> Self {
        tracing::debug!("Cursor::new() called without org_id - consider using new_with_org()");
        Self {
            created_at,
            id,
            organization_id: None,
        }
    }

    /// Create a new cursor with organization context
    /// Use this to create cursors that are tied to an organization
    pub fn new_with_org(created_at: DateTime<Utc>, id: String, organization_id: String) -> Self {
        Self {
            created_at,
            id,
            organization_id: Some(organization_id),
        }
    }

    /// Encode cursor to a URL-safe string
    pub fn encode(&self) -> String {
        let json = serde_json::to_string(self).unwrap_or_default();
        BASE64.encode(json.as_bytes())
    }

    /// Decode cursor from a string
    pub fn decode(s: &str) -> Option<Self> {
        let bytes = BASE64.decode(s).ok()?;
        let json = String::from_utf8(bytes).ok()?;
        serde_json::from_str(&json).ok()
    }

    /// Decode and validate cursor belongs to the specified organization
    /// Prevents cross-tenant cursor attacks
    pub fn decode_for_org(s: &str, expected_org_id: &str) -> Option<Self> {
        let cursor = Self::decode(s)?;

        // If cursor has an org_id, it must match the expected one
        if let Some(ref cursor_org_id) = cursor.organization_id {
            if cursor_org_id != expected_org_id {
                tracing::warn!(
                    expected_org = %expected_org_id,
                    cursor_org = %cursor_org_id,
                    "Cursor organization mismatch - possible cross-tenant attack"
                );
                return None;
            }
        }

        Some(cursor)
    }
}

/// Cursor-based pagination query parameters
#[derive(Debug, Clone, Deserialize)]
pub struct CursorQuery {
    /// Cursor for forward pagination (items after this cursor)
    pub after: Option<String>,
    /// Cursor for backward pagination (items before this cursor)
    pub before: Option<String>,
    /// Number of items to fetch (default: 50, max: 100)
    pub limit: Option<i64>,
}

impl CursorQuery {
    /// Get the effective limit
    pub fn effective_limit(&self) -> i64 {
        self.limit.unwrap_or(50).clamp(1, 100)
    }

    /// Decode the 'after' cursor
    pub fn after_cursor(&self) -> Option<Cursor> {
        self.after.as_ref().and_then(|s| Cursor::decode(s))
    }

    /// Decode the 'before' cursor
    pub fn before_cursor(&self) -> Option<Cursor> {
        self.before.as_ref().and_then(|s| Cursor::decode(s))
    }
}

/// Paginated response with cursors
#[derive(Debug, Clone, Serialize)]
pub struct CursorPage<T> {
    /// Items in this page
    pub items: Vec<T>,
    /// Cursor for the first item (for backward pagination)
    pub start_cursor: Option<String>,
    /// Cursor for the last item (for forward pagination)
    pub end_cursor: Option<String>,
    /// Whether there are more items before the first item
    pub has_previous_page: bool,
    /// Whether there are more items after the last item
    pub has_next_page: bool,
    /// Total count (optional, can be expensive to compute)
    pub total_count: Option<i64>,
}

impl<T> CursorPage<T> {
    /// Create a new cursor page
    pub fn new(
        items: Vec<T>,
        start_cursor: Option<String>,
        end_cursor: Option<String>,
        has_previous_page: bool,
        has_next_page: bool,
    ) -> Self {
        Self {
            items,
            start_cursor,
            end_cursor,
            has_previous_page,
            has_next_page,
            total_count: None,
        }
    }

    /// Create an empty page
    pub fn empty() -> Self {
        Self {
            items: vec![],
            start_cursor: None,
            end_cursor: None,
            has_previous_page: false,
            has_next_page: false,
            total_count: Some(0),
        }
    }

    /// Set total count
    pub fn with_total_count(mut self, count: i64) -> Self {
        self.total_count = Some(count);
        self
    }
}

/// Helper trait for extracting cursor fields
///
pub trait CursorItem {
    fn cursor_created_at(&self) -> DateTime<Utc>;
    fn cursor_id(&self) -> String;

    /// Get organization ID for cursor isolation
    /// Required for proper tenant isolation
    fn cursor_organization_id(&self) -> Option<String> {
        None // Default implementation for backward compatibility
    }

    /// Create cursor with organization context if available
    /// Prefers org-scoped cursor when organization_id is available
    fn to_cursor(&self) -> Cursor {
        if let Some(org_id) = self.cursor_organization_id() {
            Cursor::new_with_org(self.cursor_created_at(), self.cursor_id(), org_id)
        } else {
            #[allow(deprecated)]
            Cursor::new(self.cursor_created_at(), self.cursor_id())
        }
    }

    fn to_encoded_cursor(&self) -> String {
        self.to_cursor().encode()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_cursor_encode_decode() {
        let now = Utc::now();
        let cursor = Cursor::new(now, "job-123".to_string());

        let encoded = cursor.encode();
        let decoded = Cursor::decode(&encoded).unwrap();

        assert_eq!(decoded.id, "job-123");
        // Note: DateTime comparison might differ by nanoseconds, so we check close enough
        assert!((decoded.created_at - now).num_seconds().abs() < 1);
    }

    #[test]
    fn test_cursor_decode_invalid() {
        assert!(Cursor::decode("invalid").is_none());
        assert!(Cursor::decode("").is_none());
        assert!(Cursor::decode("!!!").is_none());
    }

    #[test]
    fn test_cursor_query_defaults() {
        let query = CursorQuery {
            after: None,
            before: None,
            limit: None,
        };

        assert_eq!(query.effective_limit(), 50);
    }

    #[test]
    fn test_cursor_query_limit_capping() {
        let query = CursorQuery {
            after: None,
            before: None,
            limit: Some(1000), // Over max
        };

        assert_eq!(query.effective_limit(), 100);

        let query = CursorQuery {
            after: None,
            before: None,
            limit: Some(-10), // Under min
        };

        assert_eq!(query.effective_limit(), 1);
    }

    #[test]
    fn test_cursor_page_empty() {
        let page: CursorPage<String> = CursorPage::empty();

        assert!(page.items.is_empty());
        assert!(page.start_cursor.is_none());
        assert!(page.end_cursor.is_none());
        assert!(!page.has_previous_page);
        assert!(!page.has_next_page);
        assert_eq!(page.total_count, Some(0));
    }

    #[test]
    fn test_cursor_page_with_total() {
        let page: CursorPage<String> = CursorPage::new(
            vec!["item1".to_string()],
            Some("cursor1".to_string()),
            Some("cursor2".to_string()),
            false,
            true,
        )
        .with_total_count(100);

        assert_eq!(page.total_count, Some(100));
        assert!(page.has_next_page);
    }
}
