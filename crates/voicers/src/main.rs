mod client;

use std::{
    path::{Path, PathBuf},
    process::{Command, Stdio},
    time::{Duration, Instant},
};

use anyhow::Result;
use crossterm::event::{self, Event, KeyCode, KeyEventKind};
use ratatui::{
    prelude::*,
    widgets::{Block, BorderType, Borders, Clear, List, ListItem, ListState, Paragraph, Wrap},
    DefaultTerminal,
};
use voicers_core::{
    ControlRequest, ControlResponse, DaemonStatus, PeerMediaState, PeerSessionState,
    PeerTransportState, DEFAULT_CONTROL_ADDR,
};

struct UiApp {
    control_addr: String,
    daemon_bin: PathBuf,
    status: Option<DaemonStatus>,
    flash: String,
    flash_until: Option<Instant>,
    input_mode: InputMode,
    dial_input: String,
    room_input: String,
    screen: Screen,
    selected_peer: usize,
    selected_config_item: usize,
    daemon_launch: DaemonLaunchState,
}

const FLASH_DURATION: Duration = Duration::from_secs(3);

#[derive(Clone, Copy, PartialEq, Eq)]
enum InputMode {
    Normal,
    Dial,
    Room,
    ControlAddr,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Screen {
    Main,
    Config,
}

enum ConfigItem {
    InputGain,
    CaptureDevice(String),
    PeerVolume { peer_id: String },
}

#[derive(Default)]
struct DaemonLaunchState {
    attempted_auto_start: bool,
    last_launch_at: Option<Instant>,
}

struct UiConfig {
    control_addr: String,
    daemon_bin: PathBuf,
}

impl UiApp {
    fn new(config: UiConfig) -> Self {
        Self {
            control_addr: config.control_addr,
            daemon_bin: config.daemon_bin,
            status: None,
            flash: default_flash(Screen::Main).to_string(),
            flash_until: None,
            input_mode: InputMode::Normal,
            dial_input: "/ip4/127.0.0.1/tcp/".to_string(),
            room_input: "dev-room".to_string(),
            screen: Screen::Main,
            selected_peer: 0,
            selected_config_item: 0,
            daemon_launch: DaemonLaunchState::default(),
        }
    }

    fn selected_peer_id(&self) -> Option<String> {
        self.status
            .as_ref()?
            .peers
            .get(self.selected_peer)
            .map(|peer| peer.peer_id.clone())
    }

    fn selected_peer(&self) -> Option<&voicers_core::PeerSummary> {
        self.status.as_ref()?.peers.get(self.selected_peer)
    }

    fn clamp_selection(&mut self) {
        let len = self
            .status
            .as_ref()
            .map(|status| status.peers.len())
            .unwrap_or(0);
        if len == 0 {
            self.selected_peer = 0;
        } else if self.selected_peer >= len {
            self.selected_peer = len - 1;
        }
    }

    fn clamp_config_selection(&mut self) {
        let len = self.config_items().len();
        if len == 0 {
            self.selected_config_item = 0;
        } else if self.selected_config_item >= len {
            self.selected_config_item = len - 1;
        }
    }

    fn config_items(&self) -> Vec<ConfigItem> {
        let mut items = vec![ConfigItem::InputGain];
        if let Some(status) = &self.status {
            items.extend(
                status
                    .audio
                    .available_capture_devices
                    .iter()
                    .cloned()
                    .map(ConfigItem::CaptureDevice),
            );
            items.extend(
                status
                    .peers
                    .iter()
                    .map(|peer| ConfigItem::PeerVolume {
                        peer_id: peer.peer_id.clone(),
                    }),
            );
        }
        items
    }

    fn set_flash(&mut self, message: impl Into<String>) {
        self.flash = message.into();
        self.flash_until = Some(Instant::now() + FLASH_DURATION);
    }

    fn set_flash_persistent(&mut self, message: impl Into<String>) {
        self.flash = message.into();
        self.flash_until = None;
    }

