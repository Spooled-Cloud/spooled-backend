//! Configuration module for Spooled Backend
//!
//! This module handles loading and validating configuration from environment
//! variables and configuration files. Supports both cloud and self-hosted deployments.

pub mod plans;

pub use plans::{LimitError, PlanLimits};

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
    pub outgoing_webhooks: OutgoingWebhookSettings,
    pub trusted_proxy: TrustedProxySettings,
    pub tracing: TracingSettings,
    pub registration: RegistrationSettings,
    pub stripe: StripeSettings,
    pub email: EmailSettings,
    pub turnstile: TurnstileSettings,
}

/// Registration/onboarding settings
#[derive(Debug, Clone, Deserialize)]
pub struct RegistrationSettings {
    /// Registration mode:
    /// - "open": Anyone can create organizations (default, for self-hosted)
    /// - "closed": Organization creation is disabled (for SaaS - admin creates orgs)
    /// - "invite": Require invite code to create organization (future)
    pub mode: RegistrationMode,
    /// Whether new signups via the email verification flow are allowed.
    ///
    /// This is separate from `REGISTRATION_MODE`:
    /// - `REGISTRATION_MODE` controls *who can create organizations* (open vs closed vs invite)
    /// - `EMAIL_SIGNUP_ENABLED` controls whether *new email-based signups* are permitted at all
    ///
    /// When disabled, existing accounts can still use email login, but new emails cannot sign up.
    pub email_signup_enabled: bool,
    /// Admin API key for creating organizations when mode is "closed"
    /// This key bypasses registration restrictions
    pub admin_api_key: Option<String>,
}

/// Registration mode
#[derive(Debug, Clone, Default, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum RegistrationMode {
    /// Anyone can create organizations (self-hosted default)
    #[default]
    Open,
    /// Organization creation disabled (SaaS mode - admin only)
    Closed,
    /// Require invite code (future feature)
    Invite,
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
    /// gRPC port for gRPC server
    pub grpc_port: u16,
    /// Environment (development, staging, production)
    pub environment: Environment,
    /// External URL for this server (used in webhook URLs, etc.)
    pub external_url: Option<String>,
    /// Optional token for metrics endpoint authentication
    /// When set, /metrics endpoint requires Authorization: Bearer <token>
    pub metrics_token: Option<String>,
    /// Origins allowed by CORS in production (comma-separated in
    /// CORS_ALLOWED_ORIGINS). Never reflected from the request.
    pub cors_allowed_origins: Vec<String>,
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
    /// When true, Redis missing/errors deny with 429 (hosted SaaS).
    /// When false (default), fail-open so self-host without Redis keeps working.
    pub fail_closed_on_redis_error: bool,
}

