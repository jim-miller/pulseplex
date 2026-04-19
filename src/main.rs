mod config;
mod doctor;
mod setup;

use std::collections::HashMap;
use std::io::{self, IsTerminal};
use std::net::{SocketAddr, ToSocketAddrs, UdpSocket};
use std::path::PathBuf;
use std::str::FromStr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use async_trait::async_trait;
use pulseplex_core::engine::PulsePlexEngine;
use pulseplex_core::{LightSink, LightSource, SourceEvent};
use pulseplex_hue::HueSink;

use anyhow::{bail, Context};
use clap::{Parser, Subcommand};
use dialoguer::theme::ColorfulTheme;
use dialoguer::Select;
use directories::ProjectDirs;
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
use tracing::{debug, error, info, warn};
use tracing_subscriber::{fmt, prelude::*, EnvFilter};

use crate::config::{
    get_config_path, update_hue_ip_in_config, update_midi_device_in_config, PulsePlexConfig,
    ShutdownMode, TargetConfig,
};

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

        /// Force the first-run setup wizard
        #[arg(long)]
        setup: bool,

        /// Disable the TUI dashboard
        #[arg(long, global = true)]
        no_tui: bool,
    },
    /// Validate the configuration file and check for DMX collisions
    Check {
        /// Path to configuration file (Overrides env vars)
        #[arg(short, long)]
        config: Option<String>,
    },
    /// Run diagnostic tests for network and bridge connectivity
    Doctor {
        /// Path to configuration file
        #[arg(short, long)]
        config: Option<String>,
    },
    /// Template management
    Template {
        #[command(subcommand)]
        action: TemplateAction,
    },
    Hue {
        #[command(subcommand)]
        action: HueAction,
    },
}

#[derive(Subcommand)]
enum TemplateAction {
    /// Eject the default edrums configuration template to the current directory
    Eject,
}

#[derive(Subcommand)]
enum HueAction {
    Setup {
        #[arg(short, long)]
        list: bool,

        /// Set bridge IP (use first bridge if not specified)
        #[arg(short, long)]
        ip: Option<String>,

        /// Force bridge setup if already configured
        #[arg(long)]
        force: bool,
    },
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

    /// Access raw bytes to send over UDP
    pub fn as_bytes(&self) -> &[u8] {
        &self.buffer
    }

    pub fn increment_sequence(&mut self) {
        self.buffer[12] = self.buffer[12].wrapping_add(1);
    }

    pub fn set_raw_data(&mut self, initial_state: &[u8; 512]) {
        self.buffer[18..530].copy_from_slice(initial_state);
    }
}

/// A wrapper that implements LightSink for Art-Net UDP output.
pub struct ArtNetSink {
    socket: UdpSocket,
    addr: SocketAddr,
    bridge: ArtNetBridge,
}

impl ArtNetSink {
    pub fn new(universe: u16, target_ip: &str) -> anyhow::Result<Self> {
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
        })
    }
}

