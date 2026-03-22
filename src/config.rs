use anyhow::{Ok, Result};
use serde::Deserialize;
use std::{collections::HashMap, fs};

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
