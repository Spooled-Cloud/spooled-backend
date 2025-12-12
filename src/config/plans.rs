//! Plan limits configuration
//!
//! Defines resource limits for each plan tier (free, starter, pro, enterprise).
//! Free tier is the default for all new organizations and is intentionally
//! restrictive to encourage upgrades.

use serde::{Deserialize, Serialize};

/// Resource limits for a plan tier
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PlanLimits {
    /// Plan tier name
    pub tier: String,
    /// Display name for the plan
    pub display_name: String,

    // Job limits
    /// Maximum jobs that can be created per day (None = unlimited)
    pub max_jobs_per_day: Option<u64>,
    /// Maximum concurrent active jobs (pending + processing + scheduled)
    pub max_active_jobs: Option<u64>,

    // Resource limits
    /// Maximum number of queues
    pub max_queues: Option<u32>,
    /// Maximum number of workers
    pub max_workers: Option<u32>,
    /// Maximum number of API keys
    pub max_api_keys: Option<u32>,
    /// Maximum number of schedules
    pub max_schedules: Option<u32>,
    /// Maximum number of workflows (0 = disabled)
    pub max_workflows: Option<u32>,
    /// Maximum number of outgoing webhooks
    pub max_webhooks: Option<u32>,

    // Size limits
    /// Maximum job payload size in bytes
    pub max_payload_size_bytes: usize,

    // Rate limits
    /// Requests per second limit
    pub rate_limit_requests_per_second: u32,
    /// Rate limit burst size
    pub rate_limit_burst: u32,

    // Retention
    /// Job data retention in days
    pub job_retention_days: u32,
    /// Job history retention in days
    pub history_retention_days: u32,
}

impl PlanLimits {
    /// Free tier - very restrictive, default for all new organizations
    ///
    /// Allows users to prototype and test, but encourages upgrade for
    /// any real production workload.
    pub fn free() -> Self {
        Self {
            tier: "free".to_string(),
            display_name: "Free".to_string(),

            // Very limited job capacity
            max_jobs_per_day: Some(1_000),
            max_active_jobs: Some(10),

            // Minimal resources
            max_queues: Some(2),
            max_workers: Some(1),
            max_api_keys: Some(2),
            max_schedules: Some(1),
            max_workflows: Some(0), // Disabled on free tier
            max_webhooks: Some(1),

            // Small payload
            max_payload_size_bytes: 64 * 1024, // 64 KB

            // Throttled
            rate_limit_requests_per_second: 5,
            rate_limit_burst: 10,

            // Short retention
            job_retention_days: 3,
            history_retention_days: 1,
        }
    }

    /// Starter tier - for small teams and projects
    pub fn starter() -> Self {
        Self {
            tier: "starter".to_string(),
            display_name: "Starter".to_string(),

            max_jobs_per_day: Some(10_000),
            max_active_jobs: Some(500),

            max_queues: Some(10),
            max_workers: Some(5),
            max_api_keys: Some(5),
            max_schedules: Some(10),
            max_workflows: Some(5),
            max_webhooks: Some(5),

            max_payload_size_bytes: 256 * 1024, // 256 KB

            rate_limit_requests_per_second: 25,
            rate_limit_burst: 50,

            job_retention_days: 14,
            history_retention_days: 7,
        }
    }

    /// Pro tier - for production workloads
    pub fn pro() -> Self {
        Self {
            tier: "pro".to_string(),
            display_name: "Pro".to_string(),

            max_jobs_per_day: Some(100_000),
            max_active_jobs: Some(5_000),

            max_queues: Some(50),
            max_workers: Some(25),
            max_api_keys: Some(25),
            max_schedules: Some(50),
            max_workflows: Some(25),
            max_webhooks: Some(20),

            max_payload_size_bytes: 1024 * 1024, // 1 MB

            rate_limit_requests_per_second: 100,
            rate_limit_burst: 200,

            job_retention_days: 30,
            history_retention_days: 30,
        }
    }

    /// Enterprise tier - unlimited everything
    pub fn enterprise() -> Self {
        Self {
            tier: "enterprise".to_string(),
            display_name: "Enterprise".to_string(),

            // Unlimited
            max_jobs_per_day: None,
            max_active_jobs: None,

            max_queues: None,
            max_workers: None,
            max_api_keys: None,
            max_schedules: None,
            max_workflows: None,
            max_webhooks: None,

            max_payload_size_bytes: 5 * 1024 * 1024, // 5 MB

            rate_limit_requests_per_second: 500,
            rate_limit_burst: 1000,

            job_retention_days: 90,
            history_retention_days: 90,
        }
    }

