# PulsePlex Codebase Analysis

## Project Overview

**PulsePlex** is a high-performance Rust application that bridges MIDI musical triggers to Art-Net/DMX lighting equipment. It's designed for drummers and musicians who want their lighting to react in real-time with sub-millisecond precision to their performance.

---

## Architecture

PulsePlex uses a **Cargo workspace** structure with clear separation of concerns and protocol-agnostic boundaries:

```
pulseplex/
├── Cargo.toml (workspace root)
├── crates/
│   ├── pulseplex-core/      # Core domain logic, traits, and math
│   └── pulseplex-midi/      # MIDI input implementation
├── src/
│   ├── main.rs              # CLI and orchestration loop
│   └── config.rs            # Configuration management
├── pulseplex.toml           # Default configuration
└── codebook.toml            # Spell-check dictionary
```

---

## Core Components

### 1. pulseplex-core (`crates/pulseplex-core/src/lib.rs`)

The "brain" of the system - pure, side-effect-free logic and interface definitions:

| Component | Purpose |
|-----------|---------|
| `Signal` | Generic enum for events (`Trigger`, `Release`, `Clock`) used to drive the engine. |
| `EventSource` | Trait for polling generic `Signal`s from any input (MIDI, Web, OSC). |
| `LightSink` | Trait for sending lighting states to any output (Art-Net, Hue, WebSockets). |
| `DecayEnvelope` | Manages light intensity decay over time with configurable curves and profiles. |
| `ArtNetBridge` | Protocol builder for 512-channel DMX universes over UDP. |
| `MockSource`/`MockSink`| Test doubles for verifying math and timing without hardware. |

### 2. pulseplex-midi (`crates/pulseplex-midi/src/lib.rs`)

Implementation of `EventSource` for MIDI hardware:

- **MIDI Parsing:** Converts raw MIDI bytes (`0x90`, `0x80`) into generic `Signal::Trigger` and `Signal::Release` events.
- **Asynchronous:** Uses a background thread and `crossbeam-channel` to ensure zero-latency MIDI capture.

### 3. src/main.rs (Orchestration)

The daemon that wires everything together:

- **`ArtNetSink`:** Implements `LightSink` to broadcast DMX data to the network.
- **Hot-Reloading:** Config watcher that updates note mappings without restarting.
- **Tick Loop:** Fixed 40Hz (25ms) orchestration loop that polls sources and updates sinks.

---

## Data Flow

```
Input Hardware (MIDI)
    ↓ (midir callback)
Signal Channel
    ↓ (EventSource::poll)
Orchestration Loop (40Hz)
    ↓ (trigger/decay logic)
active_lights HashMap<u8, DecayEnvelope>
    ↓ (intensity calculation)
DMX Frame [u8; 512]
    ↓ (LightSink::send_state)
Output Hardware (Art-Net)
```

---

## Key Design Decisions

1. **Protocol Agnosticism**: Core logic only knows about `Signal`s and `[u8; 512]` frames. Hardware details are hidden behind traits.

2. **Sync-to-Async Architecture**: Main loop runs synchronously for precise timing but communicates via lock-free channels.

3. **Time-Aware Decay**: Uses `Duration` for precise timing independent of frame rate.

4. **Black-Box Testing**: Integration tests run on a "virtual timeline" using `MockSource` and `MockSink`.

---

## Testing

- **Unit Tests:** Cover decay curves, profiles, and Art-Net packet construction.
- **Integration Tests:** `test_mock_integration` verifies the entire pipeline (Source -> Decay -> Sink) over multiple frames using mock doubles.