/// Worker configuration
#[derive(Debug, Clone, Deserialize)]
pub struct WorkerSettings {
    /// Heartbeat interval in seconds
    pub heartbeat_interval_secs: u64,
    /// Lease duration in seconds (job lock timeout)
    pub lease_duration_secs: u64,
    /// Lease for a gRPC claim or renewal that sends none (proto3 0), and the
    /// lease gRPC `Register` reports (`GRPC_DEFAULT_LEASE_DURATION_SECS`:
    /// unset, blank or 0 is 300; above 3600 is lowered to 3600; at or below
    /// `WORKER_HEARTBEAT_INTERVAL_SECS`, or below 5, fails startup).
    ///
    /// Separate from [`Self::lease_duration_secs`] on purpose: the Go SDK's
    /// `StreamJobs` has no lease parameter and the Go/PHP `Dequeue` and
    /// `RenewLease` send 0 unless set, so those workers have always run on a
    /// 300 s lease. Following the 30 s REST default re-delivered their longer
    /// jobs to a second worker while the first still ran them.
    pub grpc_default_lease_duration_secs: u64,
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

/// Webhook verification settings (legacy - kept for compatibility)
#[derive(Debug, Clone, Deserialize, Default)]
pub struct WebhookSettings {
    // Fields removed - /webhooks/{org_id}/stripe endpoint was removed
    // Billing webhook uses stripe.webhook_secret from StripeSettings instead
}

/// Outgoing (org-configured) webhook delivery settings.
///
/// Kept separate from the legacy [`WebhookSettings`] marker struct, which is
/// about *inbound* webhook verification.
#[derive(Debug, Clone, Deserialize)]
pub struct OutgoingWebhookSettings {
    /// Delivery attempts per event before giving up (`OUTGOING_WEBHOOK_MAX_ATTEMPTS`).
    pub max_attempts: i32,
    /// Hard ceiling on deliveries in flight process-wide
    /// (`OUTGOING_WEBHOOK_MAX_CONCURRENT_DELIVERIES`).
    ///
    /// Delivery tasks are spawned from the enqueue/complete hot paths and live
    /// for the request timeout plus the retry backoff, holding a payload clone
    /// the whole time. Without a ceiling, one tenant pointing a webhook at a
    /// black-holing endpoint grows tasks and sockets until the process dies.
    pub max_concurrent_deliveries: usize,
}

impl Default for OutgoingWebhookSettings {
    fn default() -> Self {
        Self {
            max_attempts: 5,
            max_concurrent_deliveries: 64,
        }
    }
}

/// How to determine the client IP for rate limiting and abuse controls.
///
/// `X-Forwarded-For` is APPENDED to by each proxy, so its leftmost entry is
/// whatever the original caller sent — attacker-controlled. Trusting it let a
/// caller land in a fresh rate-limit bucket per request simply by varying the
/// header, which removes the limiter rather than degrading it.
#[derive(Debug, Clone, Deserialize, Default)]
pub struct TrustedProxySettings {
    /// Header set by the edge proxy that carries the real client IP, e.g.
    /// `CF-Connecting-IP` (Cloudflare) or `X-Real-IP` (nginx).
    /// Set via `TRUSTED_CLIENT_IP_HEADER`. Checked first when present, and
    /// believed from any TCP peer.
    ///
    /// Unset, `CF-Connecting-IP` then `X-Real-IP` are believed only when the
    /// TCP peer is a loopback/private/link-local/unique-local address (a proxy
    /// on the same host or network); any other peer is keyed on its own
    /// address.
    pub client_ip_header: Option<String>,
    /// Number of trusted proxy hops appended to `X-Forwarded-For`.
    ///
    /// `0` (default) means **do not trust `X-Forwarded-For` at all** — correct
    /// for self-hosting with no proxy. `1` means the last entry was added by our
    /// own edge, so the client address is the second-from-last, and so on.
    /// Set via `TRUSTED_PROXY_HOPS`.
    pub forwarded_hops: usize,
}

// `Default` is derived: `None` + `0` hops is exactly the intended
// trust-nothing default, and deriving keeps it that way if fields are added.

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

/// Stripe billing configuration
#[derive(Debug, Clone, Deserialize)]
pub struct StripeSettings {
    /// Stripe secret key (sp_live_xxx or sp_test_xxx)
    pub secret_key: Option<String>,
    /// Stripe webhook signing secret (whsec_xxx)
    pub webhook_secret: Option<String>,
    /// Price ID for Starter plan
    pub starter_price_id: Option<String>,
    /// Price ID for Pro plan
    pub pro_price_id: Option<String>,
    /// Stripe billing portal configuration ID (optional)
    pub billing_portal_config_id: Option<String>,
}

/// Email sending configuration
#[derive(Debug, Clone, Deserialize)]
pub struct EmailSettings {
    /// Email provider: "resend", "sendgrid", "postmark", or "smtp"
    pub provider: EmailProvider,
    /// API key for the email provider (Resend, SendGrid, Postmark)
    pub api_key: Option<String>,
    /// From email address
    pub from_address: String,
    /// From name
    pub from_name: String,
    /// SMTP host (only for SMTP provider)
    pub smtp_host: Option<String>,
    /// SMTP port (only for SMTP provider)
    pub smtp_port: Option<u16>,
    /// SMTP username (only for SMTP provider)
    pub smtp_username: Option<String>,
    /// SMTP password (only for SMTP provider)
    pub smtp_password: Option<String>,
}

/// Cloudflare Turnstile siteverify endpoint.
pub const TURNSTILE_VERIFY_URL: &str = "https://challenges.cloudflare.com/turnstile/v0/siteverify";

/// Cloudflare Turnstile check on `POST /auth/email/start`.
#[derive(Debug, Clone, Deserialize)]
pub struct TurnstileSettings {
    /// `TURNSTILE_SECRET_KEY`. While unset no token is required or checked.
    pub secret_key: Option<String>,
    /// Siteverify endpoint ([`TURNSTILE_VERIFY_URL`]; tests point it at a mock).
    pub verify_url: String,
}

impl Default for TurnstileSettings {
    fn default() -> Self {
        Self {
            secret_key: None,
            verify_url: TURNSTILE_VERIFY_URL.to_string(),
        }
    }
}

/// Email provider type
#[derive(Debug, Clone, Default, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum EmailProvider {
    /// Console logging (development only)
    #[default]
    Console,
    /// Resend (recommended)
    Resend,
    /// SendGrid
    Sendgrid,
    /// Postmark
    Postmark,
    /// SMTP
    Smtp,
}