    fn restore_flash_if_expired(&mut self) {
        if self
            .flash_until
            .map(|deadline| Instant::now() >= deadline)
            .unwrap_or(false)
        {
            self.flash = default_flash(self.screen).to_string();
            self.flash_until = None;
        }
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    let terminal = ratatui::init();
    let result = run_tui(terminal, parse_ui_config()).await;
    ratatui::restore();
    result
}

async fn run_tui(mut terminal: DefaultTerminal, config: UiConfig) -> Result<()> {
    let mut app = UiApp::new(config);
    let mut last_refresh = Instant::now() - Duration::from_secs(2);

    loop {
        app.restore_flash_if_expired();

        if last_refresh.elapsed() >= Duration::from_millis(500) {
            refresh_status(&mut app).await;
            last_refresh = Instant::now();
        }

        terminal.draw(|frame| draw(frame, &app))?;

        if event::poll(Duration::from_millis(100))? {
            if let Event::Key(key) = event::read()? {
                if key.kind != KeyEventKind::Press {
                    continue;
                }

                match app.input_mode {
                    InputMode::Normal => {
                        if handle_key(&mut app, key.code).await? {
                            break;
                        }
                    }
                    InputMode::Dial => handle_text_input(&mut app, key.code, InputMode::Dial).await,
                    InputMode::Room => handle_text_input(&mut app, key.code, InputMode::Room).await,
                    InputMode::ControlAddr => {
                        handle_text_input(&mut app, key.code, InputMode::ControlAddr).await
                    }
                }
            }
        }
    }

    Ok(())
}

async fn handle_key(app: &mut UiApp, key: KeyCode) -> Result<bool> {
    match app.screen {
        Screen::Main => handle_main_screen(app, key).await,
        Screen::Config => handle_config_screen(app, key).await,
    }
}

async fn handle_main_screen(app: &mut UiApp, key: KeyCode) -> Result<bool> {
    match key {
        KeyCode::Char('q') => return Ok(true),
        KeyCode::Char('c') => {
            app.screen = Screen::Config;
            app.clamp_config_selection();
            app.set_flash(default_flash(Screen::Config));
        }
        KeyCode::Char('j') | KeyCode::Down => {
            let len = app
                .status
                .as_ref()
                .map(|status| status.peers.len())
                .unwrap_or(0);
            if len > 0 {
                app.selected_peer = (app.selected_peer + 1).min(len - 1);
            }
        }
        KeyCode::Char('k') | KeyCode::Up => {
            if app.selected_peer > 0 {
                app.selected_peer -= 1;
            }
        }
        KeyCode::Char('m') => {
            let response =
                client::send_request_to(&app.control_addr, ControlRequest::ToggleMuteSelf).await;
            app.set_flash(render_message(response));
            refresh_status(app).await;
        }
        KeyCode::Char('s') => {
            launch_daemon(app, false);
            refresh_status(app).await;
        }
        KeyCode::Char('x') => {
            if let Some(peer_id) = app.selected_peer_id() {
                let response = client::send_request_to(
                    &app.control_addr,
                    ControlRequest::ToggleMutePeer { peer_id },
                )
                .await;
                app.set_flash(render_message(response));
                refresh_status(app).await;
            } else {
                app.set_flash("no peer selected");
            }
        }
        KeyCode::Char('d') => {
            if let Some(saved) = app
                .status
                .as_ref()
                .and_then(|status| status.network.saved_peer_addrs.first())
            {
                app.dial_input = saved.clone();
            }
            app.input_mode = InputMode::Dial;
            app.set_flash_persistent("enter peer multiaddr and press Enter");
        }
        KeyCode::Char('i') => {
            let invite = app
                .status
                .as_ref()
                .and_then(|status| status.network.share_invite.clone())
                .unwrap_or_else(|| "no shareable invite yet".to_string());
            app.set_flash(invite);
        }
        KeyCode::Char('r') => {
            app.input_mode = InputMode::Room;
            app.set_flash_persistent("enter room name and press Enter");
        }
        KeyCode::Tab => {
            app.input_mode = InputMode::ControlAddr;
            app.set_flash_persistent("edit control addr and press Enter");
        }
        KeyCode::Char('g') => {
            refresh_status(app).await;
            app.set_flash("status refreshed");
        }
        _ => {}
    }

    Ok(false)
}

async fn handle_config_screen(app: &mut UiApp, key: KeyCode) -> Result<bool> {
    match key {
        KeyCode::Char('q') => return Ok(true),
        KeyCode::Char('c') | KeyCode::Esc => {
            app.screen = Screen::Main;
            app.set_flash(default_flash(Screen::Main));
        }
        KeyCode::Char('s') => {
            launch_daemon(app, false);
            refresh_status(app).await;
        }
        KeyCode::Char('j') | KeyCode::Down => {
            let len = app.config_items().len();
            if len > 0 {
                app.selected_config_item = (app.selected_config_item + 1).min(len - 1);
            }
        }
        KeyCode::Char('k') | KeyCode::Up => {
            if app.selected_config_item > 0 {
                app.selected_config_item -= 1;
            }
        }
        KeyCode::Char('h') | KeyCode::Left => {
            adjust_selected_config(app, -5).await;
        }
        KeyCode::Char('l') | KeyCode::Right => {
            adjust_selected_config(app, 5).await;
        }
        KeyCode::Enter => {
            activate_selected_config(app).await;
        }
        KeyCode::Char('g') => {
            refresh_status(app).await;
            app.clamp_config_selection();
            app.set_flash("status refreshed");
        }
        _ => {}
    }

    Ok(false)
}

async fn handle_text_input(app: &mut UiApp, key: KeyCode, mode: InputMode) {
    match key {
        KeyCode::Esc => {
            app.input_mode = InputMode::Normal;
            app.set_flash("input cancelled");
        }
        KeyCode::Enter => match mode {
            InputMode::Dial => {
                let address = app.dial_input.trim().to_string();
                let response = client::send_request_to(
                    &app.control_addr,
                    ControlRequest::JoinPeer { address },
                )
                .await;
                app.set_flash(render_message(response));
                app.input_mode = InputMode::Normal;
                refresh_status(app).await;
            }
            InputMode::Room => {
                let room_name = app.room_input.trim().to_string();
                let response = client::send_request_to(
                    &app.control_addr,
                    ControlRequest::CreateRoom { room_name },
                )
                .await;
                app.set_flash(render_message(response));
                app.input_mode = InputMode::Normal;
                refresh_status(app).await;
            }
            InputMode::ControlAddr => {
                app.control_addr = app.control_addr.trim().to_string();
                app.set_flash(format!("control addr set to {}", app.control_addr));
                app.input_mode = InputMode::Normal;
                refresh_status(app).await;
            }
            InputMode::Normal => {}
        },
        KeyCode::Backspace => match mode {
            InputMode::Dial => {
                app.dial_input.pop();
            }
            InputMode::Room => {
                app.room_input.pop();
            }
            InputMode::ControlAddr => {
                app.control_addr.pop();
            }
            InputMode::Normal => {}
        },
        KeyCode::Char(ch) => match mode {
            InputMode::Dial => app.dial_input.push(ch),
            InputMode::Room => app.room_input.push(ch),
            InputMode::ControlAddr => app.control_addr.push(ch),
            InputMode::Normal => {}
        },
        _ => {}
    }
}

async fn refresh_status(app: &mut UiApp) {
    match client::send_request_to(&app.control_addr, ControlRequest::GetStatus).await {
        Ok(ControlResponse::Status(status)) => {
            app.status = Some(status);
            app.clamp_selection();
            app.clamp_config_selection();
            app.daemon_launch.attempted_auto_start = true;
        }
        Ok(other) => {
            app.set_flash(render_message(Ok(other)));
        }
        Err(error) => {
            app.status = None;
            let launched = launch_daemon(app, true);
            if launched {
                app.set_flash_persistent(format!(
                    "starting daemon at {} via {}",
                    app.control_addr,
                    app.daemon_bin.display()
                ));
            } else {
                app.set_flash_persistent(format!(
                    "daemon unavailable at {}: {error} | press s to launch",
                    app.control_addr
                ));
            }
        }
    }
}

async fn activate_selected_config(app: &mut UiApp) {
    let items = app.config_items();
    let Some(item) = items.get(app.selected_config_item) else {
        return;
    };

    if let ConfigItem::CaptureDevice(device_name) = item {
        let response = client::send_request_to(
            &app.control_addr,
            ControlRequest::SelectCaptureDevice {
                device_name: device_name.clone(),
            },
        )
        .await;
        app.set_flash(render_message(response));
        refresh_status(app).await;
    }
}

async fn adjust_selected_config(app: &mut UiApp, delta_percent: i16) {
    let items = app.config_items();
    let Some(item) = items.get(app.selected_config_item) else {
        return;
    };

    match item {
        ConfigItem::InputGain => {
            let current = app
                .status
                .as_ref()
                .map(|status| status.audio.input_gain_percent)
                .unwrap_or(100);
            let next = adjust_percent(current, delta_percent);
            let response = client::send_request_to(
                &app.control_addr,
                ControlRequest::SetInputGainPercent { percent: next },
            )
            .await;
            app.set_flash(render_message(response));
            refresh_status(app).await;
        }
        ConfigItem::PeerVolume { peer_id } => {
            let current = app
                .status
                .as_ref()
                .and_then(|status| {
                    status
                        .peers
                        .iter()
                        .find(|peer| &peer.peer_id == peer_id)
                        .map(|peer| peer.output_volume_percent)
                })
                .unwrap_or(100);
            let next = adjust_percent(current, delta_percent);
            let response = client::send_request_to(
                &app.control_addr,
                ControlRequest::SetPeerVolumePercent {
                    peer_id: peer_id.clone(),
                    percent: next,
                },
            )
            .await;
            app.set_flash(render_message(response));
            refresh_status(app).await;
        }
        ConfigItem::CaptureDevice(_) => {}
    }
}

fn launch_daemon(app: &mut UiApp, auto: bool) -> bool {
    if auto && app.daemon_launch.attempted_auto_start {
        return false;
    }

    if app
        .daemon_launch
        .last_launch_at
        .map(|instant| instant.elapsed() < Duration::from_secs(2))
        .unwrap_or(false)
    {
        return false;
    }

    match spawn_daemon(&app.daemon_bin, &app.control_addr) {
        Ok(()) => {
            app.daemon_launch.last_launch_at = Some(Instant::now());
            if auto {
                app.daemon_launch.attempted_auto_start = true;
            } else {
                app.set_flash(format!("launched daemon via {}", app.daemon_bin.display()));
            }
            true
        }
        Err(error) => {
            app.daemon_launch.last_launch_at = Some(Instant::now());
            app.daemon_launch.attempted_auto_start = auto;
            app.set_flash_persistent(format!(
                "failed to launch daemon via {}: {error}",
                app.daemon_bin.display()
            ));
            false
        }
    }
}

fn spawn_daemon(daemon_bin: &Path, control_addr: &str) -> Result<()> {
    Command::new(daemon_bin)
        .arg("--control-addr")
        .arg(control_addr)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()?;
    Ok(())
}

fn render_message(response: Result<ControlResponse>) -> String {
    match response {
        Ok(ControlResponse::Ack { message }) => message,
        Ok(ControlResponse::Error { message }) => message,
        Ok(ControlResponse::Status(_)) => "status updated".to_string(),
        Err(error) => error.to_string(),
    }
}

fn default_flash(screen: Screen) -> &'static str {
    match screen {
        Screen::Main => {
            "q quit | c config | tab control addr | s start daemon | r room | d dial | i invite | m mute self | x mute peer | j/k or arrows move"
        }
        Screen::Config => {
            "q quit | c back | s start daemon | enter select device | hjkl or arrows navigate"
        }
    }
}

fn adjust_percent(current: u8, delta: i16) -> u8 {
    let next = i16::from(current) + delta;
    next.clamp(0, 200) as u8
}

fn draw(frame: &mut Frame, app: &UiApp) {
    frame.render_widget(
        Block::default().style(Style::default().bg(color_bg()).fg(color_text())),
        frame.area(),
    );

    let root = Layout::vertical([
        Constraint::Length(8),
        Constraint::Min(12),
        Constraint::Length(3),
    ])
    .split(frame.area());

    draw_summary(frame, app, root[0]);
    match app.screen {
        Screen::Main => {
            let middle =
                Layout::horizontal([Constraint::Length(40), Constraint::Min(48)]).split(root[1]);
            let right =
                Layout::vertical([Constraint::Length(12), Constraint::Min(8)]).split(middle[1]);
            draw_peer_list(frame, app, middle[0]);
            draw_peer_details(frame, app, right[0]);
            draw_notes(frame, app, right[1]);
        }
        Screen::Config => {
            let middle =
                Layout::horizontal([Constraint::Length(60), Constraint::Min(28)]).split(root[1]);
            draw_config_screen(frame, app, middle[0]);
            draw_notes(frame, app, middle[1]);
        }
    }
    draw_footer(frame, app, root[2]);

    if app.input_mode != InputMode::Normal {
        draw_input_popup(frame, app);
    }
}

fn draw_summary(frame: &mut Frame, app: &UiApp, area: Rect) {
    let lines = if let Some(status) = &app.status {
        vec![
            Line::from(vec![
                Span::styled(
                    "VOICERS  ",
                    Style::default()
                        .fg(color_accent())
                        .add_modifier(Modifier::BOLD),
                ),
                badge(
                    &status
                        .session
                        .room_name
                        .clone()
                        .unwrap_or_else(|| "NO ROOM".to_string()),
                    color_panel_alt(),
                    color_text(),
                ),
                Span::raw("  "),
                badge(
                    if status.session.self_muted {
                        "MUTED"
                    } else {
                        "LIVE"
                    },
                    if status.session.self_muted {
                        color_warn()
                    } else {
                        color_good()
                    },
                    color_bg(),
                ),
            ]),
            Line::from(vec![
                Span::styled(
                    status.session.display_name.clone(),
                    Style::default()
                        .fg(color_text())
                        .add_modifier(Modifier::BOLD),
                ),
                Span::raw("  "),
                Span::styled(
                    format!("@{}", short_id(&status.local_peer_id)),
                    Style::default().fg(color_muted()),
                ),
                Span::raw("  "),
                Span::styled(
                    format!("daemon {}", status.daemon_version),
                    Style::default().fg(color_muted()),
                ),
            ]),
            Line::from(vec![
                label("CONTROL"),
                Span::raw(" "),
                Span::styled(app.control_addr.clone(), Style::default().fg(color_text())),
                Span::raw("    "),
                label("AUDIO"),
                Span::raw(" "),
                Span::styled(
                    format!(
                        "{} / {} / {}ms / {}Hz",
                        status.audio.output_backend,
                        status
                            .audio
                            .codec
                            .clone()
                            .unwrap_or_else(|| "<none>".to_string()),
                        status.audio.frame_size_ms.unwrap_or(0),
                        status.audio.sample_rate_hz.unwrap_or(0)
                    ),
                    Style::default().fg(color_text()),
                ),
            ]),
            Line::from(vec![
                label("INVITE"),
                Span::raw(" "),
                Span::styled(
                    status
                        .network
                        .share_invite
                        .clone()
                        .unwrap_or_else(|| "<waiting for reachable addr>".to_string()),
                    Style::default().fg(color_text()),
                ),
            ]),
            Line::from(vec![
                label("CAPTURE"),
                Span::raw(" "),
                Span::styled(
                    format!(
                        "{} via {}",
                        status
                            .audio
                            .capture_device
                            .clone()
                            .unwrap_or_else(|| "<none>".to_string()),
                        status
                            .audio
                            .source
                            .clone()
                            .unwrap_or_else(|| "<none>".to_string())
                    ),
                    Style::default().fg(color_text()),
                ),
            ]),
            Line::from(vec![
                label("TRANSPORT"),
                Span::raw(" "),
                Span::styled(
                    status.network.transport_stage.clone(),
                    Style::default().fg(color_text()),
                ),
            ]),
            Line::from(vec![
                label("NAT"),
                Span::raw(" "),
                Span::styled(
                    status.network.nat_status.clone(),
                    Style::default().fg(color_text()),
                ),
            ]),
            Line::from(vec![
                label("LISTEN"),
                Span::raw(" "),
                Span::styled(
                    if status.network.listen_addrs.is_empty() {
                        "<none>".to_string()
                    } else {
                        status.network.listen_addrs.join(", ")
                    },
                    Style::default().fg(color_muted()),
                ),
            ]),
            Line::from(vec![
                label("PUBLIC"),
                Span::raw(" "),
                Span::styled(
                    if status.network.external_addrs.is_empty() {
                        "<none>".to_string()
                    } else {
                        status.network.external_addrs.join(", ")
                    },
                    Style::default().fg(color_muted()),
                ),
            ]),
            Line::from(vec![
                label("OBSERVED"),
                Span::raw(" "),
                Span::styled(
                    if status.network.observed_addrs.is_empty() {
                        "<none>".to_string()
                    } else {
                        status.network.observed_addrs.join(", ")
                    },
                    Style::default().fg(color_muted()),
                ),
            ]),
        ]
    } else {
        vec![
            Line::from(vec![
                Span::styled(
                    "VOICERS  ",
                    Style::default()
                        .fg(color_accent())
                        .add_modifier(Modifier::BOLD),
                ),
                badge("OFFLINE", color_warn(), color_bg()),
            ]),
            Line::from(vec![
                label("CONTROL"),
                Span::raw(" "),
                Span::styled(app.control_addr.clone(), Style::default().fg(color_text())),
            ]),
            Line::from(Span::styled(
                "waiting for daemon status",
                Style::default().fg(color_muted()),
            )),
        ]
    };

    let widget = Paragraph::new(lines)
        .block(panel_block("Server", color_accent()))
        .wrap(Wrap { trim: false });
    frame.render_widget(widget, area);
}

fn draw_peer_list(frame: &mut Frame, app: &UiApp, area: Rect) {
    let items = if let Some(status) = &app.status {
        if status.peers.is_empty() {
            vec![ListItem::new(Line::from(Span::styled(
                "No peers connected",
                Style::default().fg(color_muted()),
            )))]
        } else {
            status
                .peers
                .iter()
                .map(|peer| {
                    ListItem::new(vec![
                        Line::from(vec![
                            Span::styled(
                                "  ",
                                Style::default().bg(transport_color(&peer.transport)),
                            ),
                            Span::raw(" "),
                            Span::styled(
                                peer.display_name.clone(),
                                Style::default()
                                    .fg(color_text())
                                    .add_modifier(Modifier::BOLD),
                            ),
                            Span::raw(" "),
                            badge(
                                transport_label(&peer.transport),
                                transport_color(&peer.transport),
                                color_bg(),
                            ),
                        ]),
                        Line::from(vec![
                            Span::styled(
                                format!("@{}", short_id(&peer.peer_id)),
                                Style::default().fg(color_muted()),
                            ),
                            Span::raw("  "),
                            badge(
                                if peer.muted { "muted" } else { "open" },
                                if peer.muted {
                                    color_warn()
                                } else {
                                    color_good()
                                },
                                color_bg(),
                            ),
                        ]),
                        Line::from(Span::styled(
                            peer.address.clone(),
                            Style::default().fg(color_subtle()),
                        )),
                    ])
                })
                .collect()
        }
    } else {
        vec![ListItem::new(Span::styled(
            "Daemon unavailable",
            Style::default().fg(color_warn()),
        ))]
    };

    let mut state = ListState::default();
    if app
        .status
        .as_ref()
        .map(|status| !status.peers.is_empty())
        .unwrap_or(false)
    {
        state.select(Some(app.selected_peer));
    }

    let list = List::new(items)
        .block(panel_block("Voice Channel", color_panel_alt()))
        .highlight_style(
            Style::default()
                .bg(color_selected())
                .fg(color_text())
                .add_modifier(Modifier::BOLD),
        )
        .highlight_symbol(" > ");
    frame.render_stateful_widget(list, area, &mut state);
}

fn draw_peer_details(frame: &mut Frame, app: &UiApp, area: Rect) {
    let lines = if let Some(peer) = app.selected_peer() {
        vec![
            Line::from(vec![
                Span::styled(
                    peer.display_name.clone(),
                    Style::default()
                        .fg(color_text())
                        .add_modifier(Modifier::BOLD),
                ),
                Span::raw("  "),
                badge(
                    media_state_label(&peer.media),
                    media_state_color(&peer.media),
                    color_bg(),
                ),
                Span::raw("  "),
                Span::styled(
                    format!("@{}", short_id(&peer.peer_id)),
                    Style::default().fg(color_muted()),
                ),
            ]),
            Line::from(vec![
                label("SESSION"),
                Span::raw(" "),
                Span::styled(
                    session_label(&peer.session),
                    Style::default().fg(color_text()),
                ),
            ]),
            Line::from(vec![
                label("ROUTE"),
                Span::raw(" "),
                Span::styled(peer.output_bus.clone(), Style::default().fg(color_text())),
                Span::raw("    "),
                label("ADDR"),
                Span::raw(" "),
                Span::styled(peer.address.clone(), Style::default().fg(color_subtle())),
            ]),
            Line::from(vec![
                metric("SENT", peer.media.sent_packets.to_string()),
                Span::raw("   "),
                metric("RECV", peer.media.received_packets.to_string()),
                Span::raw("   "),
                metric(
                    "LAST",
                    peer.media
                        .last_sequence
                        .map(|value| value.to_string())
                        .unwrap_or_else(|| "<none>".to_string()),
                ),
            ]),
            Line::from(vec![
                metric("TX RMS", format!("{:.4}", peer.media.tx_level_rms)),
                Span::raw("   "),
                metric("RX RMS", format!("{:.4}", peer.media.rx_level_rms)),
            ]),
            Line::from(vec![
                metric("PACKETS", peer.media.queued_packets.to_string()),
                Span::raw("   "),
                metric("FRAMES", peer.media.decoded_frames.to_string()),
                Span::raw("   "),
                metric("SAMPLES", peer.media.queued_samples.to_string()),
            ]),
            Line::from(vec![
                metric("LOST", peer.media.lost_packets.to_string()),
                Span::raw("   "),
                metric("LATE", peer.media.late_packets.to_string()),
                Span::raw("   "),
                metric("PLC", peer.media.concealed_frames.to_string()),
                Span::raw("   "),
                metric("DRIFT", peer.media.drift_corrections.to_string()),
            ]),
        ]
    } else {
        vec![Line::from(Span::styled(
            "Select a peer to inspect media state",
            Style::default().fg(color_muted()),
        ))]
    };

    let widget = Paragraph::new(lines)
        .block(panel_block("Member", color_panel_alt()))
        .wrap(Wrap { trim: false });
    frame.render_widget(widget, area);
}

fn draw_config_screen(frame: &mut Frame, app: &UiApp, area: Rect) {
    let items = if let Some(status) = &app.status {
        let mut lines = vec![ListItem::new(vec![
            Line::from(vec![
                Span::styled(
                    "Input Gain",
                    Style::default()
                        .fg(color_text())
                        .add_modifier(Modifier::BOLD),
                ),
                Span::raw("  "),
                badge(
                    format!("{}%", status.audio.input_gain_percent),
                    color_good(),
                    color_bg(),
                ),
            ]),
            Line::from(Span::styled(
                "Use h/l to lower or raise the local microphone level.",
                Style::default().fg(color_muted()),
            )),
        ])];

        for device_name in &status.audio.available_capture_devices {
            let selected = status
                .audio
                .capture_device
                .as_ref()
                .map(|current| current == device_name)
                .unwrap_or(false);
            lines.push(ListItem::new(vec![
                Line::from(vec![
                    Span::styled(
                        device_name.clone(),
                        Style::default()
                            .fg(color_text())
                            .add_modifier(if selected {
                                Modifier::BOLD
                            } else {
                                Modifier::empty()
                            }),
                    ),
                    Span::raw("  "),
                    badge(
                        if selected { "active" } else { "available" },
                        if selected { color_accent() } else { color_panel_alt() },
                        color_bg(),
                    ),
                ]),
                Line::from(Span::styled(
                    "Press Enter to switch the capture device.",
                    Style::default().fg(color_muted()),
                )),
            ]));
        }

        for peer in &status.peers {
            lines.push(ListItem::new(vec![
                Line::from(vec![
                    Span::styled(
                        format!("{} output", peer.display_name),
                        Style::default()
                            .fg(color_text())
                            .add_modifier(Modifier::BOLD),
                    ),
                    Span::raw("  "),
                    badge(
                        format!("{}%", peer.output_volume_percent),
                        color_warn(),
                        color_bg(),
                    ),
                ]),
                Line::from(Span::styled(
                    "Use h/l to adjust this peer's playback volume.",
                    Style::default().fg(color_muted()),
                )),
            ]));
        }

        lines
    } else {
        vec![ListItem::new(Span::styled(
            "Daemon unavailable",
            Style::default().fg(color_warn()),
        ))]
    };

    let mut state = ListState::default();
    if !items.is_empty() && app.status.is_some() {
        state.select(Some(app.selected_config_item.min(items.len() - 1)));
    }

    let list = List::new(items)
        .block(panel_block("Configuration", color_panel_alt()))
        .highlight_style(
            Style::default()
                .bg(color_selected())
                .fg(color_text())
                .add_modifier(Modifier::BOLD),
        )
        .highlight_symbol(" > ");
    frame.render_stateful_widget(list, area, &mut state);
}

fn draw_notes(frame: &mut Frame, app: &UiApp, area: Rect) {
    let lines = app
        .status
        .as_ref()
        .map(|status| {
            if status.notes.is_empty() {
                vec![Line::from("no recent notes")]
            } else {
                status
                    .notes
                    .iter()
                    .map(|note| Line::from(note.clone()))
                    .collect()
            }
        })
        .unwrap_or_else(|| vec![Line::from("waiting for daemon")]);

    let widget = Paragraph::new(lines)
        .block(panel_block("Activity", color_panel_alt()))
        .wrap(Wrap { trim: false });
    frame.render_widget(widget, area);
}

fn draw_footer(frame: &mut Frame, app: &UiApp, area: Rect) {
    let widget = Paragraph::new(app.flash.clone())
        .style(Style::default().fg(color_text()))
        .block(panel_block("Controls", color_accent()));
    frame.render_widget(widget, area);
}

fn draw_input_popup(frame: &mut Frame, app: &UiApp) {
    let area = centered_rect(70, 18, frame.area());
    let (title, value) = match app.input_mode {
        InputMode::Dial => ("Dial Peer", app.dial_input.as_str()),
        InputMode::Room => ("Create Room", app.room_input.as_str()),
        InputMode::ControlAddr => ("Control Address", app.control_addr.as_str()),
        InputMode::Normal => return,
    };

    frame.render_widget(Clear, area);
    frame.render_widget(
        Block::default().style(Style::default().bg(Color::Rgb(0, 0, 0))),
        area,
    );
    let widget = Paragraph::new(value)
        .style(Style::default().fg(color_text()))
        .block(panel_block(title, color_accent()))
        .wrap(Wrap { trim: false });
    frame.render_widget(widget, area);
}

fn centered_rect(width: u16, height: u16, area: Rect) -> Rect {
    let vertical = Layout::vertical([
        Constraint::Percentage((100 - height) / 2),
        Constraint::Percentage(height),
        Constraint::Percentage((100 - height) / 2),
    ])
    .split(area);

    Layout::horizontal([
        Constraint::Percentage((100 - width) / 2),
        Constraint::Percentage(width),
        Constraint::Percentage((100 - width) / 2),
    ])
    .split(vertical[1])[1]
}

fn parse_ui_config() -> UiConfig {
    let mut control_addr = DEFAULT_CONTROL_ADDR.to_string();
    let mut daemon_bin = default_daemon_bin();
    let mut args = std::env::args().skip(1);

    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--control-addr" => {
                if let Some(value) = args.next() {
                    control_addr = value;
                }
            }
            "--daemon-bin" => {
                if let Some(value) = args.next() {
                    daemon_bin = PathBuf::from(value);
                }
            }
            _ => {}
        }
    }

    UiConfig {
        control_addr,
        daemon_bin,
    }
}

