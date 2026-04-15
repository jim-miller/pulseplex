use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::{env, fs};

use anyhow::{bail, Result};
use pulseplex_core::BehaviorConfig;
use serde::Deserialize;
use toml_edit::{value, DocumentMut};

#[derive(Deserialize, Debug, Clone, PartialEq)]
pub struct PulsePlexConfig {
    pub midi: MidiConfig,
    pub behavior: Vec<BehaviorDefinition>,
    pub output: OutputConfig,
    #[serde(default)]
    pub targets: Vec<TargetConfig>,
    pub shutdown: ShutdownConfig,
}

#[derive(Deserialize, Debug, Clone, PartialEq)]
pub struct MidiConfig {
    pub device_name: String,
    pub mappings: HashMap<u8, String>,
}

#[derive(Deserialize, Debug, Clone, PartialEq)]
pub struct BehaviorDefinition {
    pub id: String,
    pub decay_seconds: f32,
    #[serde(default)]
    pub velocity_curve: pulseplex_core::VelocityCurve,
    #[serde(default)]
    pub decay_profile: pulseplex_core::DecayProfile,
}

#[derive(Deserialize, Debug, Clone, PartialEq)]
pub struct OutputConfig {
    pub artnet: Option<ArtNetConfig>,
    pub dmx: Vec<DmxOutputDefinition>,
    #[serde(default)]
    pub hue: Vec<HueOutputDefinition>,
}

#[derive(Deserialize, Debug, Clone, PartialEq)]
pub struct DmxOutputDefinition {
    pub id: String,
    pub channel: usize,
    pub color: Option<[u8; 3]>,
}

#[derive(Deserialize, Debug, Clone, PartialEq)]
pub struct HueOutputDefinition {
    pub id: String,
    pub channel_id: u8,
    pub color: Option<[u8; 3]>,
}

#[derive(Deserialize, Debug, Clone, PartialEq)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum TargetConfig {
    ArtNet(ArtNetConfig),
    Hue(HueConfig),
}

#[derive(Deserialize, Debug, Clone, PartialEq)]
pub struct ArtNetConfig {
    pub target_ip: String,
    pub universe: u16,
}

#[derive(Deserialize, Debug, Clone, PartialEq)]
pub struct HueConfig {
    pub bridge_ip: String,
    pub bridge_id: String,
    pub username: String,
    pub client_key: String,
    pub area_id: String,
}

#[derive(Clone, Debug, Deserialize, PartialEq)]
#[serde(rename_all = "lowercase")]
pub enum ShutdownMode {
    #[serde(alias = "Blackout")]
    Blackout,
    #[serde(alias = "Default")]
    Default,
    #[serde(alias = "Restore")]
    Restore,
}

#[derive(Clone, Debug, Deserialize, PartialEq)]
pub struct ShutdownConfig {
    pub mode: ShutdownMode,
    pub defaults: Option<HashMap<usize, u8>>,
}

#[derive(Clone)]
pub struct CompiledConfig {
    pub midi_device: String,
    pub midi_id_map: HashMap<u8, usize>,
    pub behaviors: HashMap<usize, BehaviorConfig>,
    pub dmx_outputs: Vec<DmxOutputCompiled>,
    pub hue_outputs: Vec<HueOutputCompiled>,
    pub targets: Vec<TargetConfig>,
}

#[derive(Clone)]
pub struct DmxOutputCompiled {
    pub internal_id: usize,
    pub channel: usize,
    pub color: Option<[u8; 3]>,
}

#[derive(Clone)]
pub struct HueOutputCompiled {
    pub internal_id: usize,
    pub channel_id: u8,
    pub color: Option<[u8; 3]>,
}

impl PulsePlexConfig {
    pub fn load(path: &str) -> Result<Self> {
        let content = fs::read_to_string(path)?;
        let config: Self = toml::from_str(&content)?;
        Ok(config)
    }

    pub fn compile(&self) -> Result<CompiledConfig> {
        let mut id_to_internal = HashMap::new();
        let mut behaviors = HashMap::new();

        for (internal_id, b) in self.behavior.iter().enumerate() {
            if id_to_internal.insert(b.id.clone(), internal_id).is_some() {
                bail!("Duplicate behavior id '{}' in configuration", b.id);
            }

            behaviors.insert(
                internal_id,
                BehaviorConfig {
                    decay_seconds: b.decay_seconds,
                    velocity_curve: b.velocity_curve,
                    decay_profile: b.decay_profile,
                },
            );
        }

        let mut midi_id_map = HashMap::new();
        for (midi_note, behavior_id) in &self.midi.mappings {
            if let Some(internal_id) = id_to_internal.get(behavior_id) {
                midi_id_map.insert(*midi_note, *internal_id);
            } else {
                bail!(
                    "MIDI mapping for note {} points to non-existent behavior '{}'",
                    midi_note,
                    behavior_id
                );
            }
        }

        let mut dmx_outputs = Vec::new();
        for d in &self.output.dmx {
            if let Some(internal_id) = id_to_internal.get(&d.id) {
                dmx_outputs.push(DmxOutputCompiled {
                    internal_id: *internal_id,
                    channel: d.channel,
                    color: d.color,
                });
            }
        }

        let mut hue_outputs = Vec::new();
        for h in &self.output.hue {
            if let Some(internal_id) = id_to_internal.get(&h.id) {
                hue_outputs.push(HueOutputCompiled {
                    internal_id: *internal_id,
                    channel_id: h.channel_id,
                    color: h.color,
                });
            }
        }

        let mut targets = self.targets.clone();
        if let Some(artnet) = &self.output.artnet {
            let has_equivalent_artnet_target = targets.iter().any(
                |target| matches!(target, TargetConfig::ArtNet(existing) if existing == artnet),
            );
            if !has_equivalent_artnet_target {
                targets.push(TargetConfig::ArtNet(artnet.clone()));
            }
        }

        Ok(CompiledConfig {
            midi_device: self.midi.device_name.clone(),
            midi_id_map,
            behaviors,
            dmx_outputs,
            hue_outputs,
            targets,
        })
    }
}

