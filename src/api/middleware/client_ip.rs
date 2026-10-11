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
//!
//! # Why the TCP peer decides whether edge headers count
//!
//! `CF-Connecting-IP` and `X-Real-IP` are only as trustworthy as whoever sent
//! them. A proxy we run rewrites them on every request; a client talking to the
//! backend directly (or through a proxy that passes unknown headers on) writes
//! whatever it likes and picks its own rate-limit bucket. Without an explicit
//! `TRUSTED_CLIENT_IP_HEADER` they are therefore believed only from a peer on a
//! loopback, private, link-local or unique-local address — where a co-located
//! proxy sits (production's cloudflared reaches the backend over the docker
//! network) — and any other peer is keyed on its own address.
//!
//! # IPv6 buckets
//!
//! A single IPv6 subscriber is routinely handed a whole /64, so keying on the
//! full address gives one client 2^64 fresh buckets. Rate-limit ids collapse an
//! IPv6 address to its /64 (see [`rate_limit_id`]).

use std::net::{IpAddr, Ipv6Addr, SocketAddr};

use axum::extract::{ConnectInfo, FromRequestParts};
use axum::http::{request::Parts, Extensions, HeaderMap};

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

/// Edge headers consulted when no explicit `TRUSTED_CLIENT_IP_HEADER` is set,
/// and only when the TCP peer is internal (see [`is_internal_peer`]).
///
/// A proxy of ours writes them (and rewrites them on every request). Consulting
/// them without configuration matters because the alternative — keying every
/// request on the proxy's address — collapses every caller into ONE rate-limit
/// bucket. Production fronts the API with a Cloudflare Tunnel whose cloudflared
/// container sets `CF-Connecting-IP` and connects from the docker network.
const DEFAULT_EDGE_HEADERS: &[&str] = &["CF-Connecting-IP", "X-Real-IP"];

/// The TCP peer of a request.
///
/// Present when the server runs with
/// `into_make_service_with_connect_info::<SocketAddr>()` (main.rs); absent for
/// a router driven directly, as the integration tests do.
pub fn peer_addr(extensions: &Extensions) -> Option<IpAddr> {
    extensions
        .get::<ConnectInfo<SocketAddr>>()
        .map(|ConnectInfo(addr)| addr.ip())
}

/// Handler extractor for [`peer_addr`]; never rejects.
#[derive(Debug, Clone, Copy, Default)]
pub struct PeerAddr(pub Option<IpAddr>);

impl<S: Send + Sync> FromRequestParts<S> for PeerAddr {
    type Rejection = std::convert::Infallible;

    async fn from_request_parts(parts: &mut Parts, _state: &S) -> Result<Self, Self::Rejection> {
        Ok(Self(peer_addr(&parts.extensions)))
    }
}

/// Whether a peer can only be a proxy of our own deployment: loopback,
/// RFC 1918 private, link-local, or IPv6 unique-local (IPv4-mapped IPv6 is
/// judged as the IPv4 address it carries).
pub fn is_internal_peer(peer: IpAddr) -> bool {
    match peer.to_canonical() {
        IpAddr::V4(v4) => v4.is_loopback() || v4.is_private() || v4.is_link_local(),
        IpAddr::V6(v6) => v6.is_loopback() || v6.is_unique_local() || v6.is_unicast_link_local(),
    }
}

/// Client id for abuse controls, given the request's TCP peer.
///
/// Order of trust:
/// 1. The configured edge header (`TRUSTED_CLIENT_IP_HEADER`), from any peer:
///    the operator has said a proxy of theirs always sets it.
/// 2. Failing that, the well-known edge headers in [`DEFAULT_EDGE_HEADERS`] —
///    only when the peer is internal ([`is_internal_peer`]) or unknown (`None`,
///    a caller that cannot see the connection yet).
/// 3. `X-Forwarded-For`, counted from the RIGHT past `TRUSTED_PROXY_HOPS`
///    entries our own infrastructure appended. With the default of `0` hops this
///    branch never runs — with no known proxy count there is no entry we can
///    vouch for.
/// 4. The peer address itself.
/// 5. [`UNKNOWN_CLIENT`].
///
/// Returns a bare id (an IPv6 address as its /64, see [`rate_limit_id`]);
/// callers namespace it into their own key.
pub fn client_ip_from_peer(
    headers: &HeaderMap,
    peer: Option<IpAddr>,
    settings: &TrustedProxySettings,
) -> String {
    resolve_client_addr(headers, peer, settings)
        .map(|addr| rate_limit_id(&addr))
        .unwrap_or_else(|| UNKNOWN_CLIENT.to_string())
}

/// The client address as reported, before it is reduced to a rate-limit id
/// (e.g. for Turnstile's `remoteip`). Same order of trust as
/// [`client_ip_from_peer`]; `None` when nothing identifies the caller.
pub fn resolve_client_addr(
    headers: &HeaderMap,
    peer: Option<IpAddr>,
    settings: &TrustedProxySettings,
) -> Option<String> {
    if let Some(header_name) = settings.client_ip_header.as_deref() {
        if let Some(ip) = header_value(headers, header_name) {
            return Some(ip);
        }
    } else if peer.is_none_or(is_internal_peer) {
        for name in DEFAULT_EDGE_HEADERS {
            if let Some(ip) = header_value(headers, name) {
                return Some(ip);
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
                    return Some(ip);
                }
            }
        }
    }

    peer.map(|peer| peer.to_canonical().to_string())
}

