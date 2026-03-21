use anyhow::{anyhow, bail};
use crossbeam_channel::{unbounded, Receiver};
use midir::{MidiInput, MidiInputConnection};
use tracing::{info, trace};

pub enum MidiSignal {
    NoteOn { note: u8, velocity: u8 },
    NoteOff { note: u8 },
}

pub struct MidiReceiver {
    pub rx: Receiver<MidiSignal>,
    // Hold onto the connection so it doesn't drop and kill the bg thread
    _conn: MidiInputConnection<()>,
}

pub fn setup_midi(target_device: &str) -> anyhow::Result<MidiReceiver> {
    let mut midi_in = MidiInput::new("pulseplex-input")?;
    midi_in.ignore(midir::Ignore::None);

    let (tx, rx) = unbounded();

    let ports = midi_in.ports();
    if ports.is_empty() {
        bail!("No physical MIDI ports found. Please connect your module.")
    }

    // Search for preferred device
    let mut selected_port = None;
    trace!("Looking for MIDI device: {}", target_device);
    for p in ports.iter() {
        if let Ok(name) = midi_in.port_name(p) {
            trace!("Found: {}", name);
            if name.contains(target_device) {
                trace!("Success!");
                selected_port = Some(p.clone());
                break;
            }
        }
    }

    let port = selected_port.ok_or_else(|| {
        anyhow::anyhow!(
            "Could not find MIDI device containing string: '{}'",
            target_device
        )
    })?;

    let port_name = midi_in.port_name(&port)?;
    info!("Binding to MIDI port: {}", port_name);

    let conn = midi_in
        .connect(
            &port,
            "pulsepluex-read",
            move |_stamp, message, _| {
                if message.len() >= 3 {
                    let status = message[0] & 0xF0;
                    let note = message[1];
                    let velocity = message[2];

                    match status {
                        0x90 if velocity > 0 => {
                            let _ = tx.send(MidiSignal::NoteOn { note, velocity });
                        }
                        0x80 | 0x90 => {
                            let _ = tx.send(MidiSignal::NoteOff { note });
                        }
                        _ => {}
                    }
                }
            },
            (),
        )
        .map_err(|e| anyhow!("Failed to connect to MIDI port: {}", e))?;

    trace!("MIDI successfully initialized...");
    Ok(MidiReceiver { rx, _conn: conn })
}
