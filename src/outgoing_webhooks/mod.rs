//! Outgoing webhook dispatch and delivery
//!
//! This module is responsible for:
//! - Selecting enabled outgoing webhooks for an organization/event
//! - Recording delivery rows in `outgoing_webhook_deliveries`
//! - Performing HTTP delivery (async) and updating delivery + webhook status

pub mod service;


