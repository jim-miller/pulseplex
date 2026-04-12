mod config;
mod setup;

use std::collections::HashMap;
use std::io::{self, IsTerminal};
use std::net::{SocketAddr, ToSocketAddrs, UdpSocket};
use std::path::PathBuf;
use std::str::FromStr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use pulseplex_core::{ArtNetBridge, DecayEnvelope, LightSink, PulsePlexEngine, Signal};
use pulseplex_hue::{HueOutputMapping, HueSink};
use pulseplex_midi::setup_midi;

use anyhow::bail;
use clap::{Parser, Subcommand};
use dialoguer::theme::ColorfulTheme;
use dialoguer::Select;
use notify::Watcher;
use ratatui::{
    backend::CrosstermBackend,
    layout::{Constraint, Direction, Layout},
    style::{Color, Style},
    text::{Line, Span},
    widgets::{Block, Borders, List, ListItem, Paragraph},
    Frame, Terminal,
};
use spin_sleep::SpinSleeper;
use tracing::{info, warn};
use tracing_subscriber::{fmt, prelude::*, EnvFilter};

use crate::config::{DmxOutputCompiled, TargetConfig};

/// RAII Guard to ensure terminal state is restored on drop.
struct TerminalGuard;

impl TerminalGuard {
    fn new() -> io::Result<Self> {
        crossterm::terminal::enable_raw_mode()?;
        if let Err(e) = crossterm::execute!(
            io::stdout(),
            crossterm::terminal::EnterAlternateScreen,
            crossterm::cursor::Hide
        ) {
            let _ = crossterm::terminal::disable_raw_mode();
            return Err(e);
        }
        Ok(Self)
    }
}

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        let _ = crossterm::execute!(
            io::stdout(),
            crossterm::terminal::LeaveAlternateScreen,
            crossterm::cursor::Show
        );
        let _ = crossterm::terminal::disable_raw_mode();
    }
}

use crate::config::{get_config_path, update_midi_device_in_config, PulsePlexConfig, ShutdownMode};

#[derive(Parser)]
#[command(
    name = "pulseplex",
    version,
    about = "MIDI to Art-Net bridge for drummers"
)]
struct Args {
    #[command(subcommand)]
    command: Option<Commands>,

    /// Enable verbose logging
    #[arg(short, long, global = true)]
    verbose: bool,

    /// Disable the TUI dashboard
    #[arg(long, global = true)]
    no_tui: bool,
}

#[derive(Subcommand)]
enum Commands {
    /// Start the PulsePlex daemon (default)
    Run {
        /// Path to configuration file (Overrides env vars)
        #[arg(short, long)]
        config: Option<String>,

        /// Force the interactive MIDI device selection prompt
        #[arg(short, long)]
        select_midi: bool,
    },
    /// Validate the configuration file and check for DMX collisions
    Check {
        /// Path to configuration file (Overrides env vars)
        #[arg(short, long)]
        config: Option<String>,
    },
}

/// Helper to build a 512-channel DMX frame from intensities.
/// Used by both ArtNetSink and the TUI dashboard.
#[derive(Clone, Default)]
pub struct DmxFrameBuilder {
    mappings: Vec<DmxOutputCompiled>,
}

impl DmxFrameBuilder {
    pub fn new(mappings: Vec<DmxOutputCompiled>) -> Self {
        Self { mappings }
    }

    pub fn build_frame(&self, intensities: &HashMap<usize, DecayEnvelope>) -> [u8; 512] {
        let mut frame = [0u8; 512];
        for m in &self.mappings {
            if let Some(env) = intensities.get(&m.internal_id) {
                if let Some([r, g, b]) = m.color {
                    let r_val = (r as f32 * env.intensity) as u8;
                    let g_val = (g as f32 * env.intensity) as u8;
                    let b_val = (b as f32 * env.intensity) as u8;

                    if m.dmx_channel < 512 {
                        frame[m.dmx_channel] = r_val;
                    }
                    if m.dmx_channel + 1 < 512 {
                        frame[m.dmx_channel + 1] = g_val;
                    }
                    if m.dmx_channel + 2 < 512 {
                        frame[m.dmx_channel + 2] = b_val;
                    }
                } else if m.dmx_channel < 512 {
                    frame[m.dmx_channel] = env.dmx_value();
                }
            }
        }
        frame
    }
}

/// A wrapper that implements LightSink for Art-Net UDP output.
pub struct ArtNetSink {
    socket: UdpSocket,
    addr: SocketAddr,
    bridge: ArtNetBridge,
    builder: DmxFrameBuilder,
}

