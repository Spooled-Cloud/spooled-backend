//! Configuration module for Spooled Backend
//!
//! This module handles loading and validating configuration from environment
//! variables and configuration files.

use std::env;

use anyhow::{Context, Result};
use serde::Deserialize;

/// Main application settings
#[derive(Debug, Clone, Deserialize)]
pub struct Settings {
    pub server: ServerSettings,
    pub database: DatabaseSettings,
    pub redis: RedisSettings,
    pub jwt: JwtSettings,
    pub rate_limit: RateLimitSettings,
    pub worker: WorkerSettings,
    pub queue: QueueSettings,
    pub webhooks: WebhookSettings,
    pub tracing: TracingSettings,
}

/// Server configuration
#[derive(Debug, Clone, Deserialize)]
pub struct ServerSettings {
    /// Host to bind to
    pub host: String,
    /// Main API port
    pub port: u16,
    /// Metrics port for Prometheus scraping
    pub metrics_port: u16,
    /// Environment (development, staging, production)
    pub environment: Environment,
}

/// Environment type
#[derive(Debug, Clone, Default, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum Environment {
    #[default]
    Development,
    Staging,
    Production,
}

impl std::fmt::Display for Environment {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Environment::Development => write!(f, "development"),
            Environment::Staging => write!(f, "staging"),
            Environment::Production => write!(f, "production"),
        }
    }
}

/// Database configuration
#[derive(Debug, Clone, Deserialize)]
pub struct DatabaseSettings {
    /// Database URL (through PgBouncer for production)
    pub url: String,
    /// Direct database URL (for migrations, bypasses PgBouncer)
    pub direct_url: Option<String>,
    /// Maximum connections in the pool
    pub max_connections: u32,
    /// Minimum connections to keep open
    pub min_connections: u32,
    /// Connection acquire timeout in seconds
    pub acquire_timeout_secs: u64,
}

/// Redis configuration
#[derive(Debug, Clone, Deserialize)]
pub struct RedisSettings {
    /// Redis connection URL
    pub url: String,
    /// Connection pool size
    pub pool_size: u32,
}

/// JWT configuration
#[derive(Debug, Clone, Deserialize)]
pub struct JwtSettings {
    /// Secret key for signing JWTs
    pub secret: String,
    /// Token expiration in hours
    pub expiration_hours: u64,
}

/// Rate limiting configuration
#[derive(Debug, Clone, Deserialize)]
pub struct RateLimitSettings {
    /// Requests per second limit
    pub requests_per_second: u32,
    /// Burst size for rate limiting
    pub burst_size: u32,
}

/// Worker configuration
#[derive(Debug, Clone, Deserialize)]
pub struct WorkerSettings {
    /// Heartbeat interval in seconds
    pub heartbeat_interval_secs: u64,
    /// Lease duration in seconds (job lock timeout)
    pub lease_duration_secs: u64,
    /// Maximum concurrent jobs per worker
    pub max_concurrency: u32,
    /// Fallback poll interval when Redis unavailable
    pub fallback_poll_interval_secs: u64,
}

/// Queue configuration
#[derive(Debug, Clone, Deserialize)]
pub struct QueueSettings {
    /// Default max retries for jobs
    pub default_max_retries: i32,
    /// Default job timeout in seconds
    pub default_timeout_secs: i32,
    /// Maximum payload size in bytes
    pub max_payload_size_bytes: usize,
}

/// Webhook verification settings
#[derive(Debug, Clone, Deserialize)]
pub struct WebhookSettings {
    /// GitHub webhook secret
    pub github_secret: Option<String>,
    /// Stripe webhook secret
    pub stripe_secret: Option<String>,
}

/// OpenTelemetry/Tracing settings
#[derive(Debug, Clone, Deserialize)]
pub struct TracingSettings {
    /// OTLP endpoint for trace export (e.g., http://jaeger:4317)
    pub otlp_endpoint: Option<String>,
    /// Service name for tracing
    pub service_name: String,
    /// Enable JSON logging
    pub json_logs: bool,
}

