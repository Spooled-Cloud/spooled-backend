//! gRPC module for Spooled Backend
//!
//! High-performance gRPC API for worker communication and job processing.
//!
//! Note: This module uses manually defined types to avoid protoc dependency.
//! The API is compatible with the proto definitions in proto/spooled.proto.

pub mod service;
pub mod types;

// Re-exports for external API consumers
pub use service::start_grpc_server;
#[allow(unused_imports)]
pub use service::SpooledGrpcService;
#[allow(unused_imports)]
pub use types::*;