fn default_daemon_bin() -> PathBuf {
    if let Some(path) = std::env::var_os("VOICERSD_BIN") {
        return PathBuf::from(path);
    }

    if let Ok(current_exe) = std::env::current_exe() {
        if let Some(sibling) = sibling_binary(&current_exe, "voicersd") {
            return sibling;
        }
    }

    PathBuf::from("voicersd")
}

fn sibling_binary(current_exe: &Path, name: &str) -> Option<PathBuf> {
    let dir = current_exe.parent()?;
    let mut candidate = dir.join(name);

    if cfg!(windows) {
        candidate.set_extension("exe");
    }

    Some(candidate)
}

fn short_id(peer_id: &str) -> &str {
    peer_id.get(0..12).unwrap_or(peer_id)
}

fn transport_label(state: &PeerTransportState) -> &'static str {
    match state {
        PeerTransportState::Planned => "planned",
        PeerTransportState::Connecting => "connecting",
        PeerTransportState::Connected => "connected",
        PeerTransportState::Disconnected => "disconnected",
    }
}

fn session_label(state: &PeerSessionState) -> String {
    match state {
        PeerSessionState::None => "none".to_string(),
        PeerSessionState::Handshaking => "handshaking".to_string(),
        PeerSessionState::Active {
            room_name,
            display_name,
        } => format!(
            "active {} @ {}",
            display_name,
            room_name.clone().unwrap_or_else(|| "<no-room>".to_string())
        ),
    }
}

