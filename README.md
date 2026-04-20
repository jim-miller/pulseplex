[![CI](https://github.com/jim-miller/pulseplex/actions/workflows/ci.yml/badge.svg)](https://github.com/jim-miller/pulseplex/actions/workflows/ci.yml)
[![License: MIT](https://img.shields.io/badge/License-MIT-yellow.svg)](https://opensource.org/licenses/MIT)
[![Release](https://github.com/jim-miller/pulseplex/actions/workflows/release.yml/badge.svg)](https://github.com/jim-miller/pulseplex/actions/workflows/release.yml)

# PulsePlex 🥁💡

**PulsePlex** is a high-performance, pluggable orchestration engine built in
Rust. It is designed to bridge real-time musical triggers (MIDI), real-world
lighting and effects (DMX512-A/Art-Net), and networked lighting systems like
**Philips Hue**.

As a systems-first project, PulsePlex prioritizes deterministic timing,
zero-cost abstractions, and a strict 40Hz orchestration loop to ensure your
lighting stays perfectly in pocket with a live musical performance.

## ✨ Features

- **The Broadcast Hub Topology:** PulsePlex uses a strict 8-bit DMX Intermediate
  Representation. It performs complex decay and merging math once, and
  broadcasts the resulting DMX universe to all parallel outputs simultaneously.
- **Philips Hue Entertainment Bridge:** Transforms standard DMX signals into
  16-bit color space and streams them to your Hue Bridge via UDP/DTLS with zero
  perceived latency.
- **First-Run Setup Wizard:** Automatically discovers your Hue Bridge via mDNS,
  handles the push-link authorization, and maps your MIDI devices interactively.
- **Interactive TUI Dashboard:** Monitor active envelopes, recent hardware
  triggers, and a live visual representation of your DMX universe output in
  real-time.
- **Live Hot-Reloading:** Tweak your `.toml` configuration or `.json` fixture
  profiles and watch the lights update instantly without dropping the daemon.

---

## 🚀 Quick Start (Users)

If you are just looking to run PulsePlex with your electronic drum kit:

**1. Install** Ensure you have [Rust installed](https://rustup.rs/), then clone
and build the release binary:

```bash
git clone [https://github.com/jim-miller/pulseplex.git](https://github.com/jim-miller/pulseplex.git)
cd pulseplex
cargo install --path .
```

**2. Run the Wizard** Starting PulsePlex for the first time will automatically
launch the setup wizard to connect your Hue Bridge and select your MIDI
controller.

```bash
pulseplex run
```

### Useful CLI Commands

PulsePlex includes a suite of tools to manage your lighting rig:

- `pulseplex run` - Starts the main 40Hz orchestration daemon and TUI dashboard.
- `pulseplex run --no-tui` - Runs the daemon headlessly, streaming logs to
  `stdout`.
- `pulseplex hue setup` - Interactively discovers and configures a new Philips
  Hue Bridge without overwriting your DMX settings.
- `pulseplex doctor` - Runs network and connectivity diagnostics to troubleshoot
  lag or dropped packets.
- `pulseplex template eject` - Extracts the default `pulseplex.toml` and fixture
  JSONs to your current directory for easy customization.

---

## 🏗️ Architecture (Developers)

PulsePlex is organized as a Cargo Workspace utilizing a decoupled
**Source-Core-Sink** model:

- **`pulseplex-core`**: The protocol-agnostic math and state engine. It listens
  for normalized `SourceEvents`, calculates decay physics, performs HTP (Highest
  Takes Precedence) merging, and broadcasts a read-only `[u8; 512]` DMX universe
  buffer at exactly 40Hz.
- **`pulseplex-midi` (Source)**: Handles high-priority MIDI input, parsing
  hardware note velocities into normalized `SourceEvents` and sending them to
  the Core.
- **`pulseplex-hue` (Sink)**: A translation layer that listens to the Core's DMX
  broadcasts. It maps specific virtual DMX addresses to physical Hue bulbs,
  scaling the 8-bit math to 16-bit RGB, and streaming it via DTLS.

### The Multi-Tier Configuration

PulsePlex decouples inputs from outputs, allowing you to swap instruments or
lighting rigs without rewriting your logic:

1. **Input Map**: Maps hardware (e.g., MIDI Note 36) to a Logical ID (e.g.,
   `snare`).
2. **Behavior Map**: Defines the physics (e.g., `snare` uses a 0.5s Linear
   Decay).
3. **Fixture Library**: Defines capabilities using standard JSON profiles (e.g.,
   "Generic RGBW").
4. **Mappings & Patching**: Routes the calculated behavior intensity to specific
   fixture channels (e.g., `snare` -> `Red` on `Fixture 1`).

## 🛠️ Development

### Prerequisites

- [just](https://github.com/casey/just) (Task runner)
- Docker (for ARM64 cross-compilation)

### Commands

```bash
# Run the strict CI gate (formatting, clippy, and locked tests)
just ci

# Start the development daemon with hot-reloading active
just dev

# Cross-compile for Orange Pi / Raspberry Pi (aarch64)
just build-arm
```

