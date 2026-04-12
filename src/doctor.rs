use std::net::UdpSocket;
use std::time::Duration;

use crate::config::{get_config_path, PulsePlexConfig, TargetConfig};
use anyhow::Result;
use mdns_sd::{ServiceDaemon, ServiceEvent};

pub fn run_doctor(config_override: Option<&String>) -> Result<()> {
    println!("# PulsePlex Diagnostic Report\n");

    let path = get_config_path(config_override)?;
    println!("## 1. Configuration");
    println!("- **Path:** `{}`", path.to_string_lossy());

    if !path.exists() {
        println!("- **Status:** ❌ File not found");
        println!(
            "\n💡 Tip: Run `pulseplex` without any arguments to start the First-Run Setup Wizard."
        );
        return Ok(());
    }

    let config = match PulsePlexConfig::load(&path.to_string_lossy()) {
        Ok(c) => {
            println!("- **Parsing:** ✅ Success");
            c
        }
        Err(e) => {
            println!("- **Parsing:** ❌ Failed: {}", e);
            return Ok(());
        }
    };

    println!("\n## 2. Network Discovery (mDNS)");
    if let Ok(mdns) = ServiceDaemon::new() {
        let receiver = mdns.browse("_hue._tcp.local.")?;
        let now = std::time::Instant::now();
        let mut found = false;
        while now.elapsed() < Duration::from_secs(3) {
            if let Ok(ServiceEvent::ServiceResolved(info)) =
                receiver.recv_timeout(Duration::from_millis(500))
            {
                let name = info.get_fullname();
                let ips = info.get_addresses();
                println!("- **Found:** `{}` at {:?}", name, ips);
                found = true;
            }
        }
        if !found {
            println!("- **Status:** ⚠️ No Hue Bridges seen on local network via mDNS.");
        }
    } else {
        println!("- **Status:** ❌ Could not initialize mDNS daemon.");
    }

    println!("\n## 3. Lighting Targets");
    for (idx, target) in config.targets.iter().enumerate() {
        match target {
            TargetConfig::ArtNet(artnet) => {
                println!("### Target {}: Art-Net", idx);
                println!("- **Universe:** {}", artnet.universe);
                println!("- **Target IP:** `{}`", artnet.target_ip);
            }
            TargetConfig::Hue(hue) => {
                println!("### Target {}: Philips Hue", idx);
                println!("- **Bridge IP:** `{}`", hue.bridge_ip);
                println!(
                    "- **Bridge ID Masked:** `{}`",
                    hue.client_key.get(..8).unwrap_or("unknown")
                );

                // Real connection check
                let rt = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()?;
                match rt.block_on(check_hue_connectivity(&hue.bridge_ip)) {
                    Ok(_) => {
                        println!("- **Connectivity:** ✅ Successfully reached Bridge API via HTTPS")
                    }
                    Err(e) => println!("- **Connectivity:** ❌ Failed: {}", e),
                }
            }
        }
    }

    println!("\n## 4. System Permissions");
    match UdpSocket::bind("0.0.0.0:0") {
        Ok(s) => println!(
            "- **UDP Bind:** ✅ Success (Local addr: {:?})",
            s.local_addr()?
        ),
        Err(e) => println!("- **UDP Bind:** ❌ Failed: {}", e),
    }

    println!("\n## 5. Software Version");
    println!("- **PulsePlex:** `v{}`", env!("CARGO_PKG_VERSION"));
    println!("- **OS:** `{}`", std::env::consts::OS);

    Ok(())
}

async fn check_hue_connectivity(bridge_ip: &str) -> Result<()> {
    let client = reqwest::Client::builder()
        .danger_accept_invalid_certs(true) // In doctor, we want to know if IP is reachable first
        .timeout(Duration::from_secs(2))
        .build()?;

    let url = format!("https://{}/description.xml", bridge_ip);
    client.get(url).send().await?;
    Ok(())
}
