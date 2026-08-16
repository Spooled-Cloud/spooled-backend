//! Secure outbound HTTP client helpers
//!
//! This module centralizes configuration for outbound HTTP clients that may
//! send requests to user-provided URLs (e.g., outgoing webhooks).
//!
//! # Security
//! - Prevents SSRF via open redirects by validating redirect targets
//! - Caps redirect chain length
//! - Enforces sane timeouts

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use reqwest::dns::{Addrs, Name, Resolve, Resolving};
use reqwest::redirect::Policy;
use reqwest::Client;
use tracing::warn;

use crate::security::url_validator::is_ip_allowed;
use crate::security::{validate_webhook_url, UrlValidationOptions};

/// Maximum redirects to follow for outbound webhook requests.
///
/// Keeping this low reduces SSRF surface area and avoids long redirect chains.
const MAX_REDIRECTS: usize = 5;

/// Upper bound on a single DNS lookup for a user-supplied host.
///
/// The platform default is minutes. The hostname is attacker-supplied, so a
/// nameserver that accepts queries and never answers would otherwise pin a
/// delivery (and its concurrency slot) for that whole time.
const DNS_TIMEOUT: Duration = Duration::from_secs(5);

/// DNS resolver that applies the SSRF IP policy to EVERY address it returns.
///
/// This is what actually prevents DNS rebinding. Validating the URL and then
/// handing the hostname to the HTTP client resolves the name twice — once for
/// the check, once for the connection — and an attacker who controls the zone
/// can answer differently the second time, which is the whole attack. Because
/// this runs inside the client's own resolution step, the address that gets
/// connected to is necessarily the address that was validated, and it covers
/// redirect hops for free.
///
/// It is also the async fix for the previous blocking `to_socket_addrs()` call:
/// resolution happens on a blocking thread pool, not on a runtime worker.
#[derive(Debug)]
struct SsrfGuardedResolver {
    options: UrlValidationOptions,
}

impl Resolve for SsrfGuardedResolver {
    fn resolve(&self, name: Name) -> Resolving {
        let host = name.as_str().to_string();
        let options = self.options.clone();
        Box::pin(async move {
            let lookup_host = host.clone();
            // Port 0: reqwest overrides it with the URL's port or the scheme
            // default. We only care about the addresses.
            let resolved = tokio::time::timeout(
                DNS_TIMEOUT,
                tokio::task::spawn_blocking(move || {
                    use std::net::ToSocketAddrs;
                    (lookup_host.as_str(), 0u16)
                        .to_socket_addrs()
                        .map(|it| it.collect::<Vec<SocketAddr>>())
                }),
            )
            .await
            .map_err(|_| -> Box<dyn std::error::Error + Send + Sync> {
                warn!(host = %host, "DNS resolution timed out for outbound request");
                "DNS resolution timed out".into()
            })?
            .map_err(|e| -> Box<dyn std::error::Error + Send + Sync> {
                format!("DNS resolution task failed: {e}").into()
            })?
            .map_err(|e| -> Box<dyn std::error::Error + Send + Sync> {
                format!("DNS resolution failed: {e}").into()
            })?;

            if resolved.is_empty() {
                return Err("DNS resolution returned no addresses".into());
            }

            // Reject the whole lookup if ANY answer is blocked. Filtering to the
            // allowed subset would let an attacker mix one public address in
            // with internal ones and still get a connection attempt.
            for addr in &resolved {
                if let Err(e) = is_ip_allowed(&addr.ip(), &options) {
                    warn!(
                        host = %host,
                        resolved_ip = %addr.ip(),
                        error = %e,
                        "Blocked outbound request: hostname resolved to a disallowed address"
                    );
                    return Err(format!("blocked address {} for host {}", addr.ip(), host).into());
                }
            }

            let addrs: Addrs = Box::new(resolved.into_iter());
            Ok(addrs)
        })
    }
}

/// Build a hardened outbound HTTP client for requests to user-provided URLs.
///
/// Layers, outermost first:
/// - a validating DNS resolver, so the connected address is always one that
///   passed the IP policy (no rebinding window, no blocking lookups);
/// - a redirect policy that re-validates every hop and caps the chain;
/// - explicit timeouts, so a slow peer cannot hold a delivery slot forever.
pub fn build_outbound_http_client(
    timeout: Duration,
    connect_timeout: Duration,
    user_agent: &str,
) -> Client {
    let resolver = Arc::new(SsrfGuardedResolver {
        options: UrlValidationOptions::default(),
    });

    Client::builder()
        .timeout(timeout)
        .connect_timeout(connect_timeout)
        .dns_resolver(resolver)
        .redirect(Policy::custom(|attempt| {
            // Prevent long redirect chains.
            if attempt.previous().len() >= MAX_REDIRECTS {
                return attempt.stop();
            }

            // Scheme/host checks for the next hop. The IP policy for that hop is
            // enforced by the resolver above when the connection is made, so
            // this deliberately does NOT need to resolve anything itself — which
            // is what kept a blocking DNS call inside this synchronous closure.
            let next_url = attempt.url().as_str();
            let opts = UrlValidationOptions {
                check_dns_resolution: false,
                ..UrlValidationOptions::default()
            };
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
