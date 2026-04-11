## Project Context

PulsePlex is a high-performance MIDI-to-ArtNet bridge built in Rust. It
functions as a real-time daemon that maps musical performance data (Electronic
Drums/Keyboards) to DMX lighting fixtures with sub-millisecond precision.

## Core Architecture

- **Producer-Consumer Model:** MIDI input is handled in a background thread;
  signals are passed via `crossbeam-channel` to a 40Hz (25ms) main loop.
- **Decay Envelopes:** Lighting state is managed via mathematical envelopes
  (`Linear`, `Exponential`) rather than static "on/off" states.
- **Hot-Reloading:** The system uses `notify` to watch `pulseplex.toml` and
  updates mappings mid-flight without process restarts.

## Coding Style & Idioms

Keep functions small and focused following single responsibility and DRY
principles. Always consider opportunities to improve code quality as you work.

- **Performance:** The main loop is a "hot path." Avoid heap allocations
  (`String`, `Vec`) inside the loop. Prefer stack-allocated arrays or
  pre-allocated `HashMaps`.
- **Safety:** Prefer `anyhow::Result` for application-level error handling and
  `thiserror` for library-level errors.
- **Concurrency:** Use `std::sync::atomic` for simple flags (e.g., `AtomicBool`
  for shutdown) and `Arc` for shared configuration.
- **Logging:** Use the `tracing` crate. Use `trace!` for per-frame data,
  `debug!` for MIDI events, and `info!` for lifecycle changes.
- **Math:** Use `f32` for lighting calculations to maintain compatibility with
  DMX’s 8-bit constraints while allowing for smooth scaling.

## File Organization Rules

- `pulseplex/`: The main binary (CLI, networking, loop coordination).
- `crates/pulseplex-core/`: Pure, side-effect-free math (Envelopes, Curves,
  Art-Net packet building).
- `crates/pulseplex-midi/`: Hardware-specific MIDI binding logic.
- **Rule:** Keep `pulseplex-core` free of I/O or networking dependencies to
  ensure it remains 100% testable via unit tests.

## Behavioral Rules for AI Agents

1. **Strict Formatting:** Always use `rustfmt` standards.
2. **No Hallucinations:** Do not invent DMX op-codes. Refer to the Art-Net 4
   specification (ArtDmx packets start with `Art-Net\0` followed by OpCode
   `0x5000`).
3. **Idiomatic Refactoring:** If you see a `while let Ok(_)` that could be a
   `while config_rx.try_recv().is_ok()`, suggest the refactor.
4. **Security:** Ensure the `UdpSocket` binds to `0.0.0.0:0` for sending and
   only enable broadcast explicitly when required.

## Build, Test, and Quality Commands

- `cargo build`: Build the entire workspace
- `cargo test`: Run all tests. Every new feature should have a corresponding
  test, and all tests MUST pass before committing
- `cargo clippy --all-targets -- -D warnings`: All code must pass linting rules
  before committing
- `cargo fmt`: Format Rust code before committing
