# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Added
- Initial release preparation

## [1.0.0] - 2024-12-09

### Added

#### Core Features
- Job queue management with priority support
- Multi-tenant architecture with PostgreSQL Row-Level Security
- API key authentication with bcrypt hashing
- JWT authentication with refresh tokens
- Real-time updates via WebSocket and Server-Sent Events
- Cron-based job scheduling with timezone support
- Workflow support with job dependencies
- Dead letter queue for failed jobs
- Job retry with exponential backoff

#### API
- RESTful API with OpenAPI 3.0 specification
- gRPC interface for worker communication
- Bulk job creation endpoint
- Queue pause/resume functionality
- Worker registration and heartbeat

#### Integrations
- GitHub webhook receiver with signature verification
- Stripe webhook receiver with signature verification
- Custom webhook support with HMAC verification
- Completion webhooks for job status notifications

#### Observability
- Prometheus metrics endpoint
- Structured JSON logging
- Distributed tracing support
- Health check endpoints (liveness/readiness)

#### Infrastructure
- Docker and Docker Compose support
- Kubernetes manifests with Kustomize
- Horizontal Pod Autoscaler configuration
- PgBouncer connection pooling support

#### Security
- Row-Level Security for tenant isolation
- Rate limiting per API key
- Input validation on all endpoints
- CORS configuration
- Security headers middleware

### Documentation
- Comprehensive README
- Quick start guide
- API usage guide
- Architecture documentation
- Deployment guide
- OpenAPI specification

---

## Version History Format

### Types of Changes

- **Added**: New features
- **Changed**: Changes in existing functionality
- **Deprecated**: Soon-to-be removed features
- **Removed**: Removed features
- **Fixed**: Bug fixes
- **Security**: Vulnerability fixes

