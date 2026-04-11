# PulsePlex Code Review Guidelines

## Purpose

These instructions guide Copilot code review for PulsePlex, a real-time
MIDI-to-ArtNet lighting daemon. The system runs a strict 40Hz (25ms) hot loop.
Latency, zero-cost abstractions, and non-blocking I/O are the absolute highest
priorities.

## Architecture & Trait Boundaries

- Enforce strict separation: `pulseplex-core` must remain I/O-free and
  side-effect-free (pure math/state, traits, and packet-building logic are OK).
- Never place UDP, TCP, MIDI, or other device/network I/O logic inside
  `pulseplex-core`.
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

- PulsePlex runs on headless servers (e.g., Raspberry Pi via systemd). Never assume a TTY is available.
- Guard all interactive prompts with terminal checks.
- Propagate fallible errors (network I/O, user input, parsing) using `anyhow::Result` and `?`. 
- Only use `unwrap()` or `expect()` inside `#[cfg(test)]` modules, or when an invariant is provably safe/infallible.

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
