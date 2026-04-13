use std::net::IpAddr;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{anyhow, Context, Result};
use dialoguer::theme::ColorfulTheme;
use dialoguer::{Input, Select};
use directories::ProjectDirs;
use mdns_sd::{ServiceDaemon, ServiceEvent};
use reqwest::Client;
use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls_pki_types::{CertificateDer, ServerName, UnixTime};
use serde::Deserialize;
use serde_json::json;
use tokio::runtime::Builder;

const HUE_CA_CERT: &[u8] = include_bytes!("../assets/hue_ca_bundle.pem");
const DEFAULT_CONFIG_TEMPLATE: &str = include_str!("../assets/default_edrums.toml");

#[derive(Debug, Deserialize)]
struct DiscoveryResponse {
    id: String,
    internalipaddress: String,
}

#[derive(Debug, Deserialize)]
struct HueAuthResponse {
    success: Option<HueAuthSuccess>,
    error: Option<HueAuthError>,
}

#[derive(Debug, Deserialize)]
struct HueAuthSuccess {
    username: String,
    clientkey: String,
}

#[derive(Debug, Deserialize)]
struct HueAuthError {
    description: String,
}

/// A custom verifier that handles Philips Hue Bridge certificates.
/// macOS Security.framework rejects them because their validity period is too long.
/// This verifier keeps the connection encrypted but ignores the validity period limit.
#[derive(Debug)]
struct HueCertVerifier {
    inner: Arc<dyn ServerCertVerifier>,
}

impl HueCertVerifier {
    fn new() -> Self {
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
                //
                // SECURITY NOTE: While we ignore name mismatches, we STILL validate the certificate
                // signature against the Hue Root CA (self.inner does this). This ensures we only
                // talk to authentic Philips Hue hardware. We must ignore name mismatches because
                // Hue Bridges list their ID in the Common Name (CN) field but lack Subject
                // Alternative Name (SAN) extensions, which modern webpki/rustls strictly requires.
                if err_msg.contains("expired")
                    || err_msg.contains("not valid yet")
                    || err_msg.contains("not valid for name")
                    || err_msg.contains("not valid for any names")
                    || err_msg.contains("subjectaltname")
                {
                    Ok(ServerCertVerified::assertion())
                } else {
                    // It's a true cryptographic failure (e.g., bad signature, wrong CA). Reject it!
                    Err(e)
                }
            }
        }
    }

    // Safely delegate all actual cryptographic math back to rustls!
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

pub fn run_wizard() -> Result<PathBuf> {
    println!("Welcome to the PulsePlex Setup Wizard!");
    println!("We'll help you find your Philips Hue Bridge and configure your MIDI device.\n");

    let rt = Builder::new_current_thread().enable_all().build()?;

    let (bridge_ip, bridge_id) = rt.block_on(discover_bridge())?;
    println!("Found Hue Bridge: {} ({})", bridge_id, bridge_ip);

    let (username, client_key) = rt.block_on(perform_push_link(&bridge_ip, &bridge_id))?;
    println!("Successfully linked with Bridge!\n");

    let area_id = rt.block_on(select_entertainment_area(&bridge_ip, &bridge_id, &username))?;

    let midi_devices = pulseplex_midi::list_midi_devices()?;
    if midi_devices.is_empty() {
        return Err(anyhow!(
            "No MIDI devices found. Please connect a device and try again."
        ));
    }

    let selection = Select::with_theme(&ColorfulTheme::default())
        .with_prompt("Select your MIDI device")
        .items(&midi_devices)
        .default(0)
        .interact()?;
    let midi_device = &midi_devices[selection];

    let config_content = DEFAULT_CONFIG_TEMPLATE
        .replace("{midi_device}", midi_device)
        .replace("{bridge_ip}", &bridge_ip.to_string())
        .replace("{username}", &username)
        .replace("{client_key}", &client_key)
        .replace("{area_id}", &area_id);

    let proj_dirs = ProjectDirs::from("org", "pulseplex", "pulseplex")
        .ok_or_else(|| anyhow!("Could not determine configuration directory"))?;
    let config_dir = proj_dirs.config_dir();
    std::fs::create_dir_all(config_dir)?;

    let config_path = config_dir.join("pulseplex.toml");
    std::fs::write(&config_path, config_content)?;

    println!("\nConfiguration saved to: {:?}", config_path);
    println!("Setup complete!\n");

    Ok(config_path)
}

async fn discover_bridge() -> Result<(IpAddr, String)> {
    println!("Step 1: Discovering Hue Bridge...");

    // 1. mDNS Discovery
    if let Ok(mdns) = ServiceDaemon::new() {
        let receiver = mdns.browse("_hue._tcp.local.")?;
        let now = std::time::Instant::now();
        while now.elapsed() < Duration::from_secs(3) {
            if let Ok(ServiceEvent::ServiceResolved(info)) =
                receiver.recv_timeout(Duration::from_millis(500))
            {
                // Prefer IPv4 to avoid link-local IPv6 routing issues
                let addresses = info.get_addresses();
                let ip = addresses
                    .iter()
                    .find(|ip| ip.is_ipv4())
                    .or_else(|| addresses.iter().next())
                    .ok_or_else(|| anyhow!("No IP found for mDNS service"))?;
                let ip = *ip;

                let bridge_id = match info.get_property_val("bridgeid") {
                    Some(Some(id_bytes)) => String::from_utf8_lossy(id_bytes).to_lowercase(),
                    _ => info
                        .get_fullname()
                        .split('.')
                        .next()
                        .unwrap_or("")
                        .replace(' ', "-")
                        .to_lowercase(),
                };
                return Ok((ip, bridge_id));
            }
        }
    }

    // 2. N-UPnP Fallback
    println!("mDNS failed, trying meethue.com discovery...");
    let client = Client::new();
    let resp = client
        .get("https://discovery.meethue.com/")
        .timeout(Duration::from_secs(5))
        .send()
        .await?;

    if resp.status() == 429 {
        println!("Too many requests to meethue.com.");
    } else if let Ok(bridges) = resp.json::<Vec<DiscoveryResponse>>().await {
        if let Some(bridge) = bridges.first() {
            let ip: IpAddr = bridge.internalipaddress.parse()?;
            return Ok((ip, bridge.id.to_lowercase()));
        }
    }

    // 3. Manual Entry Failsafe
    println!("Could not find Bridge automatically.");
    let manual_ip: String = Input::with_theme(&ColorfulTheme::default())
        .with_prompt("Enter Hue Bridge IP manually")
        .interact_text()?;
    let ip: IpAddr = manual_ip.parse()?;

    let manual_id: String = Input::with_theme(&ColorfulTheme::default())
        .with_prompt("Enter Hue Bridge ID (found on the bottom of the Bridge)")
        .interact_text()?;

    Ok((ip, manual_id.to_lowercase()))
}