    /// Get plan limits by tier name
    ///
    /// Defaults to Free tier for unknown tier names
    pub fn for_tier(tier: &str) -> Self {
        match tier.to_lowercase().as_str() {
            "starter" => Self::starter(),
            "pro" => Self::pro(),
            "enterprise" => Self::enterprise(),
            _ => Self::free(), // Default to Free for unknown tiers
        }
    }

    /// Get plan limits with custom overrides merged in
    ///
    /// Custom overrides take precedence over plan defaults.
    /// Only specified fields are overridden; null/missing values use defaults.
    pub fn for_tier_with_overrides(tier: &str, custom_limits: Option<&serde_json::Value>) -> Self {
        let mut limits = Self::for_tier(tier);

        if let Some(overrides) = custom_limits {
            if let Some(obj) = overrides.as_object() {
                // Override each limit if specified
                if let Some(v) = obj.get("max_jobs_per_day") {
                    limits.max_jobs_per_day = v.as_u64();
                }
                if let Some(v) = obj.get("max_active_jobs") {
                    limits.max_active_jobs = v.as_u64();
                }
                if let Some(v) = obj.get("max_queues") {
                    limits.max_queues = v.as_u64().map(|n| n as u32);
                }
                if let Some(v) = obj.get("max_workers") {
                    limits.max_workers = v.as_u64().map(|n| n as u32);
                }
                if let Some(v) = obj.get("max_api_keys") {
                    limits.max_api_keys = v.as_u64().map(|n| n as u32);
                }
                if let Some(v) = obj.get("max_schedules") {
                    limits.max_schedules = v.as_u64().map(|n| n as u32);
                }
                if let Some(v) = obj.get("max_workflows") {
                    limits.max_workflows = v.as_u64().map(|n| n as u32);
                }
                if let Some(v) = obj.get("max_webhooks") {
                    limits.max_webhooks = v.as_u64().map(|n| n as u32);
                }
                if let Some(v) = obj.get("max_payload_size_bytes") {
                    if let Some(n) = v.as_u64() {
                        limits.max_payload_size_bytes = n as usize;
                    }
                }
                if let Some(v) = obj.get("rate_limit_requests_per_second") {
                    if let Some(n) = v.as_u64() {
                        limits.rate_limit_requests_per_second = n as u32;
                    }
                }
                if let Some(v) = obj.get("rate_limit_burst") {
                    if let Some(n) = v.as_u64() {
                        limits.rate_limit_burst = n as u32;
                    }
                }
                if let Some(v) = obj.get("job_retention_days") {
                    if let Some(n) = v.as_u64() {
                        limits.job_retention_days = n as u32;
                    }
                }
                if let Some(v) = obj.get("history_retention_days") {
                    if let Some(n) = v.as_u64() {
                        limits.history_retention_days = n as u32;
                    }
                }
            }
        }

        limits
    }

    /// Get all available plan tiers
    pub fn all_tiers() -> Vec<Self> {
        vec![
            Self::free(),
            Self::starter(),
            Self::pro(),
            Self::enterprise(),
        ]
    }

    /// Check if a limit is exceeded
    ///
    /// Returns Ok(()) if within limit, Err with message if exceeded
    pub fn check_limit(&self, resource: &str, current: u64, adding: u64) -> Result<(), LimitError> {
        let limit = match resource {
            "jobs_per_day" => self.max_jobs_per_day,
            "active_jobs" => self.max_active_jobs,
            "queues" => self.max_queues.map(|v| v as u64),
            "workers" => self.max_workers.map(|v| v as u64),
            "api_keys" => self.max_api_keys.map(|v| v as u64),
            "schedules" => self.max_schedules.map(|v| v as u64),
            "workflows" => self.max_workflows.map(|v| v as u64),
            "webhooks" => self.max_webhooks.map(|v| v as u64),
            _ => return Ok(()), // Unknown resource, allow
        };

        if let Some(max) = limit {
            let new_total = current + adding;
            if new_total > max {
                return Err(LimitError {
                    resource: resource.to_string(),
                    current,
                    limit: max,
                    plan: self.tier.clone(),
                    upgrade_to: self.suggest_upgrade(),
                });
            }
        }

        Ok(())
    }

    /// Check if a resource is disabled on this plan
    pub fn is_disabled(&self, resource: &str) -> bool {
        match resource {
            "workflows" => self.max_workflows == Some(0),
            _ => false,
        }
    }

