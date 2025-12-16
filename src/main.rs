//! Spooled Backend - Production-ready webhook queue service
//!
//! This is the main entry point for the Spooled Cloud backend service.
//! It initializes the server, database connections, and all subsystems.

// Allow dead code - components are prepared for future integration
#![allow(dead_code)]

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use tokio::signal;
use tokio::sync::watch;
use tracing::{error, info, warn};

mod api;
mod cache;
mod config;
mod db;
mod error;
mod grpc;
mod models;
mod observability;
mod queue;
mod scheduler;
mod webhook;
mod worker;

use crate::config::Settings;
use crate::db::Database;
use crate::scheduler::Scheduler;

/// Graceful shutdown timeout (max time to wait for in-flight requests)
const SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(30);

#[tokio::main]
async fn main() -> Result<()> {
    // Load environment variables from .env file if present
    let _ = dotenvy::dotenv();

    // Initialize tracing/logging
    observability::init_tracing();

    info!("Starting Spooled Backend v{}", env!("CARGO_PKG_VERSION"));

    // Load configuration
    let settings = Settings::load()?;
    info!(
        host = %settings.server.host,
        port = %settings.server.port,
        environment = %settings.server.environment,
        "Configuration loaded"
    );

    // Initialize database connection pool
    let db = Database::connect(&settings.database).await?;
    info!("Database connection pool established");

    // Run migrations
    db.run_migrations().await?;
    info!("Database migrations completed");

    // Initialize Redis cache (optional, graceful degradation if unavailable)
    let cache = match cache::RedisCache::connect(&settings.redis).await {
        Ok(cache) => {
            info!("Redis cache connected");
            Some(cache)
        }
        Err(e) => {
            warn!(error = %e, "Redis unavailable, running without cache");
            None
        }
    };

    // Initialize metrics
    let metrics = observability::Metrics::new();
    info!("Prometheus metrics initialized");

    // Build application state
    let state = api::AppState::new(db.clone(), cache.clone(), metrics, settings.clone());

    // Build the router
    let app = api::router(state.clone());

    // Bind to address
    let addr = SocketAddr::new(settings.server.host.parse()?, settings.server.port);
    let listener = tokio::net::TcpListener::bind(addr).await?;
    info!(%addr, "Server listening");

    // Start metrics server on separate port
    // Pass optional metrics token for authentication
    let metrics_addr = SocketAddr::new(settings.server.host.parse()?, settings.server.metrics_port);
    let metrics_token = std::env::var("METRICS_TOKEN").ok();
    let metrics_server = observability::start_metrics_server(
        metrics_addr,
        state.metrics.clone(),
        metrics_token.clone(),
    );
    let metrics_handle = tokio::spawn(metrics_server);
    info!(
        %metrics_addr,
        auth_required = metrics_token.is_some(),
        "Metrics server listening"
    );

    // Create shutdown channel for all components
    let (shutdown_tx, shutdown_rx) = watch::channel(false);

    // Start gRPC server (optional, on port 50051)
    let grpc_addr: SocketAddr = format!("{}:50051", settings.server.host).parse()?;
    let grpc_db = Arc::new(db.clone());
    let grpc_metrics = state.metrics.clone();
    let grpc_shutdown_rx = shutdown_rx.clone();
    let grpc_handle = tokio::spawn(async move {
        // Wait a bit to not block startup
        tokio::time::sleep(Duration::from_millis(100)).await;

        // Run until shutdown
        let mut rx = grpc_shutdown_rx;
        tokio::select! {
            _ = grpc::start_grpc_server(grpc_addr, grpc_db, grpc_metrics) => {}
            _ = rx.changed() => {
                if *rx.borrow() {
                    info!("gRPC server shutdown signal received");
                }
            }
        }
    });
    info!(%grpc_addr, "gRPC server listening");

    // Start background scheduler
    let scheduler = Scheduler::new(
        state.db.pool_arc(),
        state.cache.as_ref().map(|c| Arc::new(c.clone())),
        state.metrics.clone(),
    );
    let scheduler_shutdown_rx = shutdown_rx.clone();
    let scheduler_handle = tokio::spawn(async move {
        if let Err(e) = scheduler.run(scheduler_shutdown_rx).await {
            error!(error = %e, "Scheduler error");
        }
    });
    info!("Background scheduler started");

    // Run server with graceful shutdown
    let shutdown_signal = shutdown_signal(shutdown_tx.clone());
    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal)
        .await?;

    info!("HTTP server stopped, draining connections...");

    // Signal all components to shutdown
    let _ = shutdown_tx.send(true);

    // Wait for components to shutdown with timeout
    let shutdown_futures = async {
        let _ = scheduler_handle.await;
        info!("Scheduler shutdown complete");

        // gRPC server will stop on its own
        let _ = grpc_handle.await;
        info!("gRPC server shutdown complete");

        // Metrics server - just abort it
        metrics_handle.abort();
        info!("Metrics server shutdown complete");
    };

    // Apply timeout to shutdown
    match tokio::time::timeout(SHUTDOWN_TIMEOUT, shutdown_futures).await {
        Ok(_) => {
            info!("All components shutdown gracefully");
        }
        Err(_) => {
            warn!(
                "Shutdown timeout ({:?}) exceeded, forcing exit",
                SHUTDOWN_TIMEOUT
            );
        }
    }

    // Release jobs held by this instance (if any workers are running locally)
    release_local_jobs(&state.db).await;

    // Close database connections
    state.db.close().await;
    info!("Database connections closed");

    info!("Server shutdown complete");
    Ok(())
}

