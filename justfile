# PulsePlex Task Runner

# Run all local gates
all: fmt clippy test

# Check the project for compilation errors
check:
    cargo check --workspace

# Format all files in the workspace
fmt:
    cargo fmt --all

# Run clippy with strict warnings
clippy:
    cargo clippy --all-targets --all-features -- -D warnings

# Run all tests in the workspace
test:
    cargo test --all

# --- Local Development Workflows ---

# Run the daemon locally
run:
    cargo run -- run

# Run the daemon locally without the TUI (useful for piping logs)
run-headless:
    cargo run -- run --no-tui

# Watch for Rust code changes and restart the daemon automatically

# Requires: cargo install cargo-watch
dev:
    cargo watch -q -c -x 'run -- run'

# Watch and run tests on code changes
test-watch:
    cargo watch -q -c -x 'test --all'

# --- Cross-Platform & Deployment ---

# Build the release binary for local macOS testing
build-mac:
    cargo build --release

# Build the release binary for Raspberry Pi (Linux ARM64)
build-pi:
    cross build --release --target aarch64-unknown-linux-gnu

# Build and deploy directly to a remote server/device

# Usage: just deploy-pi user@192.168.1.x
deploy-pi TARGET: build-pi
    @echo "Deploying to {{ TARGET }}..."
    scp target/aarch64-unknown-linux-gnu/release/pulseplex {{ TARGET }}:/tmp/pulseplex
    ssh {{ TARGET }} "sudo mv /tmp/pulseplex /usr/local/bin/pulseplex && sudo chmod +x /usr/local/bin/pulseplex"
    @echo "Deployment complete! Ready to run 'pulseplex' on {{ TARGET }}."
