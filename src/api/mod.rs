//! API module for Spooled Backend
//!
//! This module contains all HTTP handlers, middleware, and routing.

pub mod handlers;
pub mod middleware;
pub mod pagination;

// Cursor pagination exports for API consumers
#[allow(unused_imports)]
pub use pagination::{Cursor, CursorItem, CursorPage, CursorQuery};

use std::sync::Arc;
use std::time::Duration;

use axum::{
    routing::{delete, get, post, put},
    Router,
};
use tower::ServiceBuilder;
use tower_http::{
    compression::CompressionLayer,
    cors::{Any, CorsLayer},
    request_id::{MakeRequestUuid, PropagateRequestIdLayer, SetRequestIdLayer},
    timeout::TimeoutLayer,
    trace::TraceLayer,
};

use crate::cache::RedisCache;
use crate::config::Settings;
use crate::db::Database;
use crate::observability::Metrics;

use self::middleware::api_versioning_middleware;

/// Application state shared across handlers
#[derive(Clone)]
pub struct AppState {
    pub db: Database,
    pub cache: Option<RedisCache>,
    pub metrics: Arc<Metrics>,
    pub settings: Settings,
}

impl AppState {
    pub fn new(
        db: Database,
        cache: Option<RedisCache>,
        metrics: Metrics,
        settings: Settings,
    ) -> Self {
        Self {
            db,
            cache,
            metrics: Arc::new(metrics),
            settings,
        }
    }
}

/// Build the main application router
///
/// CORS is now configurable based on environment
/// - Development: Allow any origin (for local testing)
/// - Production: Restrict to configured origins only
pub fn router(state: AppState) -> Router {
    use crate::config::Environment;

    // Only allow Any origin in development
    // In production, CORS should be restricted to prevent CSRF attacks
    let cors = if state.settings.server.environment == Environment::Production {
        // In production, be restrictive with CORS
        // Origins should be configured via environment variable
        tracing::info!("Production mode: Using restrictive CORS policy");
        CorsLayer::new()
            .allow_methods([
                axum::http::Method::GET,
                axum::http::Method::POST,
                axum::http::Method::PUT,
                axum::http::Method::DELETE,
                axum::http::Method::OPTIONS,
            ])
            .allow_headers([
                axum::http::header::AUTHORIZATION,
                axum::http::header::CONTENT_TYPE,
                axum::http::header::HeaderName::from_static("x-api-key"),
                axum::http::header::HeaderName::from_static("x-request-id"),
            ])
            // In production, origins should come from config
            // For now, we use permissive but not "Any" - actual origins should be configured
            .allow_origin(tower_http::cors::AllowOrigin::mirror_request())
    } else {
        // Development mode: allow any origin for easier testing
        tracing::info!("Development mode: Using permissive CORS policy");
        CorsLayer::new()
            .allow_origin(Any)
            .allow_methods(Any)
            .allow_headers(Any)
    };

    #[allow(deprecated)] // TimeoutLayer::new is deprecated but with_status_code is not yet stable
    let timeout_layer = TimeoutLayer::new(Duration::from_secs(30));

    let middleware = ServiceBuilder::new()
        .layer(SetRequestIdLayer::x_request_id(MakeRequestUuid))
        .layer(PropagateRequestIdLayer::x_request_id())
        .layer(TraceLayer::new_for_http())
        .layer(CompressionLayer::new())
        .layer(timeout_layer)
        .layer(cors);

    Router::new()
        // Health endpoints (public - no auth required)
        .route("/health", get(handlers::health::health_check))
        .route("/health/live", get(handlers::health::liveness))
        .route("/health/ready", get(handlers::health::readiness))
        // Dashboard endpoint moved to /api/v1 to require authentication
        // Previously was at /health/dashboard without auth middleware, causing runtime errors
        // Metrics endpoint (Prometheus format)
        .route("/metrics", get(handlers::metrics::prometheus_metrics))
        // API v1 with versioning middleware
        .nest(
            "/api/v1",
            api_v1_router(state.clone())
                .layer(axum::middleware::from_fn(api_versioning_middleware)),
        )
        .layer(middleware)
        .with_state(state)
}

