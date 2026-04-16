use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
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
pub struct Capability {
    pub cap_type: CapabilityType,
    pub min_dmx: u8,
    pub max_dmx: u8,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChannelDef {
    pub offset: usize,
    pub capabilities: Vec<Capability>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FixtureProfile {
    pub name: String,
    pub manufacturer: String,
    pub channels: Vec<ChannelDef>,
}

/// Represents an instantiated fixture in the system.
#[derive(Debug, Clone)]
pub struct FixtureInstance {
    pub id: String,
    pub profile: FixtureProfile,
    pub start_address: u16,
}

impl FixtureInstance {
    pub fn new(id: String, profile: FixtureProfile, start_address: u16) -> Self {
        Self {
            id,
            profile,
            start_address,
        }
    }

    /// Returns the DMX address for a specific capability.
    /// Returns None if the capability is not supported by this fixture.
    pub fn get_dmx_address(&self, cap_type: CapabilityType) -> Option<usize> {
        for channel in &self.profile.channels {
            for cap in &channel.capabilities {
                if cap.cap_type == cap_type {
                    // start_address is 1-indexed, convert to 0-indexed for buffer access
                    return Some((self.start_address as usize - 1) + channel.offset);
                }
            }
        }
        None
    }
}
