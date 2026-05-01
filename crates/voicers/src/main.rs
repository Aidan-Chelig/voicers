mod client;
mod ui;

use std::{
    io::Write,
    path::Path,
    process::{Command, Stdio},
    time::{Duration, Instant},
};

use anyhow::Result;
use crossterm::event::{self, Event, KeyCode, KeyEventKind};
use ratatui::{
    prelude::*,
    widgets::{Block, Clear, List, ListItem, ListState, Paragraph, Wrap},
    DefaultTerminal,
};
use ui::{
    model::{
        adjust_percent, best_known_peer_address, default_flash, local_fallback_test_steps,
        parse_ui_config, ranked_fallback_candidates, short_id, ConfigItem, InputMode, Screen,
        UiApp, UiConfig,
    },
    theme::{
        badge, color_accent, color_bg, color_good, color_muted, color_panel_alt, color_selected,
        color_subtle, color_text, color_warn, label, panel_block, transport_color, transport_label,
    },
};
use voicers_core::{ControlRequest, ControlResponse, KnownPeerSummary, DEFAULT_CONTROL_ADDR};

const DEFAULT_ROOM_NAME: &str = "main";

#[tokio::main]
async fn main() -> Result<()> {
    let terminal = ratatui::init();
    let result = run_tui(terminal, parse_ui_config(DEFAULT_CONTROL_ADDR)).await;
    ratatui::restore();
    result
}

async fn run_tui(mut terminal: DefaultTerminal, config: UiConfig) -> Result<()> {
    let mut app = UiApp::new(config);
    let mut last_refresh = Instant::now() - Duration::from_secs(2);
    let launched = launch_daemon(&mut app, true);
    if launched {
        app.set_flash_persistent("starting voicers and preparing your invite");
    }

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
                    InputMode::RenameSelf => {
                        handle_text_input(&mut app, key.code, InputMode::RenameSelf).await
                    }
                    InputMode::RenameKnownPeer => {
                        handle_text_input(&mut app, key.code, InputMode::RenameKnownPeer).await
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
        Screen::KnownPeers => handle_known_peers_screen(app, key).await,
        Screen::Help => handle_help_screen(app, key).await,
    }
}