    /// Suggest which plan to upgrade to
    fn suggest_upgrade(&self) -> Option<String> {
        match self.tier.as_str() {
            "free" => Some("starter".to_string()),
            "starter" => Some("pro".to_string()),
            "pro" => Some("enterprise".to_string()),
            _ => None,
        }
    }

    /// Calculate usage percentage for a resource
    pub fn usage_percentage(&self, resource: &str, current: u64) -> Option<f64> {
        let limit = match resource {
            "jobs_per_day" => self.max_jobs_per_day,
            "active_jobs" => self.max_active_jobs,
            "queues" => self.max_queues.map(|v| v as u64),
            "workers" => self.max_workers.map(|v| v as u64),
            "api_keys" => self.max_api_keys.map(|v| v as u64),
            "schedules" => self.max_schedules.map(|v| v as u64),
            "workflows" => self.max_workflows.map(|v| v as u64),
            "webhooks" => self.max_webhooks.map(|v| v as u64),
            _ => return None,
        };

        limit.map(|max| (current as f64 / max as f64) * 100.0)
    }

    /// Get warning threshold percentage for this plan
    ///
    /// Free tier warns earlier (50%) to encourage upgrades
    pub fn warning_threshold(&self) -> f64 {
        if self.tier == "free" {
            50.0
        } else {
            80.0
        }
    }
}

/// Error returned when a plan limit is exceeded
#[derive(Debug, Clone, Serialize)]
pub struct LimitError {
    /// Resource that hit the limit
    pub resource: String,
    /// Current usage
    pub current: u64,
    /// Maximum allowed
    pub limit: u64,
    /// Current plan tier
    pub plan: String,
    /// Suggested plan to upgrade to
    pub upgrade_to: Option<String>,
}

impl std::fmt::Display for LimitError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let resource_name = self.resource.replace('_', " ");
        write!(
            f,
            "{} limit reached ({}/{})",
            resource_name, self.current, self.limit
        )?;
        if let Some(ref upgrade) = self.upgrade_to {
            write!(f, ". Upgrade to {} for higher limits.", upgrade)?;
        }
        Ok(())
    }
}

impl std::error::Error for LimitError {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_free_tier_defaults() {
        let free = PlanLimits::free();
        assert_eq!(free.tier, "free");
        assert_eq!(free.max_jobs_per_day, Some(1_000));
        assert_eq!(free.max_queues, Some(2));
        assert_eq!(free.max_workflows, Some(0)); // Disabled
    }

    #[test]
    fn test_enterprise_unlimited() {
        let enterprise = PlanLimits::enterprise();
        assert!(enterprise.max_jobs_per_day.is_none());
        assert!(enterprise.max_queues.is_none());
    }

    #[test]
    fn test_for_tier_defaults_to_free() {
        let unknown = PlanLimits::for_tier("unknown");
        assert_eq!(unknown.tier, "free");

        let empty = PlanLimits::for_tier("");
        assert_eq!(empty.tier, "free");
    }

    #[test]
    fn test_check_limit_within() {
        let free = PlanLimits::free();
        assert!(free.check_limit("queues", 1, 1).is_ok());
    }

    #[test]
    fn test_check_limit_exceeded() {
        let free = PlanLimits::free();
        let result = free.check_limit("queues", 2, 1);
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert_eq!(err.resource, "queues");
        assert_eq!(err.limit, 2);
        assert_eq!(err.upgrade_to, Some("starter".to_string()));
    }

    #[test]
    fn test_check_limit_unlimited() {
        let enterprise = PlanLimits::enterprise();
        // Should always succeed for unlimited resources
        assert!(enterprise.check_limit("queues", 1000, 1000).is_ok());
    }

    #[test]
    fn test_workflows_disabled_on_free() {
        let free = PlanLimits::free();
        assert!(free.is_disabled("workflows"));

        let starter = PlanLimits::starter();
        assert!(!starter.is_disabled("workflows"));
    }

    #[test]
    fn test_usage_percentage() {
        let free = PlanLimits::free();
        let pct = free.usage_percentage("queues", 1);
        assert_eq!(pct, Some(50.0)); // 1 out of 2

        let enterprise = PlanLimits::enterprise();
        let pct = enterprise.usage_percentage("queues", 100);
        assert!(pct.is_none()); // Unlimited
    }

    #[test]
    fn test_warning_threshold() {
        let free = PlanLimits::free();
        assert_eq!(free.warning_threshold(), 50.0);

        let pro = PlanLimits::pro();
        assert_eq!(pro.warning_threshold(), 80.0);
    }
}
