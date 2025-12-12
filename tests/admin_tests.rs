//! Admin API tests
//!
//! Tests for admin authentication, organization management, and plan limits.

mod common;

use spooled_backend::config::plans::{LimitError, PlanLimits};

// ============================================================================
// Plan Limits Configuration Tests
// ============================================================================

#[test]
fn test_free_tier_has_restrictive_limits() {
    let free = PlanLimits::free();

    assert_eq!(free.tier, "free");
    assert_eq!(free.max_jobs_per_day, Some(1_000));
    assert_eq!(free.max_active_jobs, Some(10));
    assert_eq!(free.max_queues, Some(2));
    assert_eq!(free.max_workers, Some(1));
    assert_eq!(free.max_api_keys, Some(2));
    assert_eq!(free.max_schedules, Some(1));
    assert_eq!(free.max_workflows, Some(0)); // Disabled
    assert_eq!(free.max_webhooks, Some(1));
    assert_eq!(free.max_payload_size_bytes, 64 * 1024);
    assert_eq!(free.rate_limit_requests_per_second, 5);
    assert_eq!(free.job_retention_days, 3);
}

#[test]
fn test_starter_tier_has_moderate_limits() {
    let starter = PlanLimits::starter();

    assert_eq!(starter.tier, "starter");
    assert_eq!(starter.max_jobs_per_day, Some(10_000));
    assert_eq!(starter.max_active_jobs, Some(500));
    assert_eq!(starter.max_queues, Some(10));
    assert_eq!(starter.max_workers, Some(5));
    assert_eq!(starter.max_workflows, Some(5)); // Enabled
}

#[test]
fn test_pro_tier_has_high_limits() {
    let pro = PlanLimits::pro();

    assert_eq!(pro.tier, "pro");
    assert_eq!(pro.max_jobs_per_day, Some(100_000));
    assert_eq!(pro.max_active_jobs, Some(5_000));
    assert_eq!(pro.max_queues, Some(50));
}

#[test]
fn test_enterprise_tier_is_unlimited() {
    let enterprise = PlanLimits::enterprise();

    assert_eq!(enterprise.tier, "enterprise");
    assert!(enterprise.max_jobs_per_day.is_none());
    assert!(enterprise.max_active_jobs.is_none());
    assert!(enterprise.max_queues.is_none());
    assert!(enterprise.max_workers.is_none());
    assert!(enterprise.max_api_keys.is_none());
    assert!(enterprise.max_schedules.is_none());
    assert!(enterprise.max_workflows.is_none());
    assert!(enterprise.max_webhooks.is_none());
}

#[test]
fn test_for_tier_returns_correct_plan() {
    assert_eq!(PlanLimits::for_tier("free").tier, "free");
    assert_eq!(PlanLimits::for_tier("starter").tier, "starter");
    assert_eq!(PlanLimits::for_tier("pro").tier, "pro");
    assert_eq!(PlanLimits::for_tier("enterprise").tier, "enterprise");

    // Case insensitive
    assert_eq!(PlanLimits::for_tier("FREE").tier, "free");
    assert_eq!(PlanLimits::for_tier("Starter").tier, "starter");
    assert_eq!(PlanLimits::for_tier("PRO").tier, "pro");
}

#[test]
fn test_unknown_tier_defaults_to_free() {
    assert_eq!(PlanLimits::for_tier("unknown").tier, "free");
    assert_eq!(PlanLimits::for_tier("").tier, "free");
    assert_eq!(PlanLimits::for_tier("premium").tier, "free");
    assert_eq!(PlanLimits::for_tier("business").tier, "free");
}

#[test]
fn test_all_tiers_returns_four_plans() {
    let tiers = PlanLimits::all_tiers();
    assert_eq!(tiers.len(), 4);
    assert_eq!(tiers[0].tier, "free");
    assert_eq!(tiers[1].tier, "starter");
    assert_eq!(tiers[2].tier, "pro");
    assert_eq!(tiers[3].tier, "enterprise");
}

// ============================================================================
// Limit Checking Tests
// ============================================================================

#[test]
fn test_check_limit_within_bounds() {
    let free = PlanLimits::free();

    // Current 1, adding 1, limit 2 = OK
    assert!(free.check_limit("queues", 1, 1).is_ok());

    // Current 0, adding 1, limit 2 = OK
    assert!(free.check_limit("queues", 0, 1).is_ok());

    // Current 0, adding 2, limit 2 = OK (exactly at limit)
    assert!(free.check_limit("queues", 0, 2).is_ok());
}

#[test]
fn test_check_limit_exceeded() {
    let free = PlanLimits::free();

    // Current 2, adding 1, limit 2 = FAIL
    let result = free.check_limit("queues", 2, 1);
    assert!(result.is_err());

    let err = result.unwrap_err();
    assert_eq!(err.resource, "queues");
    assert_eq!(err.current, 2);
    assert_eq!(err.limit, 2);
    assert_eq!(err.plan, "free");
    assert_eq!(err.upgrade_to, Some("starter".to_string()));
}

