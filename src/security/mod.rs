//! Security utilities for Spooled Backend
//!
//! This module provides security-related functionality including:
//! - URL validation and SSRF protection
//! - Input sanitization
//! - Rate limiting helpers

pub mod http_client;
pub mod url_validator;

pub use http_client::build_outbound_http_client;
pub use url_validator::{validate_webhook_url, UrlValidationOptions};
