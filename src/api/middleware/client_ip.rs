//! Trusted client-IP extraction.
//!
//! This is the single source of truth for "who is calling", used by every
//! abuse control: the public and admin rate limiters, and the email-login
//! throttles. It exists because the codebase previously had four independent
//! extractors with three different behaviours, and the two guarding the rate
//! limiters were the spoofable ones.
//!
//! # Why `X-Forwarded-For` is not trusted by default
//!
//! `X-Forwarded-For` is a list that each proxy **appends** to. Its leftmost
//! entry is therefore whatever the original client sent — fully
//! attacker-controlled. Keying a rate limiter on it does not weaken the limiter,
//! it removes it: the caller lands in a fresh bucket on every request just by
//! varying the header.
//!
//! The genuine client address is at the RIGHT-hand end, behind however many
//! proxies we actually operate. That count is deployment knowledge, not
//! something the request can tell us, so it comes from configuration
//! (`TRUSTED_PROXY_HOPS`) and defaults to `0` — trust nothing.

use axum::http::HeaderMap;

use crate::config::TrustedProxySettings;

/// Stable bucket id used when no client address can be established.
///
/// Deliberately a single shared constant rather than a random per-request value:
/// a random id would let a caller escape rate limiting simply by omitting every
/// identifying header.
pub const UNKNOWN_CLIENT: &str = "unknown";

/// Longest plausible textual IP (IPv6 with zone/scope), used to reject junk
/// before it becomes a Redis key.
const MAX_IP_LEN: usize = 45;

/// Edge headers consulted when no explicit `TRUSTED_CLIENT_IP_HEADER` is set.
///
/// These are written by the reverse proxy (and rewritten by it on every
/// request), so on a proxied deployment a client cannot forge them; on a
/// deployment with no proxy they are simply absent. Consulting them by default
/// matters because the alternative — returning [`UNKNOWN_CLIENT`] for
/// everything — collapses every caller into ONE rate-limit bucket, which is
/// worse than the spoofable behaviour this module replaced. Production fronts
/// the API with a Cloudflare Tunnel, which sets `CF-Connecting-IP`.
const DEFAULT_EDGE_HEADERS: &[&str] = &["CF-Connecting-IP", "X-Real-IP"];

/// Resolve the client IP to use for abuse controls.
///
/// Order of trust:
/// 1. The configured edge header (`TRUSTED_CLIENT_IP_HEADER`).
/// 2. Failing that, the well-known edge headers in [`DEFAULT_EDGE_HEADERS`], so
///    a deployment behind Cloudflare or nginx is protected without configuration.
/// 3. `X-Forwarded-For`, counted from the RIGHT past `TRUSTED_PROXY_HOPS`
///    entries our own infrastructure appended. With the default of `0` hops this
///    branch never runs — with no known proxy count there is no entry we can
///    vouch for.
/// 4. [`UNKNOWN_CLIENT`].
///
/// Returns a bare address string; callers namespace it into their own key.
pub fn client_ip(headers: &HeaderMap, settings: &TrustedProxySettings) -> String {
    if let Some(header_name) = settings.client_ip_header.as_deref() {
        if let Some(ip) = header_value(headers, header_name) {
            return ip;
        }
    } else {
        for name in DEFAULT_EDGE_HEADERS {
            if let Some(ip) = header_value(headers, name) {
                return ip;
            }
        }
    }

    if settings.forwarded_hops > 0 {
        if let Some(forwarded) = headers.get("X-Forwarded-For").and_then(|v| v.to_str().ok()) {
            let entries: Vec<&str> = forwarded.split(',').collect();
            // Each proxy APPENDS the address of its immediate peer. With `hops`
            // proxies of our own, the last `hops` entries are the ones they
            // wrote, and the first of those — index `len - hops` — is the
            // address the OUTERMOST proxy actually observed: the real client.
            // Everything to its left is caller-supplied and must be ignored.
            //
            // The index is not cosmetic. `len - hops - 1` selects the entry the
            // CALLER controls and reinstates the exact rate-limit bypass this
            // module exists to close: send `X-Forwarded-For: <random>` and the
            // proxy appends the true address after it, so the attacker's value
            // is the one picked.
            if entries.len() >= settings.forwarded_hops {
                let idx = entries.len() - settings.forwarded_hops;
                if let Some(ip) = entries.get(idx).and_then(|e| sanitize(e)) {
                    return ip;
                }
            }
        }
    }

    UNKNOWN_CLIENT.to_string()
}

/// Read and sanitize a single header value.
fn header_value(headers: &HeaderMap, name: &str) -> Option<String> {
    headers
        .get(name)
        .and_then(|v| v.to_str().ok())
        .and_then(sanitize)
}

