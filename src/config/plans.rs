//! Plan limits configuration
//!
//! Defines resource limits for each plan tier (free, starter, pro, enterprise).
//! Free tier is the default for all new organizations and is intentionally
//! restrictive to encourage upgrades.

use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::env;

use lazy_static::lazy_static;
use tracing::warn;

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

/// Per-tier environment overrides, loaded once at process start.
#[derive(Debug, Clone, Default)]
struct TierEnvOverrides {
    /// JSON overrides (same schema as organizations.custom_limits)
    json: Option<Value>,

    /// Individual field overrides (take precedence over JSON overrides)
    max_jobs_per_day: Option<Option<u64>>,
    max_active_jobs: Option<Option<u64>>,
    max_queues: Option<Option<u32>>,
    max_workers: Option<Option<u32>>,
    max_api_keys: Option<Option<u32>>,
    max_schedules: Option<Option<u32>>,
    max_workflows: Option<Option<u32>>,
    max_webhooks: Option<Option<u32>>,

    max_payload_size_bytes: Option<usize>,

    rate_limit_requests_per_second: Option<u32>,
    rate_limit_burst: Option<u32>,

    job_retention_days: Option<u32>,
    history_retention_days: Option<u32>,
}

#[derive(Debug, Clone, Default)]
struct EnvPlanOverrides {
    /// Global JSON override mapping tier -> overrides
    ///
    /// Env: `SPOOLED_PLAN_LIMITS_JSON`
    global_json: Option<Value>,

    free: TierEnvOverrides,
    starter: TierEnvOverrides,
    pro: TierEnvOverrides,
    enterprise: TierEnvOverrides,
}

