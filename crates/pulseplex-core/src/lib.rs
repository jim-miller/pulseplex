use std::time::Duration;

use serde::Deserialize;

#[derive(Debug, Clone, Copy, Default, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum VelocityCurve {
    #[default]
    Linear,
    Hard,
    Soft,
}

#[derive(Debug, Clone, Copy, Default, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum DecayProfile {
    #[default]
    Linear,
    Exponential,
}

/// Generic signals that can drive the lighting engine.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Signal {
    /// A trigger event (e.g., MIDI Note On) with a payload (e.g., note number) and velocity.
    Trigger { id: u8, velocity: u8 },
    /// A release event (e.g., MIDI Note Off)
    Release { id: u8 },
    /// A clock/tick event for synchronization
    Clock,
}

/// Interface for outputting lighting state to hardware or protocols.
pub trait LightSink: Send {
    /// Send the complete frame state (512 channels) to the output.
    fn send_state(&mut self, state: &[u8; 512]) -> anyhow::Result<()>;
}

/// Interface for receiving signals from hardware or protocols.
pub trait EventSource: Send {
    /// Poll for the next available signals. Non-blocking.
    fn poll(&mut self) -> anyhow::Result<Vec<Signal>>;
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
    }

    /// Progress the decay by one tick
    pub fn tick(&mut self, dt: Duration) {
        let dt_seconds = dt.as_secs_f32();
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
    }

    pub fn is_dead(&self) -> bool {
        self.intensity <= 0.0
    }

    pub fn dmx_value(&self) -> u8 {
        (self.intensity * 255.0) as u8
    }
}

#[derive(Debug)]
pub struct ArtNetBridge {
    buffer: [u8; 530],
}

impl ArtNetBridge {
    pub fn new(universe: u16) -> Self {
        let mut buf = [0u8; 530];

        // Fixed Head ID
        buf[0..8].copy_from_slice(b"Art-Net\0");

        // OpCode (0x5000, little endian)
        buf[8] = 0x00;
        buf[9] = 0x50;

        // ProtVer
        buf[10] = 0x00;
        buf[11] = 0x0E;

        // Sequence
        // Physical port
        buf[13] = 0x00;

        // Universe (little endian)
        let uni_bytes = universe.to_le_bytes();
        buf[14] = uni_bytes[0];
        buf[15] = uni_bytes[1];

        // Length of DMX data (512, big-endian)
        buf[16] = 0x02;
        buf[17] = 0x00;

        // Data
        Self { buffer: buf }
    }

    pub fn set_channel(&mut self, channel: usize, value: u8) {
        if channel < 512 {
            self.buffer[18 + channel] = value;
        }
    }

    /// Clear channel data but keep the header
    pub fn clear_data(&mut self) {
        for i in 18..530 {
            self.buffer[i] = 0;
        }
    }

    /// Access raw bytes to send over UDP
    pub fn as_bytes(&self) -> &[u8] {
        &self.buffer
    }

    /// Optional but helps receivers
    pub fn increment_sequence(&mut self) {
        self.buffer[12] = self.buffer[12].wrapping_add(1);
    }

    pub fn set_raw_data(&mut self, initial_state: &[u8; 512]) {
        (0..initial_state.len()).for_each(|i| {
            self.set_channel(i, initial_state[i]);
        });
    }

    /// Extract the 512-byte DMX payload
    pub fn dmx_data(&self) -> &[u8; 512] {
        self.buffer[18..530].try_into().unwrap()
    }
}

/// A Mock Sink for testing. Records frames sent to it.
#[derive(Default)]
pub struct MockSink {
    pub frames: Vec<[u8; 512]>,
}

impl LightSink for MockSink {
    fn send_state(&mut self, state: &[u8; 512]) -> anyhow::Result<()> {
        self.frames.push(*state);
        Ok(())
    }
}

/// A Mock Source for testing.
pub struct MockSource {
    queue: Vec<Vec<Signal>>,
}

impl MockSource {
    pub fn new(timeline: Vec<Vec<Signal>>) -> Self {
        let mut queue = timeline;
        queue.reverse(); // Reverse so we can pop
        Self { queue }
    }
}

impl EventSource for MockSource {
    fn poll(&mut self) -> anyhow::Result<Vec<Signal>> {
        Ok(self.queue.pop().unwrap_or_default())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

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

    #[test]
    fn test_mock_integration() {
        let timeline = vec![
            vec![Signal::Trigger {
                id: 1,
                velocity: 127,
            }], // Frame 0
            vec![], // Frame 1
            vec![], // Frame 2
            vec![Signal::Trigger {
                id: 2,
                velocity: 64,
            }], // Frame 3
        ];

        let mut source = MockSource::new(timeline);
        let mut sink = MockSink::default();

        let mut active_lights: std::collections::HashMap<u8, DecayEnvelope> =
            std::collections::HashMap::new();
        let dt = Duration::from_millis(25); // 40Hz

        // Run 5 frames
        for _ in 0..5 {
            // 1. Poll
            let signals = source.poll().unwrap();
            for s in signals {
                if let Signal::Trigger { id, velocity } = s {
                    let mut env =
                        DecayEnvelope::new(0.1, VelocityCurve::Linear, DecayProfile::Linear);
                    env.trigger(velocity);
                    active_lights.insert(id, env);
                }
            }

            // 2. Tick
            active_lights.retain(|_, env| {
                env.tick(dt);
                !env.is_dead()
            });

            // 3. Build Frame
            let mut frame = [0u8; 512];
            for (id, env) in &active_lights {
                if *id == 1 {
                    frame[0] = env.dmx_value();
                } else if *id == 2 {
                    frame[1] = env.dmx_value();
                }
            }

            // 4. Sink
            sink.send_state(&frame).unwrap();
        }

        // Frame 0: Trigger id:1 at 127. Intensity 1.0 -> 0.75 after tick. DMX ~191
        assert!(sink.frames[0][0] > 180);
        // Frame 1: id:1 decays 0.75 -> 0.5. DMX ~127
        assert!(sink.frames[1][0] > 120);
        // Frame 3: Trigger id:2 at 64. Intensity ~0.5. Tick: 0.5 -> 0.25. DMX ~63
        assert!(sink.frames[3][1] > 60 && sink.frames[3][1] < 70);
    }
}
