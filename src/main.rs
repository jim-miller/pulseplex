mod config;

use std::collections::HashMap;
use std::net::{SocketAddr, ToSocketAddrs, UdpSocket};
use std::path::PathBuf;
use std::str::FromStr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use pulseplex_core::{ArtNetBridge, DecayEnvelope};
use pulseplex_midi::{setup_midi, MidiSignal};

use anyhow::bail;
use clap::{Parser, Subcommand};
use crossbeam_channel::TryRecvError;
use dialoguer::theme::ColorfulTheme;
use dialoguer::Select;
use notify::Watcher;
use spin_sleep::SpinSleeper;
use tracing::{debug, info, trace, warn};
use tracing_subscriber::{fmt, prelude::*, EnvFilter};

use crate::config::{
    get_config_path, update_midi_device_in_config, MappingConfig, PulsePlexConfig, ShutdownMode,
};

#[derive(Parser)]
#[command(
    name = "pulseplex",
    version,
    about = "MIDI to Art-Net bridge for drummers"
)]
struct Args {
    #[command(subcommand)]
    command: Option<Commands>,

    /// Enable verbose logging
    #[arg(short, long, global = true)]
    verbose: bool,
}

#[derive(Subcommand)]
enum Commands {
    /// Start the PulsePlex daemon (default)
    Run {
        /// Path to configuration file (Overrides env vars)
        #[arg(short, long)]
        config: Option<String>,

        /// Force the interactive MIDI device selection prompt
        #[arg(short, long)]
        select_midi: bool,
    },
    /// Validate the configuration file and check for DMX collisions
    Check {
        /// Path to configuration file (Overrides env vars)
        #[arg(short, long)]
        config: Option<String>,
    },
}

fn main() -> anyhow::Result<()> {
    let cli = Args::parse();

    // Setup logging (verbose flag is global)
    let filter = EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| EnvFilter::new(if cli.verbose { "trace" } else { "info" }));

    tracing_subscriber::registry()
        .with(filter)
        .with(fmt::layer())
        .init();

    match cli.command.unwrap_or(Commands::Run {
        config: None,
        select_midi: false,
    }) {
        Commands::Check { config } => {
            let path = get_config_path(config.as_ref());
            handle_check(path.to_string_lossy().as_ref())?;
        }
        Commands::Run {
            config,
            select_midi,
        } => {
            let path = get_config_path(config.as_ref());
            run_daemon(path, select_midi)?;
        }
    }

    Ok(())
}

fn run_daemon(config_path: PathBuf, force_select: bool) -> anyhow::Result<()> {
    let path_to_watch = config_path.clone();
    let config_path_str = config_path.to_string_lossy().to_string();

    let mut config = PulsePlexConfig::load(&config_path_str)?;
    info!(
        "Loaded configuration for MIDI device: {}",
        config.midi.device_name
    );

    let available_devices = pulseplex_midi::list_midi_devices()?;

    if available_devices.is_empty() {
        bail!("No MIDI input devices detected on the system.");
    }

    let current_device = &config.midi.device_name;
    let device_missing = !available_devices.contains(current_device);

    if device_missing || force_select {
        if device_missing && !current_device.is_empty() {
            warn!(
                "Configured MIDI device '{}' is not connected.",
                current_device
            );
        }

        let selection = Select::with_theme(&ColorfulTheme::default())
            .with_prompt("Select your MIDI input device")
            .items(&available_devices)
            .default(0)
            .interact()?;

        let chosen_device = &available_devices[selection];
        info!("Selected MIDI device: {}", chosen_device);

        // 3. Save the choice back to the TOML file
        update_midi_device_in_config(&config_path_str, chosen_device)?;

        // Update our working config state
        config.midi.device_name = chosen_device.to_string();
    } else {
        info!(
            "Loaded configuration for MIDI device: {}",
            config.midi.device_name
        );
    }

    let (config_tx, config_rx) = crossbeam_channel::unbounded();

    let closure_path = path_to_watch.clone();

    let mut watcher = notify::recommended_watcher(move |res: notify::Result<notify::Event>| {
        if let Ok(event) = res {
            // Send a reload ping for any modified path that ends with our config
            if event.paths.iter().any(|p| p.ends_with(&closure_path)) {
                let _ = config_tx.send(());
            }
        }
    })?;

    // Watch current dir
    watcher.watch(
        std::path::Path::new("."),
        notify::RecursiveMode::NonRecursive,
    )?;
    info!("Hot-reloading enabled for {config_path_str}");

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
                config_path_str
            );
            match PulsePlexConfig::load(&config_path_str) {
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

                        let mut env = DecayEnvelope::new(
                            mapping.decay_seconds,
                            mapping.velocity_curve,
                            mapping.decay_profile,
                        );
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
                if let Some([r, g, b]) = mapping.color {
                    artnet.set_channel(mapping.dmx_channel, (r as f32 * env.intensity) as u8);
                    artnet.set_channel(mapping.dmx_channel + 1, (g as f32 * env.intensity) as u8);
                    artnet.set_channel(mapping.dmx_channel + 2, (b as f32 * env.intensity) as u8);
                } else {
                    artnet.set_channel(mapping.dmx_channel, env.dmx_value());
                }
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

fn handle_check(path: &str) -> anyhow::Result<()> {
    info!("Checking configuration: {}", path);

    // 1. Test TOML Parsing
    let config = PulsePlexConfig::load(path)?;
    println!("✅ TOML Structure: Valid");

    // 2. Test Network String
    match SocketAddr::from_str(&config.artnet.target_ip) {
        Ok(_) => println!("✅ Network: Target IP/Port format is valid"),
        Err(_) => {
            // If SocketAddr fails, check if it's because it's a hostname
            // (Only if you want to support hostnames but warn about shorthand)
            if config.artnet.target_ip.to_socket_addrs().is_ok() {
                println!(
                    "⚠️  Network: '{}' is a valid hostname/shorthand, but not a literal IP",
                    config.artnet.target_ip
                );
            } else {
                anyhow::bail!(
                    "❌ Network: Invalid target_ip format '{}'",
                    config.artnet.target_ip
                );
            }
        }
    }

    // 3. Test DMX Bounds and Overlaps
    let mut used_channels = std::collections::HashMap::new();
    for mapping in &config.mapping {
        // Assume RGB takes 3 channels, Dimmer takes 1
        let span = if mapping.color.is_some() { 3 } else { 1 };

        for offset in 0..span {
            let channel = mapping.dmx_channel + offset;

            if channel > 511 {
                anyhow::bail!(
                    "❌ DMX: Note {} (channel {}) exceeds universe limit (511)",
                    mapping.note,
                    channel
                );
            }

            if let Some(other_note) = used_channels.insert(channel, mapping.note) {
                warn!(
                    "DMX Collision: Channel {} is used by both Note {} and Note {}",
                    channel, other_note, mapping.note
                );
            }
        }
    }

    println!("✅ DMX Mappings: All notes within 0-511 range");
    info!("Configuration check passed.");
    Ok(())
}