impl ArtNetSink {
    pub fn new(
        universe: u16,
        target_ip: &str,
        mappings: Vec<DmxOutputCompiled>,
    ) -> anyhow::Result<Self> {
        let socket = UdpSocket::bind("0.0.0.0:0")?;
        socket.set_broadcast(true)?;
        let addr = target_ip
            .to_socket_addrs()?
            .next()
            .ok_or_else(|| anyhow::anyhow!("Could not parse target_ip address"))?;

        Ok(Self {
            socket,
            addr,
            bridge: ArtNetBridge::new(universe),
            builder: DmxFrameBuilder::new(mappings),
        })
    }
}

impl LightSink for ArtNetSink {
    fn send_state(&mut self, intensities: &HashMap<usize, DecayEnvelope>) -> anyhow::Result<()> {
        let frame = self.builder.build_frame(intensities);
        self.bridge.set_raw_data(&frame);
        self.socket.send_to(self.bridge.as_bytes(), self.addr)?;
        self.bridge.increment_sequence();
        Ok(())
    }
}

/// A sink that broadcasts frames to multiple underlying sinks.
#[derive(Default)]
pub struct BroadcastSink {
    sinks: Vec<Box<dyn LightSink>>,
}

impl BroadcastSink {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn add(&mut self, sink: Box<dyn LightSink>) {
        self.sinks.push(sink);
    }

    pub fn is_empty(&self) -> bool {
        self.sinks.is_empty()
    }
}

impl LightSink for BroadcastSink {
    fn send_state(&mut self, intensities: &HashMap<usize, DecayEnvelope>) -> anyhow::Result<()> {
        let mut sent_to_any = false;
        let mut first_err: Option<anyhow::Error> = None;

        for (idx, sink) in self.sinks.iter_mut().enumerate() {
            match sink.send_state(intensities) {
                Ok(()) => {
                    sent_to_any = true;
                }
                Err(err) => {
                    warn!("Broadcast sink {} send_state failed: {err:#}", idx);
                    if first_err.is_none() {
                        first_err = Some(err);
                    }
                }
            }
        }

        if sent_to_any || self.sinks.is_empty() {
            Ok(())
        } else {
            Err(first_err.unwrap_or_else(|| anyhow::anyhow!("All broadcast sinks failed")))
        }
    }
}

/// TUI State for real-time visualization.
struct DashboardState {
    dmx_channels: [u8; 512],
    recent_signals: Vec<(Instant, Signal)>,
    start_time: Instant,
    active_notes: usize,
}

impl DashboardState {
    fn new() -> Self {
        Self {
            dmx_channels: [0u8; 512],
            recent_signals: Vec::new(),
            start_time: Instant::now(),
            active_notes: 0,
        }
    }

    fn push_signal(&mut self, signal: Signal) {
        self.recent_signals.push((Instant::now(), signal));
        if self.recent_signals.len() > 10 {
            self.recent_signals.remove(0);
        }
    }
}

fn main() -> anyhow::Result<()> {
    let cli = Args::parse();

    // Setup logging (verbose flag is global)
    let filter = EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| EnvFilter::new(if cli.verbose { "trace" } else { "info" }));

    let registry = tracing_subscriber::registry().with(filter);

    if !cli.no_tui && std::io::stdout().is_terminal() {
        // Redirect logs to stderr when TUI is active to avoid corrupting the screen
        registry.with(fmt::layer().with_writer(io::stderr)).init();
    } else {
        registry.with(fmt::layer()).init();
    }
    match cli.command.unwrap_or(Commands::Run {
        config: None,
        select_midi: false,
    }) {
        Commands::Check { config } => {
            let path = get_config_path(config.as_ref())?;
            handle_check(path.to_string_lossy().as_ref())?;
        }
        Commands::Run {
            config,
            select_midi,
        } => {
            let path = get_config_path(config.as_ref())?;

            let path = if !path.exists() && config.is_none() {
                // If default config doesn't exist, run the setup wizard
                setup::run_wizard()?
            } else {
                path
            };

            // Ensure the configuration directory exists
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent)?;
            }

            run_daemon(path, select_midi, !cli.no_tui)?;
        }
    }

    Ok(())
}

