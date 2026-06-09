.PHONY: help build test e2e full-test lint fmt check clean run docker-build docker-up docker-down docker-logs

# Default target
help:
	@echo "Available targets:"
	@echo "  build        - Build release binary"
	@echo "  test         - Run all tests (unit + integration + E2E)"
	@echo "  e2e          - Run only E2E integration tests"
	@echo "  full-test    - Alias for test (runs everything)"
	@echo "  lint         - Run clippy linter"
	@echo "  fmt          - Format code with rustfmt"
	@echo "  check        - Run cargo check"
	@echo "  clean        - Clean build artifacts"
	@echo "  run          - Run the service locally"
	@echo "  docker-build - Build Docker image"
	@echo "  docker-up    - Start service with docker-compose"
	@echo "  docker-down  - Stop service with docker-compose"
	@echo "  docker-logs  - View service logs"

# Build release binary
build:
	cargo build --release

# Run all tests (unit + integration + E2E)
test:
	cargo test --all-features

# Run only E2E integration test binaries (requires no live services)
e2e:
	cargo test --test e2e_http --test e2e_ntp_udp --test e2e_websocket --test e2e_metrics

# Full test suite — identical to `test`; kept for explicitness
full-test: test

# Run clippy linter
lint:
	cargo clippy --all-targets --all-features -- -D warnings

# Format code
fmt:
	cargo fmt --all

# Check formatting
fmt-check:
	cargo fmt --all -- --check

# Run cargo check
check:
	cargo check --all-targets

# Clean build artifacts
clean:
	cargo clean
	rm -rf target/

# Run the service locally
run:
	cargo run

# Build Docker image
docker-build:
	docker compose build

# Start service with docker-compose
docker-up:
	docker compose up -d

# Stop service with docker-compose
docker-down:
	docker compose down

# View service logs
docker-logs:
	docker compose logs -f

# Combined check (lint + test + format check)
ci: fmt-check lint test
	@echo "✓ All CI checks passed"

# Quick development check
dev-check: fmt check test
	@echo "✓ Development checks passed"
