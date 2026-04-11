use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{anyhow, Result};
use byteorder::{BigEndian, WriteBytesExt};
use crossbeam_channel::{bounded, Receiver, Sender};
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
        // If the background thread is busy, we drop the current frame to keep the hot loop fast.
        let (tx, rx) = bounded(1);

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

        Ok(Self { tx, mappings })
    }
}

impl LightSink for HueSink {
    fn send_state(&mut self, intensities: &HashMap<usize, DecayEnvelope>) -> anyhow::Result<()> {
        // Zero-allocation aligned buffer for the channel
        let mut buffer = Vec::with_capacity(self.mappings.len());
        for m in &self.mappings {
            let intensity = intensities
                .get(&m.internal_id)
                .map(|env| env.intensity)
                .unwrap_or(0.0);
            buffer.push(intensity);
        }

        // try_send to avoid blocking the hot loop. capacity is 1, so we drop if full.
        let _ = self.tx.try_send(buffer);
        Ok(())
    }
}

async fn run_hue_background(
    bridge_ip: String,
    username: String,
    client_key: String,
    _area_id: String, // Note: area_id is usually used in the HTTPS setup, streaming is bridge-wide/PSK bound
    mappings: Vec<HueOutputMapping>,
    rx: Receiver<Vec<f32>>,
) -> Result<()> {
    info!("Starting Hue background thread for bridge {}", bridge_ip);

    let addr: SocketAddr = format!("{}:2100", bridge_ip).parse()?;

    // Convert client_key (hex string) to bytes
    let psk = hex::decode(client_key.replace("-", ""))?;
    let psk_id = username.as_bytes().to_vec();

    let config = Config {
        psk: Some(Arc::new(move |_| Ok(psk.clone()))),
        psk_identity_hint: Some(psk_id),
        ..Default::default()
    };

    loop {
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
                info!("Hue background thread shutting down (channel closed).");
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
    let mut sequence: u8 = 0;

    loop {
        // Wait for next frame from the hot loop
        let intensities = match rx.recv() {
            Ok(i) => i,
            Err(_) => return Ok(()), // Channel closed, shutdown thread
        };

        let buf = build_huestream_packet(&intensities, mappings, sequence)?;
        sequence = sequence.wrapping_add(1);

        // Send over DTLS
        if let Err(e) = conn.write(&buf, None).await {
            return Err(anyhow!("Failed to write to DTLS connection: {}", e));
        }
    }
}

/// Pure helper to build HueStream binary packets for testing.
pub fn build_huestream_packet(
    intensities: &[f32],
    mappings: &[HueOutputMapping],
    sequence: u8,
) -> Result<Vec<u8>> {
    let mut buf = Vec::with_capacity(16 + mappings.len() * 9);

    // Header
    buf.extend_from_slice(b"HueStream");
    buf.push(0x01); // Version Major
    buf.push(0x00); // Version Minor
    buf.push(sequence);
    buf.extend_from_slice(&[0x00, 0x00]); // Reserved
    buf.push(0x00); // Color Space RGB
    buf.push(0x00); // Reserved

    // Lights
    for (idx, m) in mappings.iter().enumerate() {
        let intensity = intensities.get(idx).cloned().unwrap_or(0.0);

        let (r, g, b) = if let Some([rc, gc, bc]) = m.color {
            (
                (rc as f32 * intensity) as u16 * 257,
                (gc as f32 * intensity) as u16 * 257,
                (bc as f32 * intensity) as u16 * 257,
            )
        } else {
            let val = (intensity * 65535.0) as u16;
            (val, val, val)
        };

        buf.push(0x00); // Type Light
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
                color: None, // Grayscale
            },
        ];

        let intensities = vec![1.0, 0.5];
        let sequence = 42;

        let packet = build_huestream_packet(&intensities, &mappings, sequence).unwrap();

        // Header
        assert_eq!(&packet[0..9], b"HueStream");
        assert_eq!(packet[9], 0x01); // Version Major
        assert_eq!(packet[11], sequence);

        // Light 1 (RGB)
        // Offset 16: Type (0x00)
        // Offset 17-18: ID (10 -> 0x000A)
        // Offset 19-20: R (255 * 1.0 * 257 = 65535 -> 0xFFFF)
        assert_eq!(packet[16], 0x00);
        assert_eq!(packet[17], 0x00);
        assert_eq!(packet[18], 0x0A);
        assert_eq!(packet[19], 0xFF);
        assert_eq!(packet[20], 0xFF);

        // Light 2 (Grayscale)
        // Offset 16 + 9 = 25
        // ID 11 -> 0x000B
        // Value 0.5 * 65535 = 32767 -> 0x7FFF
        assert_eq!(packet[25], 0x00);
        assert_eq!(packet[26], 0x00);
        assert_eq!(packet[27], 0x0B);
        assert_eq!(packet[28], 0x7F);
        assert_eq!(packet[29], 0xFF);
    }
}