fn run_daemon(config_path: PathBuf, force_select: bool, use_tui: bool) -> anyhow::Result<()> {
    // Canonicalize paths for consistent hot-reload comparison
    let config_path = if config_path.exists() {
        config_path.canonicalize()?
    } else {
        config_path
    };
    let config_path_str = config_path.to_string_lossy().to_string();

    let mut config = PulsePlexConfig::load(&config_path_str)?;

    // MIDI detection should follow setup_midi's substring matching
    let available_devices = pulseplex_midi::list_midi_devices()?;
    let current_spec = &config.midi.device_name;

    let device_found = if current_spec.is_empty() {
        false
    } else {
        // Match if any available device contains the config string
        available_devices.iter().any(|d| d.contains(current_spec))
    };

    if !device_found || force_select {
        if available_devices.is_empty() {
            bail!("No MIDI input devices detected.");
        }

        let chosen_device = if std::io::stdin().is_terminal() {
            // Interactive: Prompt user
            let selection = Select::with_theme(&ColorfulTheme::default())
                .with_prompt("Select MIDI device")
                .items(&available_devices)
                .default(0)
                .interact()?;
            available_devices[selection].clone()
        } else {
            // Non-interactive (systemd/CI): Auto-select if unambiguous
            if available_devices.len() == 1 {
                let dev = available_devices[0].clone();
                warn!("Headless mode: Auto-selecting only device: {}", dev);
                dev
            } else {
                bail!(
                    "Headless mode detected with no valid MIDI config.\n\
                     Available devices: {:?}\n\
                     Update 'device_name' in {} to continue.",
                    available_devices,
                    config_path_str
                );
            }
        };

        update_midi_device_in_config(&config_path_str, &chosen_device)?;
        config.midi.device_name = chosen_device;
    }

    // Hot-Reloading
    let (config_tx, config_rx) = crossbeam_channel::unbounded();
    let c_path = config_path.clone();
    let mut watcher = notify::recommended_watcher(move |res: notify::Result<notify::Event>| {
        if let Ok(event) = res {
            // Check if the modified file is our specific config file
            if event
                .paths
                .iter()
                .any(|p| p.canonicalize().map(|cp| cp == c_path).unwrap_or(false))
            {
                let _ = config_tx.send(());
            }
        }
    })?;

    // Watch the parent directory of the config file
    if let Some(parent) = config_path.parent() {
        watcher.watch(parent, notify::RecursiveMode::NonRecursive)?;
        info!("Watching for config changes in: {:?}", parent);
    }

    let target_hz = 40.0; // 25ms
    let target_interval = Duration::from_secs_f64(1.0 / target_hz);

    let running = Arc::new(AtomicBool::new(true));
    let r = running.clone();

    ctrlc::set_handler(move || {
        r.store(false, Ordering::SeqCst);
    })?;

    info!("Starting PulsePlex daemon");

    let mut compiled = config.compile()?;

    // Initialize all configured sinks into a broadcast group
    let mut main_sink = BroadcastSink::new();

    // Dedicated frame builder for the dashboard to avoid extra UDP sockets
    let mut dashboard_frame_builder = DmxFrameBuilder::new(compiled.dmx_outputs.clone());

    for target in &compiled.targets {
        match target {
            TargetConfig::ArtNet(artnet) => {
                let sink = ArtNetSink::new(
                    artnet.universe,
                    &artnet.target_ip,
                    compiled.dmx_outputs.clone(),
                )?;
                main_sink.add(Box::new(sink));
                info!("Initialized Art-Net sink for universe {}", artnet.universe);
            }
            TargetConfig::Hue(hue) => {
                let hue_mappings = compiled
                    .hue_outputs
                    .iter()
                    .map(|h| HueOutputMapping {
                        internal_id: h.internal_id,
                        channel_id: h.channel_id,
                        color: h.color,
                    })
                    .collect();
                let sink = HueSink::new(
                    hue.bridge_ip.clone(),
                    hue.username.clone(),
                    hue.client_key.clone(),
                    hue.area_id.clone(),
                    hue_mappings,
                )?;
                main_sink.add(Box::new(sink));
                info!("Initialized Philips Hue sink for bridge {}", hue.bridge_ip);
            }
        }
    }

    if main_sink.is_empty() {
        bail!("No valid lighting targets configured.");
    }

    let mut midi_source = setup_midi(&compiled.midi_device, compiled.midi_id_map.clone())?;

    let mut engine = PulsePlexEngine::new(compiled.behaviors.clone());

    let sleeper = SpinSleeper::default();

    let mut initial_state = [0u8; 512];
    if matches!(config.shutdown.mode, ShutdownMode::Restore) {
        info!("Capturing current lighting state for later restoration...");

        // Temporarily bind to the specific Art-Net port just to grab a snapshot
        if let Ok(listener) = UdpSocket::bind("0.0.0.0:6454") {
            listener.set_read_timeout(Some(Duration::from_millis(1000)))?;
            let mut buf = [0u8; 1024];
            if let Ok((amt, _)) = listener.recv_from(&mut buf) {
                if amt >= 530 {
                    if let Some(dmx_data) = buf[..amt].get(18..530) {
                        initial_state.copy_from_slice(dmx_data);
                        info!("Successfully captured background DMX state.");
                    }
                } else {
                    warn!("Received short Art-Net packet while capturing background state. Restore state will be a blackout.");
                }
            } else {
                warn!("Timeout waiting for background Art-Net traffic. Restore state will be a blackout.");
            }
        } else {
            warn!("Could not bind to port 6454 to capture state. Is another lighting software running?");
        }
    }

    // TUI Setup
    let _guard = if use_tui && io::stdout().is_terminal() {
        Some(TerminalGuard::new()?)
    } else {
        None
    };

    let mut terminal = if _guard.is_some() {
        Some(Terminal::new(CrosstermBackend::new(io::stdout()))?)
    } else {
        None
    };

    let mut dashboard_state = DashboardState::new();
    let mut last_tick = Instant::now();
    let mut next_deadline = last_tick + target_interval;

    // Reused buffer for signals
    let mut signal_buffer = Vec::with_capacity(64);

    while running.load(Ordering::SeqCst) {
        let now = Instant::now();
        let delta_time = now.duration_since(last_tick);
        last_tick = now;

        // Poll for TUI events (like exit keys)
        if terminal.is_some() {
            while crossterm::event::poll(Duration::ZERO)? {
                if let crossterm::event::Event::Key(key) = crossterm::event::read()? {
                    if key.code == crossterm::event::KeyCode::Char('q')
                        || (key.code == crossterm::event::KeyCode::Char('c')
                            && key
                                .modifiers
                                .contains(crossterm::event::KeyModifiers::CONTROL))
                    {
                        running.store(false, Ordering::SeqCst);
                    }
                }
            }
        }

        // Drain the hot-reload channel completely to debounce rapid file save events
        let mut needs_reload = false;
        while config_rx.try_recv().is_ok() {
            needs_reload = true;
        }

        if needs_reload {
            info!("Reloading configuration...");
            match PulsePlexConfig::load(&config_path_str) {
                Ok(new_config) => {
                    match new_config.compile() {
                        Ok(new_compiled) => {
                            // Update MIDI source with new ID map
                            match setup_midi(
                                &new_compiled.midi_device,
                                new_compiled.midi_id_map.clone(),
                            ) {
                                Ok(new_midi_source) => {
                                    let mut applied_compiled = new_compiled;

                                    // Re-initialize sinks
                                    let mut new_main_sink = BroadcastSink::new();

                                    for target in &applied_compiled.targets {
                                        match target {
                                            TargetConfig::ArtNet(artnet) => {
                                                match ArtNetSink::new(
                                                    artnet.universe,
                                                    &artnet.target_ip,
                                                    applied_compiled.dmx_outputs.clone(),
                                                ) {
                                                    Ok(sink) => {
                                                        new_main_sink.add(Box::new(sink));
                                                    }
                                                    Err(e) => {
                                                        warn!(
                                                            "Hot reload failed to recreate Art-Net target (universe={}, target_ip={}): {}",
                                                            artnet.universe,
                                                            artnet.target_ip,
                                                            e
                                                        );
                                                    }
                                                }
                                            }
                                            TargetConfig::Hue(hue) => {
                                                let hue_mappings = applied_compiled
                                                    .hue_outputs
                                                    .iter()
                                                    .map(|h| HueOutputMapping {
                                                        internal_id: h.internal_id,
                                                        channel_id: h.channel_id,
                                                        color: h.color,
                                                    })
                                                    .collect();
                                                match HueSink::new(
                                                    hue.bridge_ip.clone(),
                                                    hue.username.clone(),
                                                    hue.client_key.clone(),
                                                    hue.area_id.clone(),
                                                    hue_mappings,
                                                ) {
                                                    Ok(sink) => new_main_sink.add(Box::new(sink)),
                                                    Err(e) => warn!(
                                                        "Hot reload failed to recreate Hue sink: {}",
                                                        e
                                                    ),
                                                }
                                            }
                                        }
                                    }
                                    if !new_main_sink.is_empty() {
                                        main_sink = new_main_sink;
                                        dashboard_frame_builder = DmxFrameBuilder::new(applied_compiled.dmx_outputs.clone());
                                    } else {
                                        warn!("Hot reload failed to initialize any targets. Keeping old targets.");
                                        applied_compiled.targets = compiled.targets.clone();
                                    }

                                    midi_source = new_midi_source;
                                    engine =
                                        PulsePlexEngine::new(applied_compiled.behaviors.clone());
                                    config = new_config;
                                    compiled = applied_compiled;
                                    info!("Reload successful.");
                                }
                                Err(e) => warn!("Reload failed: could not recreate MIDI source: {}. Keeping previous configuration.", e),
                            }
                        }
                        Err(e) => warn!(
                            "Reload failed: compilation error: {}. Keeping previous configuration.",
                            e
                        ),
                    }
                }
                Err(e) => {
                    warn!("Reload failed: {}. Keeping previous configuration.", e);
                }
            }
        }

        // Output and processing - Reusing buffers
        if let Err(e) = engine.process_tick(
            delta_time,
            &mut midi_source,
            &mut [&mut main_sink],
            &mut signal_buffer,
        ) {
            warn!("Engine error: {}. Initiating shutdown...", e);
            running.store(false, Ordering::SeqCst);
            break;
        }

        // Update TUI state
        if let Some(ref mut term) = terminal {
            for signal in &signal_buffer {
                dashboard_state.push_signal(*signal);
            }

            // Safely compute DMX visualization from logic state
            dashboard_state.dmx_channels =
                dashboard_frame_builder.build_frame(engine.active_lights());

            dashboard_state.active_notes = engine.active_lights_count();
            term.draw(|f| ui(f, &dashboard_state))?;
        }

        let sleep_start = Instant::now();
        if sleep_start < next_deadline {
            sleeper.sleep(next_deadline.duration_since(sleep_start));
        } else if terminal.is_none() {
            warn!(
                "Frame drop detected! Work took longer than {:?}",
                target_interval
            );
        }

        // advance the deadline for the next frame
        next_deadline = Instant::now().max(next_deadline) + target_interval;
    }

    if let Err(err) = perform_shutdown(&config.shutdown, &mut main_sink, &initial_state, &compiled)
    {
        warn!("Failed to perform shutdown lighting action: {err}");
    }

    info!("PulsePlex shut down cleanly.");
    Ok(())
}

