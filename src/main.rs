mod config;

use std::{
    collections::HashMap,
    net::{ToSocketAddrs, UdpSocket},
    time::{Duration, Instant},
};

use pulseplex_core::{ArtNetBridge, DecayEnvelope};
use pulseplex_midi::{setup_midi, MidiSignal};

use clap::Parser;
use crossbeam_channel::TryRecvError;
use spin_sleep::SpinSleeper;
use tracing::{debug, info, trace, warn};
use tracing_subscriber::{fmt, prelude::*, EnvFilter};

use crate::config::{MappingConfig, PulsePlexConfig};

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

    let config = PulsePlexConfig::load("pulseplex.toml")?;
    info!(
        "Loaded configuration for MIDI device: {}",
        config.midi.device_name
    );

    let note_mappings: HashMap<u8, MappingConfig> =
        config.mapping.into_iter().map(|m| (m.note, m)).collect();

    let target_hz = 40.0; // 25ms
    let target_interval = Duration::from_secs_f64(1.0 / target_hz);

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

    let mut last_tick = Instant::now();
    let mut next_deadline = last_tick + target_interval;

    loop {
        // calculate our time since last tick
        let now = Instant::now();
        let delta_time = now.duration_since(last_tick);
        last_tick = now;

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
}