pub fn build_hue_client(bridge_ip: &std::net::IpAddr, bridge_id: &str) -> Result<Client> {
    let client_config = rustls::ClientConfig::builder()
        .dangerous()
        .with_custom_certificate_verifier(Arc::new(HueCertVerifier::new()))
        .with_no_client_auth();

    let builder = Client::builder()
        .use_rustls_tls()
        .use_preconfigured_tls(client_config)
        .resolve(bridge_id, std::net::SocketAddr::new(*bridge_ip, 443));

    builder
        .build()
        .map_err(|e| anyhow!("Failed to build Hue HTTP client: {}", e))
}

pub async fn discover_bridge_by_id_fallback(_username: &str) -> Result<IpAddr> {
    if let Ok(mdns) = ServiceDaemon::new() {
        let receiver = mdns.browse("_hue._tcp.local.")?;
        let now = std::time::Instant::now();
        while now.elapsed() < Duration::from_secs(5) {
            if let Ok(ServiceEvent::ServiceResolved(info)) =
                receiver.recv_timeout(Duration::from_millis(500))
            {
                // Prefer IPv4
                let addresses = info.get_addresses();
                let ip = addresses
                    .iter()
                    .find(|ip| ip.is_ipv4())
                    .or_else(|| addresses.iter().next())
                    .ok_or_else(|| anyhow!("No IP found for mDNS service"))?;
                let ip = *ip;
                return Ok(ip);
            }
        }
    }
    Err(anyhow!("Could not find any Hue bridge on the network"))
}

async fn perform_push_link(bridge_ip: &IpAddr, bridge_id: &str) -> Result<(String, String)> {
    println!("\nStep 2: Linking with Bridge");
    println!("Please press the physical button on the center of your Hue Bridge now...");

    let client = build_hue_client(bridge_ip, bridge_id)?;

    let url = format!("https://{}/api", bridge_id);
    let body = json!({
        "devicetype": "pulseplex#daemon",
        "generateclientkey": true
    });

    // Timeout after 60 seconds of waiting for button press
    let start = std::time::Instant::now();
    let timeout = Duration::from_secs(60);

    loop {
        if start.elapsed() > timeout {
            return Err(anyhow!(
                "Timed out waiting for Hue Bridge button press (60s limit)."
            ));
        }

        let resp = client
            .post(&url)
            .json(&body)
            .send()
            .await
            .context("Failed to send push-link request")?;

        let results: Vec<HueAuthResponse> = resp
            .json()
            .await
            .context("Failed to parse Hue Auth response")?;
        if let Some(res) = results.first() {
            if let Some(success) = &res.success {
                return Ok((success.username.clone(), success.clientkey.clone()));
            } else if let Some(error) = &res.error {
                if !error.description.contains("link button not pressed") {
                    return Err(anyhow!("Bridge error: {}", error.description));
                }
            }
        }

        tokio::time::sleep(Duration::from_secs(2)).await;
    }
}

async fn select_entertainment_area(
    bridge_ip: &IpAddr,
    bridge_id: &str,
    username: &str,
) -> Result<String> {
    println!("\nStep 3: Selecting Entertainment Area");

    let client = build_hue_client(bridge_ip, bridge_id)?;

    let url = format!(
        "https://{}/clip/v2/resource/entertainment_configuration",
        bridge_id
    );
    let resp = client
        .get(&url)
        .header("hue-application-key", username)
        .send()
        .await?;

    if !resp.status().is_success() {
        return Err(anyhow!(
            "Failed to fetch areas: Bridge returned {}",
            resp.status()
        ));
    }

    #[derive(Deserialize)]
    struct V2Response {
        data: Vec<EntertainmentArea>,
    }
    #[derive(Deserialize)]
    struct EntertainmentArea {
        id: String,
        metadata: Metadata,
    }
    #[derive(Deserialize)]
    struct Metadata {
        name: String,
    }

    let v2_resp: V2Response = resp.json().await?;
    if v2_resp.data.is_empty() {
        return Err(anyhow!(
            "No Entertainment Areas found. Please create one in the Hue App first."
        ));
    }

    let area_names: Vec<String> = v2_resp
        .data
        .iter()
        .map(|a| a.metadata.name.clone())
        .collect();
    let selection = Select::with_theme(&ColorfulTheme::default())
        .with_prompt("Select your Entertainment Area")
        .items(&area_names)
        .default(0)
        .interact()?;

    Ok(v2_resp.data[selection].id.clone())
}

#[cfg(test)]
mod tests {
    use super::*;
    use rustls::client::danger::ServerCertVerified;
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