fn ui(f: &mut Frame, state: &DashboardState) {
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(3), // Header
            Constraint::Min(10),   // Main View (DMX + Signals)
            Constraint::Length(3), // Footer
        ])
        .split(f.area());

    // Header
    let uptime = state.start_time.elapsed();
    let header = Paragraph::new(format!(
        " PulsePlex v{} | Uptime: {}s | Active Envelopes: {}",
        env!("CARGO_PKG_VERSION"),
        uptime.as_secs(),
        state.active_notes
    ))
    .block(Block::default().borders(Borders::ALL).title(" Status "));
    f.render_widget(header, chunks[0]);

    // Main View
    let main_chunks = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(70), Constraint::Percentage(30)])
        .split(chunks[1]);

    // DMX Visualization (Intensity grid)
    let dmx_block = Block::default()
        .borders(Borders::ALL)
        .title(" DMX Output (Universe 0) ");
    let mut dmx_lines = Vec::new();
    for row in 0..16 {
        let mut spans = Vec::new();
        for col in 0..32 {
            let idx = row * 32 + col;
            let val = state.dmx_channels[idx];
            let color = if val == 0 {
                Color::DarkGray
            } else {
                Color::Rgb(val, val, val)
            };
            spans.push(Span::styled("■ ", Style::default().fg(color)));
        }
        dmx_lines.push(Line::from(spans));
    }
    let dmx_para = Paragraph::new(dmx_lines).block(dmx_block);
    f.render_widget(dmx_para, main_chunks[0]);

    // Signal Log
    let signal_items: Vec<ListItem> = state
        .recent_signals
        .iter()
        .rev()
        .map(|(_, sig)| {
            let text = match sig {
                Signal::Trigger { id, velocity } => format!("Trigger ID: {} Vel: {}", id, velocity),
                Signal::Release { id } => format!("Release ID: {}", id),
                Signal::Clock => "Clock Tick".to_string(),
            };
            ListItem::new(text)
        })
        .collect();
    let signal_list = List::new(signal_items).block(
        Block::default()
            .borders(Borders::ALL)
            .title(" Recent Signals "),
    );
    f.render_widget(signal_list, main_chunks[1]);

    // Footer
    let footer = Paragraph::new(" Press 'q' or Ctrl+C to Exit | Hot-reloading config active ")
        .block(Block::default().borders(Borders::ALL));
    f.render_widget(footer, chunks[2]);
}

