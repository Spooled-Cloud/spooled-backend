//! Common test utilities and fixtures

// Test utilities may not all be used in every test
#[allow(dead_code)]
use std::sync::Arc;

use sqlx::postgres::PgPoolOptions;
use sqlx::PgPool;
use testcontainers::{runners::AsyncRunner, ContainerAsync, ImageExt};
use testcontainers_modules::postgres::Postgres;
use testcontainers_modules::redis::Redis;

/// Test database container wrapper
pub struct TestDatabase {
    pub pool: Arc<PgPool>,
    _container: ContainerAsync<Postgres>,
}

impl TestDatabase {
    /// Create a new test database with migrations applied
    pub async fn new() -> Self {
        let container = Postgres::default()
            .with_tag("16-alpine")
            .start()
            .await
            .expect("Failed to start PostgreSQL container");

        let host_port = container
            .get_host_port_ipv4(5432)
            .await
            .expect("Failed to get PostgreSQL port");

        let connection_string = format!(
            "postgres://postgres:postgres@127.0.0.1:{}/postgres",
            host_port
        );

        // Wait for database to be ready
        tokio::time::sleep(tokio::time::Duration::from_secs(2)).await;

        let pool = PgPoolOptions::new()
            .max_connections(5)
            .connect(&connection_string)
            .await
            .expect("Failed to connect to PostgreSQL");

        // Run migrations
        sqlx::migrate!("./migrations")
            .run(&pool)
            .await
            .expect("Failed to run migrations");

        Self {
            pool: Arc::new(pool),
            _container: container,
        }
    }

    /// Get a reference to the pool
    pub fn pool(&self) -> &PgPool {
        &self.pool
    }
}

/// Test Redis container wrapper
pub struct TestRedis {
    pub url: String,
    _container: ContainerAsync<Redis>,
}

impl TestRedis {
    /// Create a new test Redis instance
    pub async fn new() -> Self {
        let container = Redis::default()
            .with_tag("7-alpine")
            .start()
            .await
            .expect("Failed to start Redis container");

        let host_port = container
            .get_host_port_ipv4(6379)
            .await
            .expect("Failed to get Redis port");

        let url = format!("redis://127.0.0.1:{}", host_port);

        // Wait for Redis to be ready
        tokio::time::sleep(tokio::time::Duration::from_millis(500)).await;

        Self {
            url,
            _container: container,
        }
    }
}

/// Test fixtures for creating test data
pub mod fixtures {
    use chrono::Utc;
    use uuid::Uuid;

    /// Create a test organization
    pub fn create_organization_request() -> serde_json::Value {
        serde_json::json!({
            "name": format!("Test Org {}", Uuid::new_v4()),
            "slug": format!("test-org-{}", Uuid::new_v4().to_string()[..8].to_lowercase()),
            "billing_email": "test@example.com"
        })
    }

    /// Create a test job request
    pub fn create_job_request(queue_name: &str) -> serde_json::Value {
        serde_json::json!({
            "queue_name": queue_name,
            "payload": {
                "action": "test",
                "timestamp": Utc::now().to_rfc3339()
            },
            "priority": 0,
            "max_retries": 3,
            "timeout_seconds": 60
        })
    }

    /// Create a test job request with idempotency key
    pub fn create_job_request_with_idempotency(
        queue_name: &str,
        idempotency_key: &str,
    ) -> serde_json::Value {
        serde_json::json!({
            "queue_name": queue_name,
            "payload": {
                "action": "test",
                "timestamp": Utc::now().to_rfc3339()
            },
            "priority": 0,
            "max_retries": 3,
            "timeout_seconds": 60,
            "idempotency_key": idempotency_key
        })
    }

    /// Create a test worker registration request
    pub fn create_worker_request(queue_name: &str) -> serde_json::Value {
        serde_json::json!({
            "queue_name": queue_name,
            "hostname": "test-worker-host",
            "worker_type": "http",
            "max_concurrency": 5,
            "version": "1.0.0"
        })
    }

    /// Create a test API key request
    pub fn create_api_key_request(name: &str) -> serde_json::Value {
        serde_json::json!({
            "name": name,
            "queues": ["default", "emails"],
            "rate_limit": 100
        })
    }
}

/// Helper assertions
pub mod assertions {
    use axum::http::StatusCode;

    /// Assert successful response
    pub fn assert_success(status: StatusCode) {
        assert!(
            status.is_success(),
            "Expected success status, got: {}",
            status
        );
    }

    /// Assert created response
    pub fn assert_created(status: StatusCode) {
        assert_eq!(status, StatusCode::CREATED, "Expected CREATED status");
    }

    /// Assert not found response
    pub fn assert_not_found(status: StatusCode) {
        assert_eq!(status, StatusCode::NOT_FOUND, "Expected NOT_FOUND status");
    }
}
