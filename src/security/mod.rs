//! Security utilities for Spooled Backend
//!
//! This module provides security-related functionality including:
//! - URL validation and SSRF protection
//! - Input sanitization
//! - Rate limiting helpers

pub mod url_validator;

pub use url_validator::{validate_webhook_url, UrlValidationOptions};