fn perform_shutdown(
    config: &crate::config::ShutdownConfig,
    sink: &mut dyn LightSink,
    initial_state: &[u8; 512],
    compiled: &crate::config::CompiledConfig,
) -> anyhow::Result<()> {
    let mut shutdown_intensities = HashMap::new();

    match config.mode {
        ShutdownMode::Blackout => {
            info!("Shutting down: Blackout");
            // intensities is already empty -> blackout
        }
        ShutdownMode::Default => {
            info!("Shutting down: Applying default scene");
            if let Some(defaults) = &config.defaults {
                for m in &compiled.dmx_outputs {
                    let mut max_intensity: f32 = 0.0;

                    if let Some(base_color) = m.color {
                        // For RGB, check all 3 channels and derive max intensity relative to base color
                        for (offset, &base_val) in base_color.iter().enumerate() {
                            if base_val > 0 {
                                if let Some(&default_val) = defaults.get(&(m.dmx_channel + offset))
                                {
                                    let intensity = default_val as f32 / base_val as f32;
                                    max_intensity = max_intensity.max(intensity);
                                }
                            }
                        }
                    } else {
                        // For grayscale, just check the single channel
                        if let Some(&default_val) = defaults.get(&m.dmx_channel) {
                            max_intensity = default_val as f32 / 255.0;
                        }
                    }

                    if max_intensity > 0.0 {
                        let mut env = pulseplex_core::DecayEnvelope::new(
                            1.0,
                            Default::default(),
                            Default::default(),
                        );
                        env.intensity = max_intensity.min(1.0);
                        shutdown_intensities.insert(m.internal_id, env);
                    }
                }
            }
        }
        ShutdownMode::Restore => {
            info!("Shutting down: Restoring previous state");
            for m in &compiled.dmx_outputs {
                let mut max_intensity: f32 = 0.0;

                if let Some(base_color) = m.color {
                    for (offset, &base_val) in base_color.iter().enumerate() {
                        if base_val > 0 {
                            if let Some(&captured_val) = initial_state.get(m.dmx_channel + offset) {
                                let intensity = captured_val as f32 / base_val as f32;
                                max_intensity = max_intensity.max(intensity);
                            }
                        }
                    }
                } else if let Some(&captured_val) = initial_state.get(m.dmx_channel) {
                    max_intensity = captured_val as f32 / 255.0;
                }

                if max_intensity > 0.0 {
                    let mut env = pulseplex_core::DecayEnvelope::new(
                        1.0,
                        Default::default(),
                        Default::default(),
                    );
                    env.intensity = max_intensity.min(1.0);
                    shutdown_intensities.insert(m.internal_id, env);
                }
            }
        }
    }

    sink.send_state(&shutdown_intensities)?;
    Ok(())
}

