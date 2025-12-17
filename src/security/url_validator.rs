//! URL Validation and SSRF Protection
//!
//! Provides comprehensive validation of URLs to prevent Server-Side Request Forgery (SSRF) attacks.
//!
//! # Security Features
//! - Blocks private IP ranges (RFC 1918, link-local, loopback)
//! - Blocks cloud metadata endpoints (AWS, GCP, Azure)
//! - Blocks internal hostnames (postgres, redis, etc.)
//! - Enforces HTTPS in production
//! - Validates URL format and scheme
//! - DNS rebinding protection (resolves hostname and validates IP)

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, ToSocketAddrs};
use thiserror::Error;
use tracing::warn;

/// Errors from URL validation
#[derive(Debug, Error)]
pub enum UrlValidationError {
    #[error("Invalid URL format: {0}")]
    InvalidFormat(String),

    #[error("URL scheme not allowed: {0}")]
    InvalidScheme(String),

    #[error("HTTP not allowed in production")]
    HttpNotAllowed,

    #[error("Hostname not allowed: blocked for security")]
    BlockedHostname,

    #[error("IP address not allowed: private/internal addresses blocked")]
    PrivateIpAddress,

    #[error("DNS resolution failed or resolved to blocked address")]
    DnsResolutionBlocked,

    #[error("URL host is missing")]
    MissingHost,
}

/// Options for URL validation
#[derive(Debug, Clone)]
pub struct UrlValidationOptions {
    /// Whether we're in production mode (affects HTTPS requirement)
    pub is_production: bool,
    /// Allow localhost in development mode
    pub allow_localhost_in_dev: bool,
    /// Perform DNS resolution to check for rebinding attacks
    pub check_dns_resolution: bool,
}

impl Default for UrlValidationOptions {
    fn default() -> Self {
        let is_production = std::env::var("RUST_ENV")
            .map(|v| v == "production")
            .unwrap_or(false);

        Self {
            is_production,
            allow_localhost_in_dev: true,
            check_dns_resolution: true,
        }
    }
}

/// Blocked hostnames that should never be allowed as webhook targets
const BLOCKED_HOSTNAMES: &[&str] = &[
    // Localhost variants
    "localhost",
    "localhost.localdomain",
    // Internal hostnames commonly used in Docker/Kubernetes
    "postgres",
    "postgresql",
    "mysql",
    "mariadb",
    "redis",
    "memcached",
    "elasticsearch",
    "elastic",
    "mongo",
    "mongodb",
    "rabbitmq",
    "kafka",
    "zookeeper",
    "consul",
    "vault",
    "etcd",
    "grafana",
    "prometheus",
    "jaeger",
    "zipkin",
    "nginx",
    "traefik",
    "envoy",
    "istio-proxy",
    "linkerd",
    // Kubernetes internal
    "kubernetes",
    "kubernetes.default",
    "kubernetes.default.svc",
    "kube-dns",
    // Cloud metadata endpoints
    "metadata",
    "metadata.google",
    "metadata.google.internal",
    "instance-data",
    "169.254.169.254", // AWS/GCP metadata IP
    "169.254.170.2",   // AWS ECS metadata
    // Azure metadata
    "168.63.129.16",
    // DigitalOcean metadata
    "169.254.169.123",
];

/// Blocked hostname suffixes
const BLOCKED_HOSTNAME_SUFFIXES: &[&str] = &[
    ".internal",
    ".local",
    ".localhost",
    ".localdomain",
    ".svc",
    ".svc.cluster.local",
    ".pod.cluster.local",
    ".cluster.local",
    ".compute.internal", // AWS internal
    ".ec2.internal",     // AWS EC2
    ".us-east-1.compute.internal",
    ".us-west-1.compute.internal",
    ".eu-west-1.compute.internal",
];

// Note: IP addresses are handled by validate_ip_address(), not by hostname blocklist.
// This ensures proper error types (PrivateIpAddress vs BlockedHostname).