fn media_state_label(state: &PeerMediaState) -> &'static str {
    match state.stream_state {
        voicers_core::MediaStreamState::Idle => "idle",
        voicers_core::MediaStreamState::Primed => "primed",
        voicers_core::MediaStreamState::Active => "active",
    }
}

fn panel_block<'a>(title: &'a str, accent: Color) -> Block<'a> {
    Block::default()
        .title(Line::from(vec![
            Span::styled(" ", Style::default().bg(accent)),
            Span::styled(
                format!(" {title} "),
                Style::default()
                    .fg(color_text())
                    .add_modifier(Modifier::BOLD),
            ),
        ]))
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(color_border()))
        .style(Style::default().bg(color_panel()).fg(color_text()))
}

fn badge(text: impl Into<String>, bg: Color, fg: Color) -> Span<'static> {
    Span::styled(
        format!(" {} ", text.into()),
        Style::default().bg(bg).fg(fg).add_modifier(Modifier::BOLD),
    )
}

fn label(text: &'static str) -> Span<'static> {
    Span::styled(
        text,
        Style::default()
            .fg(color_muted())
            .add_modifier(Modifier::BOLD),
    )
}

fn metric(label_text: &str, value: String) -> Span<'static> {
    Span::styled(
        format!("{label_text} {value}"),
        Style::default().fg(color_text()),
    )
}