async fn handle_main_screen(app: &mut UiApp, key: KeyCode) -> Result<bool> {
    match key {
        KeyCode::Char('q') => return Ok(true),
        KeyCode::Char('?') => {
            app.previous_screen = Screen::Main;
            app.screen = Screen::Help;
            app.set_flash(default_flash(Screen::Help));
        }
        KeyCode::Char('c') => {
            app.screen = Screen::Config;
            app.clamp_config_selection();
            app.set_flash(default_flash(Screen::Config));
        }
        KeyCode::Char('p') => {
            app.screen = Screen::KnownPeers;
            app.clamp_known_peer_selection();
            app.set_flash(default_flash(Screen::KnownPeers));
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
        KeyCode::Char('a') => {
            save_selected_live_peer(app).await;
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
                .and_then(|status| status.network.known_peers.first())
                .and_then(best_known_peer_address)
                .or_else(|| {
                    app.status
                        .as_ref()
                        .and_then(|status| status.network.saved_peer_addrs.first().cloned())
                })
            {
                app.dial_input = saved;
            } else if let Some(saved) = app
                .status
                .as_ref()
                .and_then(|status| status.network.saved_peer_addrs.first())
            {
                app.dial_input = saved.clone();
            }
            app.input_mode = InputMode::Dial;
            app.set_flash_persistent("paste an invite and press Enter");
        }
        KeyCode::Enter => {
            app.input_mode = InputMode::Dial;
            app.set_flash_persistent("paste an invite and press Enter");
        }
        KeyCode::Char('i') => {
            let invite = app
                .status
                .as_ref()
                .and_then(|status| status.network.share_invite.clone())
                .unwrap_or_else(|| "no shareable invite yet".to_string());
            match copy_to_clipboard(&invite) {
                Ok(()) => app.set_flash("invite copied to clipboard"),
                Err(error) => app.set_flash(format!("clipboard copy failed: {error}; invite: {invite}")),
            }
        }
        KeyCode::Char('C') => {
            let response =
                client::send_request_to(&app.control_addr, ControlRequest::RotateInviteCode).await;
            app.set_flash(render_message(response));
            refresh_status(app).await;
        }
        KeyCode::Char('r') => {
            app.input_mode = InputMode::Room;
            app.set_flash_persistent("enter room name and press Enter");
        }
        KeyCode::Char('y') => {
            approve_first_pending_peer(app, false).await;
        }
        KeyCode::Char('w') => {
            approve_first_pending_peer(app, true).await;
        }
        KeyCode::Char('n') => {
            reject_first_pending_peer(app).await;
        }
        KeyCode::Char('u') => {
            app.rename_input = app
                .status
                .as_ref()
                .map(|status| status.session.display_name.clone())
                .unwrap_or_default();
            app.input_mode = InputMode::RenameSelf;
            app.set_flash_persistent("rename yourself and press Enter");
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
        KeyCode::Char('?') => {
            app.previous_screen = Screen::Config;
            app.screen = Screen::Help;
            app.set_flash(default_flash(Screen::Help));
        }
        KeyCode::Char('c') | KeyCode::Esc => {
            app.screen = Screen::Main;
            app.set_flash(default_flash(Screen::Main));
        }
        KeyCode::Char('p') => {
            app.screen = Screen::KnownPeers;
            app.clamp_known_peer_selection();
            app.set_flash(default_flash(Screen::KnownPeers));
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

async fn handle_known_peers_screen(app: &mut UiApp, key: KeyCode) -> Result<bool> {
    match key {
        KeyCode::Char('q') => return Ok(true),
        KeyCode::Char('?') => {
            app.previous_screen = Screen::KnownPeers;
            app.screen = Screen::Help;
            app.set_flash(default_flash(Screen::Help));
        }
        KeyCode::Char('p') | KeyCode::Esc => {
            app.screen = Screen::Main;
            app.set_flash(default_flash(Screen::Main));
        }
        KeyCode::Char('u') => {
            app.rename_input = app
                .status
                .as_ref()
                .map(|status| status.session.display_name.clone())
                .unwrap_or_default();
            app.input_mode = InputMode::RenameSelf;
            app.set_flash_persistent("rename yourself and press Enter");
        }
        KeyCode::Char('c') => {
            app.screen = Screen::Config;
            app.clamp_config_selection();
            app.set_flash(default_flash(Screen::Config));
        }
        KeyCode::Char('s') => {
            launch_daemon(app, false);
            refresh_status(app).await;
        }
        KeyCode::Char('j') | KeyCode::Down => {
            let len = app
                .status
                .as_ref()
                .map(|status| status.network.known_peers.len())
                .unwrap_or(0);
            if len > 0 {
                app.selected_known_peer = (app.selected_known_peer + 1).min(len - 1);
            }
        }
        KeyCode::Char('k') | KeyCode::Up => {
            if app.selected_known_peer > 0 {
                app.selected_known_peer -= 1;
            }
        }
        KeyCode::Enter | KeyCode::Char('d') => {
            reconnect_selected_known_peer(app).await;
        }
        KeyCode::Char('a') => {
            save_selected_known_peer(app).await;
        }
        KeyCode::Char('D') => {
            forget_selected_known_peer(app).await;
        }
        KeyCode::Char('n') => {
            if let Some(peer) = app.selected_known_peer() {
                app.rename_input = peer.display_name.clone();
                app.input_mode = InputMode::RenameKnownPeer;
                app.set_flash_persistent("rename known peer and press Enter");
            } else {
                app.set_flash("no known peer selected");
            }
        }
        KeyCode::Char('g') => {
            refresh_status(app).await;
            app.clamp_known_peer_selection();
            app.set_flash("status refreshed");
        }
        _ => {}
    }

    Ok(false)
}

async fn handle_help_screen(app: &mut UiApp, key: KeyCode) -> Result<bool> {
    match key {
        KeyCode::Char('q') => return Ok(true),
        KeyCode::Char('?') | KeyCode::Esc => {
            app.screen = app.previous_screen;
            app.set_flash(default_flash(app.screen));
        }
        KeyCode::Char('g') => {
            refresh_status(app).await;
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
            InputMode::RenameSelf => {
                let response = client::send_request_to(
                    &app.control_addr,
                    ControlRequest::SetDisplayName {
                        display_name: app.rename_input.trim().to_string(),
                    },
                )
                .await;
                app.set_flash(render_message(response));
                app.input_mode = InputMode::Normal;
                refresh_status(app).await;
            }
            InputMode::RenameKnownPeer => {
                let Some(peer_id) = app.selected_known_peer().map(|peer| peer.peer_id.clone())
                else {
                    app.set_flash("no known peer selected");
                    app.input_mode = InputMode::Normal;
                    return;
                };
                let response = client::send_request_to(
                    &app.control_addr,
                    ControlRequest::RenameKnownPeer {
                        peer_id,
                        display_name: app.rename_input.trim().to_string(),
                    },
                )
                .await;
                app.set_flash(render_message(response));
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
            InputMode::RenameSelf => {
                app.rename_input.pop();
            }
            InputMode::RenameKnownPeer => {
                app.rename_input.pop();
            }
            InputMode::Normal => {}
        },
        KeyCode::Char(ch) => match mode {
            InputMode::Dial => app.dial_input.push(ch),
            InputMode::Room => app.room_input.push(ch),
            InputMode::ControlAddr => app.control_addr.push(ch),
            InputMode::RenameSelf => app.rename_input.push(ch),
            InputMode::RenameKnownPeer => app.rename_input.push(ch),
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
            app.clamp_known_peer_selection();
            app.daemon_launch.attempted_auto_start = true;
            ensure_default_room(app).await;
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

async fn ensure_default_room(app: &mut UiApp) {
    let needs_room = app
        .status
        .as_ref()
        .map(|status| status.session.room_name.is_none())
        .unwrap_or(false);

    if !needs_room {
        app.default_room_initialized = true;
        return;
    }

    if app.default_room_initialized {
        return;
    }

    app.default_room_initialized = true;
    let response = client::send_request_to(
        &app.control_addr,
        ControlRequest::CreateRoom {
            room_name: DEFAULT_ROOM_NAME.to_string(),
        },
    )
    .await;

    if matches!(response, Ok(ControlResponse::Ack { .. })) {
        if let Ok(ControlResponse::Status(status)) =
            client::send_request_to(&app.control_addr, ControlRequest::GetStatus).await
        {
            app.status = Some(status);
            app.clamp_selection();
            app.clamp_config_selection();
            app.clamp_known_peer_selection();
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

async fn reconnect_selected_known_peer(app: &mut UiApp) {
    let peer = app
        .status
        .as_ref()
        .and_then(|status| status.network.known_peers.get(app.selected_known_peer))
        .cloned();

    let Some(peer) = peer else {
        app.set_flash("no known peer selected");
        return;
    };

    if best_known_peer_address(&peer).is_none() {
        app.set_flash(format!("{} has no saved dial address", peer.display_name));
        return;
    }

    let response = client::send_request_to(
        &app.control_addr,
        ControlRequest::JoinPeer {
            address: peer.peer_id.clone(),
        },
    )
    .await;
    app.set_flash(render_message(response));
    refresh_status(app).await;
}

async fn save_selected_known_peer(app: &mut UiApp) {
    let Some(peer_id) = app.selected_known_peer().map(|peer| peer.peer_id.clone()) else {
        app.set_flash("no known peer selected");
        return;
    };

    let response =
        client::send_request_to(&app.control_addr, ControlRequest::SaveKnownPeer { peer_id }).await;
    app.set_flash(render_message(response));
    refresh_status(app).await;
}

async fn save_selected_live_peer(app: &mut UiApp) {
    let Some(peer_id) = app.selected_peer_id() else {
        app.set_flash("no live peer selected");
        return;
    };

    let response =
        client::send_request_to(&app.control_addr, ControlRequest::SaveKnownPeer { peer_id }).await;
    app.set_flash(render_message(response));
    refresh_status(app).await;
}

async fn forget_selected_known_peer(app: &mut UiApp) {
    let Some(peer_id) = app.selected_known_peer().map(|peer| peer.peer_id.clone()) else {
        app.set_flash("no known peer selected");
        return;
    };

    let response = client::send_request_to(
        &app.control_addr,
        ControlRequest::ForgetKnownPeer { peer_id },
    )
    .await;
    app.set_flash(render_message(response));
    refresh_status(app).await;
}

async fn approve_first_pending_peer(app: &mut UiApp, whitelist: bool) {
    let Some(peer_id) = app
        .status
        .as_ref()
        .and_then(|status| status.pending_peer_approvals.first())
        .map(|pending| pending.peer_id.clone())
    else {
        app.set_flash("no pending peer approvals");
        return;
    };

    let response = client::send_request_to(
        &app.control_addr,
        ControlRequest::ApprovePendingPeer { peer_id, whitelist },
    )
    .await;
    app.set_flash(render_message(response));
    refresh_status(app).await;
}

async fn reject_first_pending_peer(app: &mut UiApp) {
    let Some(peer_id) = app
        .status
        .as_ref()
        .and_then(|status| status.pending_peer_approvals.first())
        .map(|pending| pending.peer_id.clone())
    else {
        app.set_flash("no pending peer approvals");
        return;
    };

    let response = client::send_request_to(
        &app.control_addr,
        ControlRequest::RejectPendingPeer { peer_id },
    )
    .await;
    app.set_flash(render_message(response));
    refresh_status(app).await;
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

fn copy_to_clipboard(value: &str) -> Result<()> {
    const CLIPBOARD_COMMANDS: &[(&str, &[&str])] = &[
        ("wl-copy", &[]),
        ("xclip", &["-selection", "clipboard"]),
        ("xsel", &["--clipboard", "--input"]),
        ("pbcopy", &[]),
        ("clip.exe", &[]),
    ];

    let mut last_error = None;
    for (program, args) in CLIPBOARD_COMMANDS {
        match write_to_command_stdin(program, args, value) {
            Ok(()) => return Ok(()),
            Err(error) => last_error = Some(format!("{program}: {error}")),
        }
    }

    Err(anyhow::anyhow!(
        "{}",
        last_error.unwrap_or_else(|| "no clipboard command available".to_string())
    ))
}

fn write_to_command_stdin(program: &str, args: &[&str], value: &str) -> Result<()> {
    let mut child = Command::new(program)
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()?;

    if let Some(stdin) = child.stdin.as_mut() {
        stdin.write_all(value.as_bytes())?;
    }

    let status = child.wait()?;
    if status.success() {
        Ok(())
    } else {
        anyhow::bail!("exited with status {status}");
    }
}

fn render_message(response: Result<ControlResponse>) -> String {
    match response {
        Ok(ControlResponse::Ack { message }) => message,
        Ok(ControlResponse::Error { message }) => message,
        Ok(ControlResponse::Status(_)) => "status updated".to_string(),
        Err(error) => error.to_string(),
    }
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
            draw_home_panel(frame, app, right[0]);
            draw_notes(frame, app, right[1]);
        }
        Screen::Config => {
            let middle =
                Layout::horizontal([Constraint::Length(60), Constraint::Min(28)]).split(root[1]);
            draw_config_screen(frame, app, middle[0]);
            draw_notes(frame, app, middle[1]);
        }
        Screen::KnownPeers => {
            let middle =
                Layout::horizontal([Constraint::Length(52), Constraint::Min(36)]).split(root[1]);
            draw_known_peers_screen(frame, app, middle[0]);
            draw_known_peer_details(frame, app, middle[1]);
        }
        Screen::Help => {
            let middle =
                Layout::horizontal([Constraint::Length(72), Constraint::Min(28)]).split(root[1]);
            draw_help_screen(frame, app, middle[0]);
            draw_help_diagnostics(frame, app, middle[1]);
        }
    }
    draw_footer(frame, app, root[2]);

    if app.input_mode != InputMode::Normal {
        draw_input_popup(frame, app);
    }
}

fn draw_summary(frame: &mut Frame, app: &UiApp, area: Rect) {
    let lines = if let Some(status) = &app.status {
        let invite = status
            .network
            .share_invite
            .clone()
            .unwrap_or_else(|| "<preparing shareable invite>".to_string());
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
                        .unwrap_or_else(|| DEFAULT_ROOM_NAME.to_string()),
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
                label("INVITE"),
                Span::raw(" "),
                Span::styled(invite, Style::default().fg(color_text())),
            ]),
            Line::from(vec![
                label("JOIN"),
                Span::raw(" "),
                Span::styled(
                    "share this invite; your friend presses Enter and pastes it",
                    Style::default().fg(color_muted()),
                ),
            ]),
            Line::from(vec![
                label("STATE"),
                Span::raw(" "),
                Span::styled(
                    format!(
                        "{} | {} peers | {}",
                        status.network.transport_stage,
                        status.peers.len(),
                        status.network.selected_media_path
                    ),
                    Style::default().fg(color_text()),
                ),
            ]),
            Line::from(vec![
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
            Line::from(vec![
                label("STUN"),
                Span::raw(" "),
                Span::styled(
                    if status.network.stun_addrs.is_empty() {
                        "<none>".to_string()
                    } else {
                        status.network.stun_addrs.join(", ")
                    },
                    Style::default().fg(color_muted()),
                ),
            ]),
            Line::from(vec![
                label("MEDIA"),
                Span::raw(" "),
                Span::styled(
                    status.network.selected_media_path.clone(),
                    Style::default().fg(color_text()),
                ),
            ]),
            Line::from(vec![
                label("WEBRTC"),
                Span::raw(" "),
                Span::styled(
                    status.network.webrtc_connection_state.clone(),
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
            Line::from(Span::styled(
                "starting voicers and preparing your invite",
                Style::default().fg(color_muted()),
            )),
        ]
    };

    let widget = Paragraph::new(lines)
        .block(panel_block("Home", color_accent()))
        .wrap(Wrap { trim: false });
    frame.render_widget(widget, area);
}

fn draw_home_panel(frame: &mut Frame, app: &UiApp, area: Rect) {
    let lines = if let Some(status) = &app.status {
        let invite = status
            .network
            .share_invite
            .clone()
            .unwrap_or_else(|| "<preparing shareable invite>".to_string());
        let mut lines = vec![
            Line::from(vec![
                Span::styled(
                    "Your Invite",
                    Style::default()
                        .fg(color_text())
                        .add_modifier(Modifier::BOLD),
                ),
                Span::raw("  "),
                badge(
                    if status.network.share_invite.is_some() {
                        "ready"
                    } else {
                        "preparing"
                    },
                    if status.network.share_invite.is_some() {
                        color_good()
                    } else {
                        color_panel_alt()
                    },
                    color_bg(),
                ),
            ]),
            Line::from(Span::styled(invite, Style::default().fg(color_text()))),
            Line::from(vec![
                label("CODE"),
                Span::raw(" "),
                Span::styled(
                    status
                        .session
                        .invite_code
                        .clone()
                        .unwrap_or_else(|| "<none>".to_string()),
                    Style::default().fg(color_text()),
                ),
            ]),
            Line::from(vec![
                label("EXPIRES"),
                Span::raw(" "),
                Span::styled(
                    status
                        .session
                        .invite_expires_at_ms
                        .map(|timestamp| timestamp.to_string())
                        .unwrap_or_else(|| "<none>".to_string()),
                    Style::default().fg(color_muted()),
                ),
            ]),
            Line::from(""),
            Line::from("1. Share this invite."),
            Line::from("2. Your friend opens Voicers and presses Enter."),
            Line::from("3. They paste the invite and press Enter again."),
        ];

        if let Some(pending) = status.pending_peer_approvals.first() {
            lines.push(Line::from(""));
            lines.push(Line::from(vec![
                Span::styled(
                    "Pending Approval",
                    Style::default()
                        .fg(color_text())
                        .add_modifier(Modifier::BOLD),
                ),
                Span::raw("  "),
                badge("waiting", color_warn(), color_bg()),
            ]));
            lines.push(Line::from(format!(
                "{} @{}",
                pending.display_name,
                short_id(&pending.peer_id)
            )));
            lines.push(Line::from(Span::styled(
                pending.address.clone(),
                Style::default().fg(color_subtle()),
            )));
            if let Some(room_name) = &pending.room_name {
                lines.push(Line::from(vec![
                    label("ROOM"),
                    Span::raw(" "),
                    Span::styled(room_name.clone(), Style::default().fg(color_text())),
                ]));
            }
            lines.push(Line::from(
                "Press y to allow once, w to allow and whitelist, or n to reject.",
            ));
        }

        if let Some(peer) = app.selected_peer() {
            lines.push(Line::from(""));
            lines.push(Line::from(vec![
                Span::styled(
                    "Selected Peer",
                    Style::default()
                        .fg(color_text())
                        .add_modifier(Modifier::BOLD),
                ),
                Span::raw("  "),
                badge(
                    transport_label(&peer.transport),
                    transport_color(&peer.transport),
                    color_bg(),
                ),
            ]));
            lines.push(Line::from(format!(
                "{} @{}",
                peer.display_name,
                short_id(&peer.peer_id)
            )));
            lines.push(Line::from(Span::styled(
                peer.address.clone(),
                Style::default().fg(color_subtle()),
            )));
        }

        lines
    } else {
        vec![
            Line::from(Span::styled(
                "Starting Voicers",
                Style::default()
                    .fg(color_text())
                    .add_modifier(Modifier::BOLD),
            )),
            Line::from("Preparing the local daemon and waiting for a shareable invite."),
            Line::from(""),
            Line::from("If this takes a while, check the Activity panel below."),
        ]
    };

    let widget = Paragraph::new(lines)
        .block(panel_block("Invite", color_panel_alt()))
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
                        Style::default().fg(color_text()).add_modifier(if selected {
                            Modifier::BOLD
                        } else {
                            Modifier::empty()
                        }),
                    ),
                    Span::raw("  "),
                    badge(
                        if selected { "active" } else { "available" },
                        if selected {
                            color_accent()
                        } else {
                            color_panel_alt()
                        },
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

fn draw_known_peers_screen(frame: &mut Frame, app: &UiApp, area: Rect) {
    let items = if let Some(status) = &app.status {
        if status.network.known_peers.is_empty() {
            vec![ListItem::new(Span::styled(
                "No known peers yet",
                Style::default().fg(color_muted()),
            ))]
        } else {
            status
                .network
                .known_peers
                .iter()
                .map(|peer| {
                    ListItem::new(vec![
                        Line::from(vec![
                            Span::styled(
                                peer.display_name.clone(),
                                Style::default()
                                    .fg(color_text())
                                    .add_modifier(Modifier::BOLD),
                            ),
                            Span::raw(" "),
                            if peer.pinned {
                                badge("saved", color_accent(), color_bg())
                            } else {
                                badge("learned", color_panel_alt(), color_bg())
                            },
                            Span::raw(" "),
                            badge(
                                if peer.connected { "online" } else { "saved" },
                                if peer.connected {
                                    color_good()
                                } else {
                                    color_panel_alt()
                                },
                                color_bg(),
                            ),
                        ]),
                        Line::from(vec![
                            Span::styled(
                                format!("@{}", short_id(&peer.peer_id)),
                                Style::default().fg(color_muted()),
                            ),
                            Span::raw("  "),
                            Span::styled(
                                peer.last_dial_addr
                                    .clone()
                                    .unwrap_or_else(|| "<no saved dial addr>".to_string()),
                                Style::default().fg(color_subtle()),
                            ),
                        ]),
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
        .map(|status| !status.network.known_peers.is_empty())
        .unwrap_or(false)
    {
        state.select(Some(app.selected_known_peer.min(items.len() - 1)));
    }

    let list = List::new(items)
        .block(panel_block("Known Peers", color_panel_alt()))
        .highlight_style(
            Style::default()
                .bg(color_selected())
                .fg(color_text())
                .add_modifier(Modifier::BOLD),
        )
        .highlight_symbol(" > ");
    frame.render_stateful_widget(list, area, &mut state);
}

fn draw_known_peer_details(frame: &mut Frame, app: &UiApp, area: Rect) {
    let lines = app
        .status
        .as_ref()
        .and_then(|status| {
            status
                .network
                .known_peers
                .get(app.selected_known_peer)
                .map(|peer| known_peer_lines(status, peer))
        })
        .unwrap_or_else(|| {
            vec![Line::from(Span::styled(
                "Select a known peer to inspect saved addresses",
                Style::default().fg(color_muted()),
            ))]
        });

    let widget = Paragraph::new(lines)
        .block(panel_block("Peer Record", color_panel_alt()))
        .wrap(Wrap { trim: false });
    frame.render_widget(widget, area);
}

fn known_peer_lines(
    status: &voicers_core::DaemonStatus,
    peer: &KnownPeerSummary,
) -> Vec<Line<'static>> {
    let addresses = if peer.addresses.is_empty() {
        vec![Line::from(Span::styled(
            "<no saved addresses>",
            Style::default().fg(color_muted()),
        ))]
    } else {
        peer.addresses
            .iter()
            .map(|addr| {
                Line::from(Span::styled(
                    addr.clone(),
                    Style::default().fg(color_subtle()),
                ))
            })
            .collect()
    };

    let mut lines = vec![
        Line::from(vec![
            Span::styled(
                peer.display_name.clone(),
                Style::default()
                    .fg(color_text())
                    .add_modifier(Modifier::BOLD),
            ),
            Span::raw("  "),
            badge(
                if peer.connected {
                    "connected"
                } else {
                    "offline"
                },
                if peer.connected {
                    color_good()
                } else {
                    color_warn()
                },
                color_bg(),
            ),
            Span::raw("  "),
            badge(
                if peer.pinned { "pinned" } else { "learned" },
                if peer.pinned {
                    color_accent()
                } else {
                    color_panel_alt()
                },
                color_bg(),
            ),
        ]),
        Line::from(vec![
            label("PEER"),
            Span::raw(" "),
            Span::styled(peer.peer_id.clone(), Style::default().fg(color_muted())),
        ]),
        Line::from(vec![
            label("DIAL"),
            Span::raw(" "),
            Span::styled(
                peer.last_dial_addr
                    .clone()
                    .unwrap_or_else(|| "<none>".to_string()),
                Style::default().fg(color_text()),
            ),
        ]),
        Line::from(vec![
            label("ADDRS"),
            Span::raw(" "),
            Span::styled(
                format!("{}", peer.addresses.len()),
                Style::default().fg(color_text()),
            ),
        ]),
        Line::from(vec![
            label("RETRY"),
            Span::raw(" "),
            Span::styled(
                "Known Peers reconnect sends the peer id, then the daemon ranks and retries saved addresses.",
                Style::default().fg(color_text()),
            ),
        ]),
    ];
    let fallback_candidates = ranked_fallback_candidates(status, peer);
    if fallback_candidates.is_empty() {
        lines.push(Line::from(vec![
            label("FALLBACK"),
            Span::raw(" "),
            Span::styled(
                "<no saved reconnect candidates>",
                Style::default().fg(color_muted()),
            ),
        ]));
    } else {
        lines.push(Line::from(vec![
            label("FALLBACK"),
            Span::raw(" "),
            Span::styled("ranked reconnect order:", Style::default().fg(color_text())),
        ]));
        for (index, candidate) in fallback_candidates.iter().enumerate() {
            let badge_text = if index == 0 { "primary" } else { "retry" };
            let badge_color = if index == 0 {
                color_accent()
            } else {
                color_panel_alt()
            };
            lines.push(Line::from(vec![
                Span::styled(
                    format!("{}. ", index + 1),
                    Style::default().fg(color_muted()),
                ),
                badge(badge_text, badge_color, color_bg()),
                Span::raw(" "),
                Span::styled(
                    candidate.address.clone(),
                    Style::default().fg(color_subtle()),
                ),
            ]));
            lines.push(Line::from(vec![
                Span::raw("   "),
                Span::styled(
                    format!(
                        "{} score {} ({} ok / {} fail){}",
                        candidate.path_label,
                        candidate.score_delta,
                        candidate.successes,
                        candidate.failures,
                        if candidate.is_last_dial {
                            " | last successful dial"
                        } else {
                            ""
                        }
                    ),
                    Style::default().fg(color_muted()),
                ),
            ]));
        }
    }
    lines.extend(addresses);
    lines
}

fn draw_help_screen(frame: &mut Frame, app: &UiApp, area: Rect) {
    let mut lines = vec![
        Line::from(vec![
            Span::styled(
                "Local Two-Daemon Test",
                Style::default()
                    .fg(color_text())
                    .add_modifier(Modifier::BOLD),
            ),
            Span::raw("  "),
            badge("fallback", color_accent(), color_bg()),
        ]),
        Line::from(
            "Use separate control ports and state files when you run two daemons on one machine.",
        ),
        Line::from(""),
    ];

    lines.extend(
        local_fallback_test_steps(&app.control_addr)
            .into_iter()
            .map(Line::from),
    );
    lines.push(Line::from(""));
    lines.push(Line::from(Span::styled(
        "Simple path",
        Style::default()
            .fg(color_text())
            .add_modifier(Modifier::BOLD),
    )));
    lines.push(Line::from(
        "Run the TUI, wait for your invite, and share it. The other person opens the TUI, presses Enter, and pastes the invite.",
    ));
    lines.push(Line::from(""));
    lines.push(Line::from(
        "Known Peers reconnect issues JoinPeer with the selected peer id instead of one hard-coded address.",
    ));
    lines.push(Line::from(
        "Custom rooms publish short-lived invite codes; press C on the main screen to rotate the current code.",
    ));
    lines.push(Line::from(
        "First-time inbound room joins are held for approval; use y to allow once, w to whitelist, or n to reject.",
    ));
    lines.push(Line::from(
        "The daemon ranks direct and relayed saved addresses, then retries the next candidate after an outbound dial failure.",
    ));
    lines.push(Line::from(
        "Use the Peer Record panel and Activity notes to watch the retry sequence live.",
    ));

    let widget = Paragraph::new(lines)
        .block(panel_block("Help", color_accent()))
        .wrap(Wrap { trim: false });
    frame.render_widget(widget, area);
}

fn draw_help_diagnostics(frame: &mut Frame, app: &UiApp, area: Rect) {
    let lines = if let Some(status) = &app.status {
        let mut lines = vec![
            Line::from(vec![
                label("CONTROL"),
                Span::raw(" "),
                Span::styled(app.control_addr.clone(), Style::default().fg(color_text())),
            ]),
            Line::from(vec![
                label("MEDIA"),
                Span::raw(" "),
                Span::styled(
                    status.network.selected_media_path.clone(),
                    Style::default().fg(color_text()),
                ),
            ]),
            Line::from(vec![
                label("WEBRTC"),
                Span::raw(" "),
                Span::styled(
                    status.network.webrtc_connection_state.clone(),
                    Style::default().fg(color_muted()),
                ),
            ]),
            Line::from(vec![
                label("KNOWN"),
                Span::raw(" "),
                Span::styled(
                    status.network.known_peers.len().to_string(),
                    Style::default().fg(color_text()),
                ),
            ]),
            Line::from(""),
            Line::from(Span::styled(
                "Path Scores",
                Style::default()
                    .fg(color_text())
                    .add_modifier(Modifier::BOLD),
            )),
        ];

        if status.network.path_scores.is_empty() {
            lines.push(Line::from(Span::styled(
                "No path scores recorded yet",
                Style::default().fg(color_muted()),
            )));
        } else {
            for score in &status.network.path_scores {
                lines.push(Line::from(vec![
                    Span::styled(score.path.clone(), Style::default().fg(color_text())),
                    Span::raw(" "),
                    Span::styled(
                        format!(
                            "{} ({} ok / {} fail)",
                            score.successes as i64 - score.failures as i64,
                            score.successes,
                            score.failures
                        ),
                        Style::default().fg(color_muted()),
                    ),
                ]));
            }
        }
        lines
    } else {
        vec![Line::from(Span::styled(
            "Start a daemon to inspect reconnect diagnostics",
            Style::default().fg(color_muted()),
        ))]
    };

    let widget = Paragraph::new(lines)
        .block(panel_block("Diagnostics", color_panel_alt()))
        .wrap(Wrap { trim: false });
    frame.render_widget(widget, area);
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
        InputMode::Dial => ("Join With Invite", app.dial_input.as_str()),
        InputMode::Room => ("Create Room", app.room_input.as_str()),
        InputMode::ControlAddr => ("Control Address", app.control_addr.as_str()),
        InputMode::RenameSelf => ("Rename Yourself", app.rename_input.as_str()),
        InputMode::RenameKnownPeer => ("Rename Peer", app.rename_input.as_str()),
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