/// Validate a URL for use as a webhook target
///
/// This function performs comprehensive validation to prevent SSRF attacks.
///
/// # Arguments
/// * `url` - The URL to validate
/// * `options` - Validation options
///
/// # Returns
/// * `Ok(())` if the URL is valid
/// * `Err(UrlValidationError)` if the URL is invalid or blocked
///
/// # Examples
/// ```ignore
/// use spooled_backend::security::{validate_webhook_url, UrlValidationOptions};
///
/// let opts = UrlValidationOptions::default();
///
/// // Valid HTTPS URL
/// assert!(validate_webhook_url("https://example.com/webhook", &opts).is_ok());
///
/// // Blocked - private IP
/// assert!(validate_webhook_url("https://192.168.1.1/webhook", &opts).is_err());
///
/// // Blocked - internal hostname
/// assert!(validate_webhook_url("https://postgres:5432/", &opts).is_err());
/// ```
pub fn validate_webhook_url(
    url: &str,
    options: &UrlValidationOptions,
) -> Result<(), UrlValidationError> {
    // Parse URL
    let parsed =
        url::Url::parse(url).map_err(|e| UrlValidationError::InvalidFormat(e.to_string()))?;

    // 1. Validate scheme
    validate_scheme(&parsed, options)?;

    // 2. Get and validate host
    let host = parsed.host_str().ok_or(UrlValidationError::MissingHost)?;

    // 3. Check if host is in blocked list
    validate_hostname(host, options)?;

    // 4. If host is an IP, validate it
    // IPv6 addresses from URL parsing come with brackets like "[::1]"
    // We need to strip them before parsing
    let host_for_ip_parse = if host.starts_with('[') && host.ends_with(']') {
        &host[1..host.len() - 1]
    } else {
        host
    };

    if let Ok(ip) = host_for_ip_parse.parse::<IpAddr>() {
        validate_ip_address(&ip, options)?;
    }

    // 5. DNS resolution check (optional but recommended)
    if options.check_dns_resolution {
        validate_dns_resolution(host, parsed.port_or_known_default().unwrap_or(443), options)?;
    }

    Ok(())
}

/// Validate URL scheme
fn validate_scheme(
    url: &url::Url,
    options: &UrlValidationOptions,
) -> Result<(), UrlValidationError> {
    match url.scheme() {
        "https" => Ok(()),
        "http" => {
            if options.is_production {
                return Err(UrlValidationError::HttpNotAllowed);
            }

            // In development, only allow HTTP for localhost
            if options.allow_localhost_in_dev {
                if let Some(host) = url.host_str() {
                    if host == "localhost" || host == "127.0.0.1" {
                        return Ok(());
                    }
                }
            }

            Err(UrlValidationError::HttpNotAllowed)
        }
        scheme => Err(UrlValidationError::InvalidScheme(scheme.to_string())),
    }
}

/// Validate hostname against blocklists
fn validate_hostname(host: &str, options: &UrlValidationOptions) -> Result<(), UrlValidationError> {
    let host_lower = host.to_lowercase();

    // In development, allow localhost if configured
    if !options.is_production
        && options.allow_localhost_in_dev
        && (host_lower == "localhost" || host_lower == "127.0.0.1")
    {
        return Ok(());
    }

    // Check exact matches
    if BLOCKED_HOSTNAMES
        .iter()
        .any(|&blocked| host_lower == blocked)
    {
        warn!(hostname = %host, "Blocked webhook hostname (exact match)");
        return Err(UrlValidationError::BlockedHostname);
    }

    // Check suffixes
    if BLOCKED_HOSTNAME_SUFFIXES
        .iter()
        .any(|&suffix| host_lower.ends_with(suffix))
    {
        warn!(hostname = %host, "Blocked webhook hostname (suffix match)");
        return Err(UrlValidationError::BlockedHostname);
    }

    // Note: IP addresses in hostname form are handled by validate_ip_address()
    // which is called separately after this function

    Ok(())
}

/// Validate an IP address
fn validate_ip_address(
    ip: &IpAddr,
    options: &UrlValidationOptions,
) -> Result<(), UrlValidationError> {
    // In development, allow localhost
    if !options.is_production && options.allow_localhost_in_dev && ip.is_loopback() {
        return Ok(());
    }

    // Check loopback
    if ip.is_loopback() {
        warn!(ip = %ip, "Blocked webhook IP (loopback)");
        return Err(UrlValidationError::PrivateIpAddress);
    }

    // Check unspecified (0.0.0.0 / ::)
    if ip.is_unspecified() {
        warn!(ip = %ip, "Blocked webhook IP (unspecified)");
        return Err(UrlValidationError::PrivateIpAddress);
    }

    match ip {
        IpAddr::V4(ipv4) => validate_ipv4(ipv4)?,
        IpAddr::V6(ipv6) => validate_ipv6(ipv6)?,
    }

    Ok(())
}

