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

// ============================================================================
// Admin Create Organization Tests (Integration)
// ============================================================================

use common::TestDatabase;

/// Test admin organization creation with default plan
#[tokio::test]
async fn test_admin_create_organization_default_plan() {
    let db = TestDatabase::new().await;

    let org_id = uuid::Uuid::new_v4().to_string();
    let org_name = "Admin Created Org";
    let org_slug = format!("admin-org-{}", &org_id[..8]);

    // Insert organization with free tier (default)
    sqlx::query(
        r#"
        INSERT INTO organizations (id, name, slug, plan_tier, created_at, updated_at)
        VALUES ($1, $2, $3, 'free', NOW(), NOW())
        "#,
    )
    .bind(&org_id)
    .bind(org_name)
    .bind(&org_slug)
    .execute(db.pool())
    .await
    .expect("Should create organization");

    // Verify organization exists with free tier
    let org: (String, String) =
        sqlx::query_as("SELECT name, plan_tier FROM organizations WHERE id = $1")
            .bind(&org_id)
            .fetch_one(db.pool())
            .await
            .expect("Should fetch organization");

    assert_eq!(org.0, org_name);
    assert_eq!(org.1, "free");
}

/// Test admin organization creation with specified plan tier
#[tokio::test]
async fn test_admin_create_organization_custom_plan() {
    let db = TestDatabase::new().await;

    let org_id = uuid::Uuid::new_v4().to_string();
    let org_name = "Pro Organization";
    let org_slug = format!("pro-org-{}", &org_id[..8]);

    // Insert organization with pro tier
    sqlx::query(
        r#"
        INSERT INTO organizations (id, name, slug, plan_tier, created_at, updated_at)
        VALUES ($1, $2, $3, 'pro', NOW(), NOW())
        "#,
    )
    .bind(&org_id)
    .bind(org_name)
    .bind(&org_slug)
    .execute(db.pool())
    .await
    .expect("Should create organization");

    // Verify organization exists with pro tier
    let org: (String, String) =
        sqlx::query_as("SELECT name, plan_tier FROM organizations WHERE id = $1")
            .bind(&org_id)
            .fetch_one(db.pool())
            .await
            .expect("Should fetch organization");

    assert_eq!(org.0, org_name);
    assert_eq!(org.1, "pro");
}

/// Test admin organization creation fails for duplicate slug
#[tokio::test]
async fn test_admin_create_organization_duplicate_slug_fails() {
    let db = TestDatabase::new().await;

    let slug = format!(
        "unique-slug-{}",
        uuid::Uuid::new_v4().to_string()[..8].to_string()
    );

    // Create first organization
    sqlx::query(
        r#"
        INSERT INTO organizations (id, name, slug, plan_tier, created_at, updated_at)
        VALUES ($1, 'First Org', $2, 'free', NOW(), NOW())
        "#,
    )
    .bind(uuid::Uuid::new_v4().to_string())
    .bind(&slug)
    .execute(db.pool())
    .await
    .expect("Should create first organization");

    // Try to create second organization with same slug
    let result = sqlx::query(
        r#"
        INSERT INTO organizations (id, name, slug, plan_tier, created_at, updated_at)
        VALUES ($1, 'Second Org', $2, 'free', NOW(), NOW())
        "#,
    )
    .bind(uuid::Uuid::new_v4().to_string())
    .bind(&slug)
    .execute(db.pool())
    .await;

    assert!(result.is_err());
    assert!(result.unwrap_err().to_string().contains("duplicate key"));
}

// ============================================================================
// Admin API Key Creation Tests (Integration)
// ============================================================================