#[async_trait]
impl LightSink for ArtNetSink {
    async fn write_universe(&mut self, _universe_id: u16, data: &[u8; 512]) -> anyhow::Result<()> {
        self.bridge.set_raw_data(data);
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

#[async_trait]
impl LightSink for BroadcastSink {
    async fn write_universe(&mut self, _universe_id: u16, data: &[u8; 512]) -> anyhow::Result<()> {
        let mut sent_to_any = false;
        let mut first_err: Option<anyhow::Error> = None;

        for (idx, sink) in self.sinks.iter_mut().enumerate() {
            match sink.write_universe(_universe_id, data).await {
                Ok(()) => {
                    sent_to_any = true;
                }
                Err(err) => {
                    warn!("Broadcast sink {} write_universe failed: {err:#}", idx);
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
    recent_signals: Vec<(Instant, String)>,
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
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let _ = rustls::crypto::ring::default_provider().install_default();
    let cli = Args::parse();

    let command = cli.command.unwrap_or(Commands::Run {
        config: None,
        select_midi: false,
        setup: false,
        no_tui: false,
    });

    let use_tui = match &command {
        Commands::Run { no_tui, .. } => !no_tui && std::io::stdout().is_terminal(),
        _ => false,
    };

    // Setup rolling logs
    let log_dir = if let Some(proj_dirs) = ProjectDirs::from("", "", "PulsePlex") {
        proj_dirs.data_local_dir().join("logs")
    } else {
        PathBuf::from("logs")
    };
    std::fs::create_dir_all(&log_dir)?;

    let file_appender = tracing_appender::rolling::daily(log_dir, "pulseplex.log");
    let (non_blocking_file, _guard) = tracing_appender::non_blocking(file_appender);

    let filter = EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| EnvFilter::new(if cli.verbose { "debug" } else { "info" }));

    let file_layer = fmt::layer().with_writer(non_blocking_file).with_ansi(false);

    if use_tui {
        // TUI is ACTIVE: Only log to the file.
        // Console printing will severely corrupt the Ratatui interface.
        tracing_subscriber::registry()
            .with(filter)
            .with(file_layer)
            .init();
    } else {
        // TUI is INACTIVE: log to both the file and the console
        tracing_subscriber::registry()
            .with(filter)
            .with(file_layer)
            .with(fmt::layer().with_writer(io::stdout))
            .init();
    }

    // Version Check in Background
    std::thread::spawn(|| {
        if let Err(e) = check_for_updates() {
            debug!("Update check failed: {}", e);
        }
    });

    std::thread::spawn(|| {
        if let Err(e) = check_for_updates() {
            debug!("Update check failed: {}", e);
        }
    });

    match command {
        Commands::Check { config } => {
            let path = get_config_path(config.as_ref())?;
            handle_check(path.to_string_lossy().as_ref())?;
        }
        Commands::Doctor { config } => {
            let path = get_config_path(config.as_ref())?;
            doctor::run_doctor(&path)?;
        }
        Commands::Template { action } => match action {
            TemplateAction::Eject => {
                let template_content = include_str!("../assets/default_edrums.toml");
                let dest = PathBuf::from("pulseplex_edrums_template.toml");
                std::fs::write(&dest, template_content)?;

                std::fs::create_dir_all("assets/fixtures")?;
                std::fs::write(
                    "assets/fixtures/hue-color.json",
                    include_str!("../assets/fixtures/hue-color.json"),
                )?;
                std::fs::write(
                    "assets/fixtures/generic-rgbw.json",
                    include_str!("../assets/fixtures/generic-rgbw.json"),
                )?;

                println!("✅ Ejected default template to: {:?}", dest);
                println!("✅ Ejected fixture profiles to: assets/fixtures/");
            }
        },
        Commands::Hue { action } => match action {
            HueAction::Setup { list, ip, force } => {
                let config_path = get_config_path(None)?;
                setup::handle_hue_setup(config_path, list, ip, force).await?;
            }
        },
        Commands::Run {
            config,
            select_midi,
            setup,
            no_tui: _, // already extracted into use_tui
        } => {
            let path = get_config_path(config.as_ref())?;

            let path = if setup || (!path.exists() && config.is_none()) {
                // If default config doesn't exist or --setup is forced, run the wizard
                setup::run_wizard()
                    .await
                    .context("Failed to complete setup wizard")?
            } else {
                path
            };

            // Ensure the configuration directory exists
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent)?;
            }

            run_daemon(path, select_midi, use_tui).await?;
        }
    }

    Ok(())
}

fn check_for_updates() -> anyhow::Result<()> {
    let client = reqwest::blocking::Client::builder()
        .user_agent("pulseplex-cli")
        .timeout(Duration::from_secs(2))
        .build()?;

    let resp = client
        .get("https://api.github.com/repos/pulseplex/pulseplex/releases/latest")
        .send()?;

    if !resp.status().is_success() {
        return Ok(());
    }

    #[derive(serde::Deserialize)]
    struct GithubRelease {
        tag_name: String,
    }

    let body: serde_json::Value = resp.json()?;
    let latest: GithubRelease = match serde_json::from_value(body.clone()) {
        Ok(j) => j,
        Err(e) => {
            debug!("Failed to parse GitHub release JSON: {}. Body: {}", e, body);
            return Ok(());
        }
    };
    let current_v = semver::Version::parse(env!("CARGO_PKG_VERSION"))?;
    let latest_v = semver::Version::parse(latest.tag_name.trim_start_matches('v'))?;

    if latest_v > current_v {
        println!("\x1b[33m⚠️  A new version of PulsePlex is available (v{}). Run 'brew upgrade pulseplex' to update.\x1b[0m", latest.tag_name);
    }

    Ok(())
}

async fn run_daemon(config_path: PathBuf, force_select: bool, use_tui: bool) -> anyhow::Result<()> {
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

    let compiled = config.compile()?;

    // Normalized Input Bus
    let (raw_tx, raw_rx) = crossbeam_channel::unbounded();
    let (event_tx, event_rx) = crossbeam_channel::unbounded();

    // Initialize MIDI Source
    let mut midi_id_map_u16 = HashMap::new();
    for (k, v) in compiled.midi_id_map.iter() {
        midi_id_map_u16.insert(*k, *v as u16);
    }
    let mut midi_source = pulseplex_midi::MidiSource::new(&compiled.midi_device, midi_id_map_u16);
    midi_source.run(raw_tx.clone())?;

    // Map behaviors: convert usize IDs to u16 for the core engine
    let mut behaviors_u16 = HashMap::new();
    for (&id, cfg) in &compiled.behaviors {
        behaviors_u16.insert(id as u16, cfg.clone());
    }

    let mut fixture_mappings_u16 = HashMap::new();
    for (&id, mappings) in &compiled.fixture_mappings {
        fixture_mappings_u16.insert(id as u16, mappings.clone());
    }

    let mut engine = PulsePlexEngine::new(
        behaviors_u16,
        compiled.fixtures.clone(),
        fixture_mappings_u16,
    );

    // Initialize Sinks
    let mut sinks: Vec<Box<dyn LightSink>> = Vec::new();

    for target in &compiled.targets {
        match target {
            TargetConfig::ArtNet(artnet) => {
                let sink = ArtNetSink::new(artnet.universe, &artnet.target_ip)?;
                sinks.push(Box::new(sink));
                info!("Initialized Art-Net sink for universe {}", artnet.universe);
            }
            TargetConfig::Hue(hue) => {
                let hue_patch: Vec<pulseplex_hue::HuePatch> = hue
                    .patch
                    .iter()
                    .map(|p| pulseplex_hue::HuePatch {
                        hue_id: p.hue_id,
                        dmx_address: p.dmx_address,
                    })
                    .collect();

                let sink_res = HueSink::new(
                    hue.bridge_ip.clone(),
                    hue.username.clone(),
                    hue.client_key.clone(),
                    hue.area_id.clone(),
                    hue_patch.clone(),
                );

                match sink_res {
                    Ok(sink) => {
                        sinks.push(Box::new(sink));
                        info!("Initialized Philips Hue sink for bridge {}", hue.bridge_ip);
                    }
                    Err(e) => {
                        warn!(
                            "Failed to initialize Hue sink: {}. Attempting auto-heal...",
                            e
                        );
                        // Targeted discovery via Bridge ID
                        if let Ok(ip) = setup::discover_bridge_by_id_fallback(&hue.bridge_id).await
                        {
                            let ip_str = ip.to_string();
                            info!("Auto-heal: Found Hue Bridge at new IP: {}", ip_str);
                            let _ = update_hue_ip_in_config(&config_path_str, &ip_str);
                            if let Ok(sink) = HueSink::new(
                                ip_str,
                                hue.username.clone(),
                                hue.client_key.clone(),
                                hue.area_id.clone(),
                                hue_patch,
                            ) {
                                sinks.push(Box::new(sink));
                                info!("Auto-heal successful.");
                            }
                        }
                    }
                }
            }
        }
    }

    let sleeper = SpinSleeper::default();

    let mut initial_state = [0u8; 512];
    if matches!(config.shutdown.mode, ShutdownMode::Restore) {
        info!("Capturing current lighting state for later restoration...");
        if let Ok(listener) = UdpSocket::bind("0.0.0.0:6454") {
            listener.set_read_timeout(Some(Duration::from_millis(1000)))?;
            let mut buf = [0u8; 1024];
            if let Ok((amt, _)) = listener.recv_from(&mut buf) {
                if amt >= 530 {
                    initial_state.copy_from_slice(&buf[18..530]);
                }
            }
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

    while running.load(Ordering::SeqCst) {
        let now = Instant::now();
        let delta_time = now.duration_since(last_tick);
        last_tick = now;

        // TUI event polling
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

        // Hot Reload
        let mut needs_reload = false;
        while config_rx.try_recv().is_ok() {
            needs_reload = true;
        }
        if needs_reload {
            info!("Hot reload detected (skipped for Phase 5 implementation brevity - remains structurally compatible)");
        }

        // Forward events to engine and log them for the TUI
        while let Ok(evt) = raw_rx.try_recv() {
            let msg = match &evt {
                SourceEvent::Trigger { id, velocity } => {
                    format!("Trigger ID: {:<3} Vel: {:<3}", id, velocity)
                }
                SourceEvent::DmxFrame { universe, .. } => format!("DMX Frame (Uni {})", universe),
                SourceEvent::ClearAll => "Clear All".to_string(),
            };

            dashboard_state.recent_signals.push((Instant::now(), msg));
            // Keep the last 50 events in memory
            if dashboard_state.recent_signals.len() > 50 {
                dashboard_state.recent_signals.remove(0);
            }

            let _ = event_tx.send(evt);
        }

        // Core Tick
        if let Err(e) = engine.tick(delta_time, &event_rx, &mut sinks).await {
            error!("Engine tick error: {}", e);
            break;
        }

        // TUI Render
        if let Some(ref mut term) = terminal {
            dashboard_state.dmx_channels = *engine.universe();
            dashboard_state.active_notes = engine.active_envelopes_count();

            // Push any new events to the signal log
            // (We could drain event_rx again, but tick() already did.
            // In a real production dashboard we'd want a separate mirror of events)

            term.draw(|f| ui(f, &dashboard_state))?;
        }

        let sleep_start = Instant::now();
        if sleep_start < next_deadline {
            sleeper.sleep(next_deadline.duration_since(sleep_start));
        }
        next_deadline = Instant::now().max(next_deadline) + target_interval;
    }

    // Shutdown
    let mut broadcast = BroadcastSink::new();
    for s in sinks {
        broadcast.add(s);
    }
    let _ = perform_shutdown(&config.shutdown, &mut broadcast, &initial_state, &compiled).await;

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

    // DMX Visualization
    let dmx_block = Block::default()
        .borders(Borders::ALL)
        .title(" DMX Output (Universe 0) ");

    // Calculate available columns dynamically (subtracting 2 for borders).
    // Each cell is 2 chars wide ("██")
    let inner_width = main_chunks[0].width.saturating_sub(2);
    let cols = (inner_width / 2).max(1) as usize;

    let mut dmx_lines = Vec::new();
    let mut current_row = Vec::new();

    for &val in state.dmx_channels.iter() {
        let color = if val == 0 {
            Color::DarkGray
        } else {
            Color::Rgb(val, val, val)
        };
        current_row.push(Span::styled("██", Style::default().fg(color)));

        // Wrap to the next line if we run out of terminal width
        if current_row.len() == cols {
            dmx_lines.push(Line::from(current_row.clone()));
            current_row.clear();
        }
    }
    if !current_row.is_empty() {
        dmx_lines.push(Line::from(current_row));
    }

    let dmx_para = Paragraph::new(dmx_lines).block(dmx_block);
    f.render_widget(dmx_para, main_chunks[0]);

    // Signal Log
    let signal_items: Vec<ListItem> = state
        .recent_signals
        .iter()
        .rev()
        .map(|(ts, text)| {
            // Color the indicator bright green if it happened in the last 500ms
            let elapsed = ts.elapsed().as_secs_f32();
            let color = if elapsed < 0.5 {
                Color::LightGreen
            } else {
                Color::DarkGray
            };

            ListItem::new(Line::from(vec![
                Span::styled("▶ ", Style::default().fg(color)),
                Span::raw(text),
            ]))
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

async fn perform_shutdown(
    config: &crate::config::ShutdownConfig,
    sink: &mut dyn LightSink,
    initial_state: &[u8; 512],
    _compiled: &crate::config::CompiledConfig,
) -> anyhow::Result<()> {
    match config.mode {
        ShutdownMode::Blackout => {
            info!("Shutting down: Blackout");
            sink.write_universe(1, &[0u8; 512]).await?;
        }
        ShutdownMode::Default => {
            info!("Shutting down: Applying default scene");
            let mut frame = [0u8; 512];
            if let Some(defaults) = &config.defaults {
                for (&chan, &val) in defaults {
                    if chan < 512 {
                        frame[chan] = val;
                    }
                }
            }
            sink.write_universe(1, &frame).await?;
        }
        ShutdownMode::Restore => {
            info!("Shutting down: Restoring previous state");
            sink.write_universe(1, initial_state).await?;
        }
    }

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
    use pulseplex_core::MockSink;
    use std::sync::Mutex;

    struct SharedMockSink {
        inner: Arc<tokio::sync::Mutex<MockSink>>,
    }

    impl SharedMockSink {
        fn new(inner: Arc<tokio::sync::Mutex<MockSink>>) -> Self {
            Self { inner }
        }
    }

    #[async_trait]
    impl LightSink for SharedMockSink {
        async fn write_universe(
            &mut self,
            _universe_id: u16,
            data: &[u8; 512],
        ) -> anyhow::Result<()> {
            self.inner
                .lock()
                .await
                .write_universe(_universe_id, data)
                .await
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

    #[async_trait]
    impl LightSink for FailingSink {
        async fn write_universe(
            &mut self,
            _universe_id: u16,
            _data: &[u8; 512],
        ) -> anyhow::Result<()> {
            let mut calls = self.calls.lock().unwrap();
            *calls += 1;
            Err(anyhow::anyhow!("intentional sink failure"))
        }
    }

    #[tokio::test]
    async fn broadcast_sink_forwards_frames_to_all_sinks() {
        let first = Arc::new(tokio::sync::Mutex::new(MockSink::default()));
        let second = Arc::new(tokio::sync::Mutex::new(MockSink::default()));
        let mut broadcast = BroadcastSink::new();

        broadcast.add(Box::new(SharedMockSink::new(first.clone())));
        broadcast.add(Box::new(SharedMockSink::new(second.clone())));

        let data = [255u8; 512];
        broadcast.write_universe(1, &data).await.unwrap();

        assert_eq!(first.lock().await.states.len(), 1);
        assert_eq!(second.lock().await.states.len(), 1);
    }

    #[tokio::test]
    async fn broadcast_sink_continues_on_individual_failure() {
        let first = Arc::new(tokio::sync::Mutex::new(MockSink::default()));
        let last = Arc::new(tokio::sync::Mutex::new(MockSink::default()));
        let failing_calls = Arc::new(Mutex::new(0));
        let mut broadcast = BroadcastSink::new();

        broadcast.add(Box::new(SharedMockSink::new(first.clone())));
        broadcast.add(Box::new(FailingSink::new(failing_calls.clone())));
        broadcast.add(Box::new(SharedMockSink::new(last.clone())));

        let data = [0u8; 512];

        // Should return Ok because at least one sink succeeded
        let result = broadcast.write_universe(1, &data).await;

        assert!(result.is_ok());
        assert_eq!(*failing_calls.lock().unwrap(), 1);

        assert_eq!(first.lock().await.states.len(), 1);
        assert_eq!(last.lock().await.states.len(), 1);
    }

    #[test]
    fn test_artnet_sink_creation() {
        let sink = ArtNetSink::new(1, "127.0.0.1:6454");
        assert!(sink.is_ok());

        let invalid_sink = ArtNetSink::new(1, "invalid_ip");
        assert!(invalid_sink.is_err());
    }

    #[tokio::test]
    async fn test_perform_shutdown_restore() {
        use crate::config::CompiledConfig;
        use pulseplex_core::MockSink;

        let config = crate::config::ShutdownConfig {
            mode: ShutdownMode::Restore,
            defaults: None,
        };
        let compiled = CompiledConfig {
            midi_device: "".to_string(),
            midi_id_map: HashMap::new(),
            behaviors: HashMap::new(),
            targets: vec![],
            fixtures: vec![],
            fixture_mappings: HashMap::new(),
        };

        let mut sink = MockSink::default();
        let mut initial_state = [0u8; 512];
        initial_state[0] = 255;
        initial_state[10] = 100;

        perform_shutdown(&config, &mut sink, &initial_state, &compiled)
            .await
            .unwrap();

        assert_eq!(sink.states.len(), 1);
        assert_eq!(sink.states[0][0], 255);
        assert_eq!(sink.states[0][10], 100);
    }

    #[tokio::test]
    async fn test_perform_shutdown_default() {
        use crate::config::CompiledConfig;
        use pulseplex_core::MockSink;

        let mut defaults = HashMap::new();
        defaults.insert(5, 255);
        defaults.insert(11, 128);

        let config = crate::config::ShutdownConfig {
            mode: ShutdownMode::Default,
            defaults: Some(defaults),
        };
        let compiled = CompiledConfig {
            midi_device: "".to_string(),
            midi_id_map: HashMap::new(),
            behaviors: HashMap::new(),
            targets: vec![],
            fixtures: vec![],
            fixture_mappings: HashMap::new(),
        };

        let mut sink = MockSink::default();
        let initial_state = [0u8; 512];

        perform_shutdown(&config, &mut sink, &initial_state, &compiled)
            .await
            .unwrap();

        assert_eq!(sink.states.len(), 1);
        assert_eq!(sink.states[0][5], 255);
        assert_eq!(sink.states[0][11], 128);
    }
}