/// Reduce a client address to its rate-limit id: an IPv6 address becomes its
/// /64 (`2001:db8:1:2::/64`), IPv4 and IPv4-mapped IPv6 the plain IPv4
/// address. Anything that does not parse is kept as it came (a configured edge
/// header the operator vouches for).
pub fn rate_limit_id(addr: &str) -> String {
    match addr.parse::<IpAddr>().map(|ip| ip.to_canonical()) {
        Ok(IpAddr::V4(v4)) => v4.to_string(),
        Ok(IpAddr::V6(v6)) => {
            let [a, b, c, d, ..] = v6.segments();
            format!("{}/64", Ipv6Addr::new(a, b, c, d, 0, 0, 0, 0))
        }
        Err(_) => addr.to_string(),
    }
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

    /// Peer unknown: edge headers are believed, as for a caller that cannot see
    /// the connection.
    fn client_ip(headers: &HeaderMap, settings: &TrustedProxySettings) -> String {
        client_ip_from_peer(headers, None, settings)
    }

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

    fn peer(addr: &str) -> Option<IpAddr> {
        Some(addr.parse().unwrap())
    }

    // BEAUTH-10: without TRUSTED_CLIENT_IP_HEADER, a client talking to the
    // backend directly could name its own bucket with CF-Connecting-IP.
    #[test]
    fn edge_headers_from_a_public_peer_are_ignored() {
        let settings = TrustedProxySettings::default();
        let h = headers(&[("CF-Connecting-IP", "1.2.3.4"), ("X-Real-IP", "5.6.7.8")]);
        assert_eq!(
            client_ip_from_peer(&h, peer("203.0.113.9"), &settings),
            "203.0.113.9",
            "a public peer is keyed on its own address"
        );
        assert_eq!(
            resolve_client_addr(&h, peer("2001:db8:1:2::9"), &settings).as_deref(),
            Some("2001:db8:1:2::9")
        );
    }

    #[test]
    fn edge_headers_from_an_internal_peer_are_believed() {
        let settings = TrustedProxySettings::default();
        let h = headers(&[("CF-Connecting-IP", "1.2.3.4")]);
        // Docker bridge (production's cloudflared), loopback, other RFC 1918
        // ranges, link-local, IPv6 ULA / link-local / loopback, IPv4-mapped.
        for internal in [
            "172.18.0.5",
            "127.0.0.1",
            "10.1.2.3",
            "192.168.1.20",
            "169.254.10.1",
            "fd00::5",
            "fe80::1",
            "::1",
            "::ffff:172.18.0.5",
        ] {
            assert_eq!(
                client_ip_from_peer(&h, peer(internal), &settings),
                "1.2.3.4",
                "peer {internal}"
            );
        }
        let h = headers(&[("X-Real-IP", "198.51.100.4")]);
        assert_eq!(
            client_ip_from_peer(&h, peer("172.18.0.5"), &settings),
            "198.51.100.4"
        );
        // No header from the proxy: the proxy's own address, not one shared
        // "unknown" bucket for every deployment.
        assert_eq!(
            client_ip_from_peer(&HeaderMap::new(), peer("172.18.0.5"), &settings),
            "172.18.0.5"
        );
        for public in ["203.0.113.9", "100.64.0.1", "2001:db8::1", "::ffff:8.8.8.8"] {
            assert!(!is_internal_peer(public.parse().unwrap()), "{public}");
        }
    }

    #[test]
    fn a_configured_header_is_believed_from_any_peer() {
        let h = headers(&[("CF-Connecting-IP", "1.2.3.4")]);
        assert_eq!(
            client_ip_from_peer(&h, peer("203.0.113.9"), &cf_settings()),
            "1.2.3.4"
        );
        // The configured header names the only edge header: X-Real-IP from an
        // internal peer is not consulted in its place.
        let h = headers(&[("X-Real-IP", "5.6.7.8")]);
        assert_eq!(
            client_ip_from_peer(&h, peer("172.18.0.5"), &cf_settings()),
            "172.18.0.5"
        );
    }

    #[test]
    fn peer_addr_reads_connect_info() {
        let mut extensions = Extensions::new();
        assert_eq!(peer_addr(&extensions), None);
        extensions.insert(ConnectInfo(SocketAddr::from(([172, 18, 0, 5], 41000))));
        assert_eq!(peer_addr(&extensions), peer("172.18.0.5"));
    }

    // BEBILL-24: one IPv6 subscriber owns a /64 and could rotate addresses
    // inside it for a fresh bucket per request.
    #[test]
    fn ipv6_clients_share_their_slash_64() {
        assert_eq!(rate_limit_id("2001:db8:1:2:aaaa::1"), "2001:db8:1:2::/64");
        assert_eq!(
            rate_limit_id("2001:db8:1:2:ffff:ffff:ffff:ffff"),
            "2001:db8:1:2::/64"
        );
        assert_ne!(rate_limit_id("2001:db8:1:3::1"), "2001:db8:1:2::/64");
        assert_eq!(rate_limit_id("::ffff:203.0.113.7"), "203.0.113.7");
        assert_eq!(rate_limit_id("203.0.113.7"), "203.0.113.7");
        assert_eq!(rate_limit_id("not-an-ip"), "not-an-ip");

        let settings = TrustedProxySettings::default();
        let a = headers(&[("CF-Connecting-IP", "2001:db8:1:2::a")]);
        let b = headers(&[("CF-Connecting-IP", "2001:db8:1:2::b")]);
        assert_eq!(client_ip(&a, &settings), client_ip(&b, &settings));
        assert_eq!(
            client_ip_from_peer(&HeaderMap::new(), peer("2001:db8:5:6::7"), &settings),
            "2001:db8:5:6::/64"
        );
    }
}
