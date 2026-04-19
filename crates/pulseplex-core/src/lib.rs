use std::time::Duration;

pub mod engine;
pub mod fixture;

use serde::Deserialize;
use tracing::trace;

#[derive(Debug, Clone, Copy, Default, Deserialize, PartialEq)]
#[serde(rename_all = "lowercase")]
pub enum VelocityCurve {
    #[default]
    #[serde(alias = "Linear")]
    Linear,
    #[serde(alias = "Hard")]
    Hard,
    #[serde(alias = "Soft")]
    Soft,
}

#[derive(Debug, Clone, Copy, Default, Deserialize, PartialEq)]
#[serde(rename_all = "lowercase")]
pub enum DecayProfile {
    #[default]
    #[serde(alias = "Linear")]
    Linear,
    #[serde(alias = "Exponential")]
    Exponential,
}

/// Logical behavior for an effect (independent of input and output).
#[derive(Debug, Clone, Deserialize)]
pub struct BehaviorConfig {
    pub decay_seconds: f32,
    #[serde(default)]
    pub velocity_curve: VelocityCurve,
    #[serde(default)]
    pub decay_profile: DecayProfile,
}

/// Standardized message type for all inputs (MIDI, OSC, Art-Net, etc.)
#[derive(Debug, Clone, PartialEq)]
pub enum SourceEvent {
    /// A hardware trigger (e.g. MIDI Note On)
    Trigger { id: u16, velocity: u8 },
    /// An external DMX frame to be merged (e.g. Art-Net Input)
    DmxFrame { universe: u16, data: Box<[u8; 512]> },
    /// Clear all active effects/envelopes
    ClearAll,
}

/// Interface for outputting lighting state to hardware or protocols.
#[async_trait::async_trait]
pub trait LightSink: Send + Sync {
    /// Send the finalized DMX universe state.
    async fn write_universe(&mut self, universe_id: u16, data: &[u8; 512]) -> anyhow::Result<()>;
}

/// Interface for input hardware to send events to the core.
pub trait LightSource: Send {
    /// Start polling for events and send them to the core via the provided sender.
    fn run(&mut self, sender: crossbeam_channel::Sender<SourceEvent>) -> anyhow::Result<()>;
}

pub struct DecayEnvelope {
    pub intensity: f32,        // 0.0 to 1.0
    pub decay_per_second: f32, // e.g., 0.90
    pub velocity_curve: VelocityCurve,
    pub decay_profile: DecayProfile,
}

impl DecayEnvelope {
    pub fn new(
        decay_seconds: f32,
        velocity_curve: VelocityCurve,
        decay_profile: DecayProfile,
    ) -> Self {
        Self {
            intensity: 0.0,
            decay_per_second: 1.0 / decay_seconds.max(0.001),
            velocity_curve,
            decay_profile,
        }
    }

    /// Trigger the envelope with a velocity (0-127)
    pub fn trigger(&mut self, velocity: u8) {
        let normalized = velocity as f32 / 127.0;
        // Map 0..127 to 0.0..1.0
        self.intensity = match self.velocity_curve {
            VelocityCurve::Linear => normalized,
            VelocityCurve::Hard => normalized.powi(2),
            VelocityCurve::Soft => normalized.sqrt(),
        };
        trace!(
            "Envelope triggered: velocity={}, intensity={:.3}",
            velocity,
            self.intensity
        );
    }

    /// Progress the decay by one tick
    pub fn tick(&mut self, dt: Duration) {
        let dt_seconds = dt.as_secs_f32();
        let prev_intensity = self.intensity;
        match self.decay_profile {
            DecayProfile::Linear => {
                let reduction = self.decay_per_second * dt_seconds;
                self.intensity = (self.intensity - reduction).max(0.0);
            }
            DecayProfile::Exponential => {
                let decay_rate = 5.0 * self.decay_per_second;
                self.intensity *= (-decay_rate * dt_seconds).exp();

                // Snap to 0 at the bottom to avoid long tail
                if self.intensity < 0.01 {
                    self.intensity = 0.0;
                }
            }
        }
        if self.intensity > 0.0 || prev_intensity > 0.0 {
            trace!(
                "Envelope tick: prev={:.3}, current={:.3}",
                prev_intensity,
                self.intensity
            );
        }
    }

    pub fn is_dead(&self) -> bool {
        self.intensity <= 0.0
    }

    pub fn dmx_value(&self) -> u8 {
        (self.intensity * 255.0) as u8
    }
}

/// A Mock Sink for testing. Records frames sent to it.
#[derive(Default)]
pub struct MockSink {
    pub states: Vec<[u8; 512]>,
}

#[async_trait::async_trait]
impl LightSink for MockSink {
    async fn write_universe(&mut self, _universe_id: u16, data: &[u8; 512]) -> anyhow::Result<()> {
        self.states.push(*data);
        Ok(())
    }
}

/// A Mock Source for testing.
pub struct MockSource {
    events: Vec<SourceEvent>,
}

impl MockSource {
    pub fn new(events: Vec<SourceEvent>) -> Self {
        let mut events = events;
        events.reverse(); // Reverse so we can pop
        Self { events }
    }
}