fn env_get(key: &str) -> Option<String> {
    env::var(key)
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

fn parse_env_json(key: &str) -> Option<Value> {
    let raw = env_get(key)?;
    match serde_json::from_str::<Value>(&raw) {
        Ok(v) => Some(v),
        Err(e) => {
            warn!(env = %key, error = %e, "Failed to parse plan limits JSON from env; ignoring");
            None
        }
    }
}

fn parse_optional_u64_env(key: &str) -> Option<Option<u64>> {
    let raw = env_get(key)?;
    let lower = raw.to_lowercase();
    if lower == "unlimited" || lower == "none" || lower == "null" || lower == "-1" {
        return Some(None);
    }
    match raw.parse::<u64>() {
        Ok(v) => Some(Some(v)),
        Err(e) => {
            warn!(env = %key, value = %raw, error = %e, "Invalid u64 env var; ignoring");
            None
        }
    }
}

fn parse_optional_u32_env(key: &str) -> Option<Option<u32>> {
    let opt = parse_optional_u64_env(key)?;
    match opt {
        None => Some(None),
        Some(v) => {
            if v > u32::MAX as u64 {
                warn!(env = %key, value = v, "Env var too large for u32; ignoring");
                None
            } else {
                Some(Some(v as u32))
            }
        }
    }
}

fn parse_u32_env(key: &str) -> Option<u32> {
    let raw = env_get(key)?;
    match raw.parse::<u32>() {
        Ok(v) => Some(v),
        Err(e) => {
            warn!(env = %key, value = %raw, error = %e, "Invalid u32 env var; ignoring");
            None
        }
    }
}

fn parse_usize_env(key: &str) -> Option<usize> {
    let raw = env_get(key)?;
    match raw.parse::<usize>() {
        Ok(v) => Some(v),
        Err(e) => {
            warn!(env = %key, value = %raw, error = %e, "Invalid usize env var; ignoring");
            None
        }
    }
}

fn load_tier_fields(tier_upper: &str, out: &mut TierEnvOverrides) {
    // Option limits (support "unlimited"/"none"/"null"/-1)
    out.max_jobs_per_day =
        parse_optional_u64_env(&format!("SPOOLED_PLAN_{}_MAX_JOBS_PER_DAY", tier_upper));
    out.max_active_jobs =
        parse_optional_u64_env(&format!("SPOOLED_PLAN_{}_MAX_ACTIVE_JOBS", tier_upper));
    out.max_queues = parse_optional_u32_env(&format!("SPOOLED_PLAN_{}_MAX_QUEUES", tier_upper));
    out.max_workers = parse_optional_u32_env(&format!("SPOOLED_PLAN_{}_MAX_WORKERS", tier_upper));
    out.max_api_keys = parse_optional_u32_env(&format!("SPOOLED_PLAN_{}_MAX_API_KEYS", tier_upper));
    out.max_schedules =
        parse_optional_u32_env(&format!("SPOOLED_PLAN_{}_MAX_SCHEDULES", tier_upper));
    out.max_workflows =
        parse_optional_u32_env(&format!("SPOOLED_PLAN_{}_MAX_WORKFLOWS", tier_upper));
    out.max_webhooks = parse_optional_u32_env(&format!("SPOOLED_PLAN_{}_MAX_WEBHOOKS", tier_upper));

    // Non-option limits
    out.max_payload_size_bytes = parse_usize_env(&format!(
        "SPOOLED_PLAN_{}_MAX_PAYLOAD_SIZE_BYTES",
        tier_upper
    ));
    out.rate_limit_requests_per_second =
        parse_u32_env(&format!("SPOOLED_PLAN_{}_RATE_LIMIT_RPS", tier_upper));
    out.rate_limit_burst = parse_u32_env(&format!("SPOOLED_PLAN_{}_RATE_LIMIT_BURST", tier_upper));
    out.job_retention_days =
        parse_u32_env(&format!("SPOOLED_PLAN_{}_JOB_RETENTION_DAYS", tier_upper));
    out.history_retention_days = parse_u32_env(&format!(
        "SPOOLED_PLAN_{}_HISTORY_RETENTION_DAYS",
        tier_upper
    ));
}

impl EnvPlanOverrides {
    fn from_env() -> Self {
        // Build from Default, but set env-derived JSON fields in the initializer
        // to satisfy clippy::field_reassign_with_default.
        let mut out = Self {
            global_json: parse_env_json("SPOOLED_PLAN_LIMITS_JSON"),
            ..Self::default()
        };

        // Per-tier JSON: SPOOLED_PLAN_<TIER>_LIMITS_JSON
        out.free.json = parse_env_json("SPOOLED_PLAN_FREE_LIMITS_JSON");
        out.starter.json = parse_env_json("SPOOLED_PLAN_STARTER_LIMITS_JSON");
        out.pro.json = parse_env_json("SPOOLED_PLAN_PRO_LIMITS_JSON");
        out.enterprise.json = parse_env_json("SPOOLED_PLAN_ENTERPRISE_LIMITS_JSON");

        // Per-tier individual fields (override JSON)
        load_tier_fields("FREE", &mut out.free);
        load_tier_fields("STARTER", &mut out.starter);
        load_tier_fields("PRO", &mut out.pro);
        load_tier_fields("ENTERPRISE", &mut out.enterprise);

        out
    }

    fn tier(&self, tier: &str) -> Option<&TierEnvOverrides> {
        match tier.to_lowercase().as_str() {
            "free" => Some(&self.free),
            "starter" => Some(&self.starter),
            "pro" => Some(&self.pro),
            "enterprise" => Some(&self.enterprise),
            _ => None,
        }
    }
}

lazy_static! {
    static ref ENV_PLAN_OVERRIDES: EnvPlanOverrides = EnvPlanOverrides::from_env();
}

fn apply_overrides_from_object(limits: &mut PlanLimits, obj: &serde_json::Map<String, Value>) {
    // Option<u64>
    if let Some(v) = obj.get("max_jobs_per_day") {
        if v.is_null() {
            limits.max_jobs_per_day = None;
        } else if let Some(n) = v.as_u64() {
            limits.max_jobs_per_day = Some(n);
        } else {
            warn!(key = "max_jobs_per_day", value = %v, "Invalid override type; ignoring");
        }
    }
    if let Some(v) = obj.get("max_active_jobs") {
        if v.is_null() {
            limits.max_active_jobs = None;
        } else if let Some(n) = v.as_u64() {
            limits.max_active_jobs = Some(n);
        } else {
            warn!(key = "max_active_jobs", value = %v, "Invalid override type; ignoring");
        }
    }

    // Option<u32>
    if let Some(v) = obj.get("max_queues") {
        if v.is_null() {
            limits.max_queues = None;
        } else if let Some(n) = v.as_u64() {
            if n > u32::MAX as u64 {
                warn!(
                    key = "max_queues",
                    value = n,
                    "Override too large for u32; ignoring"
                );
            } else {
                limits.max_queues = Some(n as u32);
            }
        } else {
            warn!(key = "max_queues", value = %v, "Invalid override type; ignoring");
        }
    }
    if let Some(v) = obj.get("max_workers") {
        if v.is_null() {
            limits.max_workers = None;
        } else if let Some(n) = v.as_u64() {
            if n > u32::MAX as u64 {
                warn!(
                    key = "max_workers",
                    value = n,
                    "Override too large for u32; ignoring"
                );
            } else {
                limits.max_workers = Some(n as u32);
            }
        } else {
            warn!(key = "max_workers", value = %v, "Invalid override type; ignoring");
        }
    }
    if let Some(v) = obj.get("max_api_keys") {
        if v.is_null() {
            limits.max_api_keys = None;
        } else if let Some(n) = v.as_u64() {
            if n > u32::MAX as u64 {
                warn!(
                    key = "max_api_keys",
                    value = n,
                    "Override too large for u32; ignoring"
                );
            } else {
                limits.max_api_keys = Some(n as u32);
            }
        } else {
            warn!(key = "max_api_keys", value = %v, "Invalid override type; ignoring");
        }
    }
    if let Some(v) = obj.get("max_schedules") {
        if v.is_null() {
            limits.max_schedules = None;
        } else if let Some(n) = v.as_u64() {
            if n > u32::MAX as u64 {
                warn!(
                    key = "max_schedules",
                    value = n,
                    "Override too large for u32; ignoring"
                );
            } else {
                limits.max_schedules = Some(n as u32);
            }
        } else {
            warn!(key = "max_schedules", value = %v, "Invalid override type; ignoring");
        }
    }
    if let Some(v) = obj.get("max_workflows") {
        if v.is_null() {
            limits.max_workflows = None;
        } else if let Some(n) = v.as_u64() {
            if n > u32::MAX as u64 {
                warn!(
                    key = "max_workflows",
                    value = n,
                    "Override too large for u32; ignoring"
                );
            } else {
                limits.max_workflows = Some(n as u32);
            }
        } else {
            warn!(key = "max_workflows", value = %v, "Invalid override type; ignoring");
        }
    }
    if let Some(v) = obj.get("max_webhooks") {
        if v.is_null() {
            limits.max_webhooks = None;
        } else if let Some(n) = v.as_u64() {
            if n > u32::MAX as u64 {
                warn!(
                    key = "max_webhooks",
                    value = n,
                    "Override too large for u32; ignoring"
                );
            } else {
                limits.max_webhooks = Some(n as u32);
            }
        } else {
            warn!(key = "max_webhooks", value = %v, "Invalid override type; ignoring");
        }
    }

    // Non-option limits
    if let Some(v) = obj.get("max_payload_size_bytes") {
        if let Some(n) = v.as_u64() {
            limits.max_payload_size_bytes = n as usize;
        } else {
            warn!(key = "max_payload_size_bytes", value = %v, "Invalid override type; ignoring");
        }
    }
    if let Some(v) = obj.get("rate_limit_requests_per_second") {
        if let Some(n) = v.as_u64() {
            limits.rate_limit_requests_per_second = n as u32;
        } else {
            warn!(key = "rate_limit_requests_per_second", value = %v, "Invalid override type; ignoring");
        }
    }
    if let Some(v) = obj.get("rate_limit_burst") {
        if let Some(n) = v.as_u64() {
            limits.rate_limit_burst = n as u32;
        } else {
            warn!(key = "rate_limit_burst", value = %v, "Invalid override type; ignoring");
        }
    }
    if let Some(v) = obj.get("job_retention_days") {
        if let Some(n) = v.as_u64() {
            limits.job_retention_days = n as u32;
        } else {
            warn!(key = "job_retention_days", value = %v, "Invalid override type; ignoring");
        }
    }
    if let Some(v) = obj.get("history_retention_days") {
        if let Some(n) = v.as_u64() {
            limits.history_retention_days = n as u32;
        } else {
            warn!(key = "history_retention_days", value = %v, "Invalid override type; ignoring");
        }
    }
}

fn apply_overrides_from_json(limits: &mut PlanLimits, overrides: &Value) {
    if let Some(obj) = overrides.as_object() {
        apply_overrides_from_object(limits, obj);
    }
}

fn apply_env_overrides(limits: &mut PlanLimits) {
    let tier = limits.tier.to_lowercase();

    // Global mapping: { "free": { ... } }
    if let Some(Value::Object(map)) = ENV_PLAN_OVERRIDES.global_json.as_ref() {
        if let Some(tier_overrides) = map.get(&tier) {
            apply_overrides_from_json(limits, tier_overrides);
        }
    }

    // Per-tier JSON + individual fields
    if let Some(t) = ENV_PLAN_OVERRIDES.tier(&tier) {
        if let Some(ref v) = t.json {
            apply_overrides_from_json(limits, v);
        }

        if let Some(v) = t.max_jobs_per_day {
            limits.max_jobs_per_day = v;
        }
        if let Some(v) = t.max_active_jobs {
            limits.max_active_jobs = v;
        }
        if let Some(v) = t.max_queues {
            limits.max_queues = v;
        }
        if let Some(v) = t.max_workers {
            limits.max_workers = v;
        }
        if let Some(v) = t.max_api_keys {
            limits.max_api_keys = v;
        }
        if let Some(v) = t.max_schedules {
            limits.max_schedules = v;
        }
        if let Some(v) = t.max_workflows {
            limits.max_workflows = v;
        }
        if let Some(v) = t.max_webhooks {
            limits.max_webhooks = v;
        }

        if let Some(v) = t.max_payload_size_bytes {
            limits.max_payload_size_bytes = v;
        }

        if let Some(v) = t.rate_limit_requests_per_second {
            limits.rate_limit_requests_per_second = v;
        }
        if let Some(v) = t.rate_limit_burst {
            limits.rate_limit_burst = v;
        }
        if let Some(v) = t.job_retention_days {
            limits.job_retention_days = v;
        }
        if let Some(v) = t.history_retention_days {
            limits.history_retention_days = v;
        }
    }
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

        // Apply env-configured plan overrides first (global + per-tier).
        // Org-level custom limits take precedence and are applied last.
        apply_env_overrides(&mut limits);

        if let Some(overrides) = custom_limits {
            // Apply org-level overrides (jsonb) with support for null -> unlimited for Option fields
            apply_overrides_from_json(&mut limits, overrides);
        }

        limits
    }

    /// Get all available plan tiers
    pub fn all_tiers() -> Vec<Self> {
        vec![
            Self::for_tier_with_overrides("free", None),
            Self::for_tier_with_overrides("starter", None),
            Self::for_tier_with_overrides("pro", None),
            Self::for_tier_with_overrides("enterprise", None),
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
