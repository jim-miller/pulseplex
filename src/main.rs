mod config;

use std::collections::HashMap;
use std::net::{SocketAddr, ToSocketAddrs, UdpSocket};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use pulseplex_core::{ArtNetBridge, DecayEnvelope};
use pulseplex_midi::{setup_midi, MidiSignal};

use clap::Parser;
use crossbeam_channel::TryRecvError;
use notify::Watcher;
use spin_sleep::SpinSleeper;
use tracing::{debug, info, trace, warn};
use tracing_subscriber::{fmt, prelude::*, EnvFilter};

use crate::config::{MappingConfig, PulsePlexConfig, ShutdownMode};

#[derive(Parser)]
struct Args {
    /// Enable verbose logging
    #[arg(short, long)]
    verbose: bool,
}

fn main() -> anyhow::Result<()> {
    let args = Args::parse();
    let filter = EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| EnvFilter::new(if args.verbose { "trace" } else { "info" }));

    tracing_subscriber::registry()
        .with(filter)
        .with(fmt::layer().with_target(true))
        .init();

    const CONFIG_FILE: &str = "pulseplex.toml";

    let config = PulsePlexConfig::load(CONFIG_FILE)?;
    info!(
        "Loaded configuration for MIDI device: {}",
        config.midi.device_name
    );

    let (config_tx, config_rx) = crossbeam_channel::unbounded();

    let mut watcher = notify::recommended_watcher(move |res: notify::Result<notify::Event>| {
        if let Ok(event) = res {
            // Send a reload ping for any modified path that ends with our config
            if event.paths.iter().any(|p| p.ends_with(CONFIG_FILE)) {
                let _ = config_tx.send(());
            }
        }
    })?;

    // Watch current dir
    watcher.watch(
        std::path::Path::new("."),
        notify::RecursiveMode::NonRecursive,
    )?;
    info!("Hot-reloading enabled for {CONFIG_FILE}");

    let mut note_mappings: HashMap<u8, MappingConfig> = config
        .clone()
        .mapping
        .into_iter()
        .map(|m| (m.note, m))
        .collect();

    let target_hz = 40.0; // 25ms
    let target_interval = Duration::from_secs_f64(1.0 / target_hz);

    let running = Arc::new(AtomicBool::new(true));
    let r = running.clone();

    ctrlc::set_handler(move || {
        info!("Shutdown signal received...");
        r.store(false, Ordering::SeqCst);
    })?;

    info!("Starting PulsePlex daemon");

    let socket = UdpSocket::bind("0.0.0.0:0")?;
    socket.set_broadcast(true)?;

    let target_addr = config
        .artnet
        .target_ip
        .to_socket_addrs()?
        .next()
        .ok_or_else(|| anyhow::anyhow!("Could not parse target_ip address"))?;

    info!("Network initialized. Target: {}", target_addr);

    let mut artnet = ArtNetBridge::new(config.artnet.universe);

    let sleeper = SpinSleeper::default();
    let mut active_lights: HashMap<u8, DecayEnvelope> = HashMap::new();

    let midi_input = setup_midi(&config.midi.device_name)?;

    let mut initial_state = [0u8; 512];
    if matches!(config.shutdown.mode, ShutdownMode::Restore) {
        info!("Capturing current lighting state for later restoration...");
        socket.set_read_timeout(Some(Duration::from_millis(1000)))?;
        // Note: might need to bind to the Art-Net port 6454 to receive if
        // another controller is broadcasting
        let mut buf = [0u8; 1024];
        if let Ok((amt, _)) = socket.recv_from(&mut buf) {
            // Basic Art-Net header skip to get to the DMX payload
            if amt > 18 {
                initial_state.copy_from_slice(&buf[18..530]);
            }
        }
        // Back to non-blocking or blocking-only send
        socket.set_read_timeout(None)?;
    }

    let mut last_tick = Instant::now();
    let mut next_deadline = last_tick + target_interval;

    while running.load(Ordering::SeqCst) {
        // calculate our time since last tick
        let now = Instant::now();
        let delta_time = now.duration_since(last_tick);
        last_tick = now;

        let mut needs_reload = false;
        while config_rx.try_recv().is_ok() {
            needs_reload = true;
        }

        if needs_reload {
            info!(
                "config change detected in {}, attempting reload...",
                CONFIG_FILE
            );
            match PulsePlexConfig::load(CONFIG_FILE) {
                Ok(new_config) => {
                    // Swap mappings
                    note_mappings = new_config
                        .mapping
                        .into_iter()
                        .map(|m| (m.note, m))
                        .collect();
                }
                Err(e) => {
                    warn!("Failed to reload config (keeping previous state): {}", e);
                }
            }
        }

        // drain the MIDI queue
        loop {
            match midi_input.rx.try_recv() {
                Ok(MidiSignal::NoteOn { note, velocity }) => {
                    if let Some(mapping) = note_mappings.get(&note) {
                        debug!("Received: Note: {} Velocity: {}", note, velocity);
                        let mut env = DecayEnvelope::new(mapping.decay_seconds);
                        env.trigger(velocity);
                        active_lights.insert(note, env);
                    } else {
                        trace!("Ignored unmapped note: {}", note);
                    }
                }
                Ok(MidiSignal::NoteOff { note }) => {
                    trace!("NoteOff: {}", note);
                }
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => {
                    warn!("MIDI hardware disconnected!");
                    break;
                }
            }
        }
        // update decay envelopes
        active_lights.retain(|note, env| {
            env.tick(delta_time);
            if env.is_dead() {
                debug!("Note {} envelope finished decay and was removed.", note);
                false
            } else {
                trace!("Note {}: DMX Value {}", note, env.dmx_value());
                true
            }
        });

        // if !active_lights.is_empty() { ... }

        // output to Art-Net
        artnet.clear_data();
        for (note, env) in &active_lights {
            // simple mapping: Note 36 (Kick Drum, C1) -> DMX Ch0
            if let Some(mapping) = note_mappings.get(note) {
                artnet.set_channel(mapping.dmx_channel, env.dmx_value());
            }
        }

        // send out to DMX
        socket.send_to(artnet.as_bytes(), target_addr)?;
        artnet.increment_sequence();

        let sleep_start = Instant::now();

        if sleep_start < next_deadline {
            sleeper.sleep(next_deadline.duration_since(sleep_start));
        } else {
            // Missed our deadline and overslept
            warn!(
                "Frame drop detected! Work took longer than {:?}",
                target_interval
            );
        }

        // advance the deadline for the next frame
        // using `.max(Instant::now))` to prevent a catch-up stampede
        // if we lag too heavily, we just reset the clock and move on
        next_deadline = Instant::now().max(next_deadline) + target_interval;
    }
    perform_shutdown(&config, &socket, &target_addr, &mut artnet, &initial_state)?;

    info!("PulsePlex shut down cleanly.");
    Ok(())
}

fn perform_shutdown(
    config: &PulsePlexConfig,
    socket: &UdpSocket,
    addr: &SocketAddr,
    artnet: &mut ArtNetBridge,
    initial_state: &[u8; 512],
) -> anyhow::Result<()> {
    match config.shutdown.mode {
        ShutdownMode::Blackout => {
            info!("Shutting down: Blackout");
            artnet.clear_data();
        }
        ShutdownMode::Default => {
            info!("Shutting down: Applying default scene");
            artnet.clear_data();
            if let Some(defaults) = &config.shutdown.defaults {
                for (ch, val) in defaults {
                    artnet.set_channel(*ch, *val);
                }
            }
        }
        ShutdownMode::Restore => {
            info!("Shutting down: Restoring previous state");
            // Direct copy of the snapshot we took at startup
            // (assuming ArtNetBridge allows raw buffer setting)
            artnet.set_raw_data(initial_state);
        }
    }

    // Send the final "Exit Frame"
    socket.send_to(artnet.as_bytes(), addr)?;
    Ok(())
}
