use crate::config::{PulsePlexConfig, TargetConfig};
use crate::setup::build_hue_client;
use anyhow::{anyhow, Result};
use std::net::IpAddr;
use std::path::Path;

pub fn run_doctor(config_path: &Path) -> Result<()> {
    println!("PulsePlex Doctor - Diagnostic Utility\n");

    // 1. Check Config File
    if !config_path.exists() {
        return Err(anyhow!(
            "Configuration file not found at {:?}. Run with --setup first.",
            config_path
        ));
    }
    println!("✓ Configuration file found: {:?}", config_path);

    let config = PulsePlexConfig::load(&config_path.to_string_lossy())?;

    // 2. Check MIDI Device
    println!("\nChecking MIDI connectivity...");
    let midi_devices = pulseplex_midi::list_midi_devices()?;
    if midi_devices.contains(&config.midi.device_name) {
        println!("✓ MIDI device found: {}", config.midi.device_name);
    } else {
        println!("✗ MIDI device NOT found: {}", config.midi.device_name);
        println!("  Available devices: {:?}", midi_devices);
    }

    // 3. Check Hue Bridge Connectivity
    println!("\nChecking Hue Bridge connectivity...");

    let hue_target = config.targets.iter().find_map(|t| {
        if let TargetConfig::Hue(h) = t {
            Some(h)
        } else {
            None
        }
    });

    if let Some(hue) = hue_target {
        let bridge_ip: IpAddr = hue.bridge_ip.parse()?;

        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()?;

        // Use the IP as the ID for resolution purposes in the doctor as well
        match rt.block_on(check_hue_connectivity(&bridge_ip, &hue.username)) {
            Ok(_) => println!("✓ Successfully connected to Hue Bridge via V2 API"),
            Err(e) => {
                println!("✗ Failed to connect to Hue Bridge: {}", e);
                println!("  Suggestions:");
                println!(
                    "  - Ensure the Bridge IP ({}) is correct and accessible.",
                    bridge_ip
                );
                println!("  - Run `pulseplex run --setup` to re-link.");
            }
        }
    } else {
        println!("! No Hue target configured in 'targets'. Skipping Hue checks.");
    }

    println!("\nDiagnostic complete.");
    Ok(())
}

async fn check_hue_connectivity(bridge_ip: &IpAddr, username: &str) -> Result<()> {
    // Use the same client builder as setup to ensure macOS compatibility
    let client = build_hue_client(bridge_ip, &bridge_ip.to_string())?;

    let url = format!(
        "https://{}/clip/v2/resource/entertainment_configuration",
        bridge_ip
    );
    let resp = client
        .get(&url)
        .header("hue-application-key", username)
        .timeout(std::time::Duration::from_secs(5))
        .send()
        .await?;

    if resp.status().is_success() {
        Ok(())
    } else {
        Err(anyhow!("Bridge returned status {}", resp.status()))
    }
}
