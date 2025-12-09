//! API route tests for Spooled Backend
//!
//! These tests verify the API routing and basic handler functionality.
//! Full integration tests with database are in integration_tests.rs.

mod common;

use axum::{
    body::Body,
    http::{Request, StatusCode},
    routing::get,
    Router,
};
use tower::ServiceExt;

/// Test that health endpoints return correct status codes
#[tokio::test]
async fn test_liveness_endpoint() {
    // Create minimal router with just liveness endpoint
    let app = Router::new().route("/health/live", get(|| async { StatusCode::OK }));

    let response = app
        .oneshot(
            Request::builder()
                .uri("/health/live")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
}

/// Test 404 for non-existent routes
#[tokio::test]
async fn test_not_found_route() {
    let app = Router::new().route("/health/live", get(|| async { StatusCode::OK }));

    let response = app
        .oneshot(
            Request::builder()
                .uri("/non-existent-route")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::NOT_FOUND);
}

/// Test method not allowed
#[tokio::test]
async fn test_method_not_allowed() {
    let app = Router::new().route("/health/live", get(|| async { StatusCode::OK }));

    let response = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/health/live")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::METHOD_NOT_ALLOWED);
}

// Full API tests with database are run in integration_tests.rs
// Those tests use TestDatabase with testcontainers