/// Read an optional setting, treating an empty or whitespace-only value as
/// unset.
///
/// Compose files pass optional variables through as `${NAME:-}`, so an unset
/// variable arrives as an empty string. Read with `env::var(..).ok()`, that
/// empty string counted as configured: an empty `ADMIN_API_KEY` let anyone pass
/// closed registration with an empty `X-Admin-Key` header, and an empty
/// `STRIPE_BILLING_WEBHOOK_SECRET` made forged billing webhooks verify against
/// an empty HMAC key.
pub fn non_empty_env(name: &str) -> Option<String> {
    env::var(name)
        .ok()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
}

/// Parse an optional numeric setting, falling back to `default` when the
/// variable is unset or blank (see [`non_empty_env`]).
fn env_parse<T>(name: &str, default: &str) -> Result<T>
where
    T: std::str::FromStr,
    T::Err: std::error::Error + Send + Sync + 'static,
{
    parse_setting(name, non_empty_env(name).as_deref(), default)
}

fn parse_setting<T>(name: &str, raw: Option<&str>, default: &str) -> Result<T>
where
    T: std::str::FromStr,
    T::Err: std::error::Error + Send + Sync + 'static,
{
    raw.unwrap_or(default)
        .parse()
        .with_context(|| format!("Invalid {name}"))
}

/// Read a boolean flag (`1`/`true`/`yes`, case-insensitive). Unset or blank
/// keeps `default`; any other non-blank value is `false`.
fn env_flag(name: &str, default: bool) -> bool {
    flag_from(non_empty_env(name).as_deref(), default)
}

fn flag_from(raw: Option<&str>, default: bool) -> bool {
    match raw {
        None => default,
        Some(v) => matches!(v.to_ascii_lowercase().as_str(), "1" | "true" | "yes"),
    }
}

/// Default of `GRPC_DEFAULT_LEASE_DURATION_SECS`: the lease gRPC has always
/// given a claim that sends none.
const GRPC_DEFAULT_LEASE_SECS: u64 = 300;

/// The lease bounds gRPC applies to any requested lease.
const GRPC_LEASE_SECS_RANGE: std::ops::RangeInclusive<u64> = 5..=3600;

/// `GRPC_DEFAULT_LEASE_DURATION_SECS`: unset, blank or `0` is 300 (0 is what
/// "no lease" means on the gRPC wire), a larger value is lowered to 3600.
///
/// A small value is NOT raised to the 5 s minimum any more: `0` (or `1`-`9`)
/// became a 5-9 s lease, below the 10 s heartbeat, so every job of a worker
/// that sends no lease went to a second worker while the first still ran it
/// (W3R-03 review).
/// [`Settings::validate`] refuses a value at or below
/// `WORKER_HEARTBEAT_INTERVAL_SECS` or below 5 instead.
fn grpc_default_lease_secs(raw: Option<&str>) -> Result<u64> {
    let secs: u64 = parse_setting("GRPC_DEFAULT_LEASE_DURATION_SECS", raw, "0")?;
    Ok(match secs {
        0 => GRPC_DEFAULT_LEASE_SECS,
        secs => secs.min(*GRPC_LEASE_SECS_RANGE.end()),
    })
}

/// Origins allowed by CORS when `CORS_ALLOWED_ORIGINS` is unset or blank.
///
/// Matches the live production topology: the marketing site (apex + www) and
/// the dashboard SPA. The dashboard is served at dashboard.spooled.cloud;
/// omitting it here (an earlier default listed a non-existent
/// app.spooled.cloud) CORS-blocks every dashboard->API call, including admin
/// login. Deployments should still set CORS_ALLOWED_ORIGINS explicitly.
const DEFAULT_CORS_ORIGINS: &str =
    "https://spooled.cloud,https://www.spooled.cloud,https://dashboard.spooled.cloud";

/// Split a comma-separated origin list. An unset, blank or all-separator value
/// keeps the default list: compose passes `CORS_ALLOWED_ORIGINS: ${...:-}` as an
/// empty string, which must not turn into "no origin allowed".
fn parse_cors_origins(raw: Option<&str>) -> Vec<String> {
    let split = |value: &str| -> Vec<String> {
        value
            .split(',')
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .collect()
    };
    let origins = raw.map(split).unwrap_or_default();
    if origins.is_empty() {
        split(DEFAULT_CORS_ORIGINS)
    } else {
        origins
    }
}

