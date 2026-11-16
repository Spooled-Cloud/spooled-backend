//! Security utilities for Spooled Backend
//!
//! This module provides security-related functionality including:
//! - URL validation and SSRF protection
//! - Input sanitization
//! - Rate limiting helpers

pub mod http_client;
pub mod url_validator;

pub use http_client::build_outbound_http_client;
pub use url_validator::{
    set_url_policy_environment, validate_webhook_url, validate_webhook_url_resolved,
    UrlValidationOptions,
};