impl Settings {
    /// Load settings from environment variables
    pub fn load() -> Result<Self> {
        let settings = Settings {
            server: ServerSettings {
                host: env::var("HOST").unwrap_or_else(|_| "0.0.0.0".to_string()),
                port: env::var("PORT")
                    .unwrap_or_else(|_| "8080".to_string())
                    .parse()
                    .context("Invalid PORT")?,
                metrics_port: env::var("METRICS_PORT")
                    .unwrap_or_else(|_| "9090".to_string())
                    .parse()
                    .context("Invalid METRICS_PORT")?,
                environment: match env::var("RUST_ENV")
                    .unwrap_or_else(|_| "development".to_string())
                    .as_str()
                {
                    "production" => Environment::Production,
                    "staging" => Environment::Staging,
                    _ => Environment::Development,
                },
            },
            database: DatabaseSettings {
                // Don't expose DATABASE_URL in error messages (could contain passwords)
                url: env::var("DATABASE_URL").map_err(|_| {
                    anyhow::anyhow!("DATABASE_URL environment variable must be set")
                })?,
                direct_url: env::var("DATABASE_DIRECT_URL").ok(),
                max_connections: env::var("DATABASE_MAX_CONNECTIONS")
                    .unwrap_or_else(|_| "25".to_string())
                    .parse()
                    .context("Invalid DATABASE_MAX_CONNECTIONS")?,
                min_connections: env::var("DATABASE_MIN_CONNECTIONS")
                    .unwrap_or_else(|_| "5".to_string())
                    .parse()
                    .context("Invalid DATABASE_MIN_CONNECTIONS")?,
                acquire_timeout_secs: env::var("DATABASE_ACQUIRE_TIMEOUT_SECS")
                    .unwrap_or_else(|_| "30".to_string())
                    .parse()
                    .context("Invalid DATABASE_ACQUIRE_TIMEOUT_SECS")?,
            },
            redis: RedisSettings {
                url: env::var("REDIS_URL").unwrap_or_else(|_| "redis://localhost:6379".to_string()),
                pool_size: env::var("REDIS_POOL_SIZE")
                    .unwrap_or_else(|_| "10".to_string())
                    .parse()
                    .context("Invalid REDIS_POOL_SIZE")?,
            },
            jwt: JwtSettings {
                secret: env::var("JWT_SECRET")
                    .unwrap_or_else(|_| "development-secret-change-in-production".to_string()),
                expiration_hours: env::var("JWT_EXPIRATION_HOURS")
                    .unwrap_or_else(|_| "24".to_string())
                    .parse()
                    .context("Invalid JWT_EXPIRATION_HOURS")?,
            },
            rate_limit: RateLimitSettings {
                requests_per_second: env::var("RATE_LIMIT_REQUESTS_PER_SECOND")
                    .unwrap_or_else(|_| "100".to_string())
                    .parse()
                    .context("Invalid RATE_LIMIT_REQUESTS_PER_SECOND")?,
                burst_size: env::var("RATE_LIMIT_BURST_SIZE")
                    .unwrap_or_else(|_| "200".to_string())
                    .parse()
                    .context("Invalid RATE_LIMIT_BURST_SIZE")?,
            },
            worker: WorkerSettings {
                heartbeat_interval_secs: env::var("WORKER_HEARTBEAT_INTERVAL_SECS")
                    .unwrap_or_else(|_| "10".to_string())
                    .parse()
                    .context("Invalid WORKER_HEARTBEAT_INTERVAL_SECS")?,
                lease_duration_secs: env::var("WORKER_LEASE_DURATION_SECS")
                    .unwrap_or_else(|_| "30".to_string())
                    .parse()
                    .context("Invalid WORKER_LEASE_DURATION_SECS")?,
                max_concurrency: env::var("WORKER_MAX_CONCURRENCY")
                    .unwrap_or_else(|_| "5".to_string())
                    .parse()
                    .context("Invalid WORKER_MAX_CONCURRENCY")?,
                fallback_poll_interval_secs: env::var("WORKER_FALLBACK_POLL_INTERVAL_SECS")
                    .unwrap_or_else(|_| "5".to_string())
                    .parse()
                    .context("Invalid WORKER_FALLBACK_POLL_INTERVAL_SECS")?,
            },
            queue: QueueSettings {
                default_max_retries: env::var("QUEUE_DEFAULT_MAX_RETRIES")
                    .unwrap_or_else(|_| "3".to_string())
                    .parse()
                    .context("Invalid QUEUE_DEFAULT_MAX_RETRIES")?,
                default_timeout_secs: env::var("QUEUE_DEFAULT_TIMEOUT_SECS")
                    .unwrap_or_else(|_| "300".to_string())
                    .parse()
                    .context("Invalid QUEUE_DEFAULT_TIMEOUT_SECS")?,
                max_payload_size_bytes: env::var("QUEUE_MAX_PAYLOAD_SIZE_BYTES")
                    .unwrap_or_else(|_| "1048576".to_string())
                    .parse()
                    .context("Invalid QUEUE_MAX_PAYLOAD_SIZE_BYTES")?,
            },
            webhooks: WebhookSettings {
                github_secret: env::var("GITHUB_WEBHOOK_SECRET").ok(),
                stripe_secret: env::var("STRIPE_WEBHOOK_SECRET").ok(),
            },
            tracing: TracingSettings {
                otlp_endpoint: env::var("OTEL_EXPORTER_OTLP_ENDPOINT").ok(),
                service_name: env::var("OTEL_SERVICE_NAME")
                    .unwrap_or_else(|_| "spooled-backend".to_string()),
                json_logs: env::var("JSON_LOGS")
                    .map(|v| v == "true" || v == "1")
                    .unwrap_or(false),
            },
        };

        // Validate settings
        settings.validate()?;

        Ok(settings)
    }

