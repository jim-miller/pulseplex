use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::{env, fs};

use anyhow::{bail, Result};
use pulseplex_core::BehaviorConfig;
use serde::Deserialize;
use toml_edit::{value, DocumentMut};

#[derive(Deserialize, Debug, Clone)]
pub struct PulsePlexConfig {
    pub midi: MidiConfig,
    pub behavior: Vec<BehaviorDefinition>,
    pub output: OutputConfig,
    #[serde(default)]
    pub targets: Vec<TargetConfig>,
    pub shutdown: ShutdownConfig,
}

#[derive(Deserialize, Debug, Clone)]
pub struct MidiConfig {
    pub device_name: String,
    pub mappings: HashMap<u8, String>,
}

#[derive(Deserialize, Debug, Clone)]
pub struct BehaviorDefinition {
    pub id: String,
    pub decay_seconds: f32,
    #[serde(default)]
    pub velocity_curve: pulseplex_core::VelocityCurve,
    #[serde(default)]
    pub decay_profile: pulseplex_core::DecayProfile,
}

#[derive(Deserialize, Debug, Clone)]
pub struct OutputConfig {
    pub artnet: Option<ArtNetConfig>,
    pub dmx: Vec<DmxOutputDefinition>,
    #[serde(default)]
    pub hue: Vec<HueOutputDefinition>,
}

#[derive(Deserialize, Debug, Clone)]
pub struct DmxOutputDefinition {
    pub id: String,
    pub channel: usize,
    pub color: Option<[u8; 3]>,
}

#[derive(Deserialize, Debug, Clone)]
pub struct HueOutputDefinition {
    pub id: String,
    pub channel_id: u8,
    pub color: Option<[u8; 3]>,
}

#[derive(Deserialize, Debug, Clone)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum TargetConfig {
    ArtNet(ArtNetConfig),
    Hue(HueConfig),
}

#[derive(Deserialize, Debug, Clone)]
pub struct ArtNetConfig {
    pub target_ip: String,
    pub universe: u16,
}

#[derive(Deserialize, Debug, Clone)]
pub struct HueConfig {
    pub bridge_ip: String,
    pub username: String,
    pub client_key: String,
    pub area_id: String,
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
    #[allow(dead_code)]
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

#[derive(Clone, Debug)]
pub struct DmxOutputCompiled {
    pub internal_id: usize,
    pub dmx_channel: usize,
    pub color: Option<[u8; 3]>,
}

#[derive(Clone, Debug)]
pub struct HueOutputCompiled {
    pub internal_id: usize,
    pub channel_id: u8,
    pub color: Option<[u8; 3]>,
}

impl PulsePlexConfig {
    pub fn load(path: &str) -> Result<Self> {
        let contents = fs::read_to_string(path)
            .map_err(|e| anyhow::anyhow!("Failed to read config file {}: {}", path, e))?;
        let mut config: PulsePlexConfig = toml::from_str(&contents)
            .map_err(|e| anyhow::anyhow!("Failed to parse TOML: {}", e))?;

        // Migration: Move legacy artnet config to targets, but avoid duplicating
        // an equivalent Art-Net target when both formats are present.
        if let Some(artnet) = config.output.artnet.take() {
            let already_present = config.targets.iter().any(|target| {
                matches!(
                    target,
                    TargetConfig::ArtNet(existing)
                        if existing.target_ip == artnet.target_ip
                            && existing.universe == artnet.universe
                )
            });

            if !already_present {
                config.targets.push(TargetConfig::ArtNet(artnet));
            }
        }

        Ok(config)
    }

