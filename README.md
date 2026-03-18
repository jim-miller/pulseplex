# PulsePlex 🥁💡

**PulsePlex** is a high-performance, pluggable orchestration engine built in
Rust. It is designed to bridge real-time musical triggers (MIDI), real-world
lighting and effects (DMX512-A), and networked lighting systems like **Philips
Hue**.

As a systems-first project, PulsePlex prioritizes deterministic timing,
zero-cost abstractions, and a "Sync-to-Async" architecture to ensure that your
lighting stays perfectly in pocket with a musical performance.

## Architecture

PulsePlex is organized as a Cargo Workspace to maintain separation of concerns.

- **`pulseplex-core`**: The "brain" of the system. Contains the 512-channel DMX
  Universe buffers, Art-Net bridging, and effects logic.
- **`pulseplex-midi`**: Handles high-priority MIDI input and trigger parsing.

## Getting Started

### Prerequisites

- [Rust](https://rustup.rs/) (2021 Edition or later)

### Development

```bash
# Run all tests across the workspace
cargo test

# Start the development daemon
cargo run
```
