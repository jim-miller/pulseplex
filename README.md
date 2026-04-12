# PulsePlex 🥁💡

**PulsePlex** is a high-performance, pluggable orchestration engine built in
Rust. It is designed to bridge real-time musical triggers (MIDI), real-world
lighting and effects (DMX512-A/Art-Net), and networked lighting systems like
**Philips Hue**.

As a systems-first project, PulsePlex prioritizes deterministic timing,
zero-cost abstractions, and a strict 40Hz orchestration loop to ensure your
lighting stays perfectly in pocket with a live musical performance.

## Architecture

PulsePlex is organized as a Cargo Workspace utilizing a strict Producer-Consumer
model:

- **`pulseplex-core`**: The mathematical and state-management engine. It is 100%
  protocol-agnostic and manages the 40Hz orchestration loop, 3-Tier
  configuration, and Decay Envelopes.
- **`pulseplex-midi`**: Handles high-priority MIDI input, parsing hardware
  triggers into internal Logical IDs.
- **`pulseplex-hue`**: Streams 16-bit RGB state to Philips Hue bridges via the
  Entertainment API (UDP/DTLS) using an isolated background thread.

### The 3-Tier Configuration

PulsePlex decouples inputs from outputs using Logical IDs, allowing you to swap
instruments or lighting rigs without rewriting your entire configuration:

1. **Input Map:** Maps hardware (e.g., MIDI Note 36) to an Internal ID (e.g.,
   `1`).
2. **Behavior Map:** Defines the math (e.g., ID `1` uses a 0.5s Exponential
   Decay).
3. **Output Map:** Routes the computed intensity of ID `1` to specific hardware
   (e.g., Art-Net Channel 12, or Hue Light 5).

## Getting Started

### Prerequisites

- [Rust](https://rustup.rs/) (2021 Edition or later)
- [just](https://github.com/casey/just) (Task runner)
- `cargo-watch` (For local development)

### Development

```bash
# Run all local gates (formatting, clippy, and tests)
just all

# Start the development daemon with hot-reloading
just dev
```