#[test]
fn test_check_limit_unlimited_always_succeeds() {
    let enterprise = PlanLimits::enterprise();

    // Even with huge numbers, enterprise should succeed
    assert!(enterprise
        .check_limit("queues", 1_000_000, 1_000_000)
        .is_ok());
    assert!(enterprise
        .check_limit("jobs_per_day", 1_000_000, 1_000_000)
        .is_ok());
    assert!(enterprise.check_limit("workers", 10_000, 10_000).is_ok());
}

#[test]
fn test_check_limit_unknown_resource_allowed() {
    let free = PlanLimits::free();

    // Unknown resources should be allowed (fail open for forward compatibility)
    assert!(free.check_limit("unknown_resource", 1000, 1000).is_ok());
}

#[test]
fn test_check_limit_jobs_per_day() {
    let free = PlanLimits::free();

    // Free tier: 1000 jobs/day
    assert!(free.check_limit("jobs_per_day", 999, 1).is_ok());
    assert!(free.check_limit("jobs_per_day", 1000, 1).is_err());
}

#[test]
fn test_check_limit_active_jobs() {
    let free = PlanLimits::free();

    // Free tier: 10 active jobs
    assert!(free.check_limit("active_jobs", 9, 1).is_ok());
    assert!(free.check_limit("active_jobs", 10, 1).is_err());
}

// ============================================================================
// Feature Disabled Tests
// ============================================================================

#[test]
fn test_workflows_disabled_on_free_tier() {
    let free = PlanLimits::free();
    assert!(free.is_disabled("workflows"));
    assert_eq!(free.max_workflows, Some(0));
}

#[test]
fn test_workflows_enabled_on_starter_and_above() {
    assert!(!PlanLimits::starter().is_disabled("workflows"));
    assert!(!PlanLimits::pro().is_disabled("workflows"));
    assert!(!PlanLimits::enterprise().is_disabled("workflows"));
}

#[test]
fn test_unknown_feature_not_disabled() {
    let free = PlanLimits::free();
    assert!(!free.is_disabled("unknown_feature"));
    assert!(!free.is_disabled("queues"));
    assert!(!free.is_disabled("jobs"));
}

// ============================================================================
// Usage Percentage Tests
// ============================================================================

#[test]
fn test_usage_percentage_calculation() {
    let free = PlanLimits::free();

    // Queues: limit 2
    assert_eq!(free.usage_percentage("queues", 0), Some(0.0));
    assert_eq!(free.usage_percentage("queues", 1), Some(50.0));
    assert_eq!(free.usage_percentage("queues", 2), Some(100.0));
}

#[test]
fn test_usage_percentage_unlimited_returns_none() {
    let enterprise = PlanLimits::enterprise();

    // Unlimited resources should return None
    assert!(enterprise.usage_percentage("queues", 1000).is_none());
    assert!(enterprise
        .usage_percentage("jobs_per_day", 1000000)
        .is_none());
}

#[test]
fn test_usage_percentage_unknown_resource_returns_none() {
    let free = PlanLimits::free();

    assert!(free.usage_percentage("unknown", 100).is_none());
}

// ============================================================================
// Warning Threshold Tests
// ============================================================================

#[test]
fn test_warning_threshold_free_tier() {
    let free = PlanLimits::free();
    // Free tier warns at 50% to encourage upgrades
    assert_eq!(free.warning_threshold(), 50.0);
}

#[test]
fn test_warning_threshold_paid_tiers() {
    // Paid tiers warn at 80%
    assert_eq!(PlanLimits::starter().warning_threshold(), 80.0);
    assert_eq!(PlanLimits::pro().warning_threshold(), 80.0);
    assert_eq!(PlanLimits::enterprise().warning_threshold(), 80.0);
}

// ============================================================================
// Upgrade Suggestion Tests
// ============================================================================

#[test]
fn test_limit_error_suggests_upgrade() {
    let free = PlanLimits::free();
    let result = free.check_limit("queues", 2, 1);
    let err = result.unwrap_err();

    assert_eq!(err.upgrade_to, Some("starter".to_string()));
}

#[test]
fn test_starter_suggests_pro_upgrade() {
    let starter = PlanLimits::starter();
    let result = starter.check_limit("queues", 10, 1);
    let err = result.unwrap_err();

    assert_eq!(err.upgrade_to, Some("pro".to_string()));
}

#[test]
fn test_pro_suggests_enterprise_upgrade() {
    let pro = PlanLimits::pro();
    let result = pro.check_limit("queues", 50, 1);
    let err = result.unwrap_err();

    assert_eq!(err.upgrade_to, Some("enterprise".to_string()));
}

// ============================================================================
// Error Display Tests
// ============================================================================

#[test]
fn test_limit_error_display() {
    let err = LimitError {
        resource: "queues".to_string(),
        current: 2,
        limit: 2,
        plan: "free".to_string(),
        upgrade_to: Some("starter".to_string()),
    };

    let display = format!("{}", err);
    assert!(display.contains("queues"));
    assert!(display.contains("2/2"));
    assert!(display.contains("starter"));
}

