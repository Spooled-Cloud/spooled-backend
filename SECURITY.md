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
- gRPC uses the same API key authentication (`x-api-key` or `authorization` metadata)
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

No known vulnerabilities at this time.

---

Thank you for helping keep Spooled Backend and its users safe!
