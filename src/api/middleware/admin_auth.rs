//! Admin authentication middleware
//!
//! Validates the X-Admin-Key header against the configured ADMIN_API_KEY.
//! Used to protect admin-only endpoints.

use axum::{
    body::Body,
    extract::State,
    http::{HeaderMap, Request, StatusCode},
    middleware::Next,
    response::{IntoResponse, Response},
    Json,
};
use serde::Serialize;

use crate::api::AppState;

/// Error response for admin authentication failures
#[derive(Debug, Serialize)]
pub struct AdminAuthError {
    pub error: String,
    pub message: String,
}

impl IntoResponse for AdminAuthError {
    fn into_response(self) -> Response {
        (StatusCode::UNAUTHORIZED, Json(self)).into_response()
    }
}

/// Middleware that requires a valid admin API key
///
/// Checks the X-Admin-Key header against the ADMIN_API_KEY environment variable.
/// Returns 401 Unauthorized if:
/// - No admin key is configured on the server
/// - No X-Admin-Key header is provided
/// - The provided key doesn't match
pub async fn require_admin(
    State(state): State<AppState>,
    headers: HeaderMap,
    request: Request<Body>,
    next: Next,
) -> Response {
    // Get configured admin key
    let configured_key = match &state.settings.registration.admin_api_key {
        Some(key) if !key.is_empty() => key,
        _ => {
            tracing::warn!("Admin endpoint accessed but ADMIN_API_KEY not configured");
            return AdminAuthError {
                error: "admin_not_configured".to_string(),
                message: "Admin access is not configured on this server".to_string(),
            }
            .into_response();
        }
    };

    // Get provided key from header
    let provided_key = match headers.get("X-Admin-Key") {
        Some(value) => match value.to_str() {
            Ok(key) => key,
            Err(_) => {
                return AdminAuthError {
                    error: "invalid_header".to_string(),
                    message: "Invalid X-Admin-Key header value".to_string(),
                }
                .into_response();
            }
        },
        None => {
            return AdminAuthError {
                error: "missing_admin_key".to_string(),
                message: "X-Admin-Key header is required for admin endpoints".to_string(),
            }
            .into_response();
        }
    };

    // Constant-time comparison to prevent timing attacks
    if !constant_time_compare(configured_key, provided_key) {
        tracing::warn!("Invalid admin key attempt");
        return AdminAuthError {
            error: "invalid_admin_key".to_string(),
            message: "Invalid admin API key".to_string(),
        }
        .into_response();
    }

    tracing::info!("Admin authenticated successfully");
    next.run(request).await
}

/// Constant-time string comparison to prevent timing attacks
///
/// SECURITY: Uses SHA-256 hash comparison to prevent both timing attacks
/// AND length disclosure. Both inputs are hashed first, ensuring:
/// 1. Comparison always operates on fixed-size (32 byte) values
/// 2. No early return based on length differences
/// 3. XOR comparison in constant time
fn constant_time_compare(a: &str, b: &str) -> bool {
    use sha2::{Digest, Sha256};

    // Hash both values first to prevent length disclosure
    let mut hasher_a = Sha256::new();
    hasher_a.update(a.as_bytes());
    let hash_a = hasher_a.finalize();

    let mut hasher_b = Sha256::new();
    hasher_b.update(b.as_bytes());
    let hash_b = hasher_b.finalize();

    // Constant-time comparison of hashes
    let mut result = 0u8;
    for (x, y) in hash_a.iter().zip(hash_b.iter()) {
        result |= x ^ y;
    }
    result == 0
}

/// Check if admin key is configured
pub fn is_admin_configured(state: &AppState) -> bool {
    state
        .settings
        .registration
        .admin_api_key
        .as_ref()
        .map(|k| !k.is_empty())
        .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_constant_time_compare_equal() {
        assert!(constant_time_compare("secret", "secret"));
        assert!(constant_time_compare("", ""));
        assert!(constant_time_compare("a", "a"));
        assert!(constant_time_compare(
            "super-long-admin-key-1234567890",
            "super-long-admin-key-1234567890"
        ));
    }

    #[test]
    fn test_constant_time_compare_different() {
        // Different content
        assert!(!constant_time_compare("secret", "Secret"));
        assert!(!constant_time_compare("abc", "abd"));

        // Different lengths (should still work securely via hash comparison)
        assert!(!constant_time_compare("secret", "secre"));
        assert!(!constant_time_compare("secret", "secretx"));
        assert!(!constant_time_compare("short", "much-longer-string"));
    }

    #[test]
    fn test_constant_time_compare_timing_safety() {
        // These comparisons should take approximately the same time
        // regardless of where/if differences occur or length differences
        // This is ensured by the SHA-256 hash comparison approach
        let _ = constant_time_compare("aaaaaaaaaa", "aaaaaaaaaa"); // All same
        let _ = constant_time_compare("aaaaaaaaaa", "baaaaaaaaa"); // First char diff
        let _ = constant_time_compare("aaaaaaaaaa", "aaaaaaaaab"); // Last char diff
        let _ = constant_time_compare("aaaaaaaaaa", "aaa"); // Different length
    }
}
