//! Spooled Backend Library
//!
//! This library exposes the core components for testing and embedding.

// Allow dead code in library - components are prepared for future use by SDKs
#![allow(dead_code)]

pub mod api;
pub mod cache;
pub mod config;
pub mod db;
pub mod error;
/// gRPC module - provides worker-backend communication
/// Now properly exposed (was inconsistently commented out)
pub mod grpc;
pub mod models;
pub mod observability;
pub mod queue;
pub mod scheduler;
pub mod webhook;
pub mod worker;