/// Release any jobs that were being processed by local workers
///
/// Now validates organization matches between worker and job
/// to prevent cross-tenant job release during shutdown
async fn release_local_jobs(db: &Database) {
    // Get local hostname to identify jobs held by this instance
    let hostname = hostname::get()
        .ok()
        .and_then(|h| h.into_string().ok())
        .unwrap_or_else(|| "unknown".to_string());

    // This ensures jobs are only released if they belong to the same org as the worker
    let result = sqlx::query(
        r#"
        UPDATE jobs
        SET 
            status = 'pending',
            assigned_worker_id = NULL,
            lease_id = NULL,
            lease_expires_at = NULL,
            last_error = 'Server shutdown - job released',
            updated_at = NOW()
        WHERE status = 'processing'
          AND EXISTS (
              SELECT 1 FROM workers w
              WHERE w.id = jobs.assigned_worker_id
                AND w.hostname = $1
                AND w.organization_id = jobs.organization_id
          )
        "#,
    )
    .bind(&hostname)
    .execute(db.pool())
    .await;

    match result {
        Ok(res) if res.rows_affected() > 0 => {
            info!(count = res.rows_affected(), hostname = %hostname, "Released jobs held by local workers (with org validation)");
        }
        Ok(_) => {
            info!(hostname = %hostname, "No jobs to release during shutdown");
        }
        Err(e) => {
            warn!(error = %e, "Failed to release local jobs during shutdown");
        }
    }
}

/// Listens for shutdown signals (Ctrl+C or SIGTERM)
async fn shutdown_signal(shutdown_tx: watch::Sender<bool>) {
    let ctrl_c = async {
        signal::ctrl_c()
            .await
            .expect("Failed to install Ctrl+C handler");
    };

    #[cfg(unix)]
    let terminate = async {
        signal::unix::signal(signal::unix::SignalKind::terminate())
            .expect("Failed to install SIGTERM handler")
            .recv()
            .await;
    };

    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    tokio::select! {
        _ = ctrl_c => {
            info!("Received Ctrl+C, initiating graceful shutdown");
        }
        _ = terminate => {
            info!("Received SIGTERM, initiating graceful shutdown");
        }
    }

    // Broadcast shutdown to all components
    let _ = shutdown_tx.send(true);
}
