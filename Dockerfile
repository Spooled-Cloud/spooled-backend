# =============================================================================
# Spooled Backend Dockerfile
# Multi-stage build for optimized production image
# =============================================================================

# -----------------------------------------------------------------------------
# Stage 1: Build
# -----------------------------------------------------------------------------
FROM rust:1.87-bookworm AS builder

# Install build dependencies
RUN apt-get update && apt-get install -y \
    pkg-config \
    libssl-dev \
    && rm -rf /var/lib/apt/lists/*

# Create app directory
WORKDIR /app

# Copy manifests first (for caching)
COPY Cargo.toml Cargo.lock ./

# Create dummy main.rs for dependency caching
RUN mkdir src && echo "fn main() {}" > src/main.rs

# Build dependencies only (cached layer)
RUN cargo build --release && rm -rf src

# Copy source code
COPY src ./src
COPY migrations ./migrations

# Touch main.rs to invalidate cache for source changes
RUN touch src/main.rs

# Build the application
RUN cargo build --release

# -----------------------------------------------------------------------------
# Stage 2: Runtime
# -----------------------------------------------------------------------------
FROM debian:bookworm-slim AS runtime

# Install runtime dependencies
RUN apt-get update && apt-get install -y \
    ca-certificates \
    libssl3 \
    curl \
    && rm -rf /var/lib/apt/lists/*

# Create non-root user
RUN groupadd --gid 1000 spooled \
    && useradd --uid 1000 --gid 1000 --create-home spooled

# Create app directory
WORKDIR /app

# Copy binary from builder
COPY --from=builder /app/target/release/spooled-backend /app/spooled-backend

# Copy migrations for SQLx
COPY --from=builder /app/migrations /app/migrations

# Set ownership
RUN chown -R spooled:spooled /app

# Switch to non-root user
USER spooled

# Set environment variables
ENV RUST_LOG=info
ENV RUST_BACKTRACE=1
ENV ENVIRONMENT=production

# Expose ports
EXPOSE 8080 9090 50051

# Health check
HEALTHCHECK --interval=30s --timeout=10s --start-period=10s --retries=3 \
    CMD curl -f http://localhost:8080/health || exit 1

# Run the application
CMD ["/app/spooled-backend"]

