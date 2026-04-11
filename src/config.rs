use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::{env, fs};

use anyhow::Result;
use pulseplex_core::{BehaviorConfig, DmxOutputConfig};
use serde::Deserialize;
use toml_edit::{value, DocumentMut};

#[derive(Deserialize, Debug, Clone)]
pub struct PulsePlexConfig {
    pub midi: MidiConfig,
    pub behavior: Vec<BehaviorDefinition>,
    pub output: OutputConfig,
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
    pub dmx: Vec<DmxOutputDefinition>,
}

#[derive(Deserialize, Debug, Clone)]
pub struct DmxOutputDefinition {
    pub id: String,
    pub channel: usize,
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

pub struct CompiledConfig {
    pub midi_device: String,
    pub midi_id_map: HashMap<u8, usize>,
    pub behaviors: HashMap<usize, BehaviorConfig>,
    pub dmx_outputs: Vec<DmxOutputConfig>,
}

impl PulsePlexConfig {
    pub fn load(path: &str) -> Result<Self> {
        let contents = fs::read_to_string(path)
            .map_err(|e| anyhow::anyhow!("Failed to read config file {}: {}", path, e))?;
        let config: PulsePlexConfig = toml::from_str(&contents)
            .map_err(|e| anyhow::anyhow!("Failed to parse TOML: {}", e))?;
        Ok(config)
    }

    pub fn compile(&self) -> CompiledConfig {
        let mut id_map: HashMap<String, usize> = HashMap::new();
        let mut next_id = 0;

        let mut get_id = |name: &str| -> usize {
            *id_map.entry(name.to_string()).or_insert_with(|| {
                let id = next_id;
                next_id += 1;
                id
            })
        };

        let mut midi_id_map = HashMap::new();
        for (note, logical_id) in &self.midi.mappings {
            midi_id_map.insert(*note, get_id(logical_id));
        }

        let mut behaviors = HashMap::new();
        for b in &self.behavior {
            behaviors.insert(
                get_id(&b.id),
                BehaviorConfig {
                    decay_seconds: b.decay_seconds,
                    velocity_curve: b.velocity_curve,
                    decay_profile: b.decay_profile,
                },
            );
        }

        let mut dmx_outputs = Vec::new();
        for d in &self.output.dmx {
            dmx_outputs.push(DmxOutputConfig {
                internal_id: get_id(&d.id),
                dmx_channel: d.channel,
                color: d.color,
            });
        }

        CompiledConfig {
            midi_device: self.midi.device_name.clone(),
            midi_id_map,
            behaviors,
            dmx_outputs,
        }
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