impl Settings {
    /// Load settings from environment variables
    pub fn load() -> Result<Self> {
        let settings = Settings {
            server: ServerSettings {
                host: env::var("HOST").unwrap_or_else(|_| "0.0.0.0".to_string()),
                port: env_parse("PORT", "8080")?,
                metrics_port: env_parse("METRICS_PORT", "9090")?,
                grpc_port: env_parse("GRPC_PORT", "50051")?,
                external_url: non_empty_env("EXTERNAL_URL"),
                metrics_token: non_empty_env("METRICS_TOKEN"),
                cors_allowed_origins: parse_cors_origins(
                    non_empty_env("CORS_ALLOWED_ORIGINS").as_deref(),
                ),
                // Fail-safe: an unset or unrecognized RUST_ENV resolves to
                // Production, not Development. Development disables JWT-secret
                // strength validation and uses permissive CORS, so a prod deploy
                // that forgets (or typos) RUST_ENV must not silently fall into it.
                // Local dev opts in explicitly (`.env.example` sets
                // RUST_ENV=development).
                environment: match env::var("RUST_ENV").as_deref() {
                    Ok("production") => Environment::Production,
                    Ok("staging") => Environment::Staging,
                    Ok("development") | Ok("dev") => Environment::Development,
                    Ok(other) => {
                        eprintln!(
                            "WARNING: RUST_ENV=\"{other}\" is unrecognized; defaulting to \
                             production (fail-safe). Set RUST_ENV=development for local dev."
                        );
                        Environment::Production
                    }
                    Err(_) => {
                        eprintln!(
                            "WARNING: RUST_ENV is not set; defaulting to production (fail-safe). \
                             Set RUST_ENV=development for local dev."
                        );
                        Environment::Production
                    }
                },
            },
            database: DatabaseSettings {
                // Don't expose DATABASE_URL in error messages (could contain passwords)
                url: env::var("DATABASE_URL").map_err(|_| {
                    anyhow::anyhow!("DATABASE_URL environment variable must be set")
                })?,
                direct_url: non_empty_env("DATABASE_DIRECT_URL"),
                max_connections: env_parse("DATABASE_MAX_CONNECTIONS", "25")?,
                min_connections: env_parse("DATABASE_MIN_CONNECTIONS", "5")?,
                acquire_timeout_secs: env_parse("DATABASE_ACQUIRE_TIMEOUT_SECS", "30")?,
            },
            redis: RedisSettings {
                url: env::var("REDIS_URL").unwrap_or_else(|_| "redis://localhost:6379".to_string()),
                pool_size: env_parse("REDIS_POOL_SIZE", "10")?,
            },
            jwt: JwtSettings {
                secret: env::var("JWT_SECRET")
                    .unwrap_or_else(|_| "development-secret-change-in-production".to_string()),
                expiration_hours: env_parse("JWT_EXPIRATION_HOURS", "24")?,
            },
            rate_limit: RateLimitSettings {
                requests_per_second: env_parse("RATE_LIMIT_REQUESTS_PER_SECOND", "100")?,
                burst_size: env_parse("RATE_LIMIT_BURST_SIZE", "200")?,
                fail_closed_on_redis_error: env_flag("RATE_LIMIT_FAIL_CLOSED", false),
            },
            worker: WorkerSettings {
                heartbeat_interval_secs: env_parse("WORKER_HEARTBEAT_INTERVAL_SECS", "10")?,
                lease_duration_secs: env_parse("WORKER_LEASE_DURATION_SECS", "30")?,
                grpc_default_lease_duration_secs: grpc_default_lease_secs(
                    non_empty_env("GRPC_DEFAULT_LEASE_DURATION_SECS").as_deref(),
                )?,
                max_concurrency: env_parse("WORKER_MAX_CONCURRENCY", "5")?,
                fallback_poll_interval_secs: env_parse("WORKER_FALLBACK_POLL_INTERVAL_SECS", "5")?,
            },
            queue: QueueSettings {
                default_max_retries: env_parse("QUEUE_DEFAULT_MAX_RETRIES", "3")?,
                default_timeout_secs: env_parse("QUEUE_DEFAULT_TIMEOUT_SECS", "300")?,
                max_payload_size_bytes: env_parse("QUEUE_MAX_PAYLOAD_SIZE_BYTES", "1048576")?,
            },
            webhooks: WebhookSettings::default(),
            outgoing_webhooks: OutgoingWebhookSettings {
                max_attempts: env_parse("OUTGOING_WEBHOOK_MAX_ATTEMPTS", "5")?,
                max_concurrent_deliveries: env_parse(
                    "OUTGOING_WEBHOOK_MAX_CONCURRENT_DELIVERIES",
                    "64",
                )?,
            },
            trusted_proxy: TrustedProxySettings {
                client_ip_header: non_empty_env("TRUSTED_CLIENT_IP_HEADER"),
                forwarded_hops: env_parse("TRUSTED_PROXY_HOPS", "0")?,
            },
            tracing: TracingSettings {
                otlp_endpoint: non_empty_env("OTEL_EXPORTER_OTLP_ENDPOINT"),
                service_name: env::var("OTEL_SERVICE_NAME")
                    .unwrap_or_else(|_| "spooled-backend".to_string()),
                json_logs: env::var("JSON_LOGS")
                    .map(|v| v == "true" || v == "1")
                    .unwrap_or(false),
            },
            registration: RegistrationSettings {
                mode: match env::var("REGISTRATION_MODE")
                    .unwrap_or_else(|_| "open".to_string())
                    .to_lowercase()
                    .as_str()
                {
                    "closed" => RegistrationMode::Closed,
                    "invite" => RegistrationMode::Invite,
                    _ => RegistrationMode::Open,
                },
                email_signup_enabled: env::var("EMAIL_SIGNUP_ENABLED")
                    .map(|v| v == "true" || v == "1" || v == "yes")
                    .unwrap_or(true),
                admin_api_key: non_empty_env("ADMIN_API_KEY"),
            },
            stripe: StripeSettings {
                secret_key: non_empty_env("STRIPE_SECRET_KEY"),
                webhook_secret: non_empty_env("STRIPE_BILLING_WEBHOOK_SECRET"),
                starter_price_id: non_empty_env("STRIPE_STARTER_PRICE_ID"),
                pro_price_id: non_empty_env("STRIPE_PRO_PRICE_ID"),
                billing_portal_config_id: env::var("STRIPE_BILLING_PORTAL_CONFIG_ID")
                    .ok()
                    .and_then(|v| {
                        let trimmed = v.trim().to_string();
                        (!trimmed.is_empty()).then_some(trimmed)
                    }),
            },
            email: EmailSettings {
                provider: match env::var("EMAIL_PROVIDER")
                    .unwrap_or_else(|_| "console".to_string())
                    .to_lowercase()
                    .as_str()
                {
                    "resend" => EmailProvider::Resend,
                    "sendgrid" => EmailProvider::Sendgrid,
                    "postmark" => EmailProvider::Postmark,
                    "smtp" => EmailProvider::Smtp,
                    _ => EmailProvider::Console,
                },
                api_key: non_empty_env("EMAIL_API_KEY"),
                from_address: env::var("EMAIL_FROM_ADDRESS")
                    .unwrap_or_else(|_| "noreply@spooled.cloud".to_string()),
                from_name: env::var("EMAIL_FROM_NAME")
                    .unwrap_or_else(|_| "Spooled Cloud".to_string()),
                smtp_host: non_empty_env("SMTP_HOST"),
                smtp_port: env::var("SMTP_PORT").ok().and_then(|s| s.parse().ok()),
                smtp_username: non_empty_env("SMTP_USERNAME"),
                smtp_password: non_empty_env("SMTP_PASSWORD"),
            },
            turnstile: TurnstileSettings {
                secret_key: non_empty_env("TURNSTILE_SECRET_KEY"),
                ..TurnstileSettings::default()
            },
        };

        // Validate settings
        settings.validate()?;

        if let Some(warning) = settings.client_ip_warning() {
            tracing::warn!("{warning}");
        }

        Ok(settings)
    }

