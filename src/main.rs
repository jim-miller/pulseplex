use std::{
    collections::HashMap,
    net::UdpSocket,
    time::{Duration, Instant},
};

use clap::Parser;
use pulseplex_core::{ArtNetBridge, DecayEnvelope};
use spin_sleep::SpinSleeper;
use tracing::{debug, info, trace, warn};
use tracing_subscriber::{fmt, prelude::*, EnvFilter};

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

    info!("Starting PulsePlex daemon");

    let target_hz = 40.0;
    let target_interval = Duration::from_secs_f64(0.5 / target_hz); // 25ms

    let socket = UdpSocket::bind("0.0.0.0:0")?;
    socket.set_broadcast(true)?;

    let target_addr = "255.255.255.255:6454";

    let mut artnet = ArtNetBridge::new(0); // Universe 0

    // Greater loop accuracy needed
    let sleeper = SpinSleeper::default();

    let mut last_tick = Instant::now();
    let mut next_deadline = last_tick + target_interval;

    let mut active_lights: HashMap<u8, DecayEnvelope> = HashMap::new();

    // Mock timber to artificially trigger a note so we can watch it decay
    let mut mock_trigger_time = Instant::now();

    loop {
        // calculate our time since last tick
        let now = Instant::now();
        let delta_time = now.duration_since(last_tick);
        last_tick = now;

        // mock some MIDI input
        if mock_trigger_time.elapsed() > Duration::from_secs(2) {
            debug!("Mock MIDI NoteOn: 36 (Kick Drum)");
            let mut env = DecayEnvelope::new(1.0);
            env.trigger(127);
            active_lights.insert(36, env);
            mock_trigger_time = Instant::now();
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
            // TODO: change to config-based
            let channel = (*note as usize).saturating_sub(36);
            artnet.set_channel(channel, env.dmx_value());
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
                "Frame drop detected! Work too longer than {:?}",
                target_interval
            );
        }

        // advance the deadline for the next frame
        // using `.max(Instant::now))` to prevent a catch-up stampede
        // if we lag too heavily, we just reset the clock and move on
        next_deadline = Instant::now().max(next_deadline) + target_interval;
    }
}