/// Validate IPv4 address
fn validate_ipv4(ip: &Ipv4Addr) -> Result<(), UrlValidationError> {
    let octets = ip.octets();

    // Private ranges (RFC 1918)
    // 10.0.0.0/8
    if octets[0] == 10 {
        warn!(ip = %ip, "Blocked webhook IP (private 10.x.x.x)");
        return Err(UrlValidationError::PrivateIpAddress);
    }

    // 172.16.0.0/12 (172.16.0.0 - 172.31.255.255)
    if octets[0] == 172 && (octets[1] >= 16 && octets[1] <= 31) {
        warn!(ip = %ip, "Blocked webhook IP (private 172.16-31.x.x)");
        return Err(UrlValidationError::PrivateIpAddress);
    }

    // 192.168.0.0/16
    if octets[0] == 192 && octets[1] == 168 {
        warn!(ip = %ip, "Blocked webhook IP (private 192.168.x.x)");
        return Err(UrlValidationError::PrivateIpAddress);
    }

    // Link-local (169.254.0.0/16) - includes metadata endpoints
    if octets[0] == 169 && octets[1] == 254 {
        warn!(ip = %ip, "Blocked webhook IP (link-local/metadata)");
        return Err(UrlValidationError::PrivateIpAddress);
    }

    // Loopback (127.0.0.0/8)
    if octets[0] == 127 {
        warn!(ip = %ip, "Blocked webhook IP (loopback)");
        return Err(UrlValidationError::PrivateIpAddress);
    }

    // Carrier-grade NAT (100.64.0.0/10)
    if octets[0] == 100 && (octets[1] >= 64 && octets[1] <= 127) {
        warn!(ip = %ip, "Blocked webhook IP (CGNAT)");
        return Err(UrlValidationError::PrivateIpAddress);
    }

    // Azure metadata IP
    if octets == [168, 63, 129, 16] {
        warn!(ip = %ip, "Blocked webhook IP (Azure metadata)");
        return Err(UrlValidationError::PrivateIpAddress);
    }

    // Documentation ranges (should not be routable)
    // 192.0.2.0/24 (TEST-NET-1)
    if octets[0] == 192 && octets[1] == 0 && octets[2] == 2 {
        warn!(ip = %ip, "Blocked webhook IP (documentation range)");
        return Err(UrlValidationError::PrivateIpAddress);
    }

    // 198.51.100.0/24 (TEST-NET-2)
    if octets[0] == 198 && octets[1] == 51 && octets[2] == 100 {
        warn!(ip = %ip, "Blocked webhook IP (documentation range)");
        return Err(UrlValidationError::PrivateIpAddress);
    }

    // 203.0.113.0/24 (TEST-NET-3)
    if octets[0] == 203 && octets[1] == 0 && octets[2] == 113 {
        warn!(ip = %ip, "Blocked webhook IP (documentation range)");
        return Err(UrlValidationError::PrivateIpAddress);
    }

    // Broadcast
    if octets == [255, 255, 255, 255] {
        warn!(ip = %ip, "Blocked webhook IP (broadcast)");
        return Err(UrlValidationError::PrivateIpAddress);
    }

    Ok(())
}