/// API v1 routes - split into public and protected
fn api_v1_router(state: AppState) -> Router<AppState> {
    // Public routes (no authentication required)
    let public_routes = Router::new()
        .route("/auth/login", post(handlers::auth::login))
        .route("/auth/refresh", post(handlers::auth::refresh_token))
        .route("/auth/validate", post(handlers::auth::validate_token))
        // Webhooks are authenticated via signatures, not API keys
        .route(
            "/webhooks/{org_id}/github",
            post(handlers::webhooks::github),
        )
        .route(
            "/webhooks/{org_id}/stripe",
            post(handlers::webhooks::stripe),
        )
        .route(
            "/webhooks/{org_id}/custom",
            post(handlers::webhooks::custom),
        )
        // Organization creation is public (for onboarding)
        .route("/organizations", post(handlers::organizations::create));

    // Protected routes (require API key authentication)
    // Auth middleware is applied via route_layer which runs AFTER state extraction
    let protected_routes = Router::new()
        // Auth endpoints that need context
        .route("/auth/logout", post(handlers::auth::logout))
        .route("/auth/me", get(handlers::auth::me))
        // Dashboard
        .route("/dashboard", get(handlers::health::dashboard_data))
        // Organizations (except create which is public)
        .route("/organizations", get(handlers::organizations::list))
        .route("/organizations/{id}", get(handlers::organizations::get))
        .route("/organizations/{id}", put(handlers::organizations::update))
        .route(
            "/organizations/{id}",
            delete(handlers::organizations::delete),
        )
        // Jobs - CRUD
        .route("/jobs", get(handlers::jobs::list))
        .route("/jobs", post(handlers::jobs::create))
        .route("/jobs/stats", get(handlers::jobs::stats))
        .route("/jobs/status", get(handlers::jobs::batch_status))
        .route("/jobs/bulk", post(handlers::jobs::bulk_enqueue))
        .route("/jobs/{id}", get(handlers::jobs::get))
        .route("/jobs/{id}", delete(handlers::jobs::cancel))
        .route("/jobs/{id}/retry", post(handlers::jobs::retry))
        .route("/jobs/{id}/priority", put(handlers::jobs::boost_priority))
        // Dead Letter Queue Management
        .route("/jobs/dlq", get(handlers::jobs::list_dlq))
        .route("/jobs/dlq/retry", post(handlers::jobs::retry_dlq))
        .route("/jobs/dlq/purge", post(handlers::jobs::purge_dlq))
        // Queues - Config and Control
        .route("/queues", get(handlers::queues::list))
        .route("/queues/{name}", get(handlers::queues::get))
        .route("/queues/{name}", delete(handlers::queues::delete))
        .route(
            "/queues/{name}/config",
            put(handlers::queues::update_config),
        )
        .route("/queues/{name}/stats", get(handlers::queues::stats))
        .route("/queues/{name}/pause", post(handlers::queues::pause))
        .route("/queues/{name}/resume", post(handlers::queues::resume))
        // Workers
        .route("/workers", get(handlers::workers::list))
        .route("/workers/register", post(handlers::workers::register))
        .route("/workers/{id}", get(handlers::workers::get))
        .route(
            "/workers/{id}/heartbeat",
            post(handlers::workers::heartbeat),
        )
        .route(
            "/workers/{id}/deregister",
            post(handlers::workers::deregister),
        )
        // API Keys
        .route("/api-keys", get(handlers::api_keys::list))
        .route("/api-keys", post(handlers::api_keys::create))
        .route("/api-keys/{id}", get(handlers::api_keys::get))
        .route("/api-keys/{id}", put(handlers::api_keys::update))
        .route("/api-keys/{id}", delete(handlers::api_keys::revoke))
        // Real-time endpoints (WebSocket and SSE)
        .route("/ws", get(handlers::realtime::websocket_handler))
        .route("/events", get(handlers::realtime::sse_events_handler))
        .route(
            "/events/jobs/{id}",
            get(handlers::realtime::sse_job_handler),
        )
        .route(
            "/events/queues/{name}",
            get(handlers::realtime::sse_queue_handler),
        )
        // Workflows
        .route("/workflows", get(handlers::workflows::list))
        .route("/workflows", post(handlers::workflows::create))
        .route("/workflows/{id}", get(handlers::workflows::get))
        .route("/workflows/{id}/cancel", post(handlers::workflows::cancel))
        // Job dependencies
        .route(
            "/jobs/{id}/dependencies",
            get(handlers::workflows::get_job_dependencies),
        )
        .route(
            "/jobs/{id}/dependencies",
            post(handlers::workflows::add_dependencies),
        )
        // Schedules (cron jobs)
        .route("/schedules", get(handlers::schedules::list))
        .route("/schedules", post(handlers::schedules::create))
        .route("/schedules/{id}", get(handlers::schedules::get))
        .route("/schedules/{id}", put(handlers::schedules::update))
        .route("/schedules/{id}", delete(handlers::schedules::delete))
        .route("/schedules/{id}/pause", post(handlers::schedules::pause))
        .route("/schedules/{id}/resume", post(handlers::schedules::resume))
        .route(
            "/schedules/{id}/trigger",
            post(handlers::schedules::trigger),
        )
        .route("/schedules/{id}/history", get(handlers::schedules::history))
        // Outgoing Webhooks (notification configuration)
        .route(
            "/outgoing-webhooks",
            get(handlers::outgoing_webhooks::list),
        )
        .route(
            "/outgoing-webhooks",
            post(handlers::outgoing_webhooks::create),
        )
        .route(
            "/outgoing-webhooks/{id}",
            get(handlers::outgoing_webhooks::get),
        )
        .route(
            "/outgoing-webhooks/{id}",
            put(handlers::outgoing_webhooks::update),
        )
        .route(
            "/outgoing-webhooks/{id}",
            delete(handlers::outgoing_webhooks::delete),
        )
        .route(
            "/outgoing-webhooks/{id}/test",
            post(handlers::outgoing_webhooks::test),
        )
        .route(
            "/outgoing-webhooks/{id}/deliveries",
            get(handlers::outgoing_webhooks::deliveries),
        )
        // Apply authentication middleware to all protected routes
        // route_layer runs the middleware for matched routes only
        .route_layer(axum::middleware::from_fn_with_state(
            state,
            middleware::auth::authenticate_api_key,
        ));

    // Merge public and protected routes
    public_routes.merge(protected_routes)
}
