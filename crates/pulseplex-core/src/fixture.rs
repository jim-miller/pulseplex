use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use thiserror::Error;

#[derive(Error, Debug)]
pub enum FixtureError {
    #[error("Mode '{0}' not found in fixture profile")]
    ModeNotFound(String),
    #[error("Channel '{0}' not found in fixture profile")]
    ChannelNotFound(String),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum CapabilityType {
    Intensity,
    Red,
    Green,
    Blue,
    White,
    Pan,
    Tilt,
    Strobe,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct OflFixture {
    pub name: String,
    pub available_channels: HashMap<String, OflChannel>,
    pub modes: Vec<OflMode>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct OflChannel {
    pub capabilities: Vec<OflCapability>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct OflCapability {
    pub dmx_range: [u8; 2],
    #[serde(rename = "type")]
    pub cap_type: String,
    pub color: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct OflMode {
    pub name: String,
    pub channels: Vec<String>,
}

/// Represents an instantiated fixture in the system.
#[derive(Debug, Clone)]
pub struct FixtureInstance {
    pub id: String,
    pub name: String,
    pub universe: u16,
    pub start_address: u16,
    /// Maps CapabilityType to 0-indexed DMX offset relative to start_address
    pub capability_offsets: HashMap<CapabilityType, usize>,
}

impl FixtureInstance {
    pub fn from_ofl(
        id: String,
        fixture: &OflFixture,
        mode_name: &str,
        universe: u16,
        start_address: u16,
    ) -> Result<Self, FixtureError> {
        let mode = fixture
            .modes
            .iter()
            .find(|m| m.name == mode_name)
            .ok_or_else(|| FixtureError::ModeNotFound(mode_name.to_string()))?;

        let mut capability_offsets = HashMap::new();

        for (offset, channel_name) in mode.channels.iter().enumerate() {
            let channel = fixture
                .available_channels
                .get(channel_name)
                .ok_or_else(|| FixtureError::ChannelNotFound(channel_name.to_string()))?;

            for cap in &channel.capabilities {
                if let Some(cap_type) = map_ofl_capability_to_internal(cap) {
                    capability_offsets.insert(cap_type, offset);
                }
            }
        }

        Ok(Self {
            id,
            name: fixture.name.clone(),
            universe,
            start_address,
            capability_offsets,
        })
    }

    /// Returns the absolute DMX address (universe, channel_0_indexed) for a specific capability.
    /// Returns None if the capability is not supported by this fixture.
    pub fn get_dmx_address(&self, cap_type: CapabilityType) -> Option<(u16, usize)> {
        self.capability_offsets.get(&cap_type).map(|&offset| {
            // start_address is 1-indexed, convert to 0-indexed for buffer access
            (self.universe, (self.start_address as usize - 1) + offset)
        })
    }
}

fn map_ofl_capability_to_internal(cap: &OflCapability) -> Option<CapabilityType> {
    match cap.cap_type.as_str() {
        "Intensity" => Some(CapabilityType::Intensity),
        "ColorIntensity" => match cap.color.as_deref() {
            Some("Red") => Some(CapabilityType::Red),
            Some("Green") => Some(CapabilityType::Green),
            Some("Blue") => Some(CapabilityType::Blue),
            Some("White") => Some(CapabilityType::White),
            _ => None,
        },
        "Pan" => Some(CapabilityType::Pan),
        "Tilt" => Some(CapabilityType::Tilt),
        "Strobe" => Some(CapabilityType::Strobe),
        _ => None,
    }
}
