//! Spooled Backend - Production-ready webhook queue service
//!
//! This is the main entry point for the Spooled Cloud backend service.
//! It initializes the server, database connections, and all subsystems.

// Allow dead code - components are prepared for future integration
#![allow(dead_code)]

use std::future::{Future, IntoFuture};
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use tokio::signal;
use tokio::sync::{mpsc, watch};
use tracing::{error, info, warn};

mod api;
mod cache;
mod config;
mod db;
mod error;
mod grpc;
mod models;
mod observability;
mod outgoing_webhooks;
mod queue;
mod scheduler;
mod security;

use crate::api::handlers::health::RuntimeHealth;
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

    // Initialize Redis cache. Optional for self-host; a Redis the operator
    // configured (REDIS_URL) or that fail-closed rate limiting needs is kept
    // as a reconnecting client when it is not up yet, instead of being left
    // out until the next restart.
    let redis_required = settings.rate_limit.fail_closed_on_redis_error
        || config::non_empty_env("REDIS_URL").is_some();
    let cache = cache::RedisCache::connect_at_startup(&settings.redis, redis_required).await;

    // Initialize metrics
    let metrics = observability::Metrics::new();
    info!("Prometheus metrics initialized");

    // Build application state
    let state = api::AppState::new(db.clone(), cache.clone(), metrics, settings.clone());

    // Liveness of the gRPC server and the scheduler, reported by /health
    let runtime = Arc::new(RuntimeHealth::default());

    // Build the router
    let app = api::router(state.clone()).layer(axum::Extension(Arc::clone(&runtime)));

    // Bind to address
    let addr = SocketAddr::new(settings.server.host.parse()?, settings.server.port);
    let listener = tokio::net::TcpListener::bind(addr).await?;
    info!(%addr, "Server listening");

    // Create shutdown channel for all components
    let (shutdown_tx, shutdown_rx) = watch::channel(false);

    // Start gRPC server (configured via GRPC_PORT env var).
    // Build the address without "host:port" string concatenation: an IPv6 HOST (e.g. "::1")
    // needs bracket form ("[::1]:port"), so the old concat produced an unparseable address
    // and crashed startup. SocketAddr::new handles v4 and v6 literals correctly.
    let grpc_addr: SocketAddr = if let Ok(ip) = settings.server.host.parse::<std::net::IpAddr>() {
        SocketAddr::new(ip, settings.server.grpc_port)
    } else {
        format!("{}:{}", settings.server.host, settings.server.grpc_port).parse()?
    };
    let grpc_deps = grpc::server::GrpcServerDeps {
        db: Arc::new(db.clone()),
        metrics: state.metrics.clone(),
        cache: cache.clone(),
        outgoing_webhooks: state.outgoing_webhooks.clone(),
        queue_defaults: settings.queue.clone(),
        rate_limit_fail_closed: settings.rate_limit.fail_closed_on_redis_error,
        worker_settings: settings.worker.clone(),
    };
    // Binding (and loading TLS) happens here, before anything is spawned, so
    // a bad certificate or a busy port stops the process instead of leaving
    // it up without gRPC.
    let grpc_server = grpc::server::GrpcServer::bind(grpc_addr, grpc_deps)
        .await
        .map_err(|e| {
            error!(%grpc_addr, error = %e, "gRPC server failed to start");
            anyhow::anyhow!("gRPC server failed to start on {grpc_addr}: {e}")
        })?;

    // The metrics server, the gRPC server and the scheduler must run as long
    // as the process does. Each reports on `ended` when it stops; stopping
    // before shutdown was asked for is a failure.
    let (ended_tx, mut ended_rx) = mpsc::unbounded_channel();
    let mut running = 0;

    // Start metrics server on separate port
    // Pass optional metrics token for authentication
    let metrics_addr = SocketAddr::new(settings.server.host.parse()?, settings.server.metrics_port);
    let metrics_token = settings.server.metrics_token.clone();
    let metrics_server = observability::start_metrics_server(
        metrics_addr,
        state.metrics.clone(),
        metrics_token.clone(),
    );
    let metrics_task = supervise("metrics server", metrics_server, ended_tx.clone());
    running += 1;
    info!(
        %metrics_addr,
        auth_required = metrics_token.is_some(),
        "Metrics server listening"
    );

    let grpc_serve = grpc_server.serve(
        shutdown_rx.clone(),
        grpc::server::GRPC_DRAIN_TIMEOUT,
        Some(Arc::clone(&runtime)),
    );
    supervise("gRPC server", grpc_serve, ended_tx.clone());
    running += 1;
    info!(%grpc_addr, "gRPC server started");

    // Start background scheduler
    let scheduler = Scheduler::new(
        state.db.pool_arc(),
        state.cache.as_ref().map(|c| Arc::new(c.clone())),
        state.metrics.clone(),
        state.outgoing_webhooks.clone(),
    )
    .with_runtime_health(Arc::clone(&runtime));
    let scheduler_shutdown_rx = shutdown_rx.clone();
    let scheduler_run = async move { scheduler.run(scheduler_shutdown_rx).await };
    supervise("scheduler", scheduler_run, ended_tx.clone());
    running += 1;
    info!("Background scheduler started");

    // Serve HTTP until Ctrl+C / SIGTERM, or until one of the tasks above
    // stops on its own. Then shut everything down; after a failure the
    // process exits non-zero so the container restart policy brings it back.
    // The TCP peer rides along on every request: client-IP resolution believes
    // CF-Connecting-IP / X-Real-IP only from an internal peer (a proxy of ours).
    let http = axum::serve(
        listener,
        app.into_make_service_with_connect_info::<SocketAddr>(),
    )
    .with_graceful_shutdown(shutdown_requested(shutdown_rx.clone()))
    .into_future();
    let mut http = std::pin::pin!(http);
    let mut http_done = false;
    let mut failure: Option<String> = None;
    tokio::select! {
        result = &mut http => {
            http_done = true;
            failure = Some(match result {
                Ok(()) => "HTTP server stopped unexpectedly".to_string(),
                Err(e) => format!("HTTP server failed: {e}"),
            });
        }
        _ = shutdown_signal() => {}
        Some(ended) = ended_rx.recv() => {
            running -= 1;
            let reason = ended.error.unwrap_or_else(|| "ended unexpectedly".to_string());
            failure = Some(format!("{} stopped: {}", ended.name, reason));
        }
    }
    if let Some(failure) = &failure {
        error!(%failure, "A server task stopped, shutting down");
    }

    // HTTP and gRPC drain side by side (gRPC health turns NOT_SERVING first),
    // the scheduler finishes its current pass. Prometheus scrapes need no
    // drain.
    info!("Shutting down, draining HTTP and gRPC");
    let _ = shutdown_tx.send(true);
    metrics_task.abort();

    let drain = async {
        if !http_done {
            if let Err(e) = (&mut http).await {
                warn!(error = %e, "HTTP server error while draining");
            }
            info!("HTTP server stopped");
        }
        while running > 0 {
            let Some(ended) = ended_rx.recv().await else {
                break;
            };
            running -= 1;
            match ended.error {
                Some(e) => warn!(task = ended.name, error = %e, "Stopped with an error"),
                None => info!(task = ended.name, "Shutdown complete"),
            }
        }
    };

    // Apply timeout to shutdown
    match tokio::time::timeout(SHUTDOWN_TIMEOUT, drain).await {
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

    if let Some(failure) = failure {
        return Err(anyhow::anyhow!(failure));
    }

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

/// How a supervised task ended.
struct TaskEnd {
    name: &'static str,
    /// The error it returned, or its panic; None when it returned cleanly
    /// or was aborted.
    error: Option<String>,
}

/// The error in a finished task's output, whatever its return type.
trait TaskOutput {
    fn into_error(self) -> Option<String>;
}

impl TaskOutput for () {
    fn into_error(self) -> Option<String> {
        None
    }
}

impl<E: std::fmt::Display> TaskOutput for std::result::Result<(), E> {
    fn into_error(self) -> Option<String> {
        self.err().map(|e| format!("{e:#}"))
    }
}

/// Spawn `task` and send a [`TaskEnd`] on `ended` when it finishes, panics
/// included. Returns the handle to abort the task.
fn supervise<F>(
    name: &'static str,
    task: F,
    ended: mpsc::UnboundedSender<TaskEnd>,
) -> tokio::task::AbortHandle
where
    F: Future + Send + 'static,
    F::Output: TaskOutput + Send + 'static,
{
    let handle = tokio::spawn(task);
    let abort = handle.abort_handle();
    tokio::spawn(async move {
        let error = match handle.await {
            Ok(output) => output.into_error(),
            Err(e) if e.is_cancelled() => None,
            Err(e) => Some(panic_message(e)),
        };
        let _ = ended.send(TaskEnd { name, error });
    });
    abort
}

fn panic_message(error: tokio::task::JoinError) -> String {
    match error.try_into_panic() {
        Ok(panic) => {
            let detail = panic
                .downcast_ref::<&str>()
                .map(|s| s.to_string())
                .or_else(|| panic.downcast_ref::<String>().cloned())
                .unwrap_or_else(|| "unknown panic".to_string());
            format!("panicked: {detail}")
        }
        Err(error) => error.to_string(),
    }
}

/// Resolves once the shutdown flag is set.
async fn shutdown_requested(mut shutdown: watch::Receiver<bool>) {
    let _ = shutdown.wait_for(|stop| *stop).await;
}

/// Resolves on a shutdown signal (Ctrl+C or SIGTERM)
async fn shutdown_signal() {
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
}

#[cfg(test)]
mod tests {
    use super::SHUTDOWN_TIMEOUT;

    /// The whole-seconds value after `key` in a manifest (`40`, `40s`).
    fn grace_secs(manifest: &str, key: &str) -> u64 {
        let line = manifest
            .lines()
            .map(str::trim)
            .find(|line| line.starts_with(key))
            .unwrap_or_else(|| panic!("no {key} in the manifest"));
        line[key.len()..]
            .trim()
            .trim_end_matches('s')
            .parse()
            .unwrap_or_else(|e| panic!("{line}: {e}"))
    }

    // BEOPS-24: an orchestrator that kills the container before the shutdown
    // bound plus cleanup (releasing jobs, closing the pool) has passed cuts
    // the drain off. Docker (stop_grace_period) and Kubernetes
    // (terminationGracePeriodSeconds) must both wait for it.
    #[test]
    fn orchestrators_wait_past_the_shutdown_timeout() {
        let needed = SHUTDOWN_TIMEOUT.as_secs() + 10;
        let compose = grace_secs(
            include_str!("../docker-compose.prod.yml"),
            "stop_grace_period:",
        );
        let k8s = grace_secs(
            include_str!("../k8s/base/deployment.yaml"),
            "terminationGracePeriodSeconds:",
        );
        assert!(
            compose >= needed,
            "stop_grace_period {compose}s < {needed}s"
        );
        assert!(
            k8s >= needed,
            "terminationGracePeriodSeconds {k8s} < {needed}"
        );
    }
}
