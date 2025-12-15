//! gRPC Service implementations
//!
//! Contains the tonic service implementations for QueueService and WorkerService.

pub mod queue_service;
pub mod worker_service;

pub use queue_service::QueueServiceImpl;
pub use worker_service::WorkerServiceImpl;
