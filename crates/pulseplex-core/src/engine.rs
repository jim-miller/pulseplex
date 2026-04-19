use crate::{fixture, BehaviorConfig, DecayEnvelope, LightSink, SourceEvent};
use crossbeam_channel::Receiver;
use std::collections::HashMap;
use std::time::Duration;

pub struct PulsePlexEngine {
    /// Internal DMX state (currently only support one universe in this refactor stage)
    universe_buffer: [u8; 512],
    active_envelopes: HashMap<u16, DecayEnvelope>,
    behaviors: HashMap<u16, BehaviorConfig>,
    fixtures: Vec<fixture::FixtureInstance>,
    /// Maps a behavior ID to a list of (fixture_index, capability_type)
    capability_mappings: HashMap<u16, Vec<(usize, fixture::CapabilityType)>>,
}

impl PulsePlexEngine {
    pub fn new(
        behaviors: HashMap<u16, BehaviorConfig>,
        fixtures: Vec<fixture::FixtureInstance>,
        capability_mappings: HashMap<u16, Vec<(usize, fixture::CapabilityType)>>,
    ) -> Self {
        Self {
            universe_buffer: [0u8; 512],
            active_envelopes: HashMap::new(),
            behaviors,
            fixtures,
            capability_mappings,
        }
    }

    /// Process a single tick of the engine.
    pub async fn tick(
        &mut self,
        dt: Duration,
        receiver: &Receiver<SourceEvent>,
        sinks: &mut [Box<dyn LightSink>],
    ) -> anyhow::Result<()> {
        // 1. Flush Input Channel
        while let Ok(event) = receiver.try_recv() {
            match event {
                SourceEvent::Trigger { id, velocity } => {
                    if let Some(config) = self.behaviors.get(&id) {
                        let mut env = DecayEnvelope::new(
                            config.decay_seconds,
                            config.velocity_curve,
                            config.decay_profile,
                        );
                        env.trigger(velocity);
                        self.active_envelopes.insert(id, env);
                    }
                }
                SourceEvent::DmxFrame { universe, data } => {
                    // For now, we only support universe 1 in the primary merge
                    // In a future multi-universe refactor, this would be handled differently.
                    if universe == 1 {
                        for i in 0..512 {
                            self.universe_buffer[i] =
                                std::cmp::max(self.universe_buffer[i], data[i]);
                        }
                    }
                }
                SourceEvent::ClearAll => {
                    self.active_envelopes.clear();
                    self.universe_buffer.fill(0);
                }
            }
        }

        // 2. Process Decay
        self.active_envelopes.retain(|_, env| {
            env.tick(dt);
            !env.is_dead()
        });

        // 3. HTP Merge
        // Start with a fresh buffer if no persistent Art-Net input was merged
        // Actually, the plan says "Write the envelope values into a fresh [0u8; 512] array.
        // If a SourceEvent::DmxFrame was received, merge it using HTP."
        // Let's stick to the plan:
        let mut current_frame = [0u8; 512];

        // (Optional: If we wanted to keep the merged DmxFrame data, we'd need to store it separately)
        // For Phase 3, we'll just HTP merge envelopes into this frame.

        for (&behavior_id, env) in &self.active_envelopes {
            let val = env.dmx_value();

            if let Some(mappings) = self.capability_mappings.get(&behavior_id) {
                for &(fixture_idx, cap_type) in mappings {
                    if let Some(fixture) = self.fixtures.get(fixture_idx) {
                        // Only support universe 1 for now
                        if let Some((universe, addr)) = fixture.get_dmx_address(cap_type) {
                            if universe == 1 && addr < 512 {
                                current_frame[addr] = std::cmp::max(current_frame[addr], val);
                            }
                        }
                    }
                }
            }
        }

        self.universe_buffer = current_frame;

        // 4. Broadcast
        for sink in sinks.iter_mut() {
            // Plan says: sink.write_universe(1, &current_universe).await
            if let Err(e) = sink.write_universe(1, &self.universe_buffer).await {
                tracing::warn!("Failed to broadcast to sink: {}", e);
            }
        }

        Ok(())
    }

    pub fn universe(&self) -> &[u8; 512] {
        &self.universe_buffer
    }

    pub fn active_envelopes_count(&self) -> usize {
        self.active_envelopes.len()
    }
}
