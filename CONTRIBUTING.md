# Contributing to Spooled Backend

Thank you for your interest in contributing to Spooled Backend! This document provides guidelines and information for contributors.

## Code of Conduct

By participating in this project, you agree to abide by our [Code of Conduct](CODE_OF_CONDUCT.md).

## How to Contribute

### Reporting Bugs

1. Check if the bug has already been reported in [Issues](https://github.com/spooled-cloud/spooled-backend/issues)
2. If not, create a new issue using the bug report template
3. Include as much detail as possible: version, environment, steps to reproduce

### Suggesting Features

1. Check existing issues and discussions for similar suggestions
2. Create a new issue using the feature request template
3. Explain the use case and why it would benefit others

### Submitting Code

#### Prerequisites

- Rust 1.85 or later
- Docker and Docker Compose
- PostgreSQL 16+ (via Docker)
- Redis 7+ (via Docker)

#### Setup

```bash
# Clone the repository
git clone https://github.com/spooled-cloud/spooled-backend.git
cd spooled-backend

# Start dependencies
docker-compose up -d postgres redis pgbouncer

# Copy environment file
cp .env.example .env

# Run the server
cargo run

# Run tests
cargo test
```

#### Development Workflow

1. **Fork** the repository
2. **Create a branch** for your changes:
   ```bash
   git checkout -b feature/your-feature-name
   # or
   git checkout -b fix/your-bug-fix
   ```
3. **Make your changes** following our coding standards
4. **Write tests** for new functionality
5. **Run the test suite**:
   ```bash
   cargo test
   cargo clippy -- -D warnings
   cargo fmt --check
   ```
6. **Commit** with a clear message:
   ```bash
   git commit -m "feat: add new feature X"
   # or
   git commit -m "fix: resolve issue with Y"
   ```
7. **Push** to your fork and create a Pull Request

### Commit Message Format

We follow [Conventional Commits](https://www.conventionalcommits.org/):

```
<type>(<scope>): <description>

[optional body]

[optional footer]
```

**Types:**
- `feat`: New feature
- `fix`: Bug fix
- `docs`: Documentation changes
- `style`: Code style changes (formatting, etc.)
- `refactor`: Code changes that neither fix bugs nor add features
- `perf`: Performance improvements
- `test`: Adding or updating tests
- `chore`: Maintenance tasks
- `ci`: CI/CD changes

**Examples:**
```
feat(api): add bulk job creation endpoint
fix(queue): resolve race condition in job dequeue
docs: update API usage guide
test(auth): add tests for JWT refresh flow
```

## Coding Standards

### Rust Style

- Follow the [Rust API Guidelines](https://rust-lang.github.io/api-guidelines/)
- Use `cargo fmt` for formatting
- Use `cargo clippy` for linting
- Write documentation for public APIs

### Code Organization

```
src/
├── api/          # HTTP handlers and routing
├── cache/        # Redis caching
├── config/       # Configuration
├── db/           # Database operations
├── error/        # Error types
├── grpc/         # gRPC service
├── models/       # Data models
├── observability/# Metrics and tracing
├── queue/        # Queue management
├── scheduler/    # Cron scheduling
├── webhook/      # Webhook delivery
└── worker/       # Worker implementation
```

### Testing

- Write unit tests for new functions
- Write integration tests for API endpoints
- Test edge cases and error conditions
- Aim for meaningful coverage, not just high numbers

```bash
# Run all tests
cargo test

# Run specific test
cargo test test_name

# Run with output
cargo test -- --nocapture

# Run integration tests only
cargo test --test '*'
```

### Documentation

- Add doc comments to public APIs
- Update README if adding new features
- Update OpenAPI spec for API changes
- Include examples in documentation

## Pull Request Process

1. Ensure all tests pass
2. Update documentation as needed
3. Add entry to CHANGELOG.md (if applicable)
4. Request review from maintainers
5. Address review feedback
6. Squash commits if requested

### PR Checklist

- [ ] Tests added/updated
- [ ] Documentation updated
- [ ] CHANGELOG.md updated (for user-facing changes)
- [ ] No new warnings from `cargo clippy`
- [ ] Code formatted with `cargo fmt`

## Release Process

Releases are automated via GitHub Actions when a tag is pushed:

```bash
git tag v1.0.0
git push origin v1.0.0
```

This triggers:
1. Full test suite
2. Docker image build and push to GHCR
3. GitHub Release creation

## Getting Help

- Open a [Discussion](https://github.com/spooled-cloud/spooled-backend/discussions)
- Check existing issues and PRs
- Read the [documentation](docs/)

## License

By contributing, you agree that your contributions will be licensed under the Apache License 2.0.

