//! Common test utilities and fixtures

#![allow(dead_code)]

// Test utilities may not all be used in every test
use std::sync::Arc;

use sqlx::postgres::PgPoolOptions;
use sqlx::PgPool;
use testcontainers::{runners::AsyncRunner, ContainerAsync, ImageExt};
use testcontainers_modules::postgres::Postgres;
use testcontainers_modules::redis::Redis;

/// Where a test's PostgreSQL came from.
///
/// Holding the container in the enum keeps it alive for the test's lifetime —
/// dropping a `ContainerAsync` stops the container.
enum PgBacking {
    Container(Box<ContainerAsync<Postgres>>),
    /// A database we created on a caller-supplied server, plus the admin URL
    /// needed to drop it again.
    Borrowed {
        admin_url: String,
        db_name: String,
    },
}

/// Test database wrapper.
///
/// Prefers a caller-supplied server via `TEST_DATABASE_URL`, falling back to a
/// throwaway container. The escape hatch exists because the container path
/// requires a working Docker engine, and when Docker is unavailable the entire
/// integration suite silently becomes unrunnable — which is exactly when you
/// most want to run it. Pointing it at a local PostgreSQL keeps the suite
/// honest: same migrations, same isolation, no Docker.
///
/// Isolation is preserved either way. On the borrowed path each `TestDatabase`
/// creates its OWN uniquely-named database on that server and drops it on
/// `Drop`, so tests never share state and never touch an existing database.
pub struct TestDatabase {
    pub pool: Arc<PgPool>,
    _backing: PgBacking,
}

impl TestDatabase {
    /// Create a new test database with migrations applied
    pub async fn new() -> Self {
        if let Ok(admin_url) = std::env::var("TEST_DATABASE_URL") {
            return Self::from_existing_server(admin_url).await;
        }

        let container = Postgres::default()
            .with_tag("16-alpine")
            .start()
            .await
            .expect(
                "Failed to start PostgreSQL container. If Docker is unavailable, set \
                 TEST_DATABASE_URL to a PostgreSQL server the suite may create databases on, \
                 e.g. TEST_DATABASE_URL=postgres://localhost:5432/postgres",
            );

        let host = container
            .get_host()
            .await
            .expect("Failed to get PostgreSQL host");

        let host_port = container
            .get_host_port_ipv4(5432)
            .await
            .expect("Failed to get PostgreSQL port");

        let connection_string = format!(
            "postgres://postgres:postgres@{}:{}/postgres",
            host, host_port
        );

        // Wait for database to be ready (robust retry instead of fixed sleep)
        let pool = {
            let started = tokio::time::Instant::now();
            // First-run container init can be slow (image pull, DB init).
            let timeout = tokio::time::Duration::from_secs(90);
            let mut delay = tokio::time::Duration::from_millis(200);

            loop {
                match PgPoolOptions::new()
                    .max_connections(5)
                    .acquire_timeout(tokio::time::Duration::from_secs(10))
                    .connect(&connection_string)
                    .await
                {
                    Ok(pool) => break pool,
                    Err(e) => {
                        if started.elapsed() >= timeout {
                            panic!(
                                "Failed to connect to PostgreSQL after {:?}: {:?}",
                                timeout, e
                            );
                        }
                        tokio::time::sleep(delay).await;
                        // Cap backoff at 2s
                        delay = std::cmp::min(delay * 2, tokio::time::Duration::from_secs(2));
                    }
                }
            }
        };

        // Run migrations
        sqlx::migrate!("./migrations")
            .run(&pool)
            .await
            .expect("Failed to run migrations");

        Self {
            pool: Arc::new(pool),
            _backing: PgBacking::Container(Box::new(container)),
        }
    }

