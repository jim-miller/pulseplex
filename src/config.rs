use std::collections::HashMap;
use std::path::PathBuf;
use std::{env, fs};

use anyhow::{Ok, Result};
use pulseplex_core::{DecayProfile, VelocityCurve};
use serde::Deserialize;
use toml_edit::{value, DocumentMut};

#[derive(Deserialize, Debug, Clone)]
pub struct PulsePlexConfig {
    pub midi: MidiConfig,
    pub artnet: ArtNetConfig,
    pub mapping: Vec<MappingConfig>,
    pub shutdown: ShutdownConfig,
}

#[derive(Deserialize, Debug, Clone)]
pub struct MidiConfig {
    pub device_name: String,
}

#[derive(Deserialize, Debug, Clone)]
pub struct ArtNetConfig {
    pub target_ip: String,
    pub universe: u16,
}

#[derive(Deserialize, Debug, Clone)]
pub struct MappingConfig {
    pub note: u8,
    pub dmx_channel: usize,
    pub decay_seconds: f32,
    #[serde(default)]
    pub velocity_curve: VelocityCurve,
    #[serde(default)]
    pub decay_profile: DecayProfile,
    pub color: Option<[u8; 3]>,
}

#[derive(Clone, Debug, Deserialize)]
pub enum ShutdownMode {
    Blackout,
    Default,
    Restore,
}

#[derive(Clone, Debug, Deserialize)]
pub struct ShutdownConfig {
    pub mode: ShutdownMode,
    pub defaults: Option<HashMap<usize, u8>>,
}

impl PulsePlexConfig {
    pub fn load(path: &str) -> Result<Self> {
        let contents = fs::read_to_string(path)
            .map_err(|e| anyhow::anyhow!("Failed to read config file {}: {}", path, e))?;
        let config: PulsePlexConfig = toml::from_str(&contents)
            .map_err(|e| anyhow::anyhow!("Failed to parse TOML: {}", e))?;
        Ok(config)
    }
}

pub fn update_midi_device_in_config(config_path: &str, new_device: &str) -> anyhow::Result<()> {
    let contents = fs::read_to_string(config_path)?;
    let mut doc = contents
        .parse::<DocumentMut>()
        .map_err(|e| anyhow::anyhow!("Failed to parse config for editing: {}", e))?;

    // Safely update the value while preserving all surrounding comments and whitespace
    doc["midi"]["device_name"] = value(new_device);

    fs::write(config_path, doc.to_string())?;
    Ok(())
}

pub fn get_config_path(cli_override: Option<&String>) -> PathBuf {
    // 1. Explicit CLI argument wins
    if let Some(path) = cli_override {
        return PathBuf::from(path);
    }

    // 2. App-specific environment variable
    if let Result::Ok(path) = env::var("PULSEPLEX_CONFIG_HOME") {
        return PathBuf::from(path).join("pulseplex.toml");
    }

    // 3. XDG standard or fallback to ~/.config
    let xdg_config = env::var("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|_| {
            // Quick cross-platform home directory fetch
            let home = env::var("HOME")
                .or_else(|_| env::var("USERPROFILE"))
                .expect("Could not determine home directory");
            PathBuf::from(home).join(".config")
        });

    // 4. Final path: ~/.config/pulseplex/pulseplex.toml
    xdg_config.join("pulseplex").join("pulseplex.toml")
}
