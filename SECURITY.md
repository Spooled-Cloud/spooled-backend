# Security Policy

## Reporting a Vulnerability

We take security seriously. If you discover a security vulnerability, please report it responsibly.

### How to Report

**Please do NOT report security vulnerabilities through public GitHub issues.**

Instead, please report them via email to:

📧 **security@spooled.cloud**

Or use GitHub's private vulnerability reporting feature if available.

### What to Include

Please include the following information:

1. **Description** of the vulnerability
2. **Steps to reproduce** the issue
3. **Potential impact** of the vulnerability
4. **Suggested fix** (if you have one)
5. **Your contact information** for follow-up

### What to Expect

- **Acknowledgment**: Within 48 hours of your report
- **Initial Assessment**: Within 7 days
- **Status Updates**: Every 7 days until resolution
- **Resolution Timeline**: Typically within 90 days

### Disclosure Policy

- We will work with you to understand and resolve the issue
- We will credit you in the security advisory (unless you prefer to remain anonymous)
- We ask that you do not publicly disclose the issue until we've had a chance to address it

## Security Best Practices

When deploying Spooled Backend, please follow these security guidelines:

### Configuration

- **JWT_SECRET**: Use a strong, random secret (minimum 32 characters)
  ```bash
  openssl rand -base64 32
  ```
- **Database Passwords**: Use strong, unique passwords
- **RUST_ENV**: Set to `production` in production environments

### Unresolved Operator Actions

The following credential-remediation work remains open. Do not place current or replacement secret values in this repository, documentation, tickets, logs, or chat transcripts.

- **Rotate `ADMIN_API_KEY`.** Treat any credential that appeared in a chat transcript or shared log as exposed. Generate a new high-entropy value in the deployment secret manager, update the production service, restart or roll the backend, verify old-key rejection and new-key access, then revoke the old value.
- **Replace the tracked gRPC origin private key.** Treat the historical `certs/grpc-key.pem` and its certificate as compromised because the private key was tracked and copied into old container images. Production Compose now generates a replacement keypair in a private Docker volume and mounts it read-only into the backend. Other platforms must generate a replacement outside Git and provide it through their secret manager. Removing the file from the latest tree does not erase Git or image history.

These are current actions, not instructions to expose either credential. Coordinate rotation to avoid locking out the admin portal or interrupting hosted gRPC.

### Network Security

- Run behind a reverse proxy with TLS for REST API (:8080)
- Use TLS termination for gRPC (:50051) via envoy, nginx, or cloud load balancers
- Use network policies to restrict database access
- Don't expose metrics endpoint publicly
- Configure appropriate CORS settings

### Database Security

- Use SSL for database connections in production
- Apply principle of least privilege for database users
- Enable PostgreSQL audit logging

### Redis Security

- Use Redis AUTH in production
- Don't expose Redis to the internet
- Consider using TLS for Redis connections

### API Security (REST & gRPC)

- Rotate API keys regularly
- Use short-lived JWT tokens
- Implement proper rate limiting
- Validate and sanitize all inputs
- For gRPC API-key authentication, send the API key credential as the value of the `x-api-key` metadata field; `x-api-key` is the metadata name, not a separate credential
- Alternatively, send a JWT credential as `authorization: Bearer <jwt-token>` metadata
- gRPC health and reflection services are public by default (disable in production if needed)

### Container Security

- Run as non-root user (already configured in Dockerfile)
- Use read-only filesystem where possible
- Scan images for vulnerabilities
- Keep base images updated

## Security Features

Spooled Backend includes several security features:

- **Multi-tenant isolation** via PostgreSQL Row-Level Security
- **Bcrypt password hashing** for API keys (used by both REST and gRPC)
- **JWT authentication** with configurable expiration
- **Rate limiting** per API key
- **HMAC webhook verification** for incoming webhooks
- **Input validation** on all endpoints (REST and gRPC)
- **Constant-time comparison** for sensitive data
- **Security headers** via middleware
- **Queue-scoped authorization** across REST, realtime, and gRPC operations
- **Stream revalidation** so key revocation, expiry, organization deletion, and scope narrowing take effect on active realtime/gRPC streams
- **gRPC interceptors** for authentication and authorization
- **gRPC health service** for load balancer health checks
- **SSRF protection** for outgoing webhook URLs

### SSRF Protection

Outgoing webhook URLs are validated in production to prevent Server-Side Request Forgery attacks:

- ❌ Private IP ranges (RFC 1918: 10.x.x.x, 172.16-31.x.x, 192.168.x.x)
- ❌ Loopback addresses (127.x.x.x, localhost, ::1)
- ❌ Link-local addresses (169.254.x.x, fe80::)
- ❌ Cloud metadata endpoints (169.254.169.254, metadata.google.internal)
- ❌ Internal hostnames (.local, .internal, .corp, .lan)
- ❌ HTTP URLs (HTTPS required in production)
- ❌ CGNAT ranges (100.64.0.0/10)

DNS resolution is also validated to prevent DNS rebinding attacks.

## Vulnerability History

Security fixes are documented in [`CHANGELOG.md`](CHANGELOG.md). Deploy the latest
release and review GitHub security advisories before operating an internet-facing
instance.

---

Thank you for helping keep Spooled Backend and its users safe!
