//! HTTP handlers for the API

/// Enforce that an API key may operate on `queue`.
///
/// Queue scope is an intra-org least-privilege boundary: a key with a non-empty,
/// non-`*` `queues` list may only touch those queues. Enqueue/claim enforced this
/// inline; read/control/indirect-enqueue paths did not, so a scoped key could read,
/// cancel, retry, purge, or (via workflows/schedules) enqueue into queues outside
/// its scope. This helper is applied uniformly on every path that names a queue.
/// Unrestricted keys (empty list or `*`) always pass.
pub fn require_queue_access(
    ctx: &crate::models::ApiKeyContext,
    queue: &str,
) -> crate::error::AppResult<()> {
    if ctx.can_access_queue(queue) {
        Ok(())
    } else {
        Err(crate::error::AppError::Authorization(format!(
            "API key does not have permission for queue '{}'",
            queue
        )))
    }
}

/// Enforce that an API key is unrestricted (empty queues or `*`).
///
/// Queue-scoped keys are for job/worker traffic only. Org settings, billing
/// portal, outgoing webhooks, and API-key administration stay on unrestricted keys.
pub fn require_unrestricted_key(
    ctx: &crate::models::ApiKeyContext,
    capability: &str,
) -> crate::error::AppResult<()> {
    if ctx.is_unrestricted() {
        Ok(())
    } else {
        Err(crate::error::AppError::Authorization(format!(
            "Queue-scoped API keys cannot {capability}"
        )))
    }
}

pub mod admin;
pub mod api_keys;
pub mod auth;
pub mod billing;
pub mod email_login;
pub mod health;
pub mod jobs;
pub mod metrics;
pub mod organizations;
pub mod outgoing_webhooks;
pub mod queues;
pub mod realtime;
pub mod schedules;
pub mod webhooks;
pub mod workers;
pub mod workflows;
