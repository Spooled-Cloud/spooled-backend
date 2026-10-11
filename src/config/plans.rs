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

/// Upper bound for a retention override, in days (100 years). The retention
/// SQL (`custom_retention_days()`) applies the same bound, so the scheduler and
/// the API never disagree about which override is in force.
pub const MAX_RETENTION_DAYS: u64 = 36_500;

/// How one override key (`organizations.custom_limits`, env JSON) is read.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum OverrideKind {
    /// A resource cap: a whole number up to `max`, or `null` / `-1` for
    /// unlimited (the same spellings the `SPOOLED_PLAN_*` env vars accept).
    Cap { max: u64 },
    /// A setting with no "unlimited": a whole number up to `max`; `null`
    /// keeps the plan value.
    Setting { max: u64 },
}

/// Every key an override object may carry. Job-count caps stop at `i64::MAX`
/// because the daily quota is enforced in SQL as BIGINT.
const OVERRIDE_KEYS: &[(&str, OverrideKind)] = &[
    (
        "max_jobs_per_day",
        OverrideKind::Cap {
            max: i64::MAX as u64,
        },
    ),
    (
        "max_active_jobs",
        OverrideKind::Cap {
            max: i64::MAX as u64,
        },
    ),
    (
        "max_queues",
        OverrideKind::Cap {
            max: u32::MAX as u64,
        },
    ),
    (
        "max_workers",
        OverrideKind::Cap {
            max: u32::MAX as u64,
        },
    ),
    (
        "max_api_keys",
        OverrideKind::Cap {
            max: u32::MAX as u64,
        },
    ),
    (
        "max_schedules",
        OverrideKind::Cap {
            max: u32::MAX as u64,
        },
    ),
    (
        "max_workflows",
        OverrideKind::Cap {
            max: u32::MAX as u64,
        },
    ),
    (
        "max_webhooks",
        OverrideKind::Cap {
            max: u32::MAX as u64,
        },
    ),
    (
        "max_payload_size_bytes",
        OverrideKind::Setting {
            max: u32::MAX as u64,
        },
    ),
    (
        "rate_limit_requests_per_second",
        OverrideKind::Setting {
            max: u32::MAX as u64,
        },
    ),
    (
        "rate_limit_burst",
        OverrideKind::Setting {
            max: u32::MAX as u64,
        },
    ),
    (
        "job_retention_days",
        OverrideKind::Setting {
            max: MAX_RETENTION_DAYS,
        },
    ),
    (
        "history_retention_days",
        OverrideKind::Setting {
            max: MAX_RETENTION_DAYS,
        },
    ),
];

/// One override value after parsing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum OverrideValue {
    /// No limit (caps only).
    Unlimited,
    /// Keep the plan value (settings only).
    KeepPlan,
    /// A whole number within the key's bound.
    Value(u64),
}

fn parse_override(kind: OverrideKind, v: &Value) -> Result<OverrideValue, String> {
    match kind {
        OverrideKind::Cap { max } => {
            if v.is_null() || v.as_i64() == Some(-1) {
                return Ok(OverrideValue::Unlimited);
            }
            match v.as_u64() {
                Some(n) if n <= max => Ok(OverrideValue::Value(n)),
                _ => Err(format!(
                    "must be a whole number from 0 to {max}, or null / -1 for unlimited"
                )),
            }
        }
        OverrideKind::Setting { max } => {
            if v.is_null() {
                return Ok(OverrideValue::KeepPlan);
            }
            match v.as_u64() {
                Some(n) if n <= max => Ok(OverrideValue::Value(n)),
                _ => Err(format!(
                    "must be a whole number from 0 to {max}, or null for the plan default"
                )),
            }
        }
    }
}

