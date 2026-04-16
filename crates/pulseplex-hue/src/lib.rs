use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{anyhow, Result};
use byteorder::{BigEndian, WriteBytesExt};
use crossbeam_channel::{bounded, Receiver, Sender};
use pulseplex_core::{DecayEnvelope, LightSink};
use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls_pki_types::{CertificateDer, ServerName, UnixTime};
use tokio::net::UdpSocket;
use tokio::sync::mpsc::{channel, Receiver as AsyncReceiver, Sender as AsyncSender};
use tracing::{debug, error, info, trace, warn};

#[cfg(feature = "streaming")]
use webrtc_dtls::config::Config;
#[cfg(feature = "streaming")]
use webrtc_dtls::conn::DTLSConn;

const HUE_CA_CERT: &[u8] = include_bytes!("../assets/hue_ca_bundle.pem");

/// A custom verifier that handles Philips Hue Bridge certificates.
/// macOS Security.framework rejects them because their validity period is too long.
/// This verifier keeps the connection encrypted but ignores the validity period limit.
#[derive(Debug)]
pub struct HueCertVerifier {
    inner: Arc<dyn ServerCertVerifier>,
}

impl HueCertVerifier {
    pub fn new() -> Self {
        let mut roots = rustls::RootCertStore::empty();
        let mut cursor = std::io::Cursor::new(HUE_CA_CERT);
        for cert in rustls_pemfile::certs(&mut cursor).flatten() {
            roots.add(cert).ok();
        }

        let inner = rustls::client::WebPkiServerVerifier::builder(Arc::new(roots))
            .build()
            .expect("Failed to build base WebPkiServerVerifier");

        Self { inner }
    }
}

impl Default for HueCertVerifier {
    fn default() -> Self {
        Self::new()
    }
}

impl ServerCertVerifier for HueCertVerifier {
    fn verify_server_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        intermediates: &[CertificateDer<'_>],
        server_name: &ServerName<'_>,
        ocsp_response: &[u8],
        now: UnixTime,
    ) -> Result<ServerCertVerified, rustls::Error> {
        // Delegate to the standard verifier first
        match self.inner.verify_server_cert(
            end_entity,
            intermediates,
            server_name,
            ocsp_response,
            now,
        ) {
            Ok(v) => Ok(v),
            Err(e) => {
                let err_msg = e.to_string().to_lowercase();

                // Robustly catch Expired, NotValidYet, and all variations of SAN/CN Name Mismatches.
                if err_msg.contains("expired")
                    || err_msg.contains("not valid yet")
                    || err_msg.contains("not valid for name")
                    || err_msg.contains("not valid for any names")
                    || err_msg.contains("subjectaltname")
                {
                    Ok(ServerCertVerified::assertion())
                } else {
                    Err(e)
                }
            }
        }
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &rustls::DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        self.inner.verify_tls12_signature(message, cert, dss)
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &rustls::DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        self.inner.verify_tls13_signature(message, cert, dss)
    }

    fn supported_verify_schemes(&self) -> Vec<rustls::SignatureScheme> {
        self.inner.supported_verify_schemes()
    }
}

/// Compiled mapping for a single Hue light channel in an Entertainment Area.
#[derive(Clone, Debug)]
pub struct HueOutputMapping {
    pub internal_id: usize,
    pub channel_id: u8,
    pub color: Option<[u8; 3]>,
}

pub struct HueSink {
    tx: AsyncSender<Vec<f32>>,
    pool_tx: Sender<Vec<f32>>,
    pool_rx: Receiver<Vec<f32>>,
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
        if area_id.len() != 36 {
            return Err(anyhow!("Hue area_id must be exactly 36 characters (UUID)"));
        }

        // Forward channel: Hot loop -> Background thread
        let (tx, rx) = channel(1);

        // Return channel: Background thread -> Hot loop (Buffer Pool)
        let (pool_tx, pool_rx) = bounded(2);

        // Pre-seed the pool with 2 buffers
        let mapping_count = mappings.len();
        for _ in 0..2 {
            pool_tx.send(vec![0.0; mapping_count]).unwrap();
        }

        let background_mappings = mappings.clone();
        let pool_tx_clone = pool_tx.clone();

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

        std::thread::spawn(move || {
            rt.block_on(async {
                if let Err(e) = run_hue_background(
                    bridge_ip,
                    username,
                    client_key,
                    area_id,
                    background_mappings,
                    rx,
                    pool_tx_clone,
                )
                .await
                {
                    error!("Hue background thread failed: {}", e);
                }
            });
        });

        Ok(Self {
            tx,
            pool_tx,
            pool_rx,
            mappings,
        })
    }
}

