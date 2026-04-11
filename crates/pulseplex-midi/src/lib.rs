use std::collections::HashMap;

use anyhow::{anyhow, Context, Result};
use crossbeam_channel::{unbounded, Receiver, TryRecvError};
use midir::{MidiInput, MidiInputConnection, MidiInputPort};
use pulseplex_core::{EventSource, Signal};
use tracing::info;

pub struct MidiReceiver {
    rx: Receiver<Signal>,
    // Hold onto the connection so it doesn't drop and kill the bg thread
    _conn: MidiInputConnection<()>,
}

impl EventSource for MidiReceiver {
    fn poll(&mut self, buffer: &mut Vec<Signal>) -> anyhow::Result<()> {
        loop {
            match self.rx.try_recv() {
                Ok(signal) => buffer.push(signal),
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => {
                    return Err(anyhow!("MIDI background thread disconnected"));
                }
            }
        }
        Ok(())
    }
}

/// Scans for available MIDI input devices and returns a list of their names.
/// This creates a temporary MIDI client that is dropped after querying.
pub fn list_midi_devices() -> Result<Vec<String>> {
    // We create a temporary client just for scanning
    let midi_in =
        MidiInput::new("PulsePlex Scanner").context("Failed to initialize MIDI scanner")?;

    let mut device_names = Vec::new();

    for port in midi_in.ports() {
        // Ignore ports that fail to resolve a name to prevent the app from crashing
        if let Ok(name) = midi_in.port_name(&port) {
            device_names.push(name);
        }
    }

    Ok(device_names)
}

/// Finds a MIDI input port by substring match.
pub fn find_midi_port(midi_in: &midir::MidiInput, target_name: &str) -> Option<MidiInputPort> {
    for port in midi_in.ports() {
        if let Ok(name) = midi_in.port_name(&port) {
            if name.contains(target_name) {
                return Some(port);
            }
        }
    }
    None
}

pub fn setup_midi(target_device: &str, id_map: HashMap<u8, usize>) -> anyhow::Result<MidiReceiver> {
    let mut midi_in = MidiInput::new("pulseplex-input")?;
    midi_in.ignore(midir::Ignore::None);

    let (tx, rx) = unbounded();

    let port = find_midi_port(&midi_in, target_device).ok_or_else(|| {
        anyhow::anyhow!("Could not find MIDI device containing: '{}'", target_device)
    })?;

    let port_name = midi_in.port_name(&port)?;
    info!("Binding to MIDI port: {}", port_name);

    let _conn = midi_in
        .connect(
            &port,
            "pulseplex-read",
            move |_stamp, message, _| {
                if message.len() >= 3 {
                    let status = message[0] & 0xF0;
                    let note = message[1];
                    let velocity = message[2];

                    if let Some(&internal_id) = id_map.get(&note) {
                        match status {
                            0x90 if velocity > 0 => {
                                let _ = tx.send(Signal::Trigger {
                                    id: internal_id,
                                    velocity,
                                });
                            }
                            0x80 | 0x90 => {
                                let _ = tx.send(Signal::Release { id: internal_id });
                            }
                            _ => {}
                        }
                    }
                }
            },
            (),
        )
        .map_err(|e| anyhow!("Failed to connect to MIDI port: {}", e))?;

    Ok(MidiReceiver { rx, _conn })
}