    /// Validate settings
    ///
    /// Stronger JWT secret validation for production
    /// Now also validates JWT secret in staging environment
    fn validate(&self) -> Result<()> {
        // Validate JWT secret in both production AND staging
        // Staging often has production-like data and should be similarly protected
        if self.server.environment == Environment::Production
            || self.server.environment == Environment::Staging
        {
            // Check for common weak/default secrets
            let weak_secrets = [
                "development",
                "secret",
                "password",
                "change-me",
                "your-secret",
                "jwt-secret",
                "test",
                "demo",
            ];

            for weak in weak_secrets {
                if self.jwt.secret.to_lowercase().contains(weak) {
                    anyhow::bail!("JWT_SECRET contains weak pattern '{}' - use a strong random secret in production", weak);
                }
            }

            // Enforce minimum length (256 bits / 32 bytes for HS256)
            // Updated error message to include staging
            if self.jwt.secret.len() < 32 {
                anyhow::bail!(
                    "JWT_SECRET must be at least 32 characters in production/staging (current: {})",
                    self.jwt.secret.len()
                );
            }

            // Check for sufficient entropy (at least some variety in characters)
            let unique_chars: std::collections::HashSet<char> = self.jwt.secret.chars().collect();
            if unique_chars.len() < 10 {
                anyhow::bail!("JWT_SECRET has insufficient entropy - use a random secret with more character variety");
            }
        }

        // Validate port ranges
        if self.server.port == 0 {
            anyhow::bail!("PORT cannot be 0");
        }

        // Validate worker settings
        if self.worker.lease_duration_secs <= self.worker.heartbeat_interval_secs {
            anyhow::bail!(
                "WORKER_LEASE_DURATION_SECS must be greater than WORKER_HEARTBEAT_INTERVAL_SECS"
            );
        }

        // Validate heartbeat interval is not 0
        if self.worker.heartbeat_interval_secs == 0 {
            anyhow::bail!("WORKER_HEARTBEAT_INTERVAL_SECS cannot be 0");
        }

        // Validate rate limit settings are positive
        if self.rate_limit.requests_per_second == 0 {
            anyhow::bail!("RATE_LIMIT_REQUESTS_PER_SECOND cannot be 0");
        }
        if self.rate_limit.burst_size == 0 {
            anyhow::bail!("RATE_LIMIT_BURST_SIZE cannot be 0");
        }

        // Validate database pool settings
        if self.database.min_connections > self.database.max_connections {
            anyhow::bail!(
                "DATABASE_MIN_CONNECTIONS ({}) cannot be greater than DATABASE_MAX_CONNECTIONS ({})",
                self.database.min_connections,
                self.database.max_connections
            );
        }
        if self.database.max_connections == 0 {
            anyhow::bail!("DATABASE_MAX_CONNECTIONS cannot be 0");
        }

        // Validate queue timeout is positive
        if self.queue.default_timeout_secs <= 0 {
            anyhow::bail!("QUEUE_DEFAULT_TIMEOUT_SECS must be positive");
        }

        // Validate fallback poll interval is positive
        if self.worker.fallback_poll_interval_secs == 0 {
            anyhow::bail!("WORKER_FALLBACK_POLL_INTERVAL_SECS cannot be 0");
        }

        // Validate max_concurrency is positive
        if self.worker.max_concurrency == 0 {
            anyhow::bail!("WORKER_MAX_CONCURRENCY cannot be 0");
        }

        // Validate Redis pool_size is positive
        if self.redis.pool_size == 0 {
            anyhow::bail!("REDIS_POOL_SIZE cannot be 0");
        }

        // Validate JWT expiration has reasonable maximum
        if self.jwt.expiration_hours > 8760 {
            // 1 year
            anyhow::bail!("JWT_EXPIRATION_HOURS cannot exceed 8760 (1 year)");
        }
        if self.jwt.expiration_hours == 0 {
            anyhow::bail!("JWT_EXPIRATION_HOURS cannot be 0");
        }

        // Validate webhook secrets have minimum length in production/staging
        if self.server.environment == Environment::Production
            || self.server.environment == Environment::Staging
        {
            if let Some(ref secret) = self.webhooks.github_secret {
                if secret.len() < 16 {
                    anyhow::bail!("GITHUB_WEBHOOK_SECRET must be at least 16 characters in production/staging");
                }
            }
            if let Some(ref secret) = self.webhooks.stripe_secret {
                if secret.len() < 16 {
                    anyhow::bail!("STRIPE_WEBHOOK_SECRET must be at least 16 characters in production/staging");
                }
            }
        }

        Ok(())
    }
}