    /// Create a fresh, uniquely-named database on a caller-supplied server.
    async fn from_existing_server(admin_url: String) -> Self {
        let admin = PgPoolOptions::new()
            .max_connections(1)
            .acquire_timeout(tokio::time::Duration::from_secs(10))
            .connect(&admin_url)
            .await
            .unwrap_or_else(|e| {
                panic!("TEST_DATABASE_URL is set but unreachable ({admin_url}): {e:?}")
            });

        // Unique per TestDatabase so parallel tests cannot collide. `-` is not
        // legal unquoted in an identifier, hence the underscores.
        let db_name = format!(
            "spooled_test_{}",
            uuid::Uuid::new_v4().to_string().replace('-', "")
        );

        // AssertSqlSafe: db_name is a locally generated UUID hex with `-` stripped,
        // never caller input, and CREATE DATABASE cannot take a bind parameter.
        sqlx::query(sqlx::AssertSqlSafe(format!(
            "CREATE DATABASE \"{db_name}\""
        )))
        .execute(&admin)
        .await
        .unwrap_or_else(|e| panic!("Failed to create test database {db_name}: {e:?}"));
        admin.close().await;

        let url = swap_database(&admin_url, &db_name);
        let pool = PgPoolOptions::new()
            .max_connections(5)
            .acquire_timeout(tokio::time::Duration::from_secs(10))
            .connect(&url)
            .await
            .unwrap_or_else(|e| panic!("Failed to connect to test database {db_name}: {e:?}"));

        sqlx::migrate!("./migrations")
            .run(&pool)
            .await
            .expect("Failed to run migrations");

        Self {
            pool: Arc::new(pool),
            _backing: PgBacking::Borrowed { admin_url, db_name },
        }
    }

    /// Get a reference to the pool
    pub fn pool(&self) -> &PgPool {
        &self.pool
    }
}

impl Drop for TestDatabase {
    fn drop(&mut self) {
        // Containers clean themselves up; a borrowed server does not, and a test
        // run that left hundreds of databases behind would be worse than no
        // escape hatch at all.
        let PgBacking::Borrowed { admin_url, db_name } = &self._backing else {
            return;
        };
        let (admin_url, db_name) = (admin_url.clone(), db_name.clone());

        // Drop runs outside async context, so do the teardown on a plain thread
        // with its own runtime rather than assuming one is available.
        let _ = std::thread::spawn(move || {
            let Ok(rt) = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
            else {
                return;
            };
            rt.block_on(async move {
                let Ok(admin) = PgPoolOptions::new()
                    .max_connections(1)
                    .connect(&admin_url)
                    .await
                else {
                    return;
                };
                // FORCE terminates any connection our own pool has not yet
                // released, which otherwise makes DROP DATABASE fail.
                let _ = sqlx::query(sqlx::AssertSqlSafe(format!(
                    "DROP DATABASE IF EXISTS \"{db_name}\" WITH (FORCE)"
                )))
                .execute(&admin)
                .await;
                admin.close().await;
            });
        })
        .join();
    }
}

/// Replace the database component of a PostgreSQL URL, preserving everything else.
fn swap_database(url: &str, db_name: &str) -> String {
    // Split off any query string first so a `?sslmode=` suffix is not mistaken
    // for part of the database name.
    let (base, query) = match url.split_once('?') {
        Some((b, q)) => (b, Some(q)),
        None => (url, None),
    };
    let trimmed = base.trim_end_matches('/');
    // The database is the last path segment after the host[:port] authority.
    let authority_end = trimmed.find("://").map(|i| i + 3).unwrap_or(0);
    let swapped = match trimmed[authority_end..].find('/') {
        Some(rel) => format!("{}/{}", &trimmed[..authority_end + rel], db_name),
        None => format!("{trimmed}/{db_name}"),
    };
    match query {
        Some(q) => format!("{swapped}?{q}"),
        None => swapped,
    }
}

/// Test Redis wrapper.
///
/// Same escape hatch as [`TestDatabase`]: `TEST_REDIS_URL` points the suite at
/// an existing Redis instead of starting a container.
pub struct TestRedis {
    pub url: String,
    _container: Option<ContainerAsync<Redis>>,
}

impl TestRedis {
    /// Create a new test Redis instance
    pub async fn new() -> Self {
        if let Ok(url) = std::env::var("TEST_REDIS_URL") {
            return Self {
                url,
                _container: None,
            };
        }

        let container = Redis::default().with_tag("7-alpine").start().await.expect(
            "Failed to start Redis container. If Docker is unavailable, set TEST_REDIS_URL \
                 to an existing Redis, e.g. TEST_REDIS_URL=redis://127.0.0.1:6379",
        );

        let host_port = container
            .get_host_port_ipv4(6379)
            .await
            .expect("Failed to get Redis port");

        let url = format!("redis://127.0.0.1:{}", host_port);

        // Wait for Redis to be ready
        tokio::time::sleep(tokio::time::Duration::from_millis(500)).await;

        Self {
            url,
            _container: Some(container),
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
