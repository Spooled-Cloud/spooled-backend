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
pub mod grpc;
pub mod models;
pub mod observability;
pub mod outgoing_webhooks;
pub mod queue;
pub mod scheduler;
pub mod webhook;
pub mod worker;
