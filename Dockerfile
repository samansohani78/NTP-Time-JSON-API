# Multi-stage Dockerfile for production-ready NTP Time JSON API

# Build stage - using latest stable Rust 1.92
FROM rust:1.92-bookworm AS builder

# Install build dependencies
RUN apt-get update && apt-get install -y \
    pkg-config \
    libssl-dev \
    && rm -rf /var/lib/apt/lists/*

# Create app directory
WORKDIR /app

# Copy source code and manifests
COPY Cargo.toml Cargo.lock ./
COPY src ./src
COPY tests ./tests

# Build the application
RUN cargo build --release --bin ntp-time-json-api

# Runtime stage - using distroless (minimal, stateless, secure)
# gcr.io/distroless/cc-debian12 includes glibc and OpenSSL needed for Rust
FROM gcr.io/distroless/cc-debian13:nonroot

# Copy binary from builder (distroless uses / as workdir)
COPY --from=builder /app/target/release/ntp-time-json-api /ntp-time-json-api

# Expose HTTP port
EXPOSE 8080

# Run as non-root user (distroless nonroot = UID 65532, no shell available)
ENTRYPOINT ["/ntp-time-json-api"]