    /// Compiles the logical configuration into a high-performance internal representation.
    /// Returns an error if the logical chain is broken (orphaned IDs, missing behaviors, etc).
    pub fn compile(&self) -> Result<CompiledConfig> {
        // 1. Gather all unique logical IDs from behaviors and sort them for deterministic internal-ID assignment
        let mut all_ids: HashSet<&String> = HashSet::new();
        for b in &self.behavior {
            all_ids.insert(&b.id);
        }

        let mut sorted_ids: Vec<&String> = all_ids.into_iter().collect();
        sorted_ids.sort();

        let id_map: HashMap<String, usize> = sorted_ids
            .into_iter()
            .enumerate()
            .map(|(i, name)| (name.clone(), i))
            .collect();

        // 2. Map behaviors and validate uniqueness
        let mut behaviors = HashMap::new();
        for b in &self.behavior {
            let internal_id = *id_map.get(&b.id).ok_or_else(|| {
                anyhow::anyhow!(
                    "Behavior ID '{}' was not assigned an internal ID during config compilation",
                    b.id
                )
            })?;
            if behaviors
                .insert(
                    internal_id,
                    BehaviorConfig {
                        decay_seconds: b.decay_seconds,
                        velocity_curve: b.velocity_curve,
                        decay_profile: b.decay_profile,
                    },
                )
                .is_some()
            {
                bail!("Duplicate behavior ID defined: '{}'", b.id);
            }
        }

        // 3. Map and validate MIDI inputs
        let mut midi_id_map = HashMap::new();
        for (note, logical_id) in &self.midi.mappings {
            let internal_id = *id_map.get(logical_id).ok_or_else(|| {
                anyhow::anyhow!("MIDI note {} maps to undefined ID '{}'", note, logical_id)
            })?;

            if !behaviors.contains_key(&internal_id) {
                bail!(
                    "MIDI note {} maps to ID '{}' which has no defined behavior",
                    note,
                    logical_id
                );
            }
            midi_id_map.insert(*note, internal_id);
        }

        // 4. Map and validate DMX outputs
        let mut dmx_outputs = Vec::new();
        for d in &self.output.dmx {
            let internal_id = *id_map
                .get(&d.id)
                .ok_or_else(|| anyhow::anyhow!("DMX output maps to undefined ID '{}'", d.id))?;

            if !behaviors.contains_key(&internal_id) {
                bail!("DMX output ID '{}' has no defined behavior", d.id);
            }

            dmx_outputs.push(DmxOutputCompiled {
                internal_id,
                dmx_channel: d.channel,
                color: d.color,
            });
        }

        // 5. Map and validate Hue outputs
        let mut hue_outputs = Vec::new();
        for h in &self.output.hue {
            let internal_id = *id_map
                .get(&h.id)
                .ok_or_else(|| anyhow::anyhow!("Hue output maps to undefined ID '{}'", h.id))?;

            if !behaviors.contains_key(&internal_id) {
                bail!("Hue output ID '{}' has no defined behavior", h.id);
            }

            hue_outputs.push(HueOutputCompiled {
                internal_id,
                channel_id: h.channel_id,
                color: h.color,
            });
        }

        Ok(CompiledConfig {
            midi_device: self.midi.device_name.clone(),
            midi_id_map,
            behaviors,
            dmx_outputs,
            hue_outputs,
            targets: self.targets.clone(),
        })
    }
}

pub fn update_midi_device_in_config(config_path: &str, new_device: &str) -> Result<()> {
    let contents = fs::read_to_string(config_path)?;
    let mut doc = contents
        .parse::<DocumentMut>()
        .map_err(|e| anyhow::anyhow!("Failed to parse config for editing: {}", e))?;

    // Safely update the value while preserving all surrounding comments and whitespace
    doc["midi"]["device_name"] = value(new_device);

    // Atomic write: Save to a temp file then swap
    let path = Path::new(config_path);
    let tmp_path = path.with_extension("toml.tmp");

    fs::write(&tmp_path, doc.to_string())?;

    // Rename guarantees atomicity on POSIX systems if both are on the same filesystem
    fs::rename(&tmp_path, path).map_err(|e| {
        // Attempt to clean up the temp file if rename fails
        let _ = fs::remove_file(&tmp_path);
        anyhow::anyhow!("Failed to atomically replace config file: {}", e)
    })?;

    Ok(())
}

pub fn get_config_path(cli_override: Option<&String>) -> Result<PathBuf> {
    cli_override
        .map(PathBuf::from)
        .or_else(|| {
            env::var("PULSEPLEX_CONFIG_HOME")
                .ok()
                .map(|p| PathBuf::from(p).join("pulseplex.toml"))
        })
        .map(Ok)
        .unwrap_or_else(|| {
            let xdg_base = env::var("XDG_CONFIG_HOME")
                .map(PathBuf::from)
                .or_else(|_| {
                    env::var("HOME")
                        .or_else(|_| env::var("USERPROFILE"))
                        .map(|h| PathBuf::from(h).join(".config"))
                })
                .map_err(|_| {
                    anyhow::anyhow!("Could not determine home directory from HOME or USERPROFILE")
                })?;

            Ok(xdg_base.join("pulseplex").join("pulseplex.toml"))
        })
}
