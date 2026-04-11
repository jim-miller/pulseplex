# PulsePlex Code Review Guidelines

## Purpose

These instructions guide Copilot code review for PulsePlex, a real-time
MIDI-to-ArtNet lighting daemon. The system runs a strict 40Hz (25ms) hot loop.
Latency, zero-cost abstractions, and non-blocking I/O are the absolute highest
priorities.

## Architecture & Trait Boundaries

- Enforce strict separation: `pulseplex-core` must remain protocol-agnostic
  (pure math/state).
- Never place UDP, TCP, or MIDI I/O logic inside `pulseplex-core`.
- All new input protocols must implement `pulseplex_core::EventSource`.
- All new output protocols must implement `pulseplex_core::LightSink`.

## Performance: The Hot Loop

- Never use blocking operations (`std::thread::sleep`, blocking channel
  receives, or heavy I/O) in the main engine loop.
- Never use `std::sync::Mutex` where lock-free channels (`crossbeam_channel`) or
  atomics (`AtomicBool`) can be used.
- Avoid dynamic allocations (`String`, `Vec`) inside `while running.load()`
  loops. Use pre-allocated buffers and raw arrays.

```rust
// Avoid: Blocking the hot loop and dynamic allocations
let msg = rx.recv().unwrap();
let mut dmx_frame = vec![0u8; 512];

// Prefer: Non-blocking channel drains and stack-allocated arrays
let mut dmx_frame = [0u8; 512];
while let Ok(msg) = rx.try_recv() {
    // Process messages without blocking
}
```

## Headless Execution & Safety

- PulsePlex runs on headless servers (e.g., Raspberry Pi via systemd). Never
  assume a TTY is available.
- Guard all interactive prompts with terminal checks.
- Never use `unwrap()` or `expect()` in production code. Propagate errors using
  `anyhow::Result` and `?`.

```rust
// Avoid: Crashing in headless environments
let selection = Select::new().interact().unwrap();

// Prefer: Fallbacks and terminal checks
if std::io::stdin().is_terminal() {
    let selection = Select::new().interact()?;
} else {
    anyhow::bail!("Headless mode: Required configuration is missing.");
}
```

## Testing Standards

- Do not write tests that require physical MIDI hardware or actual UDP sockets.
- Use `pulseplex_core::MockSource` and `pulseplex_core::MockSink` for
  integration tests.
- Verify decay math by manually advancing time using `Duration` and checking
  `intensity` values.

```rust
// Avoid: Testing with real sockets
let socket = UdpSocket::bind("0.0.0.0:0").unwrap();

// Prefer: Deterministic mock testing
let mut sink = MockSink::default();
engine.tick(Duration::from_millis(25));
engine.render(&mut sink);
assert!(sink.frames[0][0] > 0);
```
