# PulsePlex Code Review Guidelines

## Purpose

These instructions guide Copilot code review for PulsePlex, a real-time
MIDI-to-ArtNet and MIDI-to-Philips Hue (DTLS) lighting daemon. The system runs a
strict 40Hz (25ms) hot loop. Latency, zero-cost abstractions, network
resilience, and non-blocking I/O are the absolute highest priorities.

## Architecture & Trait Boundaries

- Enforce strict separation: `pulseplex-core` must remain I/O-free and
  side-effect-free (pure math/state, traits, and packet-building logic are OK).
- Never place UDP, TCP, MIDI, or other device/network I/O logic inside
  `pulseplex-core`.
- All new input protocols must implement `pulseplex_core::EventSource`.
- All new output protocols must implement `pulseplex_core::LightSink`.

## Performance & Memory: The Hot Loop

- Never use blocking operations (`std::thread::sleep`, blocking channel
  receives, or heavy I/O) in the main engine loop.
- Never use `std::sync::Mutex` where lock-free channels (`crossbeam_channel`) or
  atomics (`AtomicBool`) can be used.
- **Zero-Allocation Object Pools:** The hot loop must never allocate memory.
  When passing frames to background network threads (e.g., `HueSink`), use a
  lock-free buffer recycling pool (`pool_tx`/`pool_rx`). Do not accept PR
  suggestions that drop these buffers or introduce new `Vec::new()` calls inside
  the `process_tick` loop.

```rust
// Avoid: Allocating new memory in the hot loop
let mut buffer = vec![0.0; mapping_count];

// Prefer: Retrieving a recycled buffer from the pool
let mut buffer = self.pool_rx.try_recv().unwrap_or_else(|_| vec![0.0; mapping_count]);
```

## Domain-Specific Gotchas (Philips Hue & TLS)

- **The TLS SAN Bypass:** The Philips Hue Bridge does not include a Subject
  Alternative Name (SAN) extension in its certificate. `HueCertVerifier`
  intentionally uses string-matching to bypass `NotValidForName`,
  `SubjectAltName`, and `Expired` errors while maintaining strict cryptographic
  signature validation. **Do NOT suggest removing this bypass**, or PulsePlex
  will break on macOS.
- **Graceful Degradation:** Hue channel IDs are 0-indexed. The background thread
  actively queries the Bridge for valid channels and filters out invalid ones to
  prevent the Bridge from silently dropping the entire DTLS UDP packet. Do not
  remove this filtering logic.
- **DTLS Zombie Connections:** The `webrtc_dtls` stream must explicitly send a
  `close_notify` alert (`conn.close().await`) when dropping. If not, the Hue
  Bridge will lock up for 10 seconds and ignore hot-reloads.
- **State Restoration:** DTLS is write-only. Do not attempt to read lighting
  state from the UDP stream. State capture and restoration must be performed via
  the Hue V2 REST API (`/clip/v2/resource/light`).

## Headless Execution & Safety

- PulsePlex runs on headless servers (e.g., Raspberry Pi via systemd). Never
  assume a TTY is available.
- Guard all interactive prompts (like `dialoguer`) with
  `std::io::stdout().is_terminal()` checks.
- Propagate fallible errors (network I/O, user input, parsing) using
  `anyhow::Result` and `?`.
- Only use `unwrap()` or `expect()` inside `#[cfg(test)]` modules, or when an
  invariant is provably safe/infallible.

```rust
// Avoid: Crashing on fallible I/O or user input
let selection = Select::new().interact().unwrap();

// Prefer: Provably safe unwraps (invariants guaranteed)
pub fn dmx_data(&self) -> &[u8; 512] {
    // SAFE: The slice is hardcoded to exactly 512 bytes (530 - 18)
    self.buffer[18..530].try_into().unwrap()
}
```

## Testing Standards

- Do not write tests that require physical MIDI hardware or actual UDP sockets
  unless using a local mock server (e.g., `wiremock`).
- Use `pulseplex_core::MockSource` and `pulseplex_core::MockSink` for
  integration tests.
- Verify decay math by manually advancing time using `Duration` and checking
  `intensity` values.

```

```
