.PHONY: help build test run docker-build docker-run clean fmt lint check

help:
	@echo "Available targets:"
	@echo "  build        - Build the project in release mode"
	@echo "  test         - Run all tests"
	@echo "  run          - Run the service locally"
	@echo "  docker-build - Build Docker image"
	@echo "  docker-run   - Run Docker container"
	@echo "  clean        - Clean build artifacts"
	@echo "  fmt          - Format code"
	@echo "  lint         - Run clippy linter"
	@echo "  check        - Run all checks (fmt, lint, test)"

build:
	cargo build --release

test:
	cargo test --all-features --verbose

run:
	cargo run

docker-build:
	docker build -t ntp-time-api:latest .

docker-run:
	docker run -p 8080:8080 \
		-e LOG_FORMAT=pretty \
		-e LOG_LEVEL=info \
		ntp-time-api:latest

clean:
	cargo clean

fmt:
	cargo fmt --all

lint:
	cargo clippy --all-targets --all-features -- -D warnings

check: fmt lint test
	@echo "All checks passed!"

.DEFAULT_GOAL := help
