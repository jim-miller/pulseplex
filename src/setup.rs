use std::net::IpAddr;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{anyhow, Context, Result};
use dialoguer::theme::ColorfulTheme;
use dialoguer::{Input, Select};
use directories::ProjectDirs;
use mdns_sd::{ServiceDaemon, ServiceEvent};
use pulseplex_hue::HueCertVerifier;
use reqwest::Client;
use serde::Deserialize;
use serde_json::json;
use tokio::runtime::Builder;

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
        .replace("{bridge_id}", &bridge_id)
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

pub async fn discover_bridge_by_id_fallback(target_id: &str) -> Result<IpAddr> {
    if let Ok(mdns) = ServiceDaemon::new() {
        let receiver = mdns.browse("_hue._tcp.local.")?;
        let now = std::time::Instant::now();
        while now.elapsed() < Duration::from_secs(5) {
            if let Ok(ServiceEvent::ServiceResolved(info)) =
                receiver.recv_timeout(Duration::from_millis(500))
            {
                let bridge_id = match info.get_property_val("bridgeid") {
                    Some(Some(id_bytes)) => String::from_utf8_lossy(id_bytes).to_lowercase(),
                    _ => "".to_string(),
                };

                if bridge_id == target_id.to_lowercase() {
                    // Prefer IPv4
                    let addresses = info.get_addresses();
                    let ip = addresses
                        .iter()
                        .find(|ip| ip.is_ipv4())
                        .or_else(|| addresses.iter().next())
                        .ok_or_else(|| anyhow!("No IP found for mDNS service"))?;
                    return Ok(*ip);
                }
            }
        }
    }
    Err(anyhow!(
        "Could not find Hue bridge with ID {} on the network",
        target_id
    ))
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