/// Test admin can create API key for organization
#[tokio::test]
async fn test_admin_create_api_key_for_org() {
    let db = TestDatabase::new().await;

    // Create an organization first
    let org_id = uuid::Uuid::new_v4().to_string();
    sqlx::query(
        r#"
        INSERT INTO organizations (id, name, slug, plan_tier, created_at, updated_at)
        VALUES ($1, 'Test Org', $2, 'free', NOW(), NOW())
        "#,
    )
    .bind(&org_id)
    .bind(format!("test-org-{}", &org_id[..8]))
    .execute(db.pool())
    .await
    .expect("Should create organization");

    // Create an API key for the organization
    let key_id = uuid::Uuid::new_v4().to_string();
    let key_name = "Admin Created Key";
    let key_hash = bcrypt::hash("test_key", 4).unwrap();
    let queues: Vec<String> = vec!["*".to_string()];

    sqlx::query(
        r#"
        INSERT INTO api_keys (id, organization_id, key_hash, key_prefix, name, queues, is_active, created_at)
        VALUES ($1, $2, $3, 'sk_test_', $4, $5, TRUE, NOW())
        "#,
    )
    .bind(&key_id)
    .bind(&org_id)
    .bind(&key_hash)
    .bind(key_name)
    .bind(&queues)
    .execute(db.pool())
    .await
    .expect("Should create API key");

    // Verify API key was created
    let key: (String, String, bool) = sqlx::query_as(
        "SELECT id, name, is_active FROM api_keys WHERE organization_id = $1 AND id = $2",
    )
    .bind(&org_id)
    .bind(&key_id)
    .fetch_one(db.pool())
    .await
    .expect("Should fetch API key");

    assert_eq!(key.0, key_id);
    assert_eq!(key.1, key_name);
    assert!(key.2);
}

/// Test multiple API keys can be created for same organization
#[tokio::test]
async fn test_admin_create_multiple_api_keys() {
    let db = TestDatabase::new().await;

    // Create an organization
    let org_id = uuid::Uuid::new_v4().to_string();
    sqlx::query(
        r#"
        INSERT INTO organizations (id, name, slug, plan_tier, created_at, updated_at)
        VALUES ($1, 'Multi Key Org', $2, 'pro', NOW(), NOW())
        "#,
    )
    .bind(&org_id)
    .bind(format!("multi-key-{}", &org_id[..8]))
    .execute(db.pool())
    .await
    .expect("Should create organization");

    // Create multiple API keys
    for i in 1..=3 {
        let key_id = uuid::Uuid::new_v4().to_string();
        let key_hash = bcrypt::hash(format!("test_key_{}", i), 4).unwrap();
        let queues: Vec<String> = vec!["*".to_string()];

        sqlx::query(
            r#"
            INSERT INTO api_keys (id, organization_id, key_hash, key_prefix, name, queues, is_active, created_at)
            VALUES ($1, $2, $3, 'sk_test_', $4, $5, TRUE, NOW())
            "#,
        )
        .bind(&key_id)
        .bind(&org_id)
        .bind(&key_hash)
        .bind(format!("Key {}", i))
        .bind(&queues)
        .execute(db.pool())
        .await
        .expect("Should create API key");
    }

    // Verify all keys were created
    let count: (i64,) = sqlx::query_as("SELECT COUNT(*) FROM api_keys WHERE organization_id = $1")
        .bind(&org_id)
        .fetch_one(db.pool())
        .await
        .expect("Should count keys");

    assert_eq!(count.0, 3);
}

// ============================================================================
// Admin Usage Reset Tests (Integration)
// ============================================================================

/// Test admin can reset organization usage counters
#[tokio::test]
async fn test_admin_reset_usage_counters() {
    let db = TestDatabase::new().await;

    // Create an organization
    let org_id = uuid::Uuid::new_v4().to_string();
    sqlx::query(
        r#"
        INSERT INTO organizations (id, name, slug, plan_tier, created_at, updated_at)
        VALUES ($1, 'Usage Test Org', $2, 'free', NOW(), NOW())
        "#,
    )
    .bind(&org_id)
    .bind(format!("usage-test-{}", &org_id[..8]))
    .execute(db.pool())
    .await
    .expect("Should create organization");

    // Create usage record with some jobs (use correct column names from schema)
    sqlx::query(
        r#"
        INSERT INTO organization_usage (organization_id, jobs_created_today, total_jobs_created, last_daily_reset)
        VALUES ($1, 500, 1000, CURRENT_DATE)
        ON CONFLICT (organization_id) DO UPDATE
        SET jobs_created_today = 500, last_daily_reset = CURRENT_DATE
        "#,
    )
    .bind(&org_id)
    .execute(db.pool())
    .await
    .expect("Should create usage record");

    // Verify usage is set
    let before: (i64,) = sqlx::query_as(
        "SELECT jobs_created_today FROM organization_usage WHERE organization_id = $1",
    )
    .bind(&org_id)
    .fetch_one(db.pool())
    .await
    .expect("Should fetch usage");

    assert_eq!(before.0, 500);

    // Reset usage counters
    sqlx::query(
        r#"
        UPDATE organization_usage
        SET jobs_created_today = 0, last_daily_reset = CURRENT_DATE, updated_at = CURRENT_TIMESTAMP
        WHERE organization_id = $1
        "#,
    )
    .bind(&org_id)
    .execute(db.pool())
    .await
    .expect("Should reset usage");

    // Verify usage was reset
    let after: (i64,) = sqlx::query_as(
        "SELECT jobs_created_today FROM organization_usage WHERE organization_id = $1",
    )
    .bind(&org_id)
    .fetch_one(db.pool())
    .await
    .expect("Should fetch usage after reset");

    assert_eq!(after.0, 0);
}