fn handle_check(path: &str) -> anyhow::Result<()> {
    info!("Checking configuration: {}", path);

    // 1. Test TOML Parsing
    let config = PulsePlexConfig::load(path)?;
    println!("✅ TOML Structure: Valid");

    // 2. Validate Logical ID chain via Compile
    match config.compile() {
        Ok(_) => {
            println!("✅ 3-Tier Mapping: All logical IDs and behaviors are linked correctly");
        }
        Err(e) => {
            println!("❌ Configuration Error: {}", e);
            bail!("Configuration check failed.");
        }
    }

    // 3. Test Network Strings for all targets
    if config.targets.is_empty() {
        println!("❌ Error: No lighting targets configured.");
        bail!("Configuration check failed.");
    }

    for target in &config.targets {
        match target {
            TargetConfig::ArtNet(artnet) => match SocketAddr::from_str(&artnet.target_ip) {
                Ok(_) => println!("✅ Network (Art-Net): Target IP/Port format is valid"),
                Err(_) => {
                    if artnet.target_ip.to_socket_addrs().is_ok() {
                        println!(
                            "⚠️  Network: '{}' is a valid hostname/shorthand, but not a literal IP",
                            artnet.target_ip
                        );
                    } else {
                        println!(
                            "❌ Network: Invalid target_ip format '{}'",
                            artnet.target_ip
                        );
                        bail!("Configuration check failed.");
                    }
                }
            },
            TargetConfig::Hue(hue) => {
                println!(
                    "⚠️  Network (Hue): bridge_ip={}, area_id={}",
                    hue.bridge_ip, hue.area_id
                );
            }
        }
    }

    // 4. Test DMX Bounds and Overlaps
    let mut used_channels = HashMap::new();
    let mut errors = false;
    for dmx in &config.output.dmx {
        let span = if dmx.color.is_some() { 3 } else { 1 };
        for offset in 0..span {
            let channel = dmx.channel + offset;
            if channel > 511 {
                println!(
                    "❌ DMX: ID {} (channel {}) exceeds universe limit (511)",
                    dmx.id, channel
                );
                errors = true;
            }
            if let Some(other_id) = used_channels.insert(channel, &dmx.id) {
                warn!(
                    "DMX Collision: Channel {} is used by both ID '{}' and ID '{}'",
                    channel, other_id, dmx.id
                );
            }
        }
    }

    if errors {
        bail!("Configuration check failed due to DMX errors.");
    }

    println!("✅ DMX Mappings: All channels within 0-511 range");
    info!("Configuration check passed.");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    use pulseplex_core::MockSink;

    struct SharedMockSink {
        inner: Arc<Mutex<MockSink>>,
    }

    impl SharedMockSink {
        fn new(inner: Arc<Mutex<MockSink>>) -> Self {
            Self { inner }
        }
    }

    impl LightSink for SharedMockSink {
        fn send_state(
            &mut self,
            intensities: &HashMap<usize, DecayEnvelope>,
        ) -> anyhow::Result<()> {
            self.inner.lock().unwrap().send_state(intensities)
        }
    }

    struct FailingSink {
        calls: Arc<Mutex<usize>>,
    }

    impl FailingSink {
        fn new(calls: Arc<Mutex<usize>>) -> Self {
            Self { calls }
        }
    }

    impl LightSink for FailingSink {
        fn send_state(
            &mut self,
            _intensities: &HashMap<usize, DecayEnvelope>,
        ) -> anyhow::Result<()> {
            let mut calls = self.calls.lock().unwrap();
            *calls += 1;
            Err(anyhow::anyhow!("intentional sink failure"))
        }
    }

    #[test]
    fn broadcast_sink_forwards_frames_to_all_sinks() {
        let first = Arc::new(Mutex::new(MockSink::default()));
        let second = Arc::new(Mutex::new(MockSink::default()));
        let mut broadcast = BroadcastSink::new();

        broadcast.add(Box::new(SharedMockSink::new(first.clone())));
        broadcast.add(Box::new(SharedMockSink::new(second.clone())));

        let mut intensities = HashMap::new();
        intensities.insert(
            1,
            DecayEnvelope::new(1.0, Default::default(), Default::default()),
        );

        broadcast.send_state(&intensities).unwrap();

        assert_eq!(first.lock().unwrap().states.len(), 1);
        assert_eq!(second.lock().unwrap().states.len(), 1);
    }

    #[test]
    fn broadcast_sink_continues_on_individual_failure() {
        let first = Arc::new(Mutex::new(MockSink::default()));
        let last = Arc::new(Mutex::new(MockSink::default()));
        let failing_calls = Arc::new(Mutex::new(0));
        let mut broadcast = BroadcastSink::new();

        broadcast.add(Box::new(SharedMockSink::new(first.clone())));
        broadcast.add(Box::new(FailingSink::new(failing_calls.clone())));
        broadcast.add(Box::new(SharedMockSink::new(last.clone())));

        let intensities = HashMap::new();

        // Should return Ok because at least one sink succeeded
        let result = broadcast.send_state(&intensities);

        assert!(result.is_ok());
        assert_eq!(*failing_calls.lock().unwrap(), 1);

        assert_eq!(first.lock().unwrap().states.len(), 1);
        assert_eq!(last.lock().unwrap().states.len(), 1);
    }

    #[test]
    fn test_artnet_sink_creation() {
        let sink = ArtNetSink::new(1, "127.0.0.1:6454", vec![]);
        assert!(sink.is_ok());

        let invalid_sink = ArtNetSink::new(1, "invalid_ip", vec![]);
        assert!(invalid_sink.is_err());
    }

    #[test]
    fn test_artnet_sink_send_state() {
        let mut sink = ArtNetSink::new(1, "127.0.0.1:6454", vec![]).unwrap();
        let intensities = HashMap::new();
        let result = sink.send_state(&intensities);
        assert!(result.is_ok());
    }

    #[test]
    fn test_dmx_frame_builder_rgb_and_grayscale() {
        use crate::config::DmxOutputCompiled;
        use pulseplex_core::{DecayEnvelope, DecayProfile, VelocityCurve};

        let mappings = vec![
            DmxOutputCompiled {
                internal_id: 1,
                dmx_channel: 0,
                color: None,
            },
            DmxOutputCompiled {
                internal_id: 2,
                dmx_channel: 5,
                color: Some([255, 128, 0]),
            },
        ];

        let builder = DmxFrameBuilder::new(mappings);
        let mut intensities = HashMap::new();

        let mut env1 = DecayEnvelope::new(1.0, VelocityCurve::Linear, DecayProfile::Linear);
        env1.intensity = 1.0;
        let mut env2 = DecayEnvelope::new(1.0, VelocityCurve::Linear, DecayProfile::Linear);
        env2.intensity = 0.5;

        intensities.insert(1, env1);
        intensities.insert(2, env2);

        let frame = builder.build_frame(&intensities);

        assert_eq!(frame[0], 255);
        assert_eq!(frame[5], 127); // 255 * 0.5
        assert_eq!(frame[6], 64); // 128 * 0.5
        assert_eq!(frame[7], 0); // 0 * 0.5
    }

    #[test]
    fn test_perform_shutdown_restore() {
        use crate::config::{CompiledConfig, DmxOutputCompiled};
        use pulseplex_core::MockSink;

        let dmx_outputs = vec![
            DmxOutputCompiled {
                internal_id: 100,
                dmx_channel: 0,
                color: None,
            },
            DmxOutputCompiled {
                internal_id: 101,
                dmx_channel: 10,
                color: Some([200, 0, 0]),
            },
        ];

        let config = crate::config::ShutdownConfig {
            mode: ShutdownMode::Restore,
            defaults: None,
        };
        let compiled = CompiledConfig {
            midi_device: "".to_string(),
            midi_id_map: HashMap::new(),
            behaviors: HashMap::new(),
            dmx_outputs,
            hue_outputs: vec![],
            targets: vec![],
        };

        let mut sink = MockSink::default();
        let mut initial_state = [0u8; 512];
        initial_state[0] = 255;
        initial_state[10] = 100; // 100 relative to base 200 = 0.5 intensity

        perform_shutdown(&config, &mut sink, &initial_state, &compiled).unwrap();

        assert_eq!(sink.states.len(), 1);
        // Channel 0 maps to ID 100
        assert!((*sink.states[0].get(&100).unwrap() - 1.0).abs() < 0.01);
        // Channel 10 maps to ID 101
        assert!((*sink.states[0].get(&101).unwrap() - 0.5).abs() < 0.01);
    }

    #[test]
    fn test_perform_shutdown_default() {
        use crate::config::{CompiledConfig, DmxOutputCompiled};
        use pulseplex_core::MockSink;

        let dmx_outputs = vec![
            DmxOutputCompiled {
                internal_id: 200,
                dmx_channel: 5,
                color: None,
            },
            DmxOutputCompiled {
                internal_id: 201,
                dmx_channel: 10,
                color: Some([0, 255, 0]),
            },
        ];

        let mut defaults = HashMap::new();
        defaults.insert(5, 255);
        defaults.insert(11, 128); // Green channel (+1) relative to base 255 = ~0.5 intensity

        let config = crate::config::ShutdownConfig {
            mode: ShutdownMode::Default,
            defaults: Some(defaults),
        };
        let compiled = CompiledConfig {
            midi_device: "".to_string(),
            midi_id_map: HashMap::new(),
            behaviors: HashMap::new(),
            dmx_outputs,
            hue_outputs: vec![],
            targets: vec![],
        };

        let mut sink = MockSink::default();
        let initial_state = [0u8; 512];

        perform_shutdown(&config, &mut sink, &initial_state, &compiled).unwrap();

        assert_eq!(sink.states.len(), 1);
        // Channel 5 maps to ID 200
        assert!((*sink.states[0].get(&200).unwrap() - 1.0).abs() < 0.01);
        // Channel 11 maps to Green of ID 201
        assert!((*sink.states[0].get(&201).unwrap() - 0.50).abs() < 0.01);
    }
}
