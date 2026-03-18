use std::time::Duration;

pub struct DecayEnvelope {
    pub intensity: f32,        // 0.0 to 1.0
    pub decay_per_second: f32, // e.g., 0.90
}

impl DecayEnvelope {
    pub fn new(decay_seconds: f32) -> Self {
        Self {
            intensity: 0.0,
            decay_per_second: 1.0 / decay_seconds.max(0.001),
        }
    }

    /// Trigger the envelope with a MIDI velocity (0-127)
    pub fn trigger(&mut self, velocity: u8) {
        // Map 0..127 to 0.0..1.0
        self.intensity = velocity as f32 / 127.0;
    }

    /// Progress the decay by one tick
    pub fn tick(&mut self, dt: Duration) {
        let dt_seconds = dt.as_secs_f32();
        let reduction = self.decay_per_second * dt_seconds;

        self.intensity = (self.intensity - reduction).max(0.0);
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
}

#[cfg(test)]
mod test {
    use std::time::Duration;

    use crate::DecayEnvelope;

    #[test]
    fn test_time_aware_decay() {
        let mut envelope = DecayEnvelope::new(1.0);
        envelope.trigger(127);

        // After 1/2 second, intensity should be roughly half
        envelope.tick(Duration::from_millis(500));
        assert!((envelope.intensity - 0.5).abs() < 0.01);
        assert_eq!(envelope.dmx_value(), 127);
    }
}