    /// Boot-time warning for production without `TRUSTED_CLIENT_IP_HEADER`.
    ///
    /// Not an error: behind a proxy on the same network (production's
    /// cloudflared) the default edge headers are still believed, and a direct
    /// client is keyed on its own address. But which header carries the client
    /// address is deployment knowledge, and a proxy that reaches the backend
    /// from a public address leaves every caller in that proxy's one bucket.
    fn client_ip_warning(&self) -> Option<&'static str> {
        (self.server.environment == Environment::Production
            && self.trusted_proxy.client_ip_header.is_none())
        .then_some(
            "TRUSTED_CLIENT_IP_HEADER is not set: CF-Connecting-IP / X-Real-IP are believed \
             only from loopback or private-network peers, every other caller is rate-limited \
             by its TCP address. Set TRUSTED_CLIENT_IP_HEADER=CF-Connecting-IP behind \
             Cloudflare or X-Real-IP behind nginx.",
        )
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

        // The console provider logs every login code (and the address) instead of
        // sending it: in production users never receive a code, the API still
        // answers 200, and anyone with log access can sign in as any user. It is
        // also what an unset or misspelled EMAIL_PROVIDER resolves to.
        if self.server.environment == Environment::Production
            && self.email.provider == EmailProvider::Console
        {
            anyhow::bail!(
                "EMAIL_PROVIDER=console is not allowed when RUST_ENV=production: login codes \
                 would be written to the log and never delivered. Set EMAIL_PROVIDER to \
                 resend, sendgrid, postmark or smtp (with EMAIL_API_KEY or SMTP_*)"
            );
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

        // A gRPC worker that sends no lease gets this one and heartbeats every
        // WORKER_HEARTBEAT_INTERVAL_SECS: a shorter lease expires between
        // heartbeats and its job goes to a second worker.
        let grpc_lease = self.worker.grpc_default_lease_duration_secs;
        if grpc_lease <= self.worker.heartbeat_interval_secs
            || grpc_lease < *GRPC_LEASE_SECS_RANGE.start()
        {
            anyhow::bail!(
                "GRPC_DEFAULT_LEASE_DURATION_SECS ({grpc_lease}) must be greater than \
                 WORKER_HEARTBEAT_INTERVAL_SECS ({}) and at least {}; leave it unset \
                 or 0 for the 300 s default",
                self.worker.heartbeat_interval_secs,
                GRPC_LEASE_SECS_RANGE.start()
            );
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

        // Webhook secret validation removed - the old /webhooks/{org_id}/stripe
        // endpoint was removed, and STRIPE_BILLING_WEBHOOK_SECRET is optional
        // (billing just won't work without it)

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
                grpc_port: 50051,
                environment: Environment::Development,
                external_url: None,
                metrics_token: None,
                cors_allowed_origins: vec![],
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
                fail_closed_on_redis_error: false,
            },
            worker: WorkerSettings {
                heartbeat_interval_secs: 10,
                lease_duration_secs: 30,
                grpc_default_lease_duration_secs: 300,
                max_concurrency: 5,
                fallback_poll_interval_secs: 5,
            },
            queue: QueueSettings {
                default_max_retries: 3,
                default_timeout_secs: 300,
                max_payload_size_bytes: 1048576,
            },
            webhooks: WebhookSettings::default(),
            outgoing_webhooks: OutgoingWebhookSettings::default(),
            trusted_proxy: TrustedProxySettings::default(),
            tracing: TracingSettings {
                otlp_endpoint: None,
                service_name: "spooled-backend".to_string(),
                json_logs: false,
            },
            registration: RegistrationSettings {
                mode: RegistrationMode::Open,
                email_signup_enabled: true,
                admin_api_key: None,
            },
            stripe: StripeSettings {
                secret_key: None,
                webhook_secret: None,
                starter_price_id: None,
                pro_price_id: None,
                billing_portal_config_id: None,
            },
            email: EmailSettings {
                provider: EmailProvider::Console,
                api_key: None,
                from_address: "noreply@test.local".to_string(),
                from_name: "Test".to_string(),
                smtp_host: None,
                smtp_port: None,
                smtp_username: None,
                smtp_password: None,
            },
            turnstile: TurnstileSettings::default(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn non_empty_env_treats_blank_values_as_unset() {
        std::env::set_var("SPOOLED_TEST_NON_EMPTY_ENV_BLANK", "");
        std::env::set_var("SPOOLED_TEST_NON_EMPTY_ENV_SPACES", "   ");
        std::env::set_var("SPOOLED_TEST_NON_EMPTY_ENV_SET", " value ");
        assert_eq!(non_empty_env("SPOOLED_TEST_NON_EMPTY_ENV_BLANK"), None);
        assert_eq!(non_empty_env("SPOOLED_TEST_NON_EMPTY_ENV_SPACES"), None);
        assert_eq!(
            non_empty_env("SPOOLED_TEST_NON_EMPTY_ENV_SET").as_deref(),
            Some("value")
        );
        assert_eq!(non_empty_env("SPOOLED_TEST_NON_EMPTY_ENV_MISSING"), None);
    }

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

    // ---- wave 3 / opscfg ----

    /// Names forwarded by docker-compose.prod.yml as `${NAME:-...}`.
    const FORWARDED_EMPTY_VARS: &[&str] = &[
        "EXTERNAL_URL",
        "CORS_ALLOWED_ORIGINS",
        "RATE_LIMIT_FAIL_CLOSED",
        "TRUSTED_CLIENT_IP_HEADER",
        "TRUSTED_PROXY_HOPS",
        "OUTGOING_WEBHOOK_MAX_ATTEMPTS",
        "OUTGOING_WEBHOOK_MAX_CONCURRENT_DELIVERIES",
        "DATABASE_DIRECT_URL",
        "GRPC_PORT",
        "METRICS_TOKEN",
        "OTEL_EXPORTER_OTLP_ENDPOINT",
        "GRPC_DEFAULT_LEASE_DURATION_SECS",
        "TURNSTILE_SECRET_KEY",
    ];

    static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    #[test]
    fn load_treats_empty_forwarded_env_vars_as_unset() {
        let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        std::env::set_var("RUST_ENV", "development");
        std::env::set_var("DATABASE_URL", "postgres://u:p@localhost/db");
        for name in FORWARDED_EMPTY_VARS {
            std::env::set_var(name, "");
        }

        let loaded = Settings::load();

        for name in FORWARDED_EMPTY_VARS {
            std::env::remove_var(name);
        }
        std::env::remove_var("RUST_ENV");
        std::env::remove_var("DATABASE_URL");

        let settings = loaded.expect("blank forwarded variables must not fail startup");
        assert_eq!(
            settings.server.cors_allowed_origins,
            vec![
                "https://spooled.cloud".to_string(),
                "https://www.spooled.cloud".to_string(),
                "https://dashboard.spooled.cloud".to_string()
            ]
        );
        assert!(!settings.rate_limit.fail_closed_on_redis_error);
        assert_eq!(settings.server.external_url, None);
        assert_eq!(settings.server.metrics_token, None);
        assert_eq!(settings.server.grpc_port, 50051);
        assert_eq!(settings.database.direct_url, None);
        assert_eq!(settings.trusted_proxy.client_ip_header, None);
        assert_eq!(settings.trusted_proxy.forwarded_hops, 0);
        assert_eq!(settings.outgoing_webhooks.max_attempts, 5);
        assert_eq!(settings.outgoing_webhooks.max_concurrent_deliveries, 64);
        assert_eq!(settings.tracing.otlp_endpoint, None);
        assert_eq!(settings.worker.grpc_default_lease_duration_secs, 300);
        assert_eq!(settings.turnstile.secret_key, None);
        assert_eq!(settings.turnstile.verify_url, TURNSTILE_VERIFY_URL);
    }

    fn production_settings(provider: EmailProvider) -> Settings {
        let mut settings = Settings::load_for_testing();
        settings.server.environment = Environment::Production;
        settings.jwt.secret = "Zx9fQ2mL7vB4nK8pR3tY6wC1hJ5dS0aG".to_string();
        settings.email.provider = provider;
        settings
    }

    #[test]
    fn production_refuses_the_console_email_provider() {
        let err = production_settings(EmailProvider::Console)
            .validate()
            .expect_err("console must be refused in production")
            .to_string();
        assert!(err.contains("EMAIL_PROVIDER=console"), "{err}");

        for provider in [
            EmailProvider::Resend,
            EmailProvider::Sendgrid,
            EmailProvider::Postmark,
            EmailProvider::Smtp,
        ] {
            production_settings(provider).validate().unwrap();
        }
    }

    // ---- wave 4 / authrl ----

    // BEAUTH-10: production without TRUSTED_CLIENT_IP_HEADER is told at boot
    // how client addresses are resolved and which header to set.
    #[test]
    fn production_without_a_client_ip_header_warns_once_at_boot() {
        let mut settings = production_settings(EmailProvider::Resend);
        let warning = settings.client_ip_warning().expect("warning in production");
        assert!(warning.contains("TRUSTED_CLIENT_IP_HEADER"), "{warning}");

        settings.trusted_proxy.client_ip_header = Some("CF-Connecting-IP".to_string());
        assert_eq!(settings.client_ip_warning(), None);

        let development = Settings::load_for_testing();
        assert_eq!(development.client_ip_warning(), None);
    }

    #[test]
    fn console_email_provider_is_allowed_outside_production() {
        for environment in [Environment::Development, Environment::Staging] {
            let mut settings = production_settings(EmailProvider::Console);
            settings.server.environment = environment;
            settings.validate().unwrap();
        }
    }

    #[test]
    fn cors_origins_blank_or_unset_keep_the_default_list() {
        let default = parse_cors_origins(None);
        assert_eq!(default.len(), 3);
        assert!(default.contains(&"https://dashboard.spooled.cloud".to_string()));
        assert_eq!(parse_cors_origins(Some("")), default);
        assert_eq!(parse_cors_origins(Some("  ")), default);
        assert_eq!(parse_cors_origins(Some(" , ,")), default);
        assert_eq!(
            parse_cors_origins(Some(" https://a.example , https://b.example,")),
            vec![
                "https://a.example".to_string(),
                "https://b.example".to_string()
            ]
        );
    }

    #[test]
    fn flags_and_numbers_blank_or_unset_keep_their_defaults() {
        assert!(!flag_from(None, false));
        assert!(flag_from(None, true));
        assert!(flag_from(Some("TRUE"), false));
        assert!(flag_from(Some("1"), false));
        assert!(flag_from(Some("yes"), false));
        assert!(!flag_from(Some("no"), true));

        assert_eq!(parse_setting::<u32>("X", None, "64").unwrap(), 64);
        assert_eq!(parse_setting::<u32>("X", Some("7"), "64").unwrap(), 7);
        let err = parse_setting::<u32>("TRUSTED_PROXY_HOPS", Some("abc"), "0")
            .unwrap_err()
            .to_string();
        assert!(err.contains("Invalid TRUSTED_PROXY_HOPS"), "{err}");
    }

    // W3R-03: a gRPC claim or renewal without a lease keeps the 300 s it always
    // had (Go SDK StreamJobs cannot send one), independent of the 30 s REST
    // default; a large operator value is lowered to the gRPC lease maximum.
    // 0 means unset (300), not a 5 s lease.
    #[test]
    fn grpc_default_lease_is_300_unless_configured_and_clamped() {
        assert_eq!(grpc_default_lease_secs(None).unwrap(), 300);
        assert_eq!(grpc_default_lease_secs(Some("120")).unwrap(), 120);
        assert_eq!(grpc_default_lease_secs(Some("0")).unwrap(), 300);
        assert_eq!(grpc_default_lease_secs(Some("86400")).unwrap(), 3600);
        let err = grpc_default_lease_secs(Some("5m")).unwrap_err().to_string();
        assert!(
            err.contains("Invalid GRPC_DEFAULT_LEASE_DURATION_SECS"),
            "{err}"
        );

        let settings = Settings::load_for_testing();
        assert_eq!(settings.worker.grpc_default_lease_duration_secs, 300);
        assert_ne!(
            settings.worker.grpc_default_lease_duration_secs,
            settings.worker.lease_duration_secs
        );
    }

    // W3R-03 review: GRPC_DEFAULT_LEASE_DURATION_SECS=0 (or 1-9) silently
    // became a 5-9 s lease, below the 10 s heartbeat, so a worker that sends no
    // lease lost every job to a second worker. 0 is now unset (300) and a lease
    // at or below the heartbeat interval fails startup, like
    // WORKER_LEASE_DURATION_SECS.
    #[test]
    fn grpc_default_lease_at_or_below_the_heartbeat_fails_validation() {
        let with_lease = |raw: &str| {
            let mut settings = Settings::load_for_testing();
            settings.worker.grpc_default_lease_duration_secs =
                grpc_default_lease_secs(Some(raw)).unwrap();
            settings.validate()
        };
        for raw in ["1", "5", "9", "10"] {
            let err = with_lease(raw)
                .expect_err("a lease at or below the 10 s heartbeat must be refused")
                .to_string();
            assert!(err.contains("GRPC_DEFAULT_LEASE_DURATION_SECS"), "{err}");
            assert!(err.contains("WORKER_HEARTBEAT_INTERVAL_SECS (10)"), "{err}");
        }
        for raw in ["0", "11", "300", "86400"] {
            with_lease(raw).unwrap_or_else(|e| panic!("{raw}: {e}"));
        }

        // Follows the configured heartbeat, and never below the 5 s minimum.
        let mut settings = Settings::load_for_testing();
        settings.worker.heartbeat_interval_secs = 2;
        settings.worker.grpc_default_lease_duration_secs = 4;
        assert!(settings.validate().is_err());
        settings.worker.grpc_default_lease_duration_secs = 5;
        settings.validate().unwrap();
        settings.worker.heartbeat_interval_secs = 30;
        settings.worker.lease_duration_secs = 60;
        settings.worker.grpc_default_lease_duration_secs = 30;
        assert!(settings.validate().is_err());
    }

    #[test]
    fn prod_compose_forwards_every_operator_setting() {
        let compose = include_str!("../../docker-compose.prod.yml");
        for name in [
            "RATE_LIMIT_FAIL_CLOSED",
            "CORS_ALLOWED_ORIGINS",
            "EXTERNAL_URL",
            "ENABLE_HSTS",
            "TRUSTED_CLIENT_IP_HEADER",
            "TRUSTED_PROXY_HOPS",
            "OUTGOING_WEBHOOK_MAX_ATTEMPTS",
            "OUTGOING_WEBHOOK_MAX_CONCURRENT_DELIVERIES",
            "DATABASE_TEST_BEFORE_ACQUIRE",
            "DATABASE_DIRECT_URL",
            "GRPC_PORT",
            "GRPC_DEFAULT_LEASE_DURATION_SECS",
            "TURNSTILE_SECRET_KEY",
        ] {
            assert!(
                compose.contains(&format!("      {name}: ${{{name}:-")),
                "docker-compose.prod.yml must forward {name}"
            );
        }
        assert!(compose.contains("rule_files:"));
        assert!(compose.contains("credentials_file: /tmp/metrics-token"));
    }
}
