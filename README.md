[![CI](https://github.com/jim-miller/pulseplex/actions/workflows/ci.yml/badge.svg)](https://github.com/jim-miller/pulseplex/actions/workflows/ci.yml)
[![Release](https://github.com/jim-miller/pulseplex/actions/workflows/release.yml/badge.svg)](https://github.com/jim-miller/pulseplex/actions/workflows/release.yml)

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

- **`pulseplex-core`**: The protocol-agnostic math and state engine. Manages the
  40Hz orchestration loop and HTP (Highest Takes Precedence) merging into a
  global 512-byte DMX universe.
- **`pulseplex-midi`**: Handles high-priority MIDI input, parsing hardware
  triggers into internal Logical IDs.
- **`pulseplex-hue`**: Translates the global 512-byte DMX buffer into 16-bit RGB
  state for Philips Hue bridges via the Entertainment API (UDP/DTLS) in an
  isolated background thread.

### The Multi-Tier Configuration

PulsePlex decouples inputs from outputs using a flexible multi-tier model,
allowing you to swap instruments or lighting rigs without rewriting your entire
configuration:

1. **Input Map**: Maps hardware (e.g., MIDI Note 36) to an Internal ID (e.g.,
    `snare`).
2. **Behavior Map**: Defines the math (e.g., `snare` uses a 0.5s Linear Decay).
3. **Fixture Library**: Instantiates `FixtureProfiles` (JSON) into the virtual
    DMX universe.
4. **Mappings & Patching**:
    - **Fixture Mappings**: Routes behavior intensity to specific fixture
      capabilities (e.g., `snare` -> `Red` on `Fixture 1`).
    - **Target Patch**: Maps the global DMX buffer to specific hardware IDs
      (e.g., Hue Entertainment ID 0 -> DMX address 1).

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
