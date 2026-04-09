# PulsePlex Task Runner

# Run all local gates
all: fmt clippy test

# Check the project for compilation errors
check:
    cargo check

# Format all files in the workspace
fmt:
    cargo fmt --all

# Run clippy with strict warnings
clippy:
    cargo clippy --all-targets --all-features -- -D warnings

# Run all tests in the workspace
test:
    cargo test --all