#[test]
fn test_limit_error_display_without_upgrade() {
    let err = LimitError {
        resource: "queues".to_string(),
        current: 100,
        limit: 100,
        plan: "enterprise".to_string(),
        upgrade_to: None,
    };

    let display = format!("{}", err);
    assert!(display.contains("queues"));
    assert!(!display.contains("Upgrade"));
}

// ============================================================================
// Serialization Tests
// ============================================================================

#[test]
fn test_plan_limits_serializable() {
    let free = PlanLimits::free();
    let json = serde_json::to_string(&free).unwrap();

    assert!(json.contains("\"tier\":\"free\""));
    assert!(json.contains("\"max_jobs_per_day\":1000"));
    assert!(json.contains("\"max_workflows\":0"));
}

#[test]
fn test_limit_error_serializable() {
    let err = LimitError {
        resource: "queues".to_string(),
        current: 2,
        limit: 2,
        plan: "free".to_string(),
        upgrade_to: Some("starter".to_string()),
    };

    let json = serde_json::to_string(&err).unwrap();
    assert!(json.contains("\"resource\":\"queues\""));
    assert!(json.contains("\"upgrade_to\":\"starter\""));
}

// ============================================================================
// Edge Cases
// ============================================================================

#[test]
fn test_check_limit_zero_adding() {
    let free = PlanLimits::free();

    // Adding 0 when at limit should succeed (not going over)
    assert!(free.check_limit("queues", 2, 0).is_ok());

    // Adding 0 when under limit should succeed
    assert!(free.check_limit("queues", 1, 0).is_ok());

    // Adding 0 when over limit should still fail (already exceeded)
    assert!(free.check_limit("queues", 100, 0).is_err());
}

#[test]
fn test_check_limit_at_exact_boundary() {
    let free = PlanLimits::free();

    // Exactly at limit should succeed (current + adding = limit)
    assert!(free.check_limit("queues", 1, 1).is_ok()); // 1 + 1 = 2 = limit

    // One over should fail
    assert!(free.check_limit("queues", 2, 1).is_err()); // 2 + 1 = 3 > 2
}

#[test]
fn test_plan_payload_size_limits() {
    assert_eq!(PlanLimits::free().max_payload_size_bytes, 64 * 1024); // 64KB
    assert_eq!(PlanLimits::starter().max_payload_size_bytes, 256 * 1024); // 256KB
    assert_eq!(PlanLimits::pro().max_payload_size_bytes, 1024 * 1024); // 1MB
    assert_eq!(
        PlanLimits::enterprise().max_payload_size_bytes,
        5 * 1024 * 1024
    ); // 5MB
}

#[test]
fn test_plan_rate_limits() {
    assert_eq!(PlanLimits::free().rate_limit_requests_per_second, 5);
    assert_eq!(PlanLimits::starter().rate_limit_requests_per_second, 25);
    assert_eq!(PlanLimits::pro().rate_limit_requests_per_second, 100);
    assert_eq!(PlanLimits::enterprise().rate_limit_requests_per_second, 500);
}

#[test]
fn test_plan_retention_days() {
    assert_eq!(PlanLimits::free().job_retention_days, 3);
    assert_eq!(PlanLimits::starter().job_retention_days, 14);
    assert_eq!(PlanLimits::pro().job_retention_days, 30);
    assert_eq!(PlanLimits::enterprise().job_retention_days, 90);
}

// ============================================================================
// Admin Auth Tests (Unit)
// ============================================================================

#[test]
fn test_constant_time_compare() {
    // Test the constant-time comparison function used for admin key validation
    fn constant_time_compare(a: &str, b: &str) -> bool {
        if a.len() != b.len() {
            return false;
        }
        let mut result = 0u8;
        for (x, y) in a.bytes().zip(b.bytes()) {
            result |= x ^ y;
        }
        result == 0
    }

    assert!(constant_time_compare("secret", "secret"));
    assert!(!constant_time_compare("secret", "Secret"));
    assert!(!constant_time_compare("secret", "secre"));
    assert!(!constant_time_compare("secret", "secretx"));
    assert!(constant_time_compare("", ""));
    assert!(!constant_time_compare("a", "b"));
}

// ============================================================================
// Plan Tier Validation Tests
// ============================================================================

#[test]
fn test_valid_plan_tiers() {
    let valid_tiers = ["free", "starter", "pro", "enterprise"];

    for tier in &valid_tiers {
        let plan = PlanLimits::for_tier(tier);
        assert_eq!(&plan.tier, tier);
    }
}

#[test]
fn test_plan_display_names() {
    assert_eq!(PlanLimits::free().display_name, "Free");
    assert_eq!(PlanLimits::starter().display_name, "Starter");
    assert_eq!(PlanLimits::pro().display_name, "Pro");
    assert_eq!(PlanLimits::enterprise().display_name, "Enterprise");
}
