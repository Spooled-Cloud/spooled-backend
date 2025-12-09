//! Database module for Spooled Backend
//!
//! This module handles database connections, migrations, and provides
//! a connection pool for the application.

use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use sqlx::postgres::{PgPool, PgPoolOptions};
use tracing::{info, warn};

use crate::config::DatabaseSettings;

/// Database wrapper providing connection pool management
#[derive(Clone)]
pub struct Database {
    /// PostgreSQL connection pool
    pool: Arc<PgPool>,
}

impl Database {
    /// Connect to the database with the given settings
    ///
    /// URL is no longer logged to prevent credential exposure in logs
    pub async fn connect(settings: &DatabaseSettings) -> Result<Self> {
        // Don't log the database URL as it may contain credentials
        info!(
            max_connections = settings.max_connections,
            min_connections = settings.min_connections,
            "Connecting to database"
        );

        let pool = PgPoolOptions::new()
            .max_connections(settings.max_connections)
            .min_connections(settings.min_connections)
            .acquire_timeout(Duration::from_secs(settings.acquire_timeout_secs))
            .idle_timeout(Duration::from_secs(600))
            .max_lifetime(Duration::from_secs(3600))
            .test_before_acquire(true)
            .connect(&settings.url)
            .await
            // Don't include URL in error message to prevent credential exposure
            .context("Failed to connect to database - check DATABASE_URL configuration")?;

        // Test connection
        sqlx::query("SELECT 1")
            .execute(&pool)
            .await
            .context("Failed to execute test query")?;

        info!("Database connection established");

        Ok(Self {
            pool: Arc::new(pool),
        })
    }

    /// Run database migrations
    pub async fn run_migrations(&self) -> Result<()> {
        info!("Running database migrations");

        sqlx::migrate!("./migrations")
            .run(&*self.pool)
            .await
            .context("Failed to run migrations")?;

        info!("Migrations completed successfully");
        Ok(())
    }

    /// Get a reference to the connection pool
    pub fn pool(&self) -> &PgPool {
        &self.pool
    }

    /// Get an Arc reference to the pool for sharing
    pub fn pool_arc(&self) -> Arc<PgPool> {
        Arc::clone(&self.pool)
    }

    /// Set the organization context for RLS
    ///
    /// The org_id is validated to contain only safe characters before being used.
    /// Strengthened to only allow ASCII alphanumeric (not Unicode)
    pub async fn set_org_context(&self, org_id: &str) -> Result<(), sqlx::Error> {
        // Validate org_id to prevent SQL injection
        // Is_alphanumeric() includes Unicode letters which could bypass filters
        // Now only allow ASCII letters, digits, hyphens, and underscores
        if !org_id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
        {
            warn!(org_id = %org_id, "Invalid org_id format - rejecting to prevent SQL injection");
            return Err(sqlx::Error::Protocol(
                "Invalid organization ID format".to_string(),
            ));
        }

        // Limit org_id length to prevent buffer overflow attacks
        if org_id.len() > 128 {
            warn!(org_id_len = org_id.len(), "Org ID too long - rejecting");
            return Err(sqlx::Error::Protocol(
                "Organization ID too long".to_string(),
            ));
        }

        // Additional check - org_id should not be empty
        if org_id.is_empty() {
            warn!("Empty org_id - rejecting");
            return Err(sqlx::Error::Protocol(
                "Organization ID cannot be empty".to_string(),
            ));
        }

        sqlx::query("SELECT set_config('app.current_org_id', $1, false)")
            .bind(org_id)
            .execute(&*self.pool)
            .await?;
        Ok(())
    }

    /// Health check
    pub async fn health_check(&self) -> Result<bool> {
        match sqlx::query("SELECT 1").execute(&*self.pool).await {
            Ok(_) => Ok(true),
            Err(e) => {
                warn!(error = %e, "Database health check failed");
                Ok(false)
            }
        }
    }

    /// Get connection pool statistics
    pub fn pool_stats(&self) -> PoolStats {
        PoolStats {
            size: self.pool.size(),
            idle: self.pool.num_idle(),
        }
    }

    /// Close the database connection pool gracefully
    pub async fn close(&self) {
        info!("Closing database connection pool");
        self.pool.close().await;
        info!("Database connection pool closed");
    }
}

/// Connection pool statistics
#[derive(Debug, Clone)]
pub struct PoolStats {
    /// Total connections in pool
    pub size: u32,
    /// Idle connections
    pub idle: usize,
}

impl Database {
    /// Create a Database from an existing pool (for testing)
    pub fn from_pool(pool: Arc<PgPool>) -> Self {
        Self { pool }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Integration tests will use testcontainers
    // Unit tests verify basic structure

    #[test]
    fn test_pool_stats_default() {
        let stats = PoolStats { size: 10, idle: 5 };
        assert_eq!(stats.size, 10);
        assert_eq!(stats.idle, 5);
    }
}