/// Validate IPv6 address
fn validate_ipv6(ip: &Ipv6Addr) -> Result<(), UrlValidationError> {
    let segments = ip.segments();

    // Loopback (::1)
    if ip.is_loopback() {
        warn!(ip = %ip, "Blocked webhook IP (loopback)");
        return Err(UrlValidationError::PrivateIpAddress);
    }

    // Unspecified (::)
    if ip.is_unspecified() {
        warn!(ip = %ip, "Blocked webhook IP (unspecified)");
        return Err(UrlValidationError::PrivateIpAddress);
    }

    // Link-local (fe80::/10)
    if (segments[0] & 0xffc0) == 0xfe80 {
        warn!(ip = %ip, "Blocked webhook IP (link-local)");
        return Err(UrlValidationError::PrivateIpAddress);
    }

    // Unique local (fc00::/7 - includes fd00::/8)
    if (segments[0] & 0xfe00) == 0xfc00 {
        warn!(ip = %ip, "Blocked webhook IP (unique local)");
        return Err(UrlValidationError::PrivateIpAddress);
    }

    // IPv4-mapped IPv6 (::ffff:0:0/96) - validate the embedded IPv4
    if segments[0..5] == [0, 0, 0, 0, 0] && segments[5] == 0xffff {
        let ipv4 = Ipv4Addr::new(
            (segments[6] >> 8) as u8,
            segments[6] as u8,
            (segments[7] >> 8) as u8,
            segments[7] as u8,
        );
        return validate_ipv4(&ipv4);
    }

    // IPv4-compatible IPv6 (deprecated but still check)
    if segments[0..6] == [0, 0, 0, 0, 0, 0] {
        let ipv4 = Ipv4Addr::new(
            (segments[6] >> 8) as u8,
            segments[6] as u8,
            (segments[7] >> 8) as u8,
            segments[7] as u8,
        );
        return validate_ipv4(&ipv4);
    }

    Ok(())
}