impl LightSource for MockSource {
    fn run(&mut self, sender: crossbeam_channel::Sender<SourceEvent>) -> anyhow::Result<()> {
        while let Some(event) = self.events.pop() {
            sender.send(event)?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::PulsePlexEngine;
    use std::collections::HashMap;
    use std::sync::Arc;
    use std::time::Duration;

    struct SharedMockSink {
        inner: Arc<tokio::sync::Mutex<MockSink>>,
    }

    impl SharedMockSink {
        fn new(inner: Arc<tokio::sync::Mutex<MockSink>>) -> Self {
            Self { inner }
        }
    }

    #[async_trait::async_trait]
    impl LightSink for SharedMockSink {
        async fn write_universe(
            &mut self,
            universe_id: u16,
            data: &[u8; 512],
        ) -> anyhow::Result<()> {
            self.inner
                .lock()
                .await
                .write_universe(universe_id, data)
                .await
        }
    }

    #[test]
    fn test_time_aware_decay() {
        let mut envelope = DecayEnvelope::new(1.0, VelocityCurve::Soft, DecayProfile::Linear);
        envelope.trigger(127);

        // After 1/2 second, intensity should be roughly half
        envelope.tick(Duration::from_millis(500));
        assert!((envelope.intensity - 0.5).abs() < 0.01);
        assert_eq!(envelope.dmx_value(), 127);
    }

    #[test]
    fn test_velocity_curves() {
        let mut env_hard = DecayEnvelope::new(1.0, VelocityCurve::Hard, DecayProfile::Linear);
        let mut env_soft = DecayEnvelope::new(1.0, VelocityCurve::Soft, DecayProfile::Linear);

        // MIDI velocity 64 is roughly 0.5 normalized
        env_hard.trigger(64);
        env_soft.trigger(64);

        // Hard curve: 0.5^2 = 0.25
        assert!((env_hard.intensity - 0.25).abs() < 0.01);

        // Soft curve: sqrt(0.5) ≈ 0.707
        assert!((env_soft.intensity - 0.707).abs() < 0.01);
    }

    #[test]
    fn test_decay_profiles() {
        let mut env_lin = DecayEnvelope::new(1.0, VelocityCurve::Linear, DecayProfile::Linear);
        let mut env_exp = DecayEnvelope::new(1.0, VelocityCurve::Linear, DecayProfile::Exponential);

        env_lin.trigger(127); // Intensity 1.0
        env_exp.trigger(127); // Intensity 1.0

        // Tick forward 0.5 seconds
        let dt = Duration::from_millis(500);
        env_lin.tick(dt);
        env_exp.tick(dt);

        // Linear: should be exactly 0.5
        assert_eq!(env_lin.intensity, 0.5);

        // Exponential: should be e^(-5 * 0.5) = e^-2.5 ≈ 0.082
        assert!((env_exp.intensity - 0.082).abs() < 0.01);
    }

    #[tokio::test]
    async fn test_htp_merging() {
        let mut behaviors = HashMap::new();
        behaviors.insert(
            1,
            BehaviorConfig {
                decay_seconds: 1.0,
                velocity_curve: VelocityCurve::Linear,
                decay_profile: DecayProfile::Linear,
            },
        );
        behaviors.insert(
            2,
            BehaviorConfig {
                decay_seconds: 1.0,
                velocity_curve: VelocityCurve::Linear,
                decay_profile: DecayProfile::Linear,
            },
        );

        // Setup a fixture: 1 channel, Red capability at offset 0, start address 1, universe 1.
        let mut available_channels = HashMap::new();
        available_channels.insert(
            "Red".to_string(),
            fixture::OflChannel {
                capabilities: vec![fixture::OflCapability {
                    cap_type: "ColorIntensity".to_string(),
                    dmx_range: [0, 255],
                    color: Some("Red".to_string()),
                }],
            },
        );

        let profile = fixture::OflFixture {
            name: "TestFixture".to_string(),
            available_channels,
            modes: vec![fixture::OflMode {
                name: "3-channel".to_string(),
                channels: vec!["Red".to_string()],
            }],
        };
        let fixture =
            fixture::FixtureInstance::from_ofl("f1".to_string(), &profile, "3-channel", 1, 1)
                .unwrap();

        let mut capability_mappings = HashMap::new();
        // behavior 1 -> fixture 0, Red
        capability_mappings.insert(1, vec![(0, fixture::CapabilityType::Red)]);
        // behavior 2 -> fixture 0, Red
        capability_mappings.insert(2, vec![(0, fixture::CapabilityType::Red)]);

        let mut engine = PulsePlexEngine::new(behaviors, vec![fixture], capability_mappings);
        let mock_sink_inner = Arc::new(tokio::sync::Mutex::new(MockSink::default()));
        let mut sinks: Vec<Box<dyn LightSink>> =
            vec![Box::new(SharedMockSink::new(mock_sink_inner.clone()))];

        let (tx, rx) = crossbeam_channel::unbounded();
        tx.send(SourceEvent::Trigger {
            id: 1,
            velocity: 64,
        })
        .unwrap();
        tx.send(SourceEvent::Trigger {
            id: 2,
            velocity: 100,
        })
        .unwrap();

        engine
            .tick(Duration::from_millis(0), &rx, &mut sinks)
            .await
            .unwrap();

        // The Red channel (index 0) should be the max of the two triggers.
        let mock_sink = mock_sink_inner.lock().await;
        let val = mock_sink.states[0][0];
        // 100 / 127 * 255 ≈ 200.7
        assert!(
            val > 190 && val < 210,
            "HTP merging failed: expected ~200, got {}",
            val
        );
    }
}

// Helper to allow downcasting MockSink in tests
impl MockSink {
    pub fn as_any(&self) -> &dyn std::any::Any {
        self
    }
}
