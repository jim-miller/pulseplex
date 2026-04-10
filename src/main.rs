mod config;

use std::collections::HashMap;
use std::io::IsTerminal;
use std::net::{SocketAddr, ToSocketAddrs, UdpSocket};
use std::path::PathBuf;
use std::str::FromStr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use pulseplex_core::{ArtNetBridge, DecayEnvelope, EventSource, LightSink, Signal};
use pulseplex_midi::setup_midi;

use anyhow::bail;
use clap::{Parser, Subcommand};
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

/// A wrapper that implements LightSink for Art-Net UDP output.
pub struct ArtNetSink {
    socket: UdpSocket,
    addr: SocketAddr,
    bridge: ArtNetBridge,
}

impl ArtNetSink {
    pub fn new(universe: u16, target_ip: &str) -> anyhow::Result<Self> {
        let socket = UdpSocket::bind("0.0.0.0:0")?;
        socket.set_broadcast(true)?;
        let addr = target_ip
            .to_socket_addrs()?
            .next()
            .ok_or_else(|| anyhow::anyhow!("Could not parse target_ip address"))?;

        Ok(Self {
            socket,
            addr,
            bridge: ArtNetBridge::new(universe),
        })
    }
}

impl LightSink for ArtNetSink {
    fn send_state(&mut self, state: &[u8; 512]) -> anyhow::Result<()> {
        self.bridge.set_raw_data(state);
        self.socket.send_to(self.bridge.as_bytes(), self.addr)?;
        self.bridge.increment_sequence();
        Ok(())
    }
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
            let path = get_config_path(config.as_ref())?;
            handle_check(path.to_string_lossy().as_ref())?;
        }
        Commands::Run {
            config,
            select_midi,
        } => {
            let path = get_config_path(config.as_ref())?;

            // Ensure the configuration directory exists
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent)?;
            }

            run_daemon(path, select_midi)?;
        }
    }

    Ok(())
}