fn media_state_color(state: &PeerMediaState) -> Color {
    match state.stream_state {
        voicers_core::MediaStreamState::Idle => color_warn(),
        voicers_core::MediaStreamState::Primed => color_accent_soft(),
        voicers_core::MediaStreamState::Active => color_good(),
    }
}

fn transport_color(state: &PeerTransportState) -> Color {
    match state {
        PeerTransportState::Planned => color_subtle(),
        PeerTransportState::Connecting => color_accent_soft(),
        PeerTransportState::Connected => color_good(),
        PeerTransportState::Disconnected => color_warn(),
    }
}

fn color_bg() -> Color {
    Color::Rgb(24, 26, 30)
}

fn color_panel() -> Color {
    Color::Rgb(32, 34, 40)
}

fn color_panel_alt() -> Color {
    Color::Rgb(43, 45, 49)
}

fn color_selected() -> Color {
    Color::Rgb(56, 60, 68)
}

fn color_border() -> Color {
    Color::Rgb(62, 65, 75)
}

fn color_text() -> Color {
    Color::Rgb(236, 238, 242)
}

fn color_muted() -> Color {
    Color::Rgb(153, 160, 174)
}

fn color_subtle() -> Color {
    Color::Rgb(125, 133, 150)
}

fn color_accent() -> Color {
    Color::Rgb(88, 101, 242)
}

fn color_accent_soft() -> Color {
    Color::Rgb(88, 166, 255)
}

fn color_good() -> Color {
    Color::Rgb(59, 165, 93)
}

fn color_warn() -> Color {
    Color::Rgb(237, 66, 69)
}
