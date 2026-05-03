mod client;
mod ui;

use std::{
    io,
    io::Write,
    path::Path,
    process::{Command, Stdio},
    time::{Duration, Instant},
};

use anyhow::Result;
use crossterm::{
    event::{self, DisableBracketedPaste, EnableBracketedPaste, Event, KeyCode, KeyEventKind},
    execute,
};
use ratatui::{
    prelude::*,
    widgets::{Block, Clear, List, ListItem, ListState, Paragraph, Tabs, Wrap},
    DefaultTerminal,
};
use ui::{
    model::{
        active_call_peers, adjust_percent, best_known_peer_address, default_flash,
        known_rooms, main_activity_items, parse_ui_config, ranked_fallback_candidates, short_id,
        visible_voice_peers, ConfigItem, InputMode, MainActivityItem, Screen, UiApp, UiConfig,
    },
    theme::{
        badge, color_accent, color_bg, color_good, color_muted, color_panel_alt, color_selected,
        color_subtle, color_text, color_warn, label, panel_block, transport_color, transport_label,
    },
};
use voicers_core::{
    ControlRequest, ControlResponse, KnownPeerSummary, DEFAULT_CONTROL_ADDR,
};

const DEFAULT_ROOM_NAME: &str = "main";

#[tokio::main]
async fn main() -> Result<()> {
    execute!(io::stdout(), EnableBracketedPaste)?;
    let terminal = ratatui::init();
    let result = run_tui(terminal, parse_ui_config(DEFAULT_CONTROL_ADDR)).await;
    execute!(io::stdout(), DisableBracketedPaste)?;
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
            match event::read()? {
                Event::Key(key) => {
                    if key.kind != KeyEventKind::Press {
                        continue;
                    }

                    match app.input_mode {
                        InputMode::Normal => {
                            if handle_key(&mut app, key.code).await? {
                                break;
                            }
                        }
                        InputMode::Dial => {
                            handle_text_input(&mut app, key.code, InputMode::Dial).await
                        }
                        InputMode::Room => {
                            handle_text_input(&mut app, key.code, InputMode::Room).await
                        }
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
                Event::Paste(text) => {
                    handle_paste_input(&mut app, text);
                }
                _ => {}
            }
        }
    }

    Ok(())
}

async fn handle_key(app: &mut UiApp, key: KeyCode) -> Result<bool> {
    match app.screen {
        Screen::Main => handle_main_screen(app, key).await,
        Screen::Peers => handle_peers_screen(app, key).await,
        Screen::Rooms => handle_rooms_screen(app, key).await,
        Screen::Calls => handle_calls_screen(app, key).await,
        Screen::Config => handle_config_screen(app, key).await,
        Screen::KnownPeers => handle_known_peers_screen(app, key).await,
        Screen::SeenUsers => handle_seen_users_screen(app, key).await,
        Screen::DiscoveredPeers => handle_discovered_peers_screen(app, key).await,
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
        KeyCode::Char('J') | KeyCode::Char('d') => {
            open_join_dialog(app);
        }
        KeyCode::Char('r') => {
            app.screen = Screen::Rooms;
            app.clamp_room_selection();
            app.set_flash(default_flash(Screen::Rooms));
        }
        KeyCode::Char('l') => {
            app.screen = Screen::Calls;
            app.clamp_call_selection();
            app.set_flash(default_flash(Screen::Calls));
        }
        KeyCode::Char('p') => {
            app.screen = Screen::Peers;
            app.clamp_selection();
            app.set_flash(default_flash(Screen::Peers));
        }
        KeyCode::Char('o') => {
            app.screen = Screen::KnownPeers;
            app.clamp_known_peer_selection();
            app.set_flash(default_flash(Screen::KnownPeers));
        }
        KeyCode::Char('v') => {
            app.screen = Screen::SeenUsers;
            app.clamp_seen_user_selection();
            app.set_flash(default_flash(Screen::SeenUsers));
        }
        KeyCode::Char('t') => {
            app.screen = Screen::DiscoveredPeers;
            app.clamp_discovered_peer_selection();
            app.set_flash(default_flash(Screen::DiscoveredPeers));
        }
        KeyCode::Char('c') => {
            app.screen = Screen::Config;
            app.clamp_config_selection();
            app.set_flash(default_flash(Screen::Config));
        }
        KeyCode::Char('j') | KeyCode::Down => {
            let len = app
                .status
                .as_ref()
                .map(|status| main_activity_items(status, &app.expanded_main_rooms, app.expanded_main_calls).len())
                .unwrap_or(0);
            if len > 0 {
                app.selected_main_item = (app.selected_main_item + 1).min(len - 1);
            }
        }
        KeyCode::Char('k') | KeyCode::Up => {
            if app.selected_main_item > 0 {
                app.selected_main_item -= 1;
            }
        }
        KeyCode::Enter => {
            activate_selected_main_item(app).await;
        }
        KeyCode::Char('g') => {
            refresh_status(app).await;
            app.clamp_main_selection();
            app.set_flash("status refreshed");
        }
        _ => {}
    }

    Ok(false)
}

fn open_join_dialog(app: &mut UiApp) {
    app.dial_input.clear();
    app.input_mode = InputMode::Dial;
    app.set_flash_persistent("paste an invite code and press Enter");
}