fn run_daemon(config_path: PathBuf, force_select: bool) -> anyhow::Result<()> {
    // Canonicalize paths for consistent hot-reload comparison
    let config_path = if config_path.exists() {
        config_path.canonicalize()?
    } else {
        config_path
    };
    let config_path_str = config_path.to_string_lossy().to_string();

    let mut config = PulsePlexConfig::load(&config_path_str)?;

    // MIDI detection should follow setup_midi's substring matching
    let available_devices = pulseplex_midi::list_midi_devices()?;
    let current_spec = &config.midi.device_name;

    let device_found = if current_spec.is_empty() {
        false
    } else {
        // Match if any available device contains the config string
        available_devices.iter().any(|d| d.contains(current_spec))
    };

    if !device_found || force_select {
        if available_devices.is_empty() {
            bail!("No MIDI input devices detected.");
        }

        let chosen_device = if std::io::stdin().is_terminal() {
            // Interactive: Prompt user
            let selection = Select::with_theme(&ColorfulTheme::default())
                .with_prompt("Select MIDI device")
                .items(&available_devices)
                .default(0)
                .interact()?;
            available_devices[selection].clone()
        } else {
            // Non-interactive (systemd/CI): Auto-select if unambiguous
            if available_devices.len() == 1 {
                let dev = available_devices[0].clone();
                warn!("Headless mode: Auto-selecting only device: {}", dev);
                dev
            } else {
                bail!(
                    "Headless mode detected with no valid MIDI config.\n\
                     Available devices: {:?}\n\
                     Update 'device_name' in {} to continue.",
                    available_devices,
                    config_path_str
                );
            }
        };

        update_midi_device_in_config(&config_path_str, &chosen_device)?;
        config.midi.device_name = chosen_device;
    }

    // Hot-Reloading
    let (config_tx, config_rx) = crossbeam_channel::unbounded();
    let c_path = config_path.clone();
    let mut watcher = notify::recommended_watcher(move |res: notify::Result<notify::Event>| {
        if let Ok(event) = res {
            // Check if the modified file is our specific config file
            if event
                .paths
                .iter()
                .any(|p| p.canonicalize().map(|cp| cp == c_path).unwrap_or(false))
            {
                let _ = config_tx.send(());
            }
        }
    })?;

    // Watch the parent directory of the config file
    if let Some(parent) = config_path.parent() {
        watcher.watch(parent, notify::RecursiveMode::NonRecursive)?;
        info!("Watching for config changes in: {:?}", parent);
    }
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

    let mut artnet_sink = ArtNetSink::new(config.artnet.universe, &config.artnet.target_ip)?;
    let mut midi_source = setup_midi(&config.midi.device_name)?;

    let sleeper = SpinSleeper::default();
    let mut active_lights: HashMap<u8, DecayEnvelope> = HashMap::new();

    let mut initial_state = [0u8; 512];
    if matches!(config.shutdown.mode, ShutdownMode::Restore) {
        info!("Capturing current lighting state for later restoration...");

        // Temporarily bind to the specific Art-Net port just to grab a snapshot
        if let Ok(listener) = UdpSocket::bind("0.0.0.0:6454") {
            listener.set_read_timeout(Some(Duration::from_millis(1000)))?;
            let mut buf = [0u8; 1024];
            if let Ok((amt, _)) = listener.recv_from(&mut buf) {
                if amt >= 530 {
                    if let Some(dmx_data) = buf[..amt].get(18..530) {
                        initial_state.copy_from_slice(dmx_data);
                        info!("Successfully captured background DMX state.");
                    }
                } else {
                    warn!("Received short Art-Net packet while capturing background state. Restore state will be a blackout.");
                }
            } else {
                warn!("Timeout waiting for background Art-Net traffic. Restore state will be a blackout.");
            }
        } else {
            warn!("Could not bind to port 6454 to capture state. Is another lighting software running?");
        }
    }

    let mut last_tick = Instant::now();
    let mut next_deadline = last_tick + target_interval;

    while running.load(Ordering::SeqCst) {
        let now = Instant::now();
        let delta_time = now.duration_since(last_tick);
        last_tick = now;

        // Drain the hot-reload channel completely to debounce rapid file save events
        let mut needs_reload = false;
        while config_rx.try_recv().is_ok() {
            needs_reload = true;
        }

        if needs_reload {
            info!("Reloading configuration...");
            match PulsePlexConfig::load(&config_path_str) {
                Ok(new_config) => {
                    note_mappings = new_config
                        .mapping
                        .into_iter()
                        .map(|m| (m.note, m))
                        .collect();
                    info!("Reload successful.");
                }
                Err(e) => {
                    warn!("Reload failed: {}. Keeping previous configuration.", e);
                }
            }
        }

        // drain the sources
        if let Ok(signals) = midi_source.poll() {
            for signal in signals {
                match signal {
                    Signal::Trigger { id, velocity } => {
                        if let Some(mapping) = note_mappings.get(&id) {
                            debug!("Trigger: ID: {} Velocity: {}", id, velocity);

                            let mut env = DecayEnvelope::new(
                                mapping.decay_seconds,
                                mapping.velocity_curve,
                                mapping.decay_profile,
                            );
                            env.trigger(velocity);
                            active_lights.insert(id, env);
                        } else {
                            trace!("Ignored unmapped trigger: {}", id);
                        }
                    }
                    Signal::Release { id } => {
                        trace!("Release: {}", id);
                    }
                    Signal::Clock => {
                        trace!("Clock Tick");
                    }
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

        // build the DMX frame
        let mut dmx_frame = [0u8; 512];
        for (note, env) in &active_lights {
            if let Some(mapping) = note_mappings.get(note) {
                if let Some([r, g, b]) = mapping.color {
                    let r_val = (r as f32 * env.intensity) as u8;
                    let g_val = (g as f32 * env.intensity) as u8;
                    let b_val = (b as f32 * env.intensity) as u8;

                    if mapping.dmx_channel < 512 {
                        dmx_frame[mapping.dmx_channel] = r_val;
                    }
                    if mapping.dmx_channel + 1 < 512 {
                        dmx_frame[mapping.dmx_channel + 1] = g_val;
                    }
                    if mapping.dmx_channel + 2 < 512 {
                        dmx_frame[mapping.dmx_channel + 2] = b_val;
                    }
                } else if mapping.dmx_channel < 512 {
                    dmx_frame[mapping.dmx_channel] = env.dmx_value();
                }
            }
        }

        // output
        artnet_sink.send_state(&dmx_frame)?;

        let sleep_start = Instant::now();
        if sleep_start < next_deadline {
            sleeper.sleep(next_deadline.duration_since(sleep_start));
        } else {
            warn!(
                "Frame drop detected! Work took longer than {:?}",
                target_interval
            );
        }

        // advance the deadline for the next frame
        next_deadline = Instant::now().max(next_deadline) + target_interval;
    }

    perform_shutdown(&config, &mut artnet_sink, &initial_state)?;

    info!("PulsePlex shut down cleanly.");
    Ok(())
}

fn perform_shutdown(
    config: &PulsePlexConfig,
    sink: &mut dyn LightSink,
    initial_state: &[u8; 512],
) -> anyhow::Result<()> {
    let mut final_frame = [0u8; 512];

    match config.shutdown.mode {
        ShutdownMode::Blackout => {
            info!("Shutting down: Blackout");
            // final_frame is already all zeros
        }
        ShutdownMode::Default => {
            info!("Shutting down: Applying default scene");
            if let Some(defaults) = &config.shutdown.defaults {
                for (ch, val) in defaults {
                    if *ch < 512 {
                        final_frame[*ch] = *val;
                    }
                }
            }
        }
        ShutdownMode::Restore => {
            info!("Shutting down: Restoring previous state");
            final_frame.copy_from_slice(initial_state);
        }
    }

    // Send the final "Exit Frame"
    sink.send_state(&final_frame)?;
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_artnet_sink_creation() {
        let sink = ArtNetSink::new(1, "127.0.0.1:6454");
        assert!(sink.is_ok());

        let invalid_sink = ArtNetSink::new(1, "invalid_ip");
        assert!(invalid_sink.is_err());
    }

    #[test]
    fn test_artnet_sink_send_state() {
        let mut sink = ArtNetSink::new(1, "127.0.0.1:6454").unwrap();
        let state = [42u8; 512];
        let result = sink.send_state(&state);
        assert!(result.is_ok());
    }

    #[test]
    fn test_perform_shutdown_blackout() {
        use pulseplex_core::MockSink;

        let config = PulsePlexConfig {
            shutdown: crate::config::ShutdownConfig {
                mode: ShutdownMode::Blackout,
                defaults: None,
            },
            midi: crate::config::MidiConfig {
                device_name: "".to_string(),
            },
            artnet: crate::config::ArtNetConfig {
                target_ip: "".to_string(),
                universe: 0,
            },
            mapping: vec![],
        };
        let mut sink = MockSink::default();
        let initial_state = [255u8; 512];

        perform_shutdown(&config, &mut sink, &initial_state).unwrap();
        assert_eq!(sink.frames.len(), 1);
        assert_eq!(sink.frames[0][0], 0); // Blackout frame
    }

    #[test]
    fn test_perform_shutdown_restore() {
        use pulseplex_core::MockSink;

        let config = PulsePlexConfig {
            shutdown: crate::config::ShutdownConfig {
                mode: ShutdownMode::Restore,
                defaults: None,
            },
            midi: crate::config::MidiConfig {
                device_name: "".to_string(),
            },
            artnet: crate::config::ArtNetConfig {
                target_ip: "".to_string(),
                universe: 0,
            },
            mapping: vec![],
        };
        let mut sink = MockSink::default();
        let initial_state = [128u8; 512];

        perform_shutdown(&config, &mut sink, &initial_state).unwrap();
        assert_eq!(sink.frames.len(), 1);
        assert_eq!(sink.frames[0][0], 128); // Restore frame
    }

    #[test]
    fn test_perform_shutdown_default() {
        use pulseplex_core::MockSink;

        let mut defaults = HashMap::new();
        defaults.insert(5, 42);

        let config = PulsePlexConfig {
            midi: crate::config::MidiConfig {
                device_name: "".to_string(),
            },
            artnet: crate::config::ArtNetConfig {
                target_ip: "".to_string(),
                universe: 0,
            },
            mapping: vec![],
            shutdown: crate::config::ShutdownConfig {
                mode: ShutdownMode::Default,
                defaults: Some(defaults),
            },
        };
        let mut sink = MockSink::default();
        let initial_state = [255u8; 512];

        perform_shutdown(&config, &mut sink, &initial_state).unwrap();
        assert_eq!(sink.frames.len(), 1);
        assert_eq!(sink.frames[0][0], 0); // Default channel 0 is 0
        assert_eq!(sink.frames[0][5], 42); // Configured default
    }
}
