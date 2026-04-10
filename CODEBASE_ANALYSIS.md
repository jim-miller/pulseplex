# PulsePlex Codebase Analysis

## Project Overview

**PulsePlex** is a high-performance Rust application that bridges MIDI musical triggers to Art-Net/DMX lighting equipment. It's designed for drummers and musicians who want their lighting to react in real-time with sub-millisecond precision to their performance.

---

## Architecture

PulsePlex uses a **Cargo workspace** structure with clear separation of concerns:

```
pulseplex/
├── Cargo.toml (workspace root)
├── crates/
│   ├── pulseplex-core/      # Core domain logic (DMX, envelopes)
│   └── pulseplex-midi/      # MIDI input handling
├── src/
│   ├── main.rs              # CLI and daemon orchestration
│   └── config.rs            # Configuration management
├── pulseplex.toml           # Default configuration
└── codebook.toml            # Spell-check dictionary
```

---

## Core Components

### 1. pulseplex-core (`crates/pulseplex-core/src/lib.rs`)

The "brain" of the system - pure, side-effect-free logic:

| Component | Purpose |
|-----------|---------|
| `DecayEnvelope` | Manages light intensity decay over time with configurable velocity curves (Linear, Hard, Soft) and decay profiles (Linear, Exponential) |
| `ArtNetBridge` | Builds 530-byte Art-Net UDP packets with 512-channel DMX universe, handles sequence numbering |

**Key algorithms:**
- Velocity curves map MIDI velocity (0-127) to intensity (0.0-1.0)
- Exponential decay: `intensity *= e^(-5 * decay_rate * dt)`

### 2. pulseplex-midi (`crates/pulseplex-midi/src/lib.rs`)

MIDI input handling with producer-consumer pattern:

| Function | Purpose |
|----------|---------|
| `list_midi_devices()` | Scans available MIDI input ports |
| `find_midi_port()` | Substring matching to find target device |
| `setup_midi()` | Creates MIDI connection with callback that sends `MidiSignal` via channel |

**MIDI parsing:**
- `0x90` + velocity > 0 → `NoteOn`
- `0x80` or `0x90` with velocity = 0 → `NoteOff`

### 3. src/config.rs

Configuration management with hot-reloading support:

| Struct | Purpose |
|--------|---------|
| `PulsePlexConfig` | Top-level config (MIDI, Art-Net, mappings, shutdown behavior) |
| `MappingConfig` | Note-to-DMX mapping with decay params and optional RGB color |
| `ShutdownMode` | Blackout, Default, or Restore initial state |

---

## Main Entry Point (`src/main.rs`)

### CLI Commands
- `pulseplex run [config]` - Start the daemon (default)
- `pulseplex check [config]` - Validate config and check for DMX collisions

### Run Daemon Path
```
1. Load/validate config
2. MIDI device selection (interactive or auto)
3. Setup file watcher for hot-reload
4. Initialize UDP socket (broadcast)
5. Capture initial DMX state (if Restore mode)
6. Main Loop (40 Hz / 25ms):
   ├─ Hot-reload config (debounced)
   ├─ Process MIDI events
   ├─ Update active envelopes
   ├─ Build DMX packet
   ├─ Send Art-Net packet
   └─ Sleep until next frame
7. perform_shutdown() on exit
```

### Shutdown Modes
- `Blackout`: All lights off
- `Default`: Set configured default values
- `Restore`: Return to captured initial state

---

## Data Flow

```
MIDI Hardware
    ↓ (midir callback)
crossbeam_channel (MidiSignal)
    ↓ (main thread receives)
NoteOn/NoteOff processing
    ↓ (creates DecayEnvelope)
active_lights HashMap<u8, Envelope>
    ↓ (each 25ms tick)
DecayEnvelope::tick(dt)
    ↓ (intensity calculated)
DMX channel values
    ↓ (ArtNetBridge)
UDP Socket → Art-Net Device
```

---

## Key Design Decisions

1. **Sync-to-Async Architecture**: Main loop runs synchronously at 40Hz but uses async-friendly patterns (channels, atomic flags)

2. **Zero-Cost Abstractions**: Raw arrays for Art-Net packets, no allocations in hot path

3. **Time-Aware Decay**: Uses `Duration` for precise timing independent of frame rate

4. **Hot Reloading**: File watcher on config with debouncing to avoid mid-save corruption

5. **Real-Time Considerations**:
   - `spin_sleep` for precise timing (avoids OS scheduler delays)
   - Debounced config reloads
   - Minimizing allocations in MIDI callback

---

## Dependencies

| Package | Purpose |
|---------|---------|
| `midir` | Cross-platform MIDI I/O |
| `crossbeam-channel` | MPSC channels for MIDI events |
| `tracing` | Structured logging |
| `clap` | CLI argument parsing |
| `serde` + `toml` | Config serialization |
| `notify` | File system watching |
| `dialoguer` | Interactive CLI prompts |
| `ctrlc` | Graceful signal handling |
| `spin_sleep` | High-precision sleep |

---

## Configuration Example

```toml
[[mapping]]
note = 36              # Kick drum
dmx_channel = 0
decay_seconds = 0.5
velocity_curve = "hard"
decay_profile = "exponential"
color = [255, 0, 0]    # Red
```

---

## Testing

The codebase includes unit tests covering:
- Time-aware decay accuracy
- Velocity curve behavior (Linear, Hard, Soft)
- Decay profiles (Linear, Exponential)
- Envelope lifecycle
- Art-Net protocol header structure
- Channel mapping boundaries
- Sequence increment

All tests are in `pulseplex-core` and require no I/O dependencies.
