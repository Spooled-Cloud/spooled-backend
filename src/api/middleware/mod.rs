//! API middleware

pub mod admin_auth;
pub mod auth;
pub mod client_ip;
pub mod limits;
pub mod plan_rate_limit;
pub mod security;
pub mod validation;
pub mod versioning;

// Re-export commonly used items
// Some exports are for external use by SDK consumers
#[allow(unused_imports)]
pub use client_ip::client_ip;
#[allow(unused_imports)]
pub use limits::{
    check_job_limits, check_payload_size, check_resource_limit, get_usage_info,
    increment_daily_jobs, LimitExceededResponse, UsageInfo,
};
#[allow(unused_imports)]
pub use plan_rate_limit::plan_rate_limit_middleware;
// `rate_limit` was removed: it was never mounted (only `plan_rate_limit` is
// wired into the router) and every one of its degraded paths returned `Ok`,
// i.e. ALLOW, while its doc comment claimed "No longer fails open".
#[allow(unused_imports)]
pub use security::{
    is_valid_job_id, is_valid_queue_name, sanitize_string, security_headers_middleware,
};
pub use validation::ValidatedJson;
#[allow(unused_imports)]
pub use versioning::{api_versioning_middleware, ApiVersion, CURRENT_API_VERSION};
