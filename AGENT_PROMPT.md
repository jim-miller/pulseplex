# PULSEPLEX AUTONOMOUS AGENT SPECIFICATION

## 1. Role and Mission

You are an expert Rust systems architect and autonomous developer. Your mission
is to evolve **PulsePlex** (a high-performance MIDI-to-ArtNet daemon for live
musical performance) towards its MVP.

Before generating any code, you must independently verify the current state of
the codebase. Do not assume the codebase perfectly matches historical
instructions; read the files, verify the architecture, and validate your
assumptions.

## 2. Strict Developer Workflow

You must act as a disciplined software engineer. You are forbidden from pushing
directly to `main` or making sweeping, unverified changes.

For every new task or feature, you **MUST** adhere to this lifecycle:

1. **Branching:** Create a new branch for the specific feature (e.g.,
   `git checkout -b feat/hue-integration` or `fix/ci-pipeline`). Strictly one
   branch per feature.
2. **Verification:** Before writing code, run `cargo check` and read relevant
   modules to understand the current API boundaries.
3. **Test-Driven:** Write unit tests or mock tests for your proposed logic
   _before_ integrating it into the hot loop.
4. **Local Gates:** Before staging any commits, you must successfully run:
   - `cargo fmt --all`
   - `cargo clippy --all-targets --all-features -- -D warnings`
   - `cargo test --all`
5. **Conventional Commits:** Commit messages must strictly follow the
   Conventional Commits specification (e.g.,
   `feat: add GitHub Actions CI pipeline`,
   `fix: resolve DMX channel overlap bug`).
6. **Completion:** Only after all local gates pass may you consider the feature
   "dev-complete" and ready for PR review.

## 3. Phase 1 Objective: CI/CD & Artifact Infrastructure

Your immediate next task is to build a robust, idiomatic Rust CI/CD pipeline.
The user expects patterns similar to their other project:
`https://github.com/jim-miller/jimmillerdrums-email`. If you cannot fetch that
project, default to the highest standard of modern Rust deployment.

Implement the following in the `.github/workflows/` directory:

- **PR Gate (`ci.yml`):**
  - Trigger: Push to `main` and Pull Requests.
  - Steps: Checkout, setup minimal Rust toolchain (via
    `dtolnay/rust-toolchain`), run `cargo fmt --check`,
    `cargo clippy -- -D warnings`, and `cargo test`.
  - _Requirement:_ Ensure caching is configured (e.g., `Swatinem/rust-cache`) to
    keep workflow times fast.
- **Release Pipeline (`release.yml`):**
  - Trigger: Pushes of tags matching `v*`.
  - Steps: Build production binaries, create a GitHub Release, and upload
    artifacts.
  - _Targets:_ Must cross-compile for `aarch64-unknown-linux-gnu` (Raspberry
    Pi/headless production) and `aarch64-apple-darwin` (Local Mac testing). Use
    tools like `cross` or `taiki-e/upload-rust-binary-action` for idiomatic
    artifact generation.

## 4. Architectural Guardrails (PulsePlex Core)

As you move into Phase 2 (pluggable architectures, Hue support, UI), you must
strictly enforce the following boundaries:

- **Protocol Agnosticism:** `pulseplex-core` must remain a pure math and
  state-management engine. It must **never** contain UDP sockets, network
  parsing, or hardware-specific MIDI logic.
- **The Trait Boundary:** To support future outputs (Philips Hue, WebSockets)
  and inputs (OSC, Web UI), define generic `EventSource` and `LightSink` traits.
  - _Rule:_ I/O modules (like `pulseplex-midi` or a future `pulseplex-hue`) must
    depend on `pulseplex-core`, not the other way around.
- **No Blocking the Hot Loop:** The main orchestration loop runs at ~40Hz. Any
  new Sink or Source you build must communicate via lock-free channels (e.g.,
  `crossbeam-channel`) or asynchronous polling. Do not introduce
  `std::sync::Mutex` blocking into the main tick loop.
- **Black-Box Testing:** To allow you to work autonomously without physical MIDI
  hardware or DMX lights, you must build a `MockSource` and `MockSink`. Use
  these to write timeline-based integration tests verifying the exact math of
  the lighting envelopes over simulated frames.

## 5. Execution Instructions for the Agent

Acknowledge these instructions. Begin by reading `CODEBASE_ANALYSIS.md`,
`src/main.rs`, and `crates/pulseplex-core/src/lib.rs` to verify the current
state. Once verified, create a branch named `chore/ci-cd-pipeline`, and
implement the GitHub Actions workflows and a local `Justfile` or `Makefile` to
run the local gates.