fn apply_override(limits: &mut PlanLimits, key: &str, value: OverrideValue) {
    let n = match value {
        OverrideValue::KeepPlan => return,
        OverrideValue::Unlimited => None,
        OverrideValue::Value(n) => Some(n),
    };
    // Every value was range-checked against its key's bound in OVERRIDE_KEYS,
    // so the narrowing casts below cannot truncate.
    let n32 = n.map(|n| n as u32);
    match key {
        "max_jobs_per_day" => limits.max_jobs_per_day = n,
        "max_active_jobs" => limits.max_active_jobs = n,
        "max_queues" => limits.max_queues = n32,
        "max_workers" => limits.max_workers = n32,
        "max_api_keys" => limits.max_api_keys = n32,
        "max_schedules" => limits.max_schedules = n32,
        "max_workflows" => limits.max_workflows = n32,
        "max_webhooks" => limits.max_webhooks = n32,
        // Settings never parse as Unlimited.
        _ => {
            let Some(n) = n else {
                return;
            };
            match key {
                "max_payload_size_bytes" => limits.max_payload_size_bytes = n as usize,
                "rate_limit_requests_per_second" => {
                    limits.rate_limit_requests_per_second = n as u32
                }
                "rate_limit_burst" => limits.rate_limit_burst = n as u32,
                "job_retention_days" => limits.job_retention_days = n as u32,
                "history_retention_days" => limits.history_retention_days = n as u32,
                _ => {}
            }
        }
    }
}

fn apply_overrides_from_object(limits: &mut PlanLimits, obj: &serde_json::Map<String, Value>) {
    for (key, kind) in OVERRIDE_KEYS {
        let Some(v) = obj.get(*key) else {
            continue;
        };
        match parse_override(*kind, v) {
            Ok(value) => apply_override(limits, key, value),
            Err(_) => warn!(key = *key, value = %v, "Invalid override value; ignoring"),
        }
    }
}

