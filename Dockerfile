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

# Runtime stage - using debian bookworm (stable)
FROM debian:bookworm-slim

# Install runtime dependencies
RUN apt-get update && apt-get install -y \
    ca-certificates \
    libssl3 \
    wget \
    && rm -rf /var/lib/apt/lists/*

# Create non-root user
RUN useradd -r -u 1000 -s /bin/false ntpapi

# Create app directory
WORKDIR /app

# Copy binary from builder
COPY --from=builder /app/target/release/ntp-time-json-api /app/ntp-time-json-api

# Change ownership
RUN chown -R ntpapi:ntpapi /app

# Switch to non-root user
USER ntpapi

# Expose HTTP port
EXPOSE 8080

# Set default environment variables
ENV LOG_LEVEL=info \
    LOG_FORMAT=json \
    ADDR=0.0.0.0:8080 \
    RUST_BACKTRACE=1

# Run the binary
CMD ["/app/ntp-time-json-api"]
