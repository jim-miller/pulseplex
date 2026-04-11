use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{anyhow, Result};
use byteorder::{BigEndian, WriteBytesExt};
use crossbeam_channel::{bounded, Receiver, Sender, TryRecvError, TrySendError};
use pulseplex_core::{DecayEnvelope, LightSink};
use tokio::net::UdpSocket;
use tracing::{error, info, warn};
use webrtc_dtls::config::Config;
use webrtc_dtls::conn::DTLSConn;

/// Compiled mapping for a single Hue light.
#[derive(Clone, Debug)]
pub struct HueOutputMapping {
    pub internal_id: usize,
    pub light_id: u16,
    pub color: Option<[u8; 3]>,
}

pub struct HueSink {
    tx: Sender<Vec<f32>>,
    rx_drain: Receiver<Vec<f32>>,
    mappings: Vec<HueOutputMapping>,
}

impl HueSink {
    pub fn new(
        bridge_ip: String,
        username: String,
        client_key: String,
        area_id: String,
        mappings: Vec<HueOutputMapping>,
    ) -> Result<Self> {
        // Capacity 1: we only care about the latest frame.
        let (tx, rx) = bounded(1);
        let rx_drain = rx.clone();

        let background_mappings = mappings.clone();

        // Construct runtime before spawning to surface errors
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .map_err(|e| {
                anyhow!(
                    "Failed to create tokio runtime for Hue background thread: {}",
                    e
                )
            })?;

        // Spawn background thread for DTLS streaming
        std::thread::spawn(move || {
            rt.block_on(async {
                if let Err(e) = run_hue_background(
                    bridge_ip,
                    username,
                    client_key,
                    area_id,
                    background_mappings,
                    rx,
                )
                .await
                {
                    error!("Hue background thread failed: {}", e);
                }
            });
        });

        Ok(Self {
            tx,
            rx_drain,
            mappings,
        })
    }
}

impl LightSink for HueSink {
    fn send_state(&mut self, intensities: &HashMap<usize, DecayEnvelope>) -> anyhow::Result<()> {
        let mut buffer = Vec::with_capacity(self.mappings.len());
        for m in &self.mappings {
            let intensity = intensities
                .get(&m.internal_id)
                .map(|env| env.intensity)
                .unwrap_or(0.0);
            buffer.push(intensity);
        }

        // "Latest-wins" strategy: If the buffer is full, drop the stale frame and try again.
        match self.tx.try_send(buffer) {
            Ok(_) => {}
            Err(TrySendError::Full(latest)) => {
                // Drain the stale frame and replace it with the latest state.
                let _ = self.rx_drain.try_recv();
                let _ = self.tx.try_send(latest);
            }
            Err(TrySendError::Disconnected(_)) => {
                return Err(anyhow!("Hue background thread disconnected"));
            }
        }
        Ok(())
    }
}

async fn run_hue_background(
    bridge_ip: String,
    username: String,
    client_key: String,
    _area_id: String, // reserved for future use
    mappings: Vec<HueOutputMapping>,
    rx: Receiver<Vec<f32>>,
) -> Result<()> {
    info!("Starting Hue background thread for bridge {}", bridge_ip);

    let addr: SocketAddr = format!("{}:2100", bridge_ip).parse()?;

    let psk = hex::decode(client_key.replace("-", ""))?;
    let psk_id = username.as_bytes().to_vec();

    let config = Config {
        psk: Some(Arc::new(move |_| Ok(psk.clone()))),
        psk_identity_hint: Some(psk_id),
        ..Default::default()
    };

    loop {
        // Exit if the hot loop has dropped the sender.
        match rx.try_recv() {
            Err(TryRecvError::Disconnected) => {
                info!("Hue sender disconnected; exiting background task.");
                return Ok(());
            }
            Ok(i) => {
                // We accidentally drained a frame while checking for disconnection.
                // This is unlikely to happen at the start of the loop, but we should handle it.
                // Re-connect and then stream.
                info!(
                    "Connecting to Hue bridge at {} via DTLS (after pre-emptive drain)...",
                    addr
                );
                let socket = UdpSocket::bind("0.0.0.0:0").await?;
                socket.connect(addr).await?;
                let dtls_conn =
                    match DTLSConn::new(Arc::new(socket), config.clone(), true, None).await {
                        Ok(conn) => conn,
                        Err(e) => {
                            error!("DTLS handshake failed: {}. Retrying in 5s...", e);
                            tokio::time::sleep(Duration::from_secs(5)).await;
                            continue;
                        }
                    };
                // Fall into streaming logic with the frame we just popped.
                // Simplified: we'll just continue and wait for next frame in stream_to_hue.
                // But since we already have 'i', let's start streaming.
                if let Err(e) = stream_to_hue_starting_with(dtls_conn, &mappings, &rx, i).await {
                    warn!("Hue streaming error: {}. Reconnecting...", e);
                    tokio::time::sleep(Duration::from_secs(1)).await;
                    continue;
                }
                return Ok(());
            }
            Err(TryRecvError::Empty) => {}
        }

        info!("Connecting to Hue bridge at {} via DTLS...", addr);

        let socket = UdpSocket::bind("0.0.0.0:0").await?;
        socket.connect(addr).await?;

        let dtls_conn = match DTLSConn::new(Arc::new(socket), config.clone(), true, None).await {
            Ok(conn) => conn,
            Err(e) => {
                error!("DTLS handshake failed: {}. Retrying in 5s...", e);
                tokio::time::sleep(Duration::from_secs(5)).await;
                continue;
            }
        };

        info!("Hue DTLS connection established.");

        match stream_to_hue(dtls_conn, &mappings, &rx).await {
            Ok(_) => {
                info!("Hue background thread shutting down cleanly.");
                return Ok(());
            }
            Err(e) => {
                warn!("Hue streaming error: {}. Reconnecting...", e);
                tokio::time::sleep(Duration::from_secs(1)).await;
            }
        }
    }
}