impl Settings {
    /// Load settings for testing (with defaults)
    pub fn load_for_testing() -> Self {
        Settings {
            server: ServerSettings {
                host: "127.0.0.1".to_string(),
                port: 8080,
                metrics_port: 9090,
                environment: Environment::Development,
            },
            database: DatabaseSettings {
                url: "postgres://test:test@localhost:5432/test".to_string(),
                direct_url: None,
                max_connections: 5,
                min_connections: 1,
                acquire_timeout_secs: 30,
            },
            redis: RedisSettings {
                url: "redis://localhost:6379".to_string(),
                pool_size: 5,
            },
            jwt: JwtSettings {
                secret: "test-secret".to_string(),
                expiration_hours: 24,
            },
            rate_limit: RateLimitSettings {
                requests_per_second: 100,
                burst_size: 200,
            },
            worker: WorkerSettings {
                heartbeat_interval_secs: 10,
                lease_duration_secs: 30,
                max_concurrency: 5,
                fallback_poll_interval_secs: 5,
            },
            queue: QueueSettings {
                default_max_retries: 3,
                default_timeout_secs: 300,
                max_payload_size_bytes: 1048576,
            },
            webhooks: WebhookSettings {
                github_secret: None,
                stripe_secret: None,
            },
            tracing: TracingSettings {
                otlp_endpoint: None,
                service_name: "spooled-backend".to_string(),
                json_logs: false,
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_default_environment() {
        assert_eq!(Environment::default(), Environment::Development);
    }

    #[test]
    fn test_environment_equality() {
        assert_eq!(Environment::Development, Environment::Development);
        assert_eq!(Environment::Staging, Environment::Staging);
        assert_eq!(Environment::Production, Environment::Production);
        assert_ne!(Environment::Development, Environment::Production);
    }

    #[test]
    fn test_load_for_testing() {
        let settings = Settings::load_for_testing();

        assert_eq!(settings.server.host, "127.0.0.1");
        assert_eq!(settings.server.port, 8080);
        assert_eq!(settings.server.environment, Environment::Development);
        assert_eq!(settings.database.max_connections, 5);
        assert_eq!(settings.queue.default_max_retries, 3);
        assert_eq!(settings.queue.default_timeout_secs, 300);
        assert_eq!(settings.worker.heartbeat_interval_secs, 10);
        assert_eq!(settings.worker.lease_duration_secs, 30);
    }

    #[test]
    fn test_database_settings_defaults() {
        let settings = Settings::load_for_testing();

        assert!(settings.database.max_connections > 0);
        assert!(settings.database.min_connections > 0);
        assert!(settings.database.acquire_timeout_secs > 0);
        assert!(settings.database.max_connections >= settings.database.min_connections);
    }

    #[test]
    fn test_rate_limit_settings() {
        let settings = Settings::load_for_testing();

        assert!(settings.rate_limit.requests_per_second > 0);
        assert!(settings.rate_limit.burst_size > 0);
        assert!(settings.rate_limit.burst_size >= settings.rate_limit.requests_per_second);
    }

    #[test]
    fn test_worker_settings_consistency() {
        let settings = Settings::load_for_testing();

        // Lease should be longer than heartbeat interval
        assert!(settings.worker.lease_duration_secs > settings.worker.heartbeat_interval_secs);

        // Max concurrency should be positive
        assert!(settings.worker.max_concurrency > 0);
    }

    #[test]
    fn test_queue_settings_bounds() {
        let settings = Settings::load_for_testing();

        // Max retries should be reasonable
        assert!(settings.queue.default_max_retries <= 100);

        // Timeout should be positive
        assert!(settings.queue.default_timeout_secs > 0);

        // Payload size should be at least 1KB
        assert!(settings.queue.max_payload_size_bytes >= 1024);
    }
}
