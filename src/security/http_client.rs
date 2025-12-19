//! Secure outbound HTTP client helpers
//!
//! This module centralizes configuration for outbound HTTP clients that may
//! send requests to user-provided URLs (e.g., outgoing webhooks).
//!
//! # Security
//! - Prevents SSRF via open redirects by validating redirect targets
//! - Caps redirect chain length
//! - Enforces sane timeouts

use std::time::Duration;

use reqwest::redirect::Policy;
use reqwest::Client;

use crate::security::{validate_webhook_url, UrlValidationOptions};

/// Maximum redirects to follow for outbound webhook requests.
///
/// Keeping this low reduces SSRF surface area and avoids long redirect chains.
const MAX_REDIRECTS: usize = 5;

/// Build a hardened outbound HTTP client for requests to user-provided URLs.
///
/// Notes:
/// - Redirects are allowed only if every redirect target also passes SSRF validation.
/// - We intentionally validate the redirect URL synchronously; it runs rarely and is capped.
pub fn build_outbound_http_client(
    timeout: Duration,
    connect_timeout: Duration,
    user_agent: &str,
) -> Client {
    Client::builder()
        .timeout(timeout)
        .connect_timeout(connect_timeout)
        .redirect(Policy::custom(|attempt| {
            // Prevent long redirect chains.
            if attempt.previous().len() >= MAX_REDIRECTS {
                return attempt.stop();
            }

            // Validate the next redirect target to prevent SSRF via redirects.
            let next_url = attempt.url().as_str();
            let opts = UrlValidationOptions::default();
            match validate_webhook_url(next_url, &opts) {
                Ok(()) => attempt.follow(),
                Err(_) => attempt.stop(),
            }
        }))
        .user_agent(user_agent)
        .build()
        .expect("Failed to create outbound HTTP client")
}

#[cfg(test)]
mod tests {
    use super::*;

    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    #[tokio::test]
    async fn outbound_client_does_not_follow_redirects_to_blocked_hosts() {
        // Local server that responds with a redirect to a blocked metadata endpoint.
        let server = MockServer::start().await;

        Mock::given(method("POST"))
            .and(path("/redirect"))
            .respond_with(ResponseTemplate::new(302).insert_header(
                "Location",
                "http://169.254.169.254/latest/meta-data/iam/security-credentials/",
            ))
            .expect(1)
            .mount(&server)
            .await;

        let client = build_outbound_http_client(
            Duration::from_secs(5),
            Duration::from_secs(2),
            "Spooled-Webhook-Test/1.0",
        );

        // If redirects were followed, reqwest would attempt to reach the metadata IP and error.
        // With our hardened policy, it should STOP and return the original 302 response.
        let res = client
            .post(format!("{}/redirect", server.uri()))
            .body("{}")
            .send()
            .await
            .expect("request should succeed without following redirect");

        assert!(res.status().is_redirection());
        assert_eq!(res.status().as_u16(), 302);
    }
}