/// Validate an org's `custom_limits` before it is stored.
///
/// It must be a JSON object of known override keys, each a whole number within
/// that key's range. `null` means unlimited for a resource cap (`max_*` counts,
/// as does `-1`) and the plan default for a setting (payload size, rate limit,
/// retention). Anything else used to be stored as-is and then silently ignored
/// by the limit checks, while the retention SQL read it raw.
pub fn validate_custom_limits(value: &Value) -> Result<(), String> {
    let Some(obj) = value.as_object() else {
        return Err("custom_limits must be a JSON object".to_string());
    };
    let mut problems = Vec::new();
    for (key, v) in obj {
        match OVERRIDE_KEYS
            .iter()
            .find(|(known, _)| *known == key.as_str())
        {
            None => problems.push(format!(
                "unknown key `{key}` (known keys: {})",
                OVERRIDE_KEYS
                    .iter()
                    .map(|(k, _)| *k)
                    .collect::<Vec<_>>()
                    .join(", ")
            )),
            Some((_, kind)) => {
                if let Err(e) = parse_override(*kind, v) {
                    problems.push(format!("`{key}` {e}"));
                }
            }
        }
    }
    if problems.is_empty() {
        Ok(())
    } else {
        Err(format!("Invalid custom_limits: {}", problems.join("; ")))
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

            // Must never exceed the hard ingest caps (models::job::MAX_JOB_PAYLOAD_SIZE,
            // grpc MAX_PAYLOAD_SIZE, models::workflow::MAX_WORKFLOW_JOB_PAYLOAD_SIZE = 1 MiB):
            // those validators run before the per-plan check, so a larger value here is
            // unenforceable and would overstate the product.
            max_payload_size_bytes: 1024 * 1024, // 1 MB

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

        match limit {
            Some(max) if max > 0 => Some((current as f64 / max as f64) * 100.0),
            _ => None,
        }
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

    // BEBILL-26: -1 is "unlimited" on a resource cap, as in the env vars; it was
    // silently ignored in custom_limits, leaving the plan cap in force.
    #[test]
    fn test_minus_one_means_unlimited_on_caps_only() {
        let overrides = serde_json::json!({
            "max_api_keys": -1,
            "max_jobs_per_day": -1,
            "max_schedules": 7,
            "job_retention_days": -1,
        });
        let limits = PlanLimits::for_tier_with_overrides("free", Some(&overrides));
        assert_eq!(limits.max_api_keys, None);
        assert_eq!(limits.max_jobs_per_day, None);
        assert_eq!(limits.max_schedules, Some(7));
        // A setting has no "unlimited": -1 keeps the plan value.
        assert_eq!(limits.job_retention_days, 3);
        assert!(limits.check_limit("api_keys", 100, 1).is_ok());
    }

    // BEBILL-07: values the retention SQL also ignores are ignored here, and a
    // setting no longer truncates an oversized number into a small one.
    #[test]
    fn test_bad_override_values_keep_the_plan_value() {
        for bad in [
            serde_json::json!("90"),
            serde_json::json!(1.5),
            serde_json::json!(-5),
            serde_json::json!(MAX_RETENTION_DAYS + 1),
        ] {
            let overrides = serde_json::json!({ "job_retention_days": bad });
            let limits = PlanLimits::for_tier_with_overrides("pro", Some(&overrides));
            assert_eq!(limits.job_retention_days, 30, "{overrides}");
        }
        let overrides = serde_json::json!({ "rate_limit_burst": u64::from(u32::MAX) + 7 });
        let limits = PlanLimits::for_tier_with_overrides("pro", Some(&overrides));
        assert_eq!(limits.rate_limit_burst, 200);

        let overrides =
            serde_json::json!({ "job_retention_days": 0, "history_retention_days": null });
        let limits = PlanLimits::for_tier_with_overrides("pro", Some(&overrides));
        assert_eq!(limits.job_retention_days, 0);
        assert_eq!(limits.history_retention_days, 30);
    }

    #[test]
    fn test_validate_custom_limits() {
        for ok in [
            serde_json::json!({}),
            serde_json::json!({"max_api_keys": 10, "max_jobs_per_day": null, "max_queues": -1}),
            serde_json::json!({"job_retention_days": 0, "history_retention_days": MAX_RETENTION_DAYS}),
            serde_json::json!({"max_payload_size_bytes": null, "rate_limit_burst": 0}),
        ] {
            assert!(validate_custom_limits(&ok).is_ok(), "{ok}");
        }
        for (bad, needle) in [
            (serde_json::json!([1]), "JSON object"),
            (serde_json::json!("x"), "JSON object"),
            (
                serde_json::json!({"max_api_key": 3}),
                "unknown key `max_api_key`",
            ),
            (
                serde_json::json!({"job_retention_days": "90"}),
                "`job_retention_days`",
            ),
            (
                serde_json::json!({"job_retention_days": 1.5}),
                "`job_retention_days`",
            ),
            (
                serde_json::json!({"job_retention_days": -1}),
                "`job_retention_days`",
            ),
            (
                serde_json::json!({"job_retention_days": MAX_RETENTION_DAYS + 1}),
                "`job_retention_days`",
            ),
            (serde_json::json!({"max_api_keys": -2}), "`max_api_keys`"),
            (
                serde_json::json!({"max_api_keys": 4_294_967_296u64}),
                "`max_api_keys`",
            ),
            (
                serde_json::json!({"max_jobs_per_day": u64::MAX}),
                "`max_jobs_per_day`",
            ),
            (serde_json::json!({"max_workers": true}), "`max_workers`"),
        ] {
            let err = validate_custom_limits(&bad).expect_err(&bad.to_string());
            assert!(err.contains(needle), "{bad}: {err}");
        }
        // Every problem is reported at once.
        let err =
            validate_custom_limits(&serde_json::json!({"a": 1, "max_queues": "2"})).unwrap_err();
        assert!(err.contains("`a`") && err.contains("`max_queues`"), "{err}");
    }
}