/// Trim and bound a candidate address, rejecting empty/oversized junk.
fn sanitize(raw: &str) -> Option<String> {
    let trimmed = raw.trim();
    if trimmed.is_empty() || trimmed.len() > MAX_IP_LEN {
        return None;
    }
    Some(trimmed.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn headers(pairs: &[(&str, &str)]) -> HeaderMap {
        let mut h = HeaderMap::new();
        for (k, v) in pairs {
            h.insert(
                axum::http::HeaderName::from_bytes(k.as_bytes()).unwrap(),
                v.parse().unwrap(),
            );
        }
        h
    }

    fn cf_settings() -> TrustedProxySettings {
        TrustedProxySettings {
            client_ip_header: Some("CF-Connecting-IP".to_string()),
            forwarded_hops: 1,
        }
    }

    #[test]
    fn prefers_configured_edge_header() {
        let h = headers(&[
            ("CF-Connecting-IP", "203.0.113.7"),
            ("X-Forwarded-For", "1.2.3.4, 203.0.113.7"),
        ]);
        assert_eq!(client_ip(&h, &cf_settings()), "203.0.113.7");
    }

    #[test]
    fn ignores_spoofed_leftmost_forwarded_entry() {
        // Attacker sends "9.9.9.9"; our single proxy appends the address it
        // actually saw. With 1 trusted hop the LAST entry is the real client.
        let h = headers(&[("X-Forwarded-For", "9.9.9.9, 203.0.113.7")]);
        assert_eq!(
            client_ip(&h, &cf_settings()),
            "203.0.113.7",
            "must pick the proxy-appended address, never the caller's claim"
        );

        // A longer spoofed chain changes nothing: still the proxy's entry.
        let h = headers(&[("X-Forwarded-For", "a, b, c, 203.0.113.7")]);
        assert_eq!(client_ip(&h, &cf_settings()), "203.0.113.7");

        // Client sends nothing; the single proxy's entry is the whole chain.
        let h = headers(&[("X-Forwarded-For", "203.0.113.7")]);
        assert_eq!(client_ip(&h, &cf_settings()), "203.0.113.7");
    }

    #[test]
    fn two_hops_skips_both_proxy_entries() {
        // Cloudflare appends the client, nginx appends Cloudflare.
        let settings = TrustedProxySettings {
            client_ip_header: None,
            forwarded_hops: 2,
        };
        let h = headers(&[("X-Forwarded-For", "spoof, 203.0.113.7, 172.16.0.9")]);
        assert_eq!(client_ip(&h, &settings), "203.0.113.7");
    }

    #[test]
    fn default_settings_do_not_trust_forwarded_but_do_trust_edge_headers() {
        let settings = TrustedProxySettings::default();

        // A bare client-supplied chain buys nothing without a known hop count.
        let h = headers(&[("X-Forwarded-For", "9.9.9.9")]);
        assert_eq!(
            client_ip(&h, &settings),
            UNKNOWN_CLIENT,
            "self-host default must not trust a client-supplied header"
        );

        // But an unconfigured deployment behind Cloudflare/nginx must still get
        // per-client buckets rather than collapsing everyone into "unknown".
        let h = headers(&[
            ("CF-Connecting-IP", "203.0.113.7"),
            ("X-Forwarded-For", "9.9.9.9"),
        ]);
        assert_eq!(client_ip(&h, &settings), "203.0.113.7");

        let h = headers(&[("X-Real-IP", "198.51.100.4")]);
        assert_eq!(client_ip(&h, &settings), "198.51.100.4");
    }

    #[test]
    fn falls_back_to_stable_unknown_not_a_random_value() {
        let settings = TrustedProxySettings::default();
        let a = client_ip(&HeaderMap::new(), &settings);
        let b = client_ip(&HeaderMap::new(), &settings);
        assert_eq!(a, b, "fallback must be stable or it defeats rate limiting");
        assert_eq!(a, UNKNOWN_CLIENT);
    }

    #[test]
    fn rejects_oversized_and_empty_entries() {
        let long = "x".repeat(MAX_IP_LEN + 1);
        let h = headers(&[("CF-Connecting-IP", long.as_str())]);
        assert_eq!(client_ip(&h, &cf_settings()), UNKNOWN_CLIENT);

        let h = headers(&[("CF-Connecting-IP", "   ")]);
        assert_eq!(client_ip(&h, &cf_settings()), UNKNOWN_CLIENT);
    }

    #[test]
    fn hops_exceeding_chain_length_yields_unknown() {
        // Fewer entries than configured hops means the chain was not written by
        // the proxies we expect — trust none of it rather than guess.
        let settings = TrustedProxySettings {
            client_ip_header: None,
            forwarded_hops: 3,
        };
        let h = headers(&[("X-Forwarded-For", "1.1.1.1, 2.2.2.2")]);
        assert_eq!(client_ip(&h, &settings), UNKNOWN_CLIENT);
    }
}