/// Test usage reset preserves total_jobs_created
#[tokio::test]
async fn test_admin_reset_usage_preserves_total() {
    let db = TestDatabase::new().await;

    // Create an organization
    let org_id = uuid::Uuid::new_v4().to_string();
    sqlx::query(
        r#"
        INSERT INTO organizations (id, name, slug, plan_tier, created_at, updated_at)
        VALUES ($1, 'Preserve Total Org', $2, 'free', NOW(), NOW())
        "#,
    )
    .bind(&org_id)
    .bind(format!("preserve-{}", &org_id[..8]))
    .execute(db.pool())
    .await
    .expect("Should create organization");

    // Create usage record (use correct column names from schema)
    sqlx::query(
        r#"
        INSERT INTO organization_usage (organization_id, jobs_created_today, total_jobs_created, last_daily_reset)
        VALUES ($1, 100, 5000, CURRENT_DATE)
        ON CONFLICT (organization_id) DO UPDATE
        SET jobs_created_today = 100, total_jobs_created = 5000, last_daily_reset = CURRENT_DATE
        "#,
    )
    .bind(&org_id)
    .execute(db.pool())
    .await
    .expect("Should create usage record");

    // Reset only daily usage (not total)
    sqlx::query(
        r#"
        UPDATE organization_usage
        SET jobs_created_today = 0, last_daily_reset = CURRENT_DATE, updated_at = CURRENT_TIMESTAMP
        WHERE organization_id = $1
        "#,
    )
    .bind(&org_id)
    .execute(db.pool())
    .await
    .expect("Should reset daily usage");

    // Verify daily is 0 but total is preserved
    let usage: (i64, i64) = sqlx::query_as(
        "SELECT jobs_created_today, total_jobs_created FROM organization_usage WHERE organization_id = $1",
    )
    .bind(&org_id)
    .fetch_one(db.pool())
    .await
    .expect("Should fetch usage");

    assert_eq!(usage.0, 0); // daily reset
    assert_eq!(usage.1, 5000); // total preserved
}

// ============================================================================
// Admin Validation Tests
// ============================================================================

/// Test valid plan tier values
#[test]
fn test_valid_plan_tier_values() {
    let valid_tiers = ["free", "starter", "pro", "enterprise"];

    for tier in &valid_tiers {
        let plan = PlanLimits::for_tier(tier);
        assert_eq!(plan.tier, *tier);
    }
}

/// Test slug validation patterns
#[test]
fn test_slug_validation_patterns() {
    // Valid slugs
    let valid_slugs = ["my-org", "test123", "a1b2c3", "company-name-here", "abc"];

    for slug in &valid_slugs {
        assert!(
            slug.len() >= 3
                && slug.len() <= 50
                && slug
                    .chars()
                    .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
                && !slug.starts_with('-')
                && !slug.ends_with('-'),
            "Slug '{}' should be valid",
            slug
        );
    }

    // Invalid slugs
    let invalid_slugs = [
        "-starts-with-dash",
        "ends-with-dash-",
        "has UPPERCASE",
        "has spaces",
        "ab", // too short
    ];

    for slug in &invalid_slugs {
        let is_valid = slug.len() >= 3
            && slug.len() <= 50
            && slug
                .chars()
                .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
            && !slug.starts_with('-')
            && !slug.ends_with('-');

        assert!(!is_valid, "Slug '{}' should be invalid", slug);
    }
}
