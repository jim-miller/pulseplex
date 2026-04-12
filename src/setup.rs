use std::net::IpAddr;
use std::path::PathBuf;
use std::time::Duration;

use anyhow::{anyhow, Result};
use dialoguer::theme::ColorfulTheme;
use dialoguer::{Input, Select};
use directories::ProjectDirs;
use mdns_sd::{ServiceDaemon, ServiceEvent};
use reqwest::Client;
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
                let ip = *info
                    .get_addresses()
                    .iter()
                    .next()
                    .ok_or_else(|| anyhow!("No IP found for mDNS service"))?;
                let bridge_id = info
                    .get_fullname()
                    .split('.')
                    .next()
                    .unwrap_or("")
                    .to_lowercase();
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

fn build_hue_client(bridge_ip: &std::net::IpAddr, bridge_id: &str) -> Result<Client> {
    let mut builder = Client::builder();

    // Split the bundle by END CERTIFICATE and re-add the delimiter to each block
    let pem_str = std::str::from_utf8(HUE_CA_CERT)?;
    for block in pem_str.split("-----END CERTIFICATE-----") {
        let trimmed = block.trim();
        if trimmed.is_empty() {
            continue;
        }
        let cert_pem = format!("{}\n-----END CERTIFICATE-----", trimmed);
        match reqwest::Certificate::from_pem(cert_pem.as_bytes()) {
            Ok(cert) => {
                builder = builder.add_root_certificate(cert);
            }
            Err(e) => {
                tracing::warn!("Failed to parse a certificate from bundle: {}", e);
            }
        }
    }

    builder
        .resolve(bridge_id, std::net::SocketAddr::new(*bridge_ip, 443))
        .build()
        .map_err(|e| anyhow!("Failed to build Hue HTTP client: {}", e))
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

    loop {
        let resp = client.post(&url).json(&body).send().await?;

        let results: Vec<HueAuthResponse> = resp.json().await?;
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

    // Fetch Entertainment Areas using CLIP V2
    let url = format!(
        "https://{}/clip/v2/resource/entertainment_configuration",
        bridge_id
    );
    let resp = client
        .get(&url)
        .header("hue-application-key", username)
        .send()
        .await?;

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

    #[test]
    fn test_ca_bundle_parsing() {
        // Just verify we can parse at least one certificate from the bundle
        let pem_str = std::str::from_utf8(HUE_CA_CERT).unwrap();
        let mut count = 0;
        for block in pem_str.split("-----END CERTIFICATE-----") {
            let trimmed = block.trim();
            if trimmed.is_empty() {
                continue;
            }
            let cert_pem = format!("{}\n-----END CERTIFICATE-----", trimmed);
            if reqwest::Certificate::from_pem(cert_pem.as_bytes()).is_ok() {
                count += 1;
            }
        }
        assert!(count >= 1, "Should parse at least one Hue CA certificate");
    }
}