async fn stream_to_hue(
    conn: DTLSConn,
    mappings: &[HueOutputMapping],
    rx: &Receiver<Vec<f32>>,
) -> Result<()> {
    let rx_clone = rx.clone();
    let intensities = match tokio::task::spawn_blocking(move || rx_clone.recv()).await {
        Ok(Ok(i)) => i,
        Ok(Err(_)) => return Ok(()),
        Err(e) => return Err(anyhow!("Hue receive task failed: {}", e)),
    };
    stream_to_hue_starting_with(conn, mappings, rx, intensities).await
}

async fn stream_to_hue_starting_with(
    conn: DTLSConn,
    mappings: &[HueOutputMapping],
    rx: &Receiver<Vec<f32>>,
    first_intensities: Vec<f32>,
) -> Result<()> {
    let mut sequence: u8 = 0;
    let mut current_intensities = first_intensities;

    loop {
        let buf = build_huestream_packet(&current_intensities, mappings, sequence)?;
        sequence = sequence.wrapping_add(1);

        if let Err(e) = conn.write(&buf, None).await {
            return Err(anyhow!("Failed to write to DTLS connection: {}", e));
        }

        // Wait for next frame without blocking the Tokio executor.
        let rx_clone = rx.clone();
        current_intensities = match tokio::task::spawn_blocking(move || rx_clone.recv()).await {
            Ok(Ok(i)) => i,
            Ok(Err(_)) => return Ok(()), // Channel closed
            Err(e) => return Err(anyhow!("Hue receive task failed: {}", e)),
        };
    }
}

pub fn build_huestream_packet(
    intensities: &[f32],
    mappings: &[HueOutputMapping],
    sequence: u8,
) -> Result<Vec<u8>> {
    let mut buf = Vec::with_capacity(16 + mappings.len() * 9);

    buf.extend_from_slice(b"HueStream");
    buf.push(0x01); // Version Major
    buf.push(0x00); // Version Minor
    buf.push(sequence);
    buf.extend_from_slice(&[0x00, 0x00]);
    buf.push(0x00); // Color Space RGB
    buf.push(0x00);

    let scale_rgb_channel = |channel: u8, intensity: f32| -> u16 {
        ((channel as f32 * 257.0) * intensity)
            .clamp(0.0, 65535.0)
            .round() as u16
    };
    let scale_intensity =
        |intensity: f32| -> u16 { (intensity * 65535.0).clamp(0.0, 65535.0).round() as u16 };

    for (idx, m) in mappings.iter().enumerate() {
        let intensity = intensities.get(idx).cloned().unwrap_or(0.0);

        let (r, g, b) = if let Some([rc, gc, bc]) = m.color {
            (
                scale_rgb_channel(rc, intensity),
                scale_rgb_channel(gc, intensity),
                scale_rgb_channel(bc, intensity),
            )
        } else {
            let val = scale_intensity(intensity);
            (val, val, val)
        };

        buf.push(0x00);
        buf.write_u16::<BigEndian>(m.light_id)?;
        buf.write_u16::<BigEndian>(r)?;
        buf.write_u16::<BigEndian>(g)?;
        buf.write_u16::<BigEndian>(b)?;
    }

    Ok(buf)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_huestream_packet_layout() {
        let mappings = vec![
            HueOutputMapping {
                internal_id: 1,
                light_id: 10,
                color: Some([255, 128, 0]),
            },
            HueOutputMapping {
                internal_id: 2,
                light_id: 11,
                color: None,
            },
        ];

        let intensities = vec![1.0, 0.5];
        let sequence = 42;

        let packet = build_huestream_packet(&intensities, &mappings, sequence).unwrap();

        assert_eq!(&packet[0..9], b"HueStream");
        assert_eq!(packet[11], sequence);

        // Light 1 (RGB)
        assert_eq!(packet[17], 0x00);
        assert_eq!(packet[18], 0x0A);
        assert_eq!(packet[19], 0xFF);
        assert_eq!(packet[20], 0xFF);

        // Light 2 (Grayscale)
        assert_eq!(packet[27], 0x0B);
        assert_eq!(packet[28], 0x80);
        assert_eq!(packet[29], 0x00);
    }
}