/// Validate DNS resolution
///
/// This prevents DNS rebinding attacks where a hostname initially resolves to a public IP
/// but later resolves to an internal IP.
fn validate_dns_resolution(
    host: &str,
    port: u16,
    options: &UrlValidationOptions,
) -> Result<(), UrlValidationError> {
    // If host is already an IP, skip DNS resolution (already validated)
    if host.parse::<IpAddr>().is_ok() {
        return Ok(());
    }

    // Resolve hostname
    let addr = format!("{}:{}", host, port);
    let resolved: Vec<_> = match addr.to_socket_addrs() {
        Ok(addrs) => addrs.collect(),
        Err(e) => {
            warn!(host = %host, error = %e, "DNS resolution failed for webhook URL");
            return Err(UrlValidationError::DnsResolutionBlocked);
        }
    };

    if resolved.is_empty() {
        warn!(host = %host, "DNS resolution returned no addresses for webhook URL");
        return Err(UrlValidationError::DnsResolutionBlocked);
    }

    // Validate all resolved IPs
    for socket_addr in &resolved {
        let ip = socket_addr.ip();
        if let Err(e) = validate_ip_address(&ip, options) {
            warn!(
                host = %host,
                resolved_ip = %ip,
                "DNS resolution returned blocked IP"
            );
            return Err(e);
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_options() -> UrlValidationOptions {
        UrlValidationOptions {
            is_production: true,
            allow_localhost_in_dev: false,
            check_dns_resolution: false, // Disable for tests
        }
    }

    fn dev_options() -> UrlValidationOptions {
        UrlValidationOptions {
            is_production: false,
            allow_localhost_in_dev: true,
            check_dns_resolution: false,
        }
    }

    #[test]
    fn test_valid_https_url() {
        let opts = test_options();
        assert!(validate_webhook_url("https://example.com/webhook", &opts).is_ok());
        assert!(validate_webhook_url("https://api.example.com/v1/hook", &opts).is_ok());
        assert!(validate_webhook_url("https://webhook.site/test", &opts).is_ok());
    }

    #[test]
    fn test_http_blocked_in_production() {
        let opts = test_options();
        assert!(matches!(
            validate_webhook_url("http://example.com/webhook", &opts),
            Err(UrlValidationError::HttpNotAllowed)
        ));
    }

    #[test]
    fn test_http_localhost_allowed_in_dev() {
        let opts = dev_options();
        assert!(validate_webhook_url("http://localhost:3000/webhook", &opts).is_ok());
        assert!(validate_webhook_url("http://127.0.0.1:3000/webhook", &opts).is_ok());
    }

    #[test]
    fn test_blocked_localhost_in_production() {
        let opts = test_options();
        assert!(matches!(
            validate_webhook_url("https://localhost/webhook", &opts),
            Err(UrlValidationError::BlockedHostname)
        ));
        assert!(matches!(
            validate_webhook_url("https://127.0.0.1/webhook", &opts),
            Err(UrlValidationError::PrivateIpAddress)
        ));
    }

    #[test]
    fn test_blocked_private_ips() {
        let opts = test_options();

        // RFC 1918 ranges
        assert!(validate_webhook_url("https://10.0.0.1/webhook", &opts).is_err());
        assert!(validate_webhook_url("https://10.255.255.255/webhook", &opts).is_err());
        assert!(validate_webhook_url("https://172.16.0.1/webhook", &opts).is_err());
        assert!(validate_webhook_url("https://172.31.255.255/webhook", &opts).is_err());
        assert!(validate_webhook_url("https://192.168.0.1/webhook", &opts).is_err());
        assert!(validate_webhook_url("https://192.168.255.255/webhook", &opts).is_err());
    }

    #[test]
    fn test_blocked_metadata_endpoints() {
        let opts = test_options();

        // AWS metadata
        assert!(validate_webhook_url("https://169.254.169.254/latest/meta-data/", &opts).is_err());

        // GCP metadata
        assert!(
            validate_webhook_url("https://metadata.google.internal/computeMetadata/", &opts)
                .is_err()
        );
    }

    #[test]
    fn test_blocked_internal_hostnames() {
        let opts = test_options();

        assert!(validate_webhook_url("https://postgres:5432/", &opts).is_err());
        assert!(validate_webhook_url("https://redis:6379/", &opts).is_err());
        assert!(validate_webhook_url("https://elasticsearch:9200/", &opts).is_err());
        assert!(validate_webhook_url("https://kubernetes.default.svc/", &opts).is_err());
    }

    #[test]
    fn test_blocked_internal_suffixes() {
        let opts = test_options();

        assert!(validate_webhook_url("https://my-service.internal/", &opts).is_err());
        assert!(validate_webhook_url("https://app.local/", &opts).is_err());
        assert!(validate_webhook_url("https://backend.svc.cluster.local/", &opts).is_err());
    }

    #[test]
    fn test_invalid_schemes() {
        let opts = test_options();

        assert!(matches!(
            validate_webhook_url("ftp://example.com/webhook", &opts),
            Err(UrlValidationError::InvalidScheme(_))
        ));
        assert!(matches!(
            validate_webhook_url("file:///etc/passwd", &opts),
            Err(UrlValidationError::InvalidScheme(_))
        ));
        // javascript: URLs parse as valid URLs with scheme "javascript"
        assert!(matches!(
            validate_webhook_url("javascript:alert(1)", &opts),
            Err(UrlValidationError::InvalidScheme(_))
        ));
    }

    #[test]
    fn test_ipv6_addresses() {
        let opts = test_options();

        // Loopback (::1)
        assert!(validate_webhook_url("https://[::1]/webhook", &opts).is_err());

        // Link-local (fe80::/10)
        assert!(validate_webhook_url("https://[fe80::1]/webhook", &opts).is_err());

        // Unique local (fc00::/7, fd00::/8)
        assert!(validate_webhook_url("https://[fd00::1]/webhook", &opts).is_err());
        assert!(validate_webhook_url("https://[fc00::1]/webhook", &opts).is_err());
    }

    #[test]
    fn test_invalid_urls() {
        let opts = test_options();

        assert!(matches!(
            validate_webhook_url("not-a-url", &opts),
            Err(UrlValidationError::InvalidFormat(_))
        ));
        assert!(matches!(
            validate_webhook_url("", &opts),
            Err(UrlValidationError::InvalidFormat(_))
        ));
    }

    #[test]
    fn test_172_range_boundary() {
        let opts = test_options();

        // 172.16-31 should be blocked
        assert!(validate_webhook_url("https://172.16.0.1/", &opts).is_err());
        assert!(validate_webhook_url("https://172.20.0.1/", &opts).is_err());
        assert!(validate_webhook_url("https://172.31.0.1/", &opts).is_err());

        // 172.15 and 172.32 should be allowed (if public)
        // Note: These might still be blocked by DNS resolution in real scenarios
    }

    #[test]
    fn test_cgnat_range() {
        let opts = test_options();

        // Carrier-grade NAT (100.64.0.0/10)
        assert!(validate_webhook_url("https://100.64.0.1/", &opts).is_err());
        assert!(validate_webhook_url("https://100.127.255.255/", &opts).is_err());
    }
}