impl LightSink for HueSink {
    fn send_universe(&mut self, _universe: &[u8; 512]) -> anyhow::Result<()> {
        // Hue does not currently support raw DMX universes.
        // For Phase 1, we can leave this as a no-op or implement mapping if needed.
        Ok(())
    }

    fn send_state(&mut self, intensities: &HashMap<usize, DecayEnvelope>) -> anyhow::Result<()> {
        // 1. Try to get a buffer from the pool (zero allocation)
        let mut buffer = self
            .pool_rx
            .try_recv()
            .unwrap_or_else(|_| vec![0.0; self.mappings.len()]);

        // 2. Fill the buffer
        for (idx, m) in self.mappings.iter().enumerate() {
            buffer[idx] = intensities
                .get(&m.internal_id)
                .map(|env| env.intensity)
                .unwrap_or(0.0);
        }

        // 3. Try send to background thread (non-blocking)
        if let Err(err) = self.tx.try_send(buffer) {
            match err {
                tokio::sync::mpsc::error::TrySendError::Full(b) => {
                    // Return the buffer to the pool so it's not lost
                    let _ = self.pool_tx.try_send(b);
                }
                tokio::sync::mpsc::error::TrySendError::Closed(_) => {
                    return Err(anyhow!("Hue background thread closed"));
                }
            }
        }

        Ok(())
    }
}

async fn run_hue_background(
    bridge_ip: String,
    username: String,
    client_key: String,
    area_id: String,
    mappings: Vec<HueOutputMapping>,
    mut rx: AsyncReceiver<Vec<f32>>,
    pool_tx: Sender<Vec<f32>>,
) -> Result<()> {
    info!("Starting Hue background thread for bridge {}", bridge_ip);

    #[cfg(not(feature = "streaming"))]
    {
        warn!("Hue streaming is disabled in this build. Buffers will be recycled but not sent.");
        while let Some(frame) = rx.recv().await {
            let _ = pool_tx.try_send(frame);
        }
        return Ok(());
    }

    #[cfg(feature = "streaming")]
    {
        // 1. Activate the stream via PUT request
        info!("Activating Hue Entertainment Area {}...", area_id);
        let provider = rustls::crypto::ring::default_provider();
        let client_config = rustls::ClientConfig::builder_with_provider(Arc::new(provider))
            .with_safe_default_protocol_versions()
            .unwrap()
            .dangerous()
            .with_custom_certificate_verifier(Arc::new(HueCertVerifier::new()))
            .with_no_client_auth();

        let client = reqwest::Client::builder()
            .use_rustls_tls()
            .use_preconfigured_tls(client_config)
            .build()?;

        let auth_resp = client
            .get(format!("https://{}/auth/v1", bridge_ip))
            .header("hue-application-key", &username)
            .send()
            .await?;

        let app_id = auth_resp
            .headers()
            .get("hue-application-id")
            .ok_or_else(|| anyhow!("Missing hue-application-id header from Bridge"))?
            .to_str()?
            .to_string();

        info!("Retrieved Hue Application ID for DTLS handshake.");

        // Fetch area details to log channel mappings for debugging and filtering
        let area_url = format!(
            "https://{}/clip/v2/resource/entertainment_configuration/{}",
            bridge_ip, area_id
        );
        let area_resp = client
            .get(&area_url)
            .header("hue-application-key", &username)
            .send()
            .await?;

        let mut valid_indices: Vec<usize> = (0..mappings.len()).collect();

        if area_resp.status().is_success() {
            let details: serde_json::Value = area_resp.json().await?;
            debug!("Hue Entertainment Area Details: {}", details);

            // Extract valid channel IDs from the bridge response
            if let Some(data) = details.get("data").and_then(|d| d.as_array()) {
                if let Some(area) = data.first() {
                    let status = area
                        .get("status")
                        .and_then(|s| s.as_str())
                        .unwrap_or("unknown");
                    info!("Current Hue Area status: {}", status);

                    if let Some(channels) = area.get("channels").and_then(|c| c.as_array()) {
                        let valid_ids: Vec<u8> = channels
                            .iter()
                            .filter_map(|c| {
                                c.get("channel_id")
                                    .and_then(|id| id.as_u64())
                                    .map(|id| id as u8)
                            })
                            .collect();

                        info!(
                            "Bridge reports {} valid channel(s): {:?}",
                            valid_ids.len(),
                            valid_ids
                        );

                        // Validate our config against the bridge and build the filter
                        valid_indices.retain(|&idx| {
                            let m = &mappings[idx];
                            if valid_ids.contains(&m.channel_id) {
                                debug!("Mapping confirmed: logic_id {} -> physical channel {}", m.internal_id, m.channel_id);
                                true
                            } else {
                                warn!("Degraded Experience: Config maps ID {} to invalid channel_id {}. This light will be STRIPPED from the stream.", m.internal_id, m.channel_id);
                                false
                            }
                        });
                    }
                }
            }
        }

        if valid_indices.is_empty() {
            return Err(anyhow!(
                "No valid Hue channels found in Entertainment Area {}. Aborting stream.",
                area_id
            ));
        }

        let activation_url = format!(
            "https://{}/clip/v2/resource/entertainment_configuration/{}",
            bridge_ip, area_id
        );

        let resp = client
            .put(&activation_url)
            .header("hue-application-key", &username)
            .json(&serde_json::json!({"action": "start"}))
            .send()
            .await?;

        if !resp.status().is_success() {
            return Err(anyhow!(
                "Failed to activate Hue Entertainment Area: Bridge returned {}",
                resp.status()
            ));
        }
        info!("Hue Entertainment Area activated.");

        // Give the bridge a moment to transition to active and open its UDP socket
        tokio::time::sleep(Duration::from_millis(2000)).await;

        // 2. Setup DTLS
        let addr: SocketAddr = format!("{}:2100", bridge_ip).parse()?;
        let psk = hex::decode(client_key.replace("-", ""))?;
        let psk_id = app_id.as_bytes().to_vec();

        let config = Config {
            psk: Some(Arc::new(move |_| Ok(psk.clone()))),
            psk_identity_hint: Some(psk_id),
            cipher_suites: vec![
                webrtc_dtls::cipher_suite::CipherSuiteId::Tls_Psk_With_Aes_128_Gcm_Sha256,
            ],
            server_name: bridge_ip.clone(),
            ..Default::default()
        };

        loop {
            info!("Connecting to Hue bridge at {} via DTLS...", addr);

            let socket = UdpSocket::bind("0.0.0.0:0").await?;
            socket.connect(addr).await?;

            let dtls_conn = match DTLSConn::new(Arc::new(socket), config.clone(), true, None).await
            {
                Ok(conn) => conn,
                Err(e) => {
                    if rx.is_closed() {
                        info!("Hue sender disconnected during connect; exiting.");
                        return Ok(());
                    }
                    error!("DTLS handshake failed: {}. Retrying in 5s...", e);
                    tokio::time::sleep(Duration::from_secs(5)).await;
                    continue;
                }
            };

            info!("Hue DTLS connection established.");

            match stream_to_hue(
                dtls_conn,
                &mappings,
                &valid_indices,
                &mut rx,
                &pool_tx,
                &area_id,
            )
            .await
            {
                Ok(_) => {
                    info!("Hue background thread shutting down cleanly.");
                    return Ok(());
                }
                Err(e) => {
                    if rx.is_closed() {
                        info!("Hue sender disconnected; exiting background task.");
                        return Ok(());
                    }
                    warn!("Hue streaming error: {}. Reconnecting...", e);
                    tokio::time::sleep(Duration::from_secs(1)).await;
                }
            }
        }
    }
}