async fn handle_peers_screen(app: &mut UiApp, key: KeyCode) -> Result<bool> {
    match key {
        KeyCode::Char('q') => return Ok(true),
        KeyCode::Char('?') => {
            app.previous_screen = Screen::Peers;
            app.screen = Screen::Help;
            app.set_flash(default_flash(Screen::Help));
        }
        KeyCode::Char('p') | KeyCode::Esc => {
            app.screen = Screen::Main;
            app.clamp_main_selection();
            app.set_flash(default_flash(Screen::Main));
        }
        KeyCode::Char('J') => {
            open_join_dialog(app);
        }
        KeyCode::Char('o') => {
            app.screen = Screen::KnownPeers;
            app.clamp_known_peer_selection();
            app.set_flash(default_flash(Screen::KnownPeers));
        }
        KeyCode::Char('r') => {
            app.screen = Screen::Rooms;
            app.clamp_room_selection();
            app.set_flash(default_flash(Screen::Rooms));
        }
        KeyCode::Char('l') => {
            app.screen = Screen::Calls;
            app.clamp_call_selection();
            app.set_flash(default_flash(Screen::Calls));
        }
        KeyCode::Char('v') => {
            app.screen = Screen::SeenUsers;
            app.clamp_seen_user_selection();
            app.set_flash(default_flash(Screen::SeenUsers));
        }
        KeyCode::Char('t') => {
            app.screen = Screen::DiscoveredPeers;
            app.clamp_discovered_peer_selection();
            app.set_flash(default_flash(Screen::DiscoveredPeers));
        }
        KeyCode::Char('c') => {
            app.screen = Screen::Config;
            app.clamp_config_selection();
            app.set_flash(default_flash(Screen::Config));
        }
        KeyCode::Char('j') | KeyCode::Down => {
            let len = app
                .status
                .as_ref()
                .map(|status| visible_voice_peers(status).len())
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
        KeyCode::Char('a') => {
            save_selected_live_peer(app).await;
        }
        KeyCode::Char('g') => {
            refresh_status(app).await;
            app.clamp_selection();
            app.set_flash("status refreshed");
        }
        _ => {}
    }

    Ok(false)
}

async fn handle_rooms_screen(app: &mut UiApp, key: KeyCode) -> Result<bool> {
    match key {
        KeyCode::Char('q') => return Ok(true),
        KeyCode::Char('?') => {
            app.previous_screen = Screen::Rooms;
            app.screen = Screen::Help;
            app.set_flash(default_flash(Screen::Help));
        }
        KeyCode::Char('r') | KeyCode::Esc => {
            app.screen = Screen::Main;
            app.clamp_main_selection();
            app.set_flash(default_flash(Screen::Main));
        }
        KeyCode::Char('i') => {
            let invite = app
                .status
                .as_ref()
                .and_then(preferred_clipboard_invite)
                .unwrap_or_else(|| "no room invite available yet".to_string());
            match copy_to_clipboard(&invite) {
                Ok(()) => app.set_flash("room invite copied to clipboard"),
                Err(error) => app.set_flash(format!("clipboard copy failed: {error}; invite: {invite}")),
            }
        }
        KeyCode::Char('R') => {
            app.input_mode = InputMode::Room;
            app.set_flash_persistent("enter room name and press Enter");
        }
        KeyCode::Char('d') => {
            open_join_dialog(app);
        }
        KeyCode::Char('J') => {
            open_join_dialog(app);
        }
        KeyCode::Tab => {
            app.input_mode = InputMode::ControlAddr;
            app.set_flash_persistent("edit control addr and press Enter");
        }
        KeyCode::Char('m') => {
            let response =
                client::send_request_to(&app.control_addr, ControlRequest::ToggleMuteSelf).await;
            app.set_flash(render_message(response));
            refresh_status(app).await;
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
        KeyCode::Char('C') => {
            let response =
                client::send_request_to(&app.control_addr, ControlRequest::RotateInviteCode).await;
            app.set_flash(render_message(response));
            refresh_status(app).await;
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
        KeyCode::Char('p') => {
            app.screen = Screen::Peers;
            app.clamp_selection();
            app.set_flash(default_flash(Screen::Peers));
        }
        KeyCode::Char('l') => {
            app.screen = Screen::Calls;
            app.clamp_call_selection();
            app.set_flash(default_flash(Screen::Calls));
        }
        KeyCode::Char('o') => {
            app.screen = Screen::KnownPeers;
            app.clamp_known_peer_selection();
            app.set_flash(default_flash(Screen::KnownPeers));
        }
        KeyCode::Char('v') => {
            app.screen = Screen::SeenUsers;
            app.clamp_seen_user_selection();
            app.set_flash(default_flash(Screen::SeenUsers));
        }
        KeyCode::Char('t') => {
            app.screen = Screen::DiscoveredPeers;
            app.clamp_discovered_peer_selection();
            app.set_flash(default_flash(Screen::DiscoveredPeers));
        }
        KeyCode::Char('j') | KeyCode::Down => {
            let len = app
                .status
                .as_ref()
                .map(|status| known_rooms(status).len())
                .unwrap_or(0);
            if len > 0 {
                app.selected_room = (app.selected_room + 1).min(len - 1);
            }
        }
        KeyCode::Char('k') | KeyCode::Up => {
            if app.selected_room > 0 {
                app.selected_room -= 1;
            }
        }
        KeyCode::Enter => {
            if let Some(room) = app
                .status
                .as_ref()
                .and_then(|status| known_rooms(status).get(app.selected_room).map(|room| room.name.clone()))
            {
                let response = client::send_request_to(
                    &app.control_addr,
                    ControlRequest::CreateRoom { room_name: room },
                )
                .await;
                app.set_flash(render_message(response));
                refresh_status(app).await;
            } else {
                app.set_flash("no room selected");
            }
        }
        KeyCode::Char('g') => {
            refresh_status(app).await;
            app.clamp_room_selection();
            app.set_flash("status refreshed");
        }
        _ => {}
    }

    Ok(false)
}

async fn handle_calls_screen(app: &mut UiApp, key: KeyCode) -> Result<bool> {
    match key {
        KeyCode::Char('q') => return Ok(true),
        KeyCode::Char('?') => {
            app.previous_screen = Screen::Calls;
            app.screen = Screen::Help;
            app.set_flash(default_flash(Screen::Help));
        }
        KeyCode::Char('l') | KeyCode::Esc => {
            app.screen = Screen::Main;
            app.clamp_main_selection();
            app.set_flash(default_flash(Screen::Main));
        }
        KeyCode::Char('J') => {
            open_join_dialog(app);
        }
        KeyCode::Char('p') => {
            app.screen = Screen::Peers;
            app.clamp_selection();
            app.set_flash(default_flash(Screen::Peers));
        }
        KeyCode::Char('r') => {
            app.screen = Screen::Rooms;
            app.clamp_room_selection();
            app.set_flash(default_flash(Screen::Rooms));
        }
        KeyCode::Char('o') => {
            app.screen = Screen::KnownPeers;
            app.clamp_known_peer_selection();
            app.set_flash(default_flash(Screen::KnownPeers));
        }
        KeyCode::Char('v') => {
            app.screen = Screen::SeenUsers;
            app.clamp_seen_user_selection();
            app.set_flash(default_flash(Screen::SeenUsers));
        }
        KeyCode::Char('t') => {
            app.screen = Screen::DiscoveredPeers;
            app.clamp_discovered_peer_selection();
            app.set_flash(default_flash(Screen::DiscoveredPeers));
        }
        KeyCode::Char('j') | KeyCode::Down => {
            let len = app
                .status
                .as_ref()
                .map(|status| active_call_peers(status).len())
                .unwrap_or(0);
            if len > 0 {
                app.selected_call = (app.selected_call + 1).min(len - 1);
            }
        }
        KeyCode::Char('k') | KeyCode::Up => {
            if app.selected_call > 0 {
                app.selected_call -= 1;
            }
        }
        KeyCode::Char('x') => {
            if let Some(peer_id) = app.selected_call_peer().map(|peer| peer.peer_id.clone()) {
                let response = client::send_request_to(
                    &app.control_addr,
                    ControlRequest::ToggleMutePeer { peer_id },
                )
                .await;
                app.set_flash(render_message(response));
                refresh_status(app).await;
            } else {
                app.set_flash("no call selected");
            }
        }
        KeyCode::Char('a') => {
            if let Some(peer_id) = app.selected_call_peer().map(|peer| peer.peer_id.clone()) {
                let response =
                    client::send_request_to(&app.control_addr, ControlRequest::SaveKnownPeer { peer_id }).await;
                app.set_flash(render_message(response));
                refresh_status(app).await;
            } else {
                app.set_flash("no call selected");
            }
        }
        KeyCode::Char('i') => {
            let invite = app
                .status
                .as_ref()
                .and_then(preferred_call_invite)
                .unwrap_or_else(|| "no direct-call invite available yet".to_string());
            match copy_to_clipboard(&invite) {
                Ok(()) => app.set_flash("direct-call invite copied to clipboard"),
                Err(error) => app.set_flash(format!("clipboard copy failed: {error}; invite: {invite}")),
            }
        }
        KeyCode::Char('g') => {
            refresh_status(app).await;
            app.clamp_call_selection();
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
            app.clamp_main_selection();
            app.set_flash(default_flash(Screen::Main));
        }
        KeyCode::Char('J') => {
            open_join_dialog(app);
        }
        KeyCode::Char('p') => {
            app.screen = Screen::Peers;
            app.clamp_selection();
            app.set_flash(default_flash(Screen::Peers));
        }
        KeyCode::Char('o') => {
            app.screen = Screen::KnownPeers;
            app.clamp_known_peer_selection();
            app.set_flash(default_flash(Screen::KnownPeers));
        }
        KeyCode::Char('v') => {
            app.screen = Screen::SeenUsers;
            app.clamp_seen_user_selection();
            app.set_flash(default_flash(Screen::SeenUsers));
        }
        KeyCode::Char('t') => {
            app.screen = Screen::DiscoveredPeers;
            app.clamp_discovered_peer_selection();
            app.set_flash(default_flash(Screen::DiscoveredPeers));
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
        KeyCode::Char('o') | KeyCode::Esc => {
            app.screen = Screen::Main;
            app.clamp_main_selection();
            app.set_flash(default_flash(Screen::Main));
        }
        KeyCode::Char('J') => {
            open_join_dialog(app);
        }
        KeyCode::Char('p') => {
            app.screen = Screen::Peers;
            app.clamp_selection();
            app.set_flash(default_flash(Screen::Peers));
        }
        KeyCode::Char('v') => {
            app.screen = Screen::SeenUsers;
            app.clamp_seen_user_selection();
            app.set_flash(default_flash(Screen::SeenUsers));
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
                .map(|status| status.network.friends.len())
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
        KeyCode::Char('D') => {
            forget_selected_known_peer(app).await;
        }
        KeyCode::Char('t') => {
            toggle_trusted_contact(app).await;
        }
        KeyCode::Char('r') => {
            if let Some(peer) = app.selected_known_peer() {
                app.rename_input = peer.display_name.clone();
                app.input_mode = InputMode::RenameKnownPeer;
                app.set_flash_persistent("rename friend and press Enter");
            } else {
                app.set_flash("no friend selected");
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

async fn handle_seen_users_screen(app: &mut UiApp, key: KeyCode) -> Result<bool> {
    match key {
        KeyCode::Char('q') => return Ok(true),
        KeyCode::Char('?') => {
            app.previous_screen = Screen::SeenUsers;
            app.screen = Screen::Help;
            app.set_flash(default_flash(Screen::Help));
        }
        KeyCode::Char('v') | KeyCode::Esc => {
            app.screen = Screen::Main;
            app.clamp_main_selection();
            app.set_flash(default_flash(Screen::Main));
        }
        KeyCode::Char('J') => {
            open_join_dialog(app);
        }
        KeyCode::Char('o') => {
            app.screen = Screen::KnownPeers;
            app.clamp_known_peer_selection();
            app.set_flash(default_flash(Screen::KnownPeers));
        }
        KeyCode::Char('p') => {
            app.screen = Screen::Peers;
            app.clamp_selection();
            app.set_flash(default_flash(Screen::Peers));
        }
        KeyCode::Char('c') => {
            app.screen = Screen::Config;
            app.clamp_config_selection();
            app.set_flash(default_flash(Screen::Config));
        }
        KeyCode::Char('t') => {
            app.screen = Screen::DiscoveredPeers;
            app.clamp_discovered_peer_selection();
            app.set_flash(default_flash(Screen::DiscoveredPeers));
        }
        KeyCode::Char('j') | KeyCode::Down => {
            let len = app
                .status
                .as_ref()
                .map(|status| status.network.seen_users.len())
                .unwrap_or(0);
            if len > 0 {
                app.selected_seen_user = (app.selected_seen_user + 1).min(len - 1);
            }
        }
        KeyCode::Char('k') | KeyCode::Up => {
            if app.selected_seen_user > 0 {
                app.selected_seen_user -= 1;
            }
        }
        KeyCode::Enter | KeyCode::Char('d') => {
            reconnect_selected_seen_user(app).await;
        }
        KeyCode::Char('a') => {
            save_selected_seen_user(app).await;
        }
        KeyCode::Char('g') => {
            refresh_status(app).await;
            app.clamp_seen_user_selection();
            app.set_flash("status refreshed");
        }
        _ => {}
    }

    Ok(false)
}

async fn handle_discovered_peers_screen(app: &mut UiApp, key: KeyCode) -> Result<bool> {
    match key {
        KeyCode::Char('q') => return Ok(true),
        KeyCode::Char('?') => {
            app.previous_screen = Screen::DiscoveredPeers;
            app.screen = Screen::Help;
            app.set_flash(default_flash(Screen::Help));
        }
        KeyCode::Char('t') | KeyCode::Esc => {
            app.screen = Screen::Main;
            app.clamp_main_selection();
            app.set_flash(default_flash(Screen::Main));
        }
        KeyCode::Char('J') => {
            open_join_dialog(app);
        }
        KeyCode::Char('p') => {
            app.screen = Screen::Peers;
            app.clamp_selection();
            app.set_flash(default_flash(Screen::Peers));
        }
        KeyCode::Char('o') => {
            app.screen = Screen::KnownPeers;
            app.clamp_known_peer_selection();
            app.set_flash(default_flash(Screen::KnownPeers));
        }
        KeyCode::Char('v') => {
            app.screen = Screen::SeenUsers;
            app.clamp_seen_user_selection();
            app.set_flash(default_flash(Screen::SeenUsers));
        }
        KeyCode::Char('c') => {
            app.screen = Screen::Config;
            app.clamp_config_selection();
            app.set_flash(default_flash(Screen::Config));
        }
        KeyCode::Char('j') | KeyCode::Down => {
            let len = app
                .status
                .as_ref()
                .map(|status| status.network.discovered_peers.len())
                .unwrap_or(0);
            if len > 0 {
                app.selected_discovered_peer = (app.selected_discovered_peer + 1).min(len - 1);
            }
        }
        KeyCode::Char('k') | KeyCode::Up => {
            if app.selected_discovered_peer > 0 {
                app.selected_discovered_peer -= 1;
            }
        }
        KeyCode::Enter | KeyCode::Char('d') => {
            reconnect_selected_discovered_peer(app).await;
        }
        KeyCode::Char('g') => {
            refresh_status(app).await;
            app.clamp_discovered_peer_selection();
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
                    app.set_flash("no friend selected");
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
            InputMode::Dial => append_input_value(app, InputMode::Dial, &ch.to_string()),
            InputMode::Room => append_input_value(app, InputMode::Room, &ch.to_string()),
            InputMode::ControlAddr => append_input_value(app, InputMode::ControlAddr, &ch.to_string()),
            InputMode::RenameSelf => append_input_value(app, InputMode::RenameSelf, &ch.to_string()),
            InputMode::RenameKnownPeer => {
                append_input_value(app, InputMode::RenameKnownPeer, &ch.to_string())
            }
            InputMode::Normal => {}
        },
        _ => {}
    }
}

fn handle_paste_input(app: &mut UiApp, text: String) {
    if app.input_mode == InputMode::Normal {
        return;
    }

    append_input_value(app, app.input_mode, &text);
}

fn append_input_value(app: &mut UiApp, mode: InputMode, value: &str) {
    match mode {
        InputMode::Dial => app.dial_input.push_str(value),
        InputMode::Room => app.room_input.push_str(value),
        InputMode::ControlAddr => app.control_addr.push_str(value),
        InputMode::RenameSelf | InputMode::RenameKnownPeer => app.rename_input.push_str(value),
        InputMode::Normal => {}
    }
}

async fn activate_selected_main_item(app: &mut UiApp) {
    let Some(status) = &app.status else {
        return;
    };
    let items = main_activity_items(status, &app.expanded_main_rooms, app.expanded_main_calls);
    let Some(item) = items.get(app.selected_main_item).cloned() else {
        return;
    };

    match item {
        MainActivityItem::RoomGroup { room_name, .. } => {
            if !app.expanded_main_rooms.insert(room_name.clone()) {
                app.expanded_main_rooms.remove(&room_name);
            }
            app.clamp_main_selection();
        }
        MainActivityItem::CallsGroup { .. } => {
            app.expanded_main_calls = !app.expanded_main_calls;
            app.clamp_main_selection();
        }
        MainActivityItem::RoomPeer { peer, .. } => {
            app.screen = Screen::Peers;
            if let Some(status) = &app.status {
                if let Some(index) = visible_voice_peers(status)
                    .iter()
                    .position(|candidate| candidate.peer_id == peer.peer_id)
                {
                    app.selected_peer = index;
                }
            }
            app.set_flash(default_flash(Screen::Peers));
        }
        MainActivityItem::CallPeer { peer } => {
            app.screen = Screen::Calls;
            if let Some(status) = &app.status {
                if let Some(index) = active_call_peers(status)
                    .iter()
                    .position(|candidate| candidate.peer_id == peer.peer_id)
                {
                    app.selected_call = index;
                }
            }
            app.set_flash(default_flash(Screen::Calls));
        }
    }
}

async fn refresh_status(app: &mut UiApp) {
    match client::send_request_to(&app.control_addr, ControlRequest::GetStatus).await {
        Ok(ControlResponse::Status(status)) => {
            app.status = Some(status);
            sync_main_expansions(app);
            app.clamp_selection();
            app.clamp_config_selection();
            app.clamp_main_selection();
            app.clamp_room_selection();
            app.clamp_call_selection();
            app.clamp_known_peer_selection();
            app.clamp_seen_user_selection();
            app.clamp_discovered_peer_selection();
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
            sync_main_expansions(app);
            app.clamp_selection();
            app.clamp_config_selection();
            app.clamp_main_selection();
            app.clamp_room_selection();
            app.clamp_call_selection();
            app.clamp_known_peer_selection();
            app.clamp_seen_user_selection();
            app.clamp_discovered_peer_selection();
        }
    }
}

fn sync_main_expansions(app: &mut UiApp) {
    let Some(status) = &app.status else {
        return;
    };
    let engaged_room_names: Vec<String> = status
        .rooms
        .iter()
        .filter(|room| room.engaged)
        .map(|room| room.name.clone())
        .collect();
    if app.expanded_main_rooms.is_empty() {
        for room_name in engaged_room_names {
            app.expanded_main_rooms.insert(room_name);
        }
    } else {
        app.expanded_main_rooms
            .retain(|room_name| status.rooms.iter().any(|room| room.name == *room_name));
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
        .and_then(|status| status.network.friends.get(app.selected_known_peer))
        .cloned();

    let Some(peer) = peer else {
        app.set_flash("no friend selected");
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
        app.set_flash("no friend selected");
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

async fn toggle_trusted_contact(app: &mut UiApp) {
    let Some(peer) = app.selected_known_peer().cloned() else {
        app.set_flash("no friend selected");
        return;
    };

    let request = if peer.trusted_contact {
        ControlRequest::UnmarkTrustedContact {
            peer_id: peer.peer_id.clone(),
        }
    } else {
        ControlRequest::MarkTrustedContact {
            peer_id: peer.peer_id.clone(),
        }
    };

    let response = client::send_request_to(&app.control_addr, request).await;
    app.set_flash(render_message(response));
    refresh_status(app).await;
}

async fn reconnect_selected_seen_user(app: &mut UiApp) {
    let Some(peer) = app.selected_seen_user().cloned() else {
        app.set_flash("no seen user selected");
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

async fn save_selected_seen_user(app: &mut UiApp) {
    let Some(peer_id) = app.selected_seen_user().map(|peer| peer.peer_id.clone()) else {
        app.set_flash("no seen user selected");
        return;
    };

    let response =
        client::send_request_to(&app.control_addr, ControlRequest::SaveKnownPeer { peer_id }).await;
    app.set_flash(render_message(response));
    refresh_status(app).await;
}

async fn reconnect_selected_discovered_peer(app: &mut UiApp) {
    let Some(peer) = app.selected_discovered_peer().cloned() else {
        app.set_flash("no network peer selected");
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

fn preferred_clipboard_invite(status: &voicers_core::DaemonStatus) -> Option<String> {
    status
        .rooms
        .iter()
        .find(|room| room.engaged)
        .and_then(|room| room.current_invite.as_ref())
        .and_then(|invite| invite.share_invite.clone())
}

fn preferred_call_invite(status: &voicers_core::DaemonStatus) -> Option<String> {
    status.network.direct_call_invite.clone()
}

fn format_invite_preview(invite: &str) -> String {
    const MAX_PREVIEW_CHARS: usize = 88;

    let trimmed = invite.trim();
    let char_count = trimmed.chars().count();
    if char_count <= MAX_PREVIEW_CHARS {
        trimmed.to_string()
    } else {
        let head: String = trimmed.chars().take(56).collect();
        let tail: String = trimmed
            .chars()
            .rev()
            .take(20)
            .collect::<Vec<_>>()
            .into_iter()
            .rev()
            .collect();
        format!("{head}...{tail} ({char_count} chars)")
    }
}

fn current_room_invite(
    status: &voicers_core::DaemonStatus,
) -> Option<&voicers_core::RoomInviteSummary> {
    status
        .rooms
        .iter()
        .find(|room| room.engaged)
        .and_then(|room| room.current_invite.as_ref())
}

fn draw(frame: &mut Frame, app: &UiApp) {
    frame.render_widget(
        Block::default().style(Style::default().bg(color_bg()).fg(color_text())),
        frame.area(),
    );

    let root = Layout::vertical([
        Constraint::Length(3),
        Constraint::Length(8),
        Constraint::Min(12),
        Constraint::Length(3),
    ])
    .split(frame.area());

    draw_tabs(frame, app, root[0]);
    draw_summary(frame, app, root[1]);
    match app.screen {
        Screen::Main => {
            let middle =
                Layout::horizontal([Constraint::Length(48), Constraint::Min(40)]).split(root[2]);
            draw_main_screen(frame, app, middle[0]);
            draw_main_details(frame, app, middle[1]);
        }
        Screen::Peers => {
            let middle =
                Layout::horizontal([Constraint::Length(52), Constraint::Min(36)]).split(root[2]);
            draw_live_peers_screen(frame, app, middle[0]);
            draw_live_peer_details(frame, app, middle[1]);
        }
        Screen::Rooms => {
            let middle =
                Layout::horizontal([Constraint::Length(48), Constraint::Min(40)]).split(root[2]);
            draw_rooms_screen(frame, app, middle[0]);
            draw_room_details(frame, app, middle[1]);
        }
        Screen::Calls => {
            let middle =
                Layout::horizontal([Constraint::Length(52), Constraint::Min(36)]).split(root[2]);
            draw_calls_screen(frame, app, middle[0]);
            draw_call_details(frame, app, middle[1]);
        }
        Screen::Config => {
            let middle =
                Layout::horizontal([Constraint::Length(60), Constraint::Min(28)]).split(root[2]);
            draw_config_screen(frame, app, middle[0]);
            draw_notes(frame, app, middle[1]);
        }
        Screen::KnownPeers => {
            let middle =
                Layout::horizontal([Constraint::Length(52), Constraint::Min(36)]).split(root[2]);
            draw_known_peers_screen(frame, app, middle[0]);
            draw_known_peer_details(frame, app, middle[1]);
        }
        Screen::SeenUsers => {
            let middle =
                Layout::horizontal([Constraint::Length(52), Constraint::Min(36)]).split(root[2]);
            draw_seen_users_screen(frame, app, middle[0]);
            draw_seen_user_details(frame, app, middle[1]);
        }
        Screen::DiscoveredPeers => {
            let middle =
                Layout::horizontal([Constraint::Length(52), Constraint::Min(36)]).split(root[2]);
            draw_discovered_peers_screen(frame, app, middle[0]);
            draw_discovered_peer_details(frame, app, middle[1]);
        }
        Screen::Help => {
            let middle =
                Layout::horizontal([Constraint::Length(72), Constraint::Min(28)]).split(root[2]);
            draw_help_screen(frame, app, middle[0]);
            draw_help_diagnostics(frame, app, middle[1]);
        }
    }
    draw_footer(frame, app, root[3]);

    if app.input_mode != InputMode::Normal {
        draw_input_popup(frame, app);
    }
}

fn draw_summary(frame: &mut Frame, app: &UiApp, area: Rect) {
    let lines = if let Some(status) = &app.status {
        let room_invite = current_room_invite(status);
        let invite = room_invite
            .and_then(|invite| invite.share_invite.clone())
            .unwrap_or_else(|| "<no room invite yet>".to_string());
        let invite_preview = format_invite_preview(&invite);
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
                Span::styled(invite_preview, Style::default().fg(color_text())),
            ]),
            Line::from(vec![
                label("TYPE"),
                Span::raw(" "),
                Span::styled(
                    if room_invite.is_some() {
                        "room invite for your engaged room"
                    } else {
                        "no room invite available"
                    },
                    Style::default().fg(color_text()),
                ),
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
        .block(panel_block("Rooms", color_accent()))
        .wrap(Wrap { trim: false });
    frame.render_widget(widget, area);
}

fn draw_tabs(frame: &mut Frame, app: &UiApp, area: Rect) {
    let titles = vec![
        Line::from("Main"),
        Line::from("Rooms"),
        Line::from("Calls"),
        Line::from("Friends"),
        Line::from("Peers"),
        Line::from("Configuration"),
        Line::from("Seen Users"),
        Line::from("Network Peers"),
    ];

    let tabs = Tabs::new(titles)
        .block(panel_block("Tabs", color_panel_alt()))
        .select(current_tab_index(app.screen))
        .style(Style::default().fg(color_muted()))
        .highlight_style(
            Style::default()
                .fg(color_text())
                .bg(color_selected())
                .add_modifier(Modifier::BOLD),
        )
        .divider(" ");
    frame.render_widget(tabs, area);
}

fn current_tab_index(screen: Screen) -> usize {
    match screen {
        Screen::Main => 0,
        Screen::Rooms => 1,
        Screen::Calls => 2,
        Screen::KnownPeers => 3,
        Screen::Peers => 4,
        Screen::Config => 5,
        Screen::SeenUsers => 6,
        Screen::DiscoveredPeers => 7,
        Screen::Help => 0,
    }
}

fn draw_main_screen(frame: &mut Frame, app: &UiApp, area: Rect) {
    let items = if let Some(status) = &app.status {
        let items = main_activity_items(status, &app.expanded_main_rooms, app.expanded_main_calls);
        if items.is_empty() {
            vec![ListItem::new(Span::styled(
                "No engaged rooms or active calls",
                Style::default().fg(color_muted()),
            ))]
        } else {
            items.into_iter()
                .map(|item| match item {
                    MainActivityItem::RoomGroup {
                        room_name,
                        engaged_users,
                    } => {
                        let expanded = app.expanded_main_rooms.contains(&room_name);
                        ListItem::new(Line::from(vec![
                            Span::styled(
                                if expanded { "▾ " } else { "▸ " },
                                Style::default().fg(color_muted()),
                            ),
                            Span::styled(
                                room_name,
                                Style::default()
                                    .fg(color_text())
                                    .add_modifier(Modifier::BOLD),
                            ),
                            Span::raw(" "),
                            badge(format!("{engaged_users} users"), color_good(), color_bg()),
                        ]))
                    }
                    MainActivityItem::RoomPeer { peer, .. } => ListItem::new(Line::from(vec![
                        Span::raw("   "),
                        Span::styled(peer.display_name, Style::default().fg(color_text())),
                        Span::raw(" "),
                        badge(
                            if peer.muted { "muted" } else { "open" },
                            if peer.muted {
                                color_warn()
                            } else {
                                color_good()
                            },
                            color_bg(),
                        ),
                    ])),
                    MainActivityItem::CallsGroup { active_calls } => {
                        let expanded = app.expanded_main_calls;
                        ListItem::new(Line::from(vec![
                            Span::styled(
                                if expanded { "▾ " } else { "▸ " },
                                Style::default().fg(color_muted()),
                            ),
                            Span::styled(
                                "Calls",
                                Style::default()
                                    .fg(color_text())
                                    .add_modifier(Modifier::BOLD),
                            ),
                            Span::raw(" "),
                            badge(format!("{active_calls} active"), color_panel_alt(), color_bg()),
                        ]))
                    }
                    MainActivityItem::CallPeer { peer } => ListItem::new(Line::from(vec![
                        Span::raw("   "),
                        Span::styled(peer.display_name, Style::default().fg(color_text())),
                        Span::raw(" "),
                        badge(transport_label(&peer.transport), transport_color(&peer.transport), color_bg()),
                    ])),
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
    if let Some(status) = &app.status {
        if !main_activity_items(status, &app.expanded_main_rooms, app.expanded_main_calls).is_empty() {
            state.select(Some(app.selected_main_item.min(items.len() - 1)));
        }
    }

    let list = List::new(items)
        .block(panel_block("Main", color_panel_alt()))
        .highlight_style(
            Style::default()
                .bg(color_selected())
                .fg(color_text())
                .add_modifier(Modifier::BOLD),
        )
        .highlight_symbol(" > ");
    frame.render_stateful_widget(list, area, &mut state);
}

fn draw_main_details(frame: &mut Frame, app: &UiApp, area: Rect) {
    let lines = if let Some(status) = &app.status {
        let items = main_activity_items(status, &app.expanded_main_rooms, app.expanded_main_calls);
        match items.get(app.selected_main_item) {
            Some(MainActivityItem::RoomGroup {
                room_name,
                engaged_users,
            }) => vec![
                Line::from(Span::styled(
                    room_name.clone(),
                    Style::default().fg(color_text()).add_modifier(Modifier::BOLD),
                )),
                Line::from(""),
                Line::from(vec![
                    label("TYPE"),
                    Span::raw(" "),
                    Span::styled("engaged room", Style::default().fg(color_text())),
                ]),
                Line::from(vec![
                    label("USERS"),
                    Span::raw(" "),
                    Span::styled(engaged_users.to_string(), Style::default().fg(color_text())),
                ]),
                Line::from(""),
                Line::from("Press Enter to expand or collapse this room."),
                Line::from("Use the Rooms tab to explicitly enter or manage rooms."),
            ],
            Some(MainActivityItem::RoomPeer { room_name, peer }) => vec![
                Line::from(Span::styled(
                    peer.display_name.clone(),
                    Style::default().fg(color_text()).add_modifier(Modifier::BOLD),
                )),
                Line::from(vec![
                    Span::styled(format!("@{}", short_id(&peer.peer_id)), Style::default().fg(color_muted())),
                    Span::raw("  "),
                    badge(transport_label(&peer.transport), transport_color(&peer.transport), color_bg()),
                ]),
                Line::from(""),
                Line::from(vec![
                    label("ROOM"),
                    Span::raw(" "),
                    Span::styled(room_name.clone(), Style::default().fg(color_text())),
                ]),
                Line::from(vec![
                    label("ADDR"),
                    Span::raw(" "),
                    Span::styled(peer.address.clone(), Style::default().fg(color_subtle())),
                ]),
                Line::from(""),
                Line::from("Press Enter to jump to the Peers page for this user."),
            ],
            Some(MainActivityItem::CallsGroup { active_calls }) => vec![
                Line::from(Span::styled(
                    "Calls",
                    Style::default().fg(color_text()).add_modifier(Modifier::BOLD),
                )),
                Line::from(""),
                Line::from(vec![
                    label("TYPE"),
                    Span::raw(" "),
                    Span::styled("direct calls", Style::default().fg(color_text())),
                ]),
                Line::from(vec![
                    label("ACTIVE"),
                    Span::raw(" "),
                    Span::styled(active_calls.to_string(), Style::default().fg(color_text())),
                ]),
                Line::from(""),
                Line::from("Press Enter to expand or collapse calls."),
                Line::from("Use the Calls tab for call-specific actions and invite copy."),
            ],
            Some(MainActivityItem::CallPeer { peer }) => vec![
                Line::from(Span::styled(
                    peer.display_name.clone(),
                    Style::default().fg(color_text()).add_modifier(Modifier::BOLD),
                )),
                Line::from(vec![
                    Span::styled(format!("@{}", short_id(&peer.peer_id)), Style::default().fg(color_muted())),
                    Span::raw("  "),
                    badge(transport_label(&peer.transport), transport_color(&peer.transport), color_bg()),
                ]),
                Line::from(""),
                Line::from(vec![
                    label("ADDR"),
                    Span::raw(" "),
                    Span::styled(peer.address.clone(), Style::default().fg(color_subtle())),
                ]),
                Line::from(""),
                Line::from("Press Enter to jump to the Calls page for this user."),
            ],
            None => vec![Line::from(Span::styled(
                "Select a room or call group",
                Style::default().fg(color_muted()),
            ))],
        }
    } else {
        vec![Line::from(Span::styled(
            "Daemon unavailable",
            Style::default().fg(color_warn()),
        ))]
    };

    let widget = Paragraph::new(lines)
        .block(panel_block("Details", color_panel_alt()))
        .wrap(Wrap { trim: false });
    frame.render_widget(widget, area);
}

#[allow(dead_code)]
fn draw_home_panel(frame: &mut Frame, app: &UiApp, area: Rect) {
    let lines = if let Some(status) = &app.status {
        let room_invite = current_room_invite(status);
        let invite = room_invite
            .and_then(|invite| invite.share_invite.clone())
            .or_else(|| status.network.direct_call_invite.clone())
            .unwrap_or_else(|| "<preparing shareable invite>".to_string());
        let invite_preview = format_invite_preview(&invite);
        let invite_ready = invite != "<preparing shareable invite>";
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
                    if invite_ready {
                        "ready"
                    } else {
                        "preparing"
                    },
                    if invite_ready {
                        color_good()
                    } else {
                        color_panel_alt()
                    },
                    color_bg(),
                ),
            ]),
            Line::from(Span::styled(invite_preview, Style::default().fg(color_text()))),
            Line::from(vec![
                label("KIND"),
                Span::raw(" "),
                Span::styled(
                    if room_invite.is_some() {
                        "room invite"
                    } else {
                        "direct-call invite"
                    },
                    Style::default().fg(color_text()),
                ),
            ]),
            Line::from(vec![
                label("CODE"),
                Span::raw(" "),
                Span::styled(
                    room_invite
                        .map(|invite| invite.invite_code.clone())
                        .unwrap_or_else(|| "<none>".to_string()),
                    Style::default().fg(color_text()),
                ),
            ]),
            Line::from(vec![
                label("EXPIRES"),
                Span::raw(" "),
                Span::styled(
                    room_invite
                        .and_then(|invite| invite.expires_at_ms)
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

#[allow(dead_code)]
fn draw_peer_list(frame: &mut Frame, app: &UiApp, area: Rect) {
    let title = app
        .status
        .as_ref()
        .map(|status| {
            format!(
                "room: {}",
                status
                    .session
                    .room_name
                    .clone()
                    .unwrap_or_else(|| DEFAULT_ROOM_NAME.to_string())
            )
        })
        .unwrap_or_else(|| "room".to_string());
    let items = if let Some(status) = &app.status {
        let visible_peers = visible_voice_peers(status);
        if visible_peers.is_empty() {
            vec![ListItem::new(vec![
                Line::from(Span::styled(
                    "is anyone there?",
                    Style::default().fg(color_muted()),
                )),
            ])]
        } else {
            visible_peers
                .into_iter()
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
        .map(|status| !visible_voice_peers(status).is_empty())
        .unwrap_or(false)
    {
        state.select(Some(app.selected_peer));
    }

    let list = List::new(items)
        .block(panel_block(&title, color_panel_alt()))
        .highlight_style(
            Style::default()
                .bg(color_selected())
                .fg(color_text())
                .add_modifier(Modifier::BOLD),
        )
        .highlight_symbol(" > ");
    frame.render_stateful_widget(list, area, &mut state);
}

fn draw_live_peers_screen(frame: &mut Frame, app: &UiApp, area: Rect) {
    let items = if let Some(status) = &app.status {
        let visible_peers = visible_voice_peers(status);
        if visible_peers.is_empty() {
            vec![ListItem::new(vec![
                Line::from(vec![
                    Span::styled(
                        "Nobody here yet",
                        Style::default()
                            .fg(color_text())
                            .add_modifier(Modifier::BOLD),
                    ),
                    Span::raw("  "),
                    badge(
                        status
                            .session
                            .room_name
                            .clone()
                            .unwrap_or_else(|| DEFAULT_ROOM_NAME.to_string()),
                        color_panel_alt(),
                        color_text(),
                    ),
                ]),
                Line::from(Span::styled(
                    "Share your invite to bring someone into this room.",
                    Style::default().fg(color_muted()),
                )),
            ])]
        } else {
            visible_peers
                .into_iter()
                .map(|peer| {
                    let relay_label = peer.media.route_via.as_deref().map(|via_id| {
                        let name = status
                            .network
                            .known_peers
                            .iter()
                            .find(|k| k.peer_id == via_id)
                            .map(|k| k.display_name.clone())
                            .unwrap_or_else(|| format!("@{}", &via_id[..via_id.len().min(8)]));
                        format!("via @{name}")
                    });
                    ListItem::new(vec![
                        Line::from(vec![
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
                            Span::raw(" "),
                            badge(
                                if peer.muted { "muted" } else { "open" },
                                if peer.muted {
                                    color_warn()
                                } else {
                                    color_good()
                                },
                                color_bg(),
                            ),
                            Span::raw(relay_label.map(|l| format!("  {l}")).unwrap_or_default()),
                        ]),
                        Line::from(vec![
                            Span::styled(
                                format!("@{}", short_id(&peer.peer_id)),
                                Style::default().fg(color_muted()),
                            ),
                            Span::raw("  "),
                            Span::styled(peer.output_bus.clone(), Style::default().fg(color_muted())),
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
        .map(|status| !visible_voice_peers(status).is_empty())
        .unwrap_or(false)
    {
        state.select(Some(app.selected_peer.min(items.len() - 1)));
    }

    let list = List::new(items)
        .block(panel_block("Peers", color_panel_alt()))
        .highlight_style(
            Style::default()
                .bg(color_selected())
                .fg(color_text())
                .add_modifier(Modifier::BOLD),
        )
        .highlight_symbol(" > ");
    frame.render_stateful_widget(list, area, &mut state);
}

fn draw_rooms_screen(frame: &mut Frame, app: &UiApp, area: Rect) {
    let items = if let Some(status) = &app.status {
        let rooms = known_rooms(status);
        if rooms.is_empty() {
            vec![ListItem::new(Span::styled(
                "No known rooms yet",
                Style::default().fg(color_muted()),
            ))]
        } else {
            rooms.into_iter()
                .map(|room| {
                    ListItem::new(vec![
                        Line::from(vec![
                            Span::styled(
                                room.name.clone(),
                                Style::default()
                                    .fg(color_text())
                                    .add_modifier(Modifier::BOLD),
                            ),
                            Span::raw(" "),
                            if room.is_current {
                                badge("current", color_accent(), color_bg())
                            } else {
                                badge("known", color_panel_alt(), color_bg())
                            },
                            Span::raw(" "),
                            if room.engaged_users > 0 {
                                badge(
                                    format!("{} engaged", room.engaged_users),
                                    color_good(),
                                    color_bg(),
                                )
                            } else {
                                badge("idle", color_panel_alt(), color_bg())
                            },
                        ]),
                        Line::from(vec![
                            Span::styled(
                                format!("{} pending approvals", room.pending_approvals),
                                Style::default().fg(color_muted()),
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
        .map(|status| !known_rooms(status).is_empty())
        .unwrap_or(false)
    {
        state.select(Some(app.selected_room.min(items.len() - 1)));
    }

    let list = List::new(items)
        .block(panel_block("Rooms", color_panel_alt()))
        .highlight_style(
            Style::default()
                .bg(color_selected())
                .fg(color_text())
                .add_modifier(Modifier::BOLD),
        )
        .highlight_symbol(" > ");
    frame.render_stateful_widget(list, area, &mut state);
}

fn draw_room_details(frame: &mut Frame, app: &UiApp, area: Rect) {
    let lines = if let Some(status) = &app.status {
        if let Some(room) = known_rooms(status).get(app.selected_room) {
            vec![
                Line::from(vec![
                    Span::styled(
                        room.name.clone(),
                        Style::default()
                            .fg(color_text())
                            .add_modifier(Modifier::BOLD),
                    ),
                    Span::raw("  "),
                    badge(
                        if room.is_current { "current" } else { "known" },
                        if room.is_current {
                            color_accent()
                        } else {
                            color_panel_alt()
                        },
                        color_bg(),
                    ),
                ]),
                Line::from(vec![
                    label("ENGAGED"),
                    Span::raw(" "),
                    Span::styled(room.engaged_users.to_string(), Style::default().fg(color_text())),
                ]),
                Line::from(vec![
                    label("PENDING"),
                    Span::raw(" "),
                    Span::styled(
                        room.pending_approvals.to_string(),
                        Style::default().fg(color_text()),
                    ),
                ]),
                Line::from(""),
                Line::from("Press Enter to enter this room."),
                Line::from("Press Shift+R to type a different room name."),
            ]
        } else {
            vec![Line::from(Span::styled(
                "Select a room to inspect or engage it",
                Style::default().fg(color_muted()),
            ))]
        }
    } else {
        vec![Line::from(Span::styled(
            "Daemon unavailable",
            Style::default().fg(color_warn()),
        ))]
    };

    let widget = Paragraph::new(lines)
        .block(panel_block("Room Details", color_panel_alt()))
        .wrap(Wrap { trim: false });
    frame.render_widget(widget, area);
}

fn draw_calls_screen(frame: &mut Frame, app: &UiApp, area: Rect) {
    let items = if let Some(status) = &app.status {
        let calls = active_call_peers(status);
        if calls.is_empty() {
            vec![ListItem::new(Span::styled(
                "No active call users outside your current room",
                Style::default().fg(color_muted()),
            ))]
        } else {
            calls
                .into_iter()
                .map(|peer| {
                    let session_label = match &peer.session {
                        voicers_core::PeerSessionState::Active { room_name, .. } => room_name
                            .clone()
                            .unwrap_or_else(|| "roomless".to_string()),
                        voicers_core::PeerSessionState::Handshaking => "handshaking".to_string(),
                        voicers_core::PeerSessionState::None => "transport".to_string(),
                    };
                    let relay_label = peer.media.route_via.as_deref().map(|via_id| {
                        let name = status
                            .network
                            .known_peers
                            .iter()
                            .find(|k| k.peer_id == via_id)
                            .map(|k| k.display_name.clone())
                            .unwrap_or_else(|| format!("@{}", &via_id[..via_id.len().min(8)]));
                        format!("via @{name}")
                    });
                    ListItem::new(vec![
                        Line::from(vec![
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
                            Span::raw(relay_label.map(|l| format!("  {l}")).unwrap_or_default()),
                        ]),
                        Line::from(vec![
                            Span::styled(
                                format!("@{}", short_id(&peer.peer_id)),
                                Style::default().fg(color_muted()),
                            ),
                            Span::raw("  "),
                            Span::styled(session_label, Style::default().fg(color_muted())),
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
        .map(|status| !active_call_peers(status).is_empty())
        .unwrap_or(false)
    {
        state.select(Some(app.selected_call.min(items.len() - 1)));
    }

    let list = List::new(items)
        .block(panel_block("Calls", color_panel_alt()))
        .highlight_style(
            Style::default()
                .bg(color_selected())
                .fg(color_text())
                .add_modifier(Modifier::BOLD),
        )
        .highlight_symbol(" > ");
    frame.render_stateful_widget(list, area, &mut state);
}

fn draw_call_details(frame: &mut Frame, app: &UiApp, area: Rect) {
    let lines = if let Some(status) = &app.status {
        if let Some(peer) = app.selected_call_peer() {
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
                        transport_label(&peer.transport),
                        transport_color(&peer.transport),
                        color_bg(),
                    ),
                ]),
                Line::from(vec![
                    label("PEER"),
                    Span::raw(" "),
                    Span::styled(peer.peer_id.clone(), Style::default().fg(color_muted())),
                ]),
                Line::from(vec![
                    label("ADDR"),
                    Span::raw(" "),
                    Span::styled(peer.address.clone(), Style::default().fg(color_subtle())),
                ]),
                Line::from(vec![
                    label("OUTPUT"),
                    Span::raw(" "),
                    Span::styled(peer.output_bus.clone(), Style::default().fg(color_text())),
                ]),
            ];
            match &peer.session {
                voicers_core::PeerSessionState::Active { room_name, display_name } => {
                    lines.push(Line::from(vec![
                        label("MODE"),
                        Span::raw(" "),
                        Span::styled("call", Style::default().fg(color_text())),
                    ]));
                    lines.push(Line::from(vec![
                        label("REMOTE"),
                        Span::raw(" "),
                        Span::styled(display_name.clone(), Style::default().fg(color_text())),
                    ]));
                    lines.push(Line::from(vec![
                        label("ROOM"),
                        Span::raw(" "),
                        Span::styled(
                            room_name.clone().unwrap_or_else(|| "roomless".to_string()),
                            Style::default().fg(color_text()),
                        ),
                    ]));
                }
                voicers_core::PeerSessionState::Handshaking => {
                    lines.push(Line::from(vec![
                        label("MODE"),
                        Span::raw(" "),
                        Span::styled("handshaking", Style::default().fg(color_text())),
                    ]));
                }
                voicers_core::PeerSessionState::None => {
                    lines.push(Line::from(vec![
                        label("MODE"),
                        Span::raw(" "),
                        Span::styled("transport only", Style::default().fg(color_text())),
                    ]));
                }
            }
            lines.push(Line::from(""));
            lines.push(Line::from(format!(
                "Current room is {}. This user is listed here because they are active outside that room engagement.",
                status.session.room_name.clone().unwrap_or_else(|| DEFAULT_ROOM_NAME.to_string())
            )));
            lines
        } else {
            vec![Line::from(Span::styled(
                "Select an active call user to inspect it",
                Style::default().fg(color_muted()),
            ))]
        }
    } else {
        vec![Line::from(Span::styled(
            "Daemon unavailable",
            Style::default().fg(color_warn()),
        ))]
    };

    let widget = Paragraph::new(lines)
        .block(panel_block("Call Details", color_panel_alt()))
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

        for peer in visible_voice_peers(status) {
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
        if status.network.friends.is_empty() {
            vec![ListItem::new(Span::styled(
                "No friends yet",
                Style::default().fg(color_muted()),
            ))]
        } else {
            status
                .network
                .friends
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
                            if peer.trusted_contact {
                                Span::styled(" [T]", Style::default().fg(color_accent()))
                            } else {
                                Span::raw("")
                            },
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
        .map(|status| !status.network.friends.is_empty())
        .unwrap_or(false)
    {
        state.select(Some(app.selected_known_peer.min(items.len() - 1)));
    }

    let list = List::new(items)
        .block(panel_block("Friends", color_panel_alt()))
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
                .friends
                .get(app.selected_known_peer)
                .map(|peer| known_peer_lines(status, peer))
        })
        .unwrap_or_else(|| {
            vec![Line::from(Span::styled(
                "Select a friend to inspect saved addresses",
                Style::default().fg(color_muted()),
            ))]
        });

    let widget = Paragraph::new(lines)
        .block(panel_block("Friend Record", color_panel_alt()))
        .wrap(Wrap { trim: false });
    frame.render_widget(widget, area);
}

fn draw_seen_users_screen(frame: &mut Frame, app: &UiApp, area: Rect) {
    let items = if let Some(status) = &app.status {
        if status.network.seen_users.is_empty() {
            vec![ListItem::new(Span::styled(
                "No seen users yet",
                Style::default().fg(color_muted()),
            ))]
        } else {
            status
                .network
                .seen_users
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
                            badge(
                                if peer.connected { "online" } else { "seen" },
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
        .map(|status| !status.network.seen_users.is_empty())
        .unwrap_or(false)
    {
        state.select(Some(app.selected_seen_user.min(items.len() - 1)));
    }

    let list = List::new(items)
        .block(panel_block("Seen Users", color_panel_alt()))
        .highlight_style(
            Style::default()
                .bg(color_selected())
                .fg(color_text())
                .add_modifier(Modifier::BOLD),
        )
        .highlight_symbol(" > ");
    frame.render_stateful_widget(list, area, &mut state);
}

fn draw_seen_user_details(frame: &mut Frame, app: &UiApp, area: Rect) {
    let lines = app
        .status
        .as_ref()
        .and_then(|status| {
            status
                .network
                .seen_users
                .get(app.selected_seen_user)
                .map(|peer| known_peer_lines(status, peer))
        })
        .unwrap_or_else(|| {
            vec![Line::from(Span::styled(
                "Select a seen user to inspect saved addresses",
                Style::default().fg(color_muted()),
            ))]
        });

    let widget = Paragraph::new(lines)
        .block(panel_block("Seen User Record", color_panel_alt()))
        .wrap(Wrap { trim: false });
    frame.render_widget(widget, area);
}

fn draw_discovered_peers_screen(frame: &mut Frame, app: &UiApp, area: Rect) {
    let items = if let Some(status) = &app.status {
        if status.network.discovered_peers.is_empty() {
            vec![ListItem::new(Span::styled(
                "No discovered network peers",
                Style::default().fg(color_muted()),
            ))]
        } else {
            status
                .network
                .discovered_peers
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
                            badge(
                                if peer.connected { "linked" } else { "cached" },
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
                                    .unwrap_or_else(|| "<no saved route>".to_string()),
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
        .map(|status| !status.network.discovered_peers.is_empty())
        .unwrap_or(false)
    {
        state.select(Some(app.selected_discovered_peer.min(items.len() - 1)));
    }

    let list = List::new(items)
        .block(panel_block("Network Peers", color_panel_alt()))
        .highlight_style(
            Style::default()
                .bg(color_selected())
                .fg(color_text())
                .add_modifier(Modifier::BOLD),
        )
        .highlight_symbol(" > ");
    frame.render_stateful_widget(list, area, &mut state);
}

fn draw_discovered_peer_details(frame: &mut Frame, app: &UiApp, area: Rect) {
    let lines = app
        .status
        .as_ref()
        .and_then(|status| {
            status
                .network
                .discovered_peers
                .get(app.selected_discovered_peer)
                .map(|peer| {
                    let mut lines = known_peer_lines(status, peer);
                    lines.insert(
                        0,
                        Line::from(Span::styled(
                            "Routing/discovery record only. This is not treated as a friend or an engaged user.",
                            Style::default().fg(color_muted()),
                        )),
                    );
                    lines
                })
        })
        .unwrap_or_else(|| {
            vec![Line::from(Span::styled(
                "Select a network peer to inspect saved route hints",
                Style::default().fg(color_muted()),
            ))]
        });

    let widget = Paragraph::new(lines)
        .block(panel_block("Route Record", color_panel_alt()))
        .wrap(Wrap { trim: false });
    frame.render_widget(widget, area);
}

fn draw_live_peer_details(frame: &mut Frame, app: &UiApp, area: Rect) {
    let lines = if let Some(status) = &app.status {
        if let Some(peer) = app.selected_peer() {
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
                        transport_label(&peer.transport),
                        transport_color(&peer.transport),
                        color_bg(),
                    ),
                ]),
                Line::from(vec![
                    label("PEER"),
                    Span::raw(" "),
                    Span::styled(peer.peer_id.clone(), Style::default().fg(color_muted())),
                ]),
                Line::from(vec![
                    label("ROOM"),
                    Span::raw(" "),
                    Span::styled(
                        status
                            .session
                            .room_name
                            .clone()
                            .unwrap_or_else(|| DEFAULT_ROOM_NAME.to_string()),
                        Style::default().fg(color_text()),
                    ),
                ]),
                Line::from(vec![
                    label("ADDR"),
                    Span::raw(" "),
                    Span::styled(peer.address.clone(), Style::default().fg(color_subtle())),
                ]),
                Line::from(vec![
                    label("OUTPUT"),
                    Span::raw(" "),
                    Span::styled(peer.output_bus.clone(), Style::default().fg(color_text())),
                ]),
                Line::from(vec![
                    label("VOLUME"),
                    Span::raw(" "),
                    Span::styled(
                        format!("{}%", peer.output_volume_percent),
                        Style::default().fg(color_text()),
                    ),
                ]),
                Line::from(vec![
                    label("MUTE"),
                    Span::raw(" "),
                    Span::styled(
                        if peer.muted { "muted" } else { "open" },
                        Style::default().fg(if peer.muted {
                            color_warn()
                        } else {
                            color_good()
                        }),
                    ),
                ]),
            ];

            if let voicers_core::PeerSessionState::Active {
                room_name,
                display_name,
            } = &peer.session
            {
                lines.push(Line::from(vec![
                    label("SESSION"),
                    Span::raw(" "),
                    Span::styled(display_name.clone(), Style::default().fg(color_text())),
                ]));
                if let Some(room_name) = room_name {
                    lines.push(Line::from(vec![
                        label("REMOTE ROOM"),
                        Span::raw(" "),
                        Span::styled(room_name.clone(), Style::default().fg(color_text())),
                    ]));
                }
            }

            lines
        } else {
            vec![Line::from(Span::styled(
                format!(
                    "Room {} is empty right now",
                    status
                        .session
                        .room_name
                        .clone()
                        .unwrap_or_else(|| DEFAULT_ROOM_NAME.to_string())
                ),
                Style::default().fg(color_muted()),
            ))]
        }
    } else {
        vec![Line::from(Span::styled(
            "Daemon unavailable",
            Style::default().fg(color_warn()),
        ))]
    };

    let widget = Paragraph::new(lines)
        .block(panel_block("Peer Details", color_panel_alt()))
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
                "Friends reconnect sends the peer id, then the daemon ranks and retries saved addresses.",
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
    let target = app.previous_screen;
    let mut lines = vec![
        Line::from(vec![
            Span::styled(
                help_title(target),
                Style::default()
                    .fg(color_text())
                    .add_modifier(Modifier::BOLD),
            ),
            Span::raw("  "),
            badge("keybinds", color_accent(), color_bg()),
        ]),
        Line::from(help_summary(target)),
        Line::from(""),
        Line::from(Span::styled(
            "Available Keys",
            Style::default()
                .fg(color_text())
                .add_modifier(Modifier::BOLD),
        )),
    ];

    for (keys, meaning) in help_keybinds(target) {
        lines.push(Line::from(vec![
            badge(*keys, color_panel_alt(), color_text()),
            Span::raw(" "),
            Span::styled(*meaning, Style::default().fg(color_text())),
        ]));
    }

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
                label("FRIENDS"),
                Span::raw(" "),
                Span::styled(
                    status.network.friends.len().to_string(),
                    Style::default().fg(color_text()),
                ),
            ]),
            Line::from(vec![
                label("SEEN"),
                Span::raw(" "),
                Span::styled(
                    status.network.seen_users.len().to_string(),
                    Style::default().fg(color_text()),
                ),
            ]),
            Line::from(vec![
                label("NETWORK"),
                Span::raw(" "),
                Span::styled(
                    status.network.discovered_peers.len().to_string(),
                    Style::default().fg(color_text()),
                ),
            ]),
            Line::from(vec![
                label("ROOMS"),
                Span::raw(" "),
                Span::styled(
                    known_rooms(status).len().to_string(),
                    Style::default().fg(color_text()),
                ),
            ]),
            Line::from(vec![
                label("CALLS"),
                Span::raw(" "),
                Span::styled(
                    active_call_peers(status).len().to_string(),
                    Style::default().fg(color_text()),
                ),
            ]),
            Line::from(""),
            Line::from(Span::styled(
                "Notes",
                Style::default()
                    .fg(color_text())
                    .add_modifier(Modifier::BOLD),
            )),
        ];

        if status.notes.is_empty() {
            lines.push(Line::from(Span::styled(
                "No recent activity notes",
                Style::default().fg(color_muted()),
            )));
        } else {
            for note in &status.notes {
                lines.push(Line::from(Span::styled(
                    note.clone(),
                    Style::default().fg(color_muted()),
                )));
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
        .block(panel_block("Context", color_panel_alt()))
        .wrap(Wrap { trim: false });
    frame.render_widget(widget, area);
}

fn help_title(screen: Screen) -> &'static str {
    match screen {
        Screen::Main => "Main",
        Screen::Peers => "Engaged Users",
        Screen::Rooms => "Rooms",
        Screen::Calls => "Calls",
        Screen::Config => "Configuration",
        Screen::KnownPeers => "Friends",
        Screen::SeenUsers => "Seen Users",
        Screen::DiscoveredPeers => "Network Peers",
        Screen::Help => "Help",
    }
}

fn help_summary(screen: Screen) -> &'static str {
    match screen {
        Screen::Main => "Engaged rooms and active calls grouped into expandable activity sections.",
        Screen::Peers => "Users actively engaged in your current room.",
        Screen::Rooms => "Durable room records with engagement state, roles, and current-room badges.",
        Screen::Calls => "Active users whose session is outside your current room engagement.",
        Screen::Config => "Local audio controls and per-engaged-user output controls.",
        Screen::KnownPeers => "Your saved friend list with reconnect and rename actions.",
        Screen::SeenUsers => "Users you have previously engaged with but have not explicitly saved as friends.",
        Screen::DiscoveredPeers => "Diagnostic routing records learned from invites, DHT lookups, and other network discovery.",
        Screen::Help => "Per-screen help and keybind reference.",
    }
}

fn help_keybinds(screen: Screen) -> &'static [(&'static str, &'static str)] {
    match screen {
        Screen::Main => &[
            ("q", "Quit the TUI."),
            ("?", "Open this page-specific help."),
            ("d / J", "Open the join dialog and paste an invite code."),
            ("j / k or arrows", "Move the activity selection."),
            ("Enter", "Expand/collapse a group or open the relevant detailed page."),
            ("r / l / p", "Open Rooms, Calls, or Engaged Users."),
            ("o / v / t", "Open Friends, Seen Users, or Network Peers."),
            ("c", "Open Configuration."),
            ("g", "Refresh status immediately."),
        ],
        Screen::Peers => &[
            ("q", "Quit the TUI."),
            ("?", "Open this page-specific help."),
            ("J", "Open the join dialog and paste an invite code."),
            ("j / k or arrows", "Move the selection."),
            ("x", "Mute or unmute the selected engaged user."),
            ("a", "Add the selected engaged user to Friends."),
            ("p / Esc", "Return to Main."),
            ("r / l / o / v / t / c", "Jump to Rooms, Calls, Friends, Seen Users, Network Peers, or Configuration."),
            ("g", "Refresh status immediately."),
        ],
        Screen::Rooms => &[
            ("q", "Quit the TUI."),
            ("?", "Open this page-specific help."),
            ("j / k or arrows", "Move the selection."),
            ("d / J", "Open the join dialog and paste an invite code."),
            ("Enter", "Enter the selected room."),
            ("R", "Type a new room name and enter it."),
            ("i", "Copy the current room invite to the clipboard."),
            ("C", "Rotate the room invite for your engaged custom room."),
            ("y / w / n", "Approve once, approve and whitelist, or reject a pending join."),
            ("m / u", "Toggle self mute or rename yourself."),
            ("Tab", "Edit the daemon control address."),
            ("r / Esc", "Return to Main."),
            ("p / l / o / v / t", "Jump to Engaged Users, Calls, Friends, Seen Users, or Network Peers."),
            ("g", "Refresh status immediately."),
        ],
        Screen::Calls => &[
            ("q", "Quit the TUI."),
            ("?", "Open this page-specific help."),
            ("J", "Open the join dialog and paste an invite code."),
            ("j / k or arrows", "Move the selection."),
            ("x", "Mute or unmute the selected call user."),
            ("a", "Add the selected call user to Friends."),
            ("i", "Copy your direct-call invite to the clipboard."),
            ("l / Esc", "Return to Main."),
            ("p / r / o / v / t", "Jump to Engaged Users, Rooms, Friends, Seen Users, or Network Peers."),
            ("g", "Refresh status immediately."),
        ],
        Screen::Config => &[
            ("q", "Quit the TUI."),
            ("?", "Open this page-specific help."),
            ("J", "Open the join dialog and paste an invite code."),
            ("j / k or arrows", "Move the selection."),
            ("h / l or arrows", "Lower or raise the selected value."),
            ("Enter", "Activate the selected capture device."),
            ("u", "Rename yourself."),
            ("c / Esc", "Return to Main."),
            ("p / r / l / o / v / t", "Jump to Engaged Users, Rooms, Calls, Friends, Seen Users, or Network Peers."),
            ("g", "Refresh status immediately."),
        ],
        Screen::KnownPeers => &[
            ("q", "Quit the TUI."),
            ("?", "Open this page-specific help."),
            ("J", "Open the join dialog and paste an invite code."),
            ("j / k or arrows", "Move the selection."),
            ("Enter / d", "Reconnect to the selected friend."),
            ("e", "Rename the selected friend."),
            ("D", "Remove the selected friend from Friends."),
            ("o / Esc", "Return to Main."),
            ("p / r / l / v / t / c", "Jump to Engaged Users, Rooms, Calls, Seen Users, Network Peers, or Configuration."),
            ("g", "Refresh status immediately."),
        ],
        Screen::SeenUsers => &[
            ("q", "Quit the TUI."),
            ("?", "Open this page-specific help."),
            ("J", "Open the join dialog and paste an invite code."),
            ("j / k or arrows", "Move the selection."),
            ("Enter / d", "Reconnect to the selected seen user."),
            ("a", "Add the selected seen user to Friends."),
            ("v / Esc", "Return to Main."),
            ("o / p / r / l / t / c", "Jump to Friends, Engaged Users, Rooms, Calls, Network Peers, or Configuration."),
            ("g", "Refresh status immediately."),
        ],
        Screen::DiscoveredPeers => &[
            ("q", "Quit the TUI."),
            ("?", "Open this page-specific help."),
            ("J", "Open the join dialog and paste an invite code."),
            ("j / k or arrows", "Move the selection."),
            ("Enter / d", "Dial the selected routing candidate by peer id."),
            ("t / Esc", "Return to Main."),
            ("p / r / l / o / v / c", "Jump to Engaged Users, Rooms, Calls, Friends, Seen Users, or Configuration."),
            ("g", "Refresh status immediately."),
        ],
        Screen::Help => &[
            ("q", "Quit the TUI."),
            ("? / Esc", "Close help and return to the previous page."),
            ("g", "Refresh status immediately."),
        ],
    }
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
