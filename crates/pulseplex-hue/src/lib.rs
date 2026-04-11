use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{anyhow, Result};
use byteorder::{BigEndian, WriteBytesExt};
use crossbeam_channel::{unbounded, Receiver, Sender};
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
    tx: Sender<HashMap<usize, f32>>,
}

impl HueSink {
    pub fn new(
        bridge_ip: String,
        username: String,
        client_key: String,
        mappings: Vec<HueOutputMapping>,
    ) -> Result<Self> {
        let (tx, rx) = unbounded();

        // Spawn background thread for DTLS streaming
        std::thread::spawn(move || {
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("Failed to create tokio runtime for Hue background thread");

            rt.block_on(async {
                if let Err(e) =
                    run_hue_background(bridge_ip, username, client_key, mappings, rx).await
                {
                    error!("Hue background thread failed: {}", e);
                }
            });
        });

        Ok(Self { tx })
    }
}

impl LightSink for HueSink {
    fn send_state(&mut self, intensities: &HashMap<usize, DecayEnvelope>) -> anyhow::Result<()> {
        // We only send the raw intensities to the background thread to keep the hot loop fast.
        let mut simplified = HashMap::with_capacity(intensities.len());
        for (&id, env) in intensities {
            simplified.insert(id, env.intensity);
        }

        // try_send to avoid blocking the hot loop. If the background thread is busy, we drop the frame.
        let _ = self.tx.try_send(simplified);
        Ok(())
    }
}

async fn run_hue_background(
    bridge_ip: String,
    username: String,
    client_key: String,
    mappings: Vec<HueOutputMapping>,
    rx: Receiver<HashMap<usize, f32>>,
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

        // webrtc-dtls doesn't have a simple "connect" for client side PSK easily?
        // Actually, it does.
        let dtls_conn = match DTLSConn::new(Arc::new(socket), config.clone(), true, None).await {
            Ok(conn) => conn,
            Err(e) => {
                error!("DTLS handshake failed: {}. Retrying in 5s...", e);
                tokio::time::sleep(Duration::from_secs(5)).await;
                continue;
            }
        };

        info!("Hue DTLS connection established.");

        if let Err(e) = stream_to_hue(dtls_conn, &mappings, &rx).await {
            warn!("Hue streaming error: {}. Reconnecting...", e);
            tokio::time::sleep(Duration::from_secs(1)).await;
        }
    }
}

async fn stream_to_hue(
    conn: DTLSConn,
    mappings: &[HueOutputMapping],
    rx: &Receiver<HashMap<usize, f32>>,
) -> Result<()> {
    let mut sequence: u8 = 0;

    loop {
        // Wait for next frame from the hot loop
        // We use recv() here because this is a dedicated background thread.
        let intensities = match rx.recv() {
            Ok(i) => i,
            Err(_) => return Ok(()), // Channel closed
        };

        // Format HueStream packet
        let mut buf = Vec::with_capacity(16 + mappings.len() * 9);

        // Header
        buf.extend_from_slice(b"HueStream");
        buf.push(0x01); // Version Major
        buf.push(0x00); // Version Minor
        buf.push(sequence);
        buf.extend_from_slice(&[0x00, 0x00]); // Reserved
        buf.push(0x00); // Color Space RGB
        buf.push(0x00); // Reserved

        sequence = sequence.wrapping_add(1);

        // Lights
        for m in mappings {
            let intensity = intensities.get(&m.internal_id).cloned().unwrap_or(0.0);

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

        // Send over DTLS
        if let Err(e) = conn.write(&buf, None).await {
            return Err(anyhow!("Failed to write to DTLS connection: {}", e));
        }
    }
}