pub fn update_hue_ip_in_config(config_path: &str, new_ip: &str) -> Result<()> {
    let contents = fs::read_to_string(config_path)?;
    let mut doc = contents
        .parse::<DocumentMut>()
        .map_err(|e| anyhow::anyhow!("Failed to parse config for editing: {}", e))?;

    let mut updated = false;

    if let Some(targets) = doc.get_mut("targets") {
        // Case 1: [[targets]] - Array of Tables
        if let Some(array) = targets.as_array_of_tables_mut() {
            for target in array.iter_mut() {
                if target.get("type").and_then(|v| v.as_str()) == Some("hue") {
                    target["bridge_ip"] = value(new_ip);
                    updated = true;
                }
            }
        }
        // Case 2: [targets] - Array of Inline Tables
        else if let Some(array) = targets.as_array_mut() {
            for target in array.iter_mut() {
                if let Some(target_inline) = target.as_inline_table_mut() {
                    if target_inline.get("type").and_then(|v| v.as_str()) == Some("hue") {
                        target_inline.insert("bridge_ip", value(new_ip).into_value().unwrap());
                        updated = true;
                    }
                }
            }
        }
    }

    if !updated {
        bail!("No Hue target found in config to update");
    }

    let path = Path::new(config_path);
    let tmp_path = path.with_extension("toml.tmp");
    fs::write(&tmp_path, doc.to_string())?;
    fs::rename(&tmp_path, path).map_err(|e| {
        let _ = fs::remove_file(&tmp_path);
        anyhow::anyhow!("Failed to atomically replace config file: {}", e)
    })?;

    Ok(())
}

pub fn update_midi_device_in_config(config_path: &str, new_device: &str) -> Result<()> {
    let contents = fs::read_to_string(config_path)?;
    let mut doc = contents
        .parse::<DocumentMut>()
        .map_err(|e| anyhow::anyhow!("Failed to parse config for editing: {}", e))?;

    doc["midi"]["device_name"] = value(new_device);

    let path = Path::new(config_path);
    let tmp_path = path.with_extension("toml.tmp");
    fs::write(&tmp_path, doc.to_string())?;
    fs::rename(&tmp_path, path).map_err(|e| {
        let _ = fs::remove_file(&tmp_path);
        anyhow::anyhow!("Failed to atomically replace config file: {}", e)
    })?;

    Ok(())
}

use directories::ProjectDirs;

pub fn get_config_path(cli_override: Option<&String>) -> Result<PathBuf> {
    if let Some(path) = cli_override {
        return Ok(PathBuf::from(path));
    }

    if let Ok(path) = env::var("PULSEPLEX_CONFIG_HOME") {
        return Ok(PathBuf::from(path).join("pulseplex.toml"));
    }

    // Check current directory first
    let local_config = PathBuf::from("pulseplex.toml");
    if local_config.exists() {
        return Ok(local_config);
    }

    let proj_dirs = ProjectDirs::from("", "", "PulsePlex")
        .ok_or_else(|| anyhow::anyhow!("Could not determine configuration directory"))?;

    Ok(proj_dirs.config_dir().join("pulseplex.toml"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_default_template_parsing() {
        // This test ensures that the template we give users actually parses with current enum variants
        let template = include_str!("../assets/default_edrums.toml");
        let rendered = template
            .replace("{midi_device}", "Test Device")
            .replace("{bridge_ip}", "127.0.0.1")
            .replace("{username}", "test-user")
            .replace("{client_key}", "test-key")
            .replace("{area_id}", "12345678-1234-1234-1234-123456789012");

        let config: Result<PulsePlexConfig, _> = toml::from_str(&rendered);
        assert!(
            config.is_ok(),
            "Template failed to parse: {:?}",
            config.err()
        );

        let config = config.unwrap();
        assert_eq!(config.shutdown.mode, ShutdownMode::Blackout);
        assert_eq!(config.behavior[0].id, "snare");
    }

    #[test]
    fn test_case_insensitivity() {
        let toml_str = r#"
            [midi]
            device_name = "Test"
            mappings = {}

            [[behavior]]
            id = "test"
            decay_seconds = 1.0
            velocity_curve = "Hard"
            decay_profile = "Exponential"

            [output]
            dmx = []
            hue = []

            [shutdown]
            mode = "Restore"
        "#;

        let config: PulsePlexConfig = toml::from_str(toml_str).unwrap();
        assert_eq!(config.shutdown.mode, ShutdownMode::Restore);
        assert_eq!(
            config.behavior[0].velocity_curve,
            pulseplex_core::VelocityCurve::Hard
        );
        assert_eq!(
            config.behavior[0].decay_profile,
            pulseplex_core::DecayProfile::Exponential
        );
    }
}
