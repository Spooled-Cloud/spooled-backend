//! API middleware

pub mod admin_auth;
pub mod auth;
pub mod limits;
pub mod rate_limit;
pub mod security;
pub mod validation;
pub mod versioning;

// Re-export commonly used items
// Some exports are for external use by SDK consumers
#[allow(unused_imports)]
pub use limits::{
    check_job_limits, check_payload_size, check_resource_limit, get_usage_info,
    increment_daily_jobs, LimitExceededResponse, UsageInfo,
};
#[allow(unused_imports)]
pub use rate_limit::{rate_limit_middleware, RateLimitConfig, RateLimitState};
#[allow(unused_imports)]
pub use security::{
    is_valid_job_id, is_valid_queue_name, sanitize_string, security_headers_middleware,
};
pub use validation::ValidatedJson;
#[allow(unused_imports)]
pub use versioning::{api_versioning_middleware, ApiVersion, CURRENT_API_VERSION};