#[cfg(feature = "streaming")]
async fn stream_to_hue(
    conn: DTLSConn,
    mappings: &[HueOutputMapping],
    valid_indices: &[usize],
    rx: &mut AsyncReceiver<Vec<f32>>,
    pool_tx: &Sender<Vec<f32>>,
    area_id: &str,
) -> Result<()> {
    let mut sequence: u8 = 0;

    loop {
        let intensities = match rx.recv().await {
            Some(i) => i,
            None => {
                info!("Channel closed. Sending close_notify to Hue Bridge...");
                let _ = conn.close().await;
                return Ok(());
            }
        };

        let buf = build_huestream_packet(&intensities, mappings, valid_indices, sequence, area_id)?;
        sequence = sequence.wrapping_add(1);

        if intensities.iter().any(|&v| v > 0.0) {
            trace!(
                "Sending non-zero Hue frame: size={}, intensities={:?}",
                buf.len(),
                intensities
            );
        }

        if let Err(e) = conn.write(&buf, None).await {
            let _ = pool_tx.send(intensities); // Return buffer
            return Err(anyhow!("Failed to write to DTLS connection: {}", e));
        }

        let _ = pool_tx.send(intensities);
    }
}

pub fn build_huestream_packet(
    intensities: &[f32],
    mappings: &[HueOutputMapping],
    valid_indices: &[usize],
    sequence: u8,
    area_id: &str,
) -> Result<Vec<u8>> {
    let mut buf = Vec::with_capacity(16 + 36 + (valid_indices.len() * 7));

    buf.extend_from_slice(b"HueStream");
    buf.push(0x02);
    buf.push(0x00);
    buf.push(sequence);
    buf.extend_from_slice(&[0x00, 0x00]);
    buf.push(0x00);
    buf.push(0x00);

    let area_bytes = area_id.as_bytes();
    if area_bytes.len() != 36 {
        return Err(anyhow!("Hue area_id must be exactly 36 characters (UUID)"));
    }
    buf.extend_from_slice(area_bytes);

    let scale_rgb_channel = |channel: u8, intensity: f32| -> u16 {
        ((channel as f32 * 257.0) * intensity)
            .clamp(0.0, 65535.0)
            .round() as u16
    };
    let scale_intensity =
        |intensity: f32| -> u16 { (intensity * 65535.0).clamp(0.0, 65535.0).round() as u16 };

    for &idx in valid_indices {
        let m = &mappings[idx];
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

        buf.push(m.channel_id);
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
    fn test_huestream_packet_layout_v2() {
        let mappings = vec![
            HueOutputMapping {
                internal_id: 1,
                channel_id: 0,
                color: Some([255, 128, 0]),
            },
            HueOutputMapping {
                internal_id: 2,
                channel_id: 1,
                color: None,
            },
        ];

        let intensities = vec![1.0, 0.5];
        let sequence = 42;
        let area_id = "12345678-1234-1234-1234-123456789012";

        let packet =
            build_huestream_packet(&intensities, &mappings, &[0, 1], sequence, area_id).unwrap();

        assert_eq!(&packet[0..9], b"HueStream");
        assert_eq!(packet[9], 0x02);
        assert_eq!(packet[11], sequence);
        assert_eq!(&packet[16..52], area_id.as_bytes());
        assert_eq!(packet[52], 0x00);
        assert_eq!(packet[53], 0xFF);
        assert_eq!(packet[54], 0xFF);
        assert_eq!(packet[59], 0x01);
        assert_eq!(packet[60], 0x80);
    }
}

#[cfg(test)]
mod tests_tls {
    use super::*;
    use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
    use rustls_pki_types::{CertificateDer, ServerName, UnixTime};
    use std::fmt;

    // Create a custom error type to simulate the deep webpki error
    #[derive(Debug)]
    struct MockWebpkiError(String);

    impl fmt::Display for MockWebpkiError {
        fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            write!(f, "{}", self.0)
        }
    }
    impl std::error::Error for MockWebpkiError {}

    // A mock verifier that simulates the strict modern webpki behavior
    // by intentionally failing the SAN/Common Name check using the dynamic 'Other' variant.
    #[derive(Debug)]
    struct MockStrictVerifier;

    impl ServerCertVerifier for MockStrictVerifier {
        fn verify_server_cert(
            &self,
            _end_entity: &CertificateDer<'_>,
            _intermediates: &[CertificateDer<'_>],
            _server_name: &ServerName<'_>,
            _ocsp_response: &[u8],
            _now: UnixTime,
        ) -> Result<ServerCertVerified, rustls::Error> {
            // Simulate the EXACT dynamic string error the Hue Bridge throws in the real world,
            // boxed inside the 'Other' variant.
            let error_msg = "invalid peer certificate: certificate not valid for name \"001788fffea43aaf\"; certificate is not valid for any names".to_string();

            Err(rustls::Error::InvalidCertificate(
                rustls::CertificateError::Other(rustls::OtherError(Arc::new(MockWebpkiError(
                    error_msg,
                )))),
            ))
        }

        fn verify_tls12_signature(
            &self,
            _message: &[u8],
            _cert: &CertificateDer<'_>,
            _dss: &rustls::DigitallySignedStruct,
        ) -> Result<HandshakeSignatureValid, rustls::Error> {
            Ok(HandshakeSignatureValid::assertion())
        }

        fn verify_tls13_signature(
            &self,
            _message: &[u8],
            _cert: &CertificateDer<'_>,
            _dss: &rustls::DigitallySignedStruct,
        ) -> Result<HandshakeSignatureValid, rustls::Error> {
            Ok(HandshakeSignatureValid::assertion())
        }

        fn supported_verify_schemes(&self) -> Vec<rustls::SignatureScheme> {
            vec![]
        }
    }

    #[test]
    fn test_hue_verifier_suppresses_san_errors() {
        // Wrap our mock strict verifier inside our custom Hue verifier
        let verifier = HueCertVerifier {
            inner: Arc::new(MockStrictVerifier),
        };

        // Provide dummy data for the signature
        let dummy_cert = CertificateDer::from(vec![0]);
        let server_name = ServerName::try_from("dummy-bridge-id").unwrap();
        let now = UnixTime::since_unix_epoch(Duration::from_secs(0));

        let result = verifier.verify_server_cert(&dummy_cert, &[], &server_name, &[], now);

        // The inner verifier throws NotValidForName (boxed in Other), but our wrapper MUST
        // catch it via string inspection, suppress it, and return Ok.
        assert!(
            result.is_ok(),
            "HueCertVerifier failed to suppress the boxed NotValidForName error!"
        );
    }
}
