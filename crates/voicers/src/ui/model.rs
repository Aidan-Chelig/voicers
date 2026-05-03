use std::{
    collections::HashSet,
    path::{Path, PathBuf},
    time::{Duration, Instant},
};

use voicers_core::{DaemonStatus, KnownPeerSummary, PeerSessionState, PeerSummary, RoomSummary};

pub struct UiApp {
    pub control_addr: String,
    pub daemon_bin: PathBuf,
    pub status: Option<DaemonStatus>,
    pub flash: String,
    pub flash_until: Option<Instant>,
    pub input_mode: InputMode,
    pub dial_input: String,
    pub room_input: String,
    pub rename_input: String,
    pub screen: Screen,
    pub previous_screen: Screen,
    pub selected_peer: usize,
    pub selected_main_item: usize,
    pub selected_config_item: usize,
    pub selected_known_peer: usize,
    pub selected_seen_user: usize,
    pub selected_discovered_peer: usize,
    pub selected_room: usize,
    pub selected_call: usize,
    pub expanded_main_rooms: HashSet<String>,
    pub expanded_main_calls: bool,
    pub daemon_launch: DaemonLaunchState,
    pub default_room_initialized: bool,
}

pub const FLASH_DURATION: Duration = Duration::from_secs(3);

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum InputMode {
    Normal,
    Dial,
    Room,
    ControlAddr,
    RenameSelf,
    RenameKnownPeer,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Screen {
    Main,
    Peers,
    Rooms,
    Calls,
    Config,
    KnownPeers,
    SeenUsers,
    DiscoveredPeers,
    Help,
}

pub struct FallbackCandidatePreview {
    pub address: String,
    pub path_label: &'static str,
    pub score_delta: i64,
    pub successes: u64,
    pub failures: u64,
    pub is_last_dial: bool,
}

pub struct KnownRoomPreview {
    pub name: String,
    pub engaged_users: usize,
    pub pending_approvals: usize,
    pub is_current: bool,
}

#[derive(Clone)]
pub enum MainActivityItem {
    RoomGroup {
        room_name: String,
        engaged_users: usize,
    },
    RoomPeer {
        room_name: String,
        peer: PeerSummary,
    },
    CallsGroup {
        active_calls: usize,
    },
    CallPeer {
        peer: PeerSummary,
    },
}

pub enum ConfigItem {
    InputGain,
    CaptureDevice(String),
    PeerVolume { peer_id: String },
}

#[derive(Default)]
pub struct DaemonLaunchState {
    pub attempted_auto_start: bool,
    pub last_launch_at: Option<Instant>,
}

pub struct UiConfig {
    pub control_addr: String,
    pub daemon_bin: PathBuf,
}

impl UiApp {
    pub fn new(config: UiConfig) -> Self {
        Self {
            control_addr: config.control_addr,
            daemon_bin: config.daemon_bin,
            status: None,
            flash: default_flash(Screen::Main).to_string(),
            flash_until: None,
            input_mode: InputMode::Normal,
            dial_input: "/ip4/127.0.0.1/tcp/".to_string(),
            room_input: "dev-room".to_string(),
            rename_input: String::new(),
            screen: Screen::Main,
            previous_screen: Screen::Main,
            selected_peer: 0,
            selected_main_item: 0,
            selected_config_item: 0,
            selected_known_peer: 0,
            selected_seen_user: 0,
            selected_discovered_peer: 0,
            selected_room: 0,
            selected_call: 0,
            expanded_main_rooms: HashSet::new(),
            expanded_main_calls: true,
            daemon_launch: DaemonLaunchState::default(),
            default_room_initialized: false,
        }
    }

    pub fn selected_peer_id(&self) -> Option<String> {
        visible_voice_peers(self.status.as_ref()?)
            .get(self.selected_peer)
            .map(|peer| peer.peer_id.clone())
    }

    pub fn selected_peer(&self) -> Option<&PeerSummary> {
        visible_voice_peers(self.status.as_ref()?)
            .get(self.selected_peer)
            .copied()
    }

    pub fn selected_call_peer(&self) -> Option<&PeerSummary> {
        active_call_peers(self.status.as_ref()?)
            .get(self.selected_call)
            .copied()
    }

    pub fn selected_known_peer(&self) -> Option<&KnownPeerSummary> {
        self.status
            .as_ref()?
            .network
            .friends
            .get(self.selected_known_peer)
    }

    pub fn selected_seen_user(&self) -> Option<&KnownPeerSummary> {
        self.status
            .as_ref()?
            .network
            .seen_users
            .get(self.selected_seen_user)
    }

    pub fn selected_discovered_peer(&self) -> Option<&KnownPeerSummary> {
        self.status
            .as_ref()?
            .network
            .discovered_peers
            .get(self.selected_discovered_peer)
    }

    pub fn clamp_selection(&mut self) {
        let len = self
            .status
            .as_ref()
            .map(|status| visible_voice_peers(status).len())
            .unwrap_or(0);
        if len == 0 {
            self.selected_peer = 0;
        } else if self.selected_peer >= len {
            self.selected_peer = len - 1;
        }
    }

    pub fn clamp_config_selection(&mut self) {
        let len = self.config_items().len();
        if len == 0 {
            self.selected_config_item = 0;
        } else if self.selected_config_item >= len {
            self.selected_config_item = len - 1;
        }
    }

    pub fn clamp_main_selection(&mut self) {
        let len = self
            .status
            .as_ref()
            .map(|status| main_activity_items(status, &self.expanded_main_rooms, self.expanded_main_calls).len())
            .unwrap_or(0);
        if len == 0 {
            self.selected_main_item = 0;
        } else if self.selected_main_item >= len {
            self.selected_main_item = len - 1;
        }
    }

    pub fn clamp_room_selection(&mut self) {
        let len = self
            .status
            .as_ref()
            .map(|status| known_rooms(status).len())
            .unwrap_or(0);
        if len == 0 {
            self.selected_room = 0;
        } else if self.selected_room >= len {
            self.selected_room = len - 1;
        }
    }

    pub fn clamp_call_selection(&mut self) {
        let len = self
            .status
            .as_ref()
            .map(|status| active_call_peers(status).len())
            .unwrap_or(0);
        if len == 0 {
            self.selected_call = 0;
        } else if self.selected_call >= len {
            self.selected_call = len - 1;
        }
    }

    pub fn clamp_known_peer_selection(&mut self) {
        let len = self
            .status
            .as_ref()
            .map(|status| status.network.friends.len())
            .unwrap_or(0);
        if len == 0 {
            self.selected_known_peer = 0;
        } else if self.selected_known_peer >= len {
            self.selected_known_peer = len - 1;
        }
    }

    pub fn clamp_seen_user_selection(&mut self) {
        let len = self
            .status
            .as_ref()
            .map(|status| status.network.seen_users.len())
            .unwrap_or(0);
        if len == 0 {
            self.selected_seen_user = 0;
        } else if self.selected_seen_user >= len {
            self.selected_seen_user = len - 1;
        }
    }

    pub fn clamp_discovered_peer_selection(&mut self) {
        let len = self
            .status
            .as_ref()
            .map(|status| status.network.discovered_peers.len())
            .unwrap_or(0);
        if len == 0 {
            self.selected_discovered_peer = 0;
        } else if self.selected_discovered_peer >= len {
            self.selected_discovered_peer = len - 1;
        }
    }

    pub fn config_items(&self) -> Vec<ConfigItem> {
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
                visible_voice_peers(status)
                    .into_iter()
                    .map(|peer| ConfigItem::PeerVolume {
                        peer_id: peer.peer_id.clone(),
                    }),
            );
        }
        items
    }

    pub fn set_flash(&mut self, message: impl Into<String>) {
        self.flash = message.into();
        self.flash_until = Some(Instant::now() + FLASH_DURATION);
    }

    pub fn set_flash_persistent(&mut self, message: impl Into<String>) {
        self.flash = message.into();
        self.flash_until = None;
    }

    pub fn restore_flash_if_expired(&mut self) {
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

pub fn visible_voice_peers(status: &DaemonStatus) -> Vec<&PeerSummary> {
    status
        .peers
        .iter()
        .filter(|peer| matches!(peer.session, PeerSessionState::Active { .. }))
        .collect()
}

pub fn default_flash(screen: Screen) -> &'static str {
    match screen {
        Screen::Main => "main | ? keybinds | j/k move | Enter toggle/open | J join invite",
        Screen::Peers => "engaged users | ? keybinds | x mute | a add friend",
        Screen::Rooms => "rooms | ? keybinds | J join invite | Enter enter room",
        Screen::Calls => "calls | ? keybinds | x mute | a add friend",
        Screen::Config => "configuration | ? keybinds | h/l adjust | Enter activate",
        Screen::KnownPeers => "friends | ? keybinds | Enter reconnect | D remove | t trust toggle",
        Screen::SeenUsers => "seen users | ? keybinds | Enter reconnect | a add friend",
        Screen::DiscoveredPeers => "network peers | ? keybinds | Enter inspect route candidate",
        Screen::Help => "? or esc closes help",
    }
}

pub fn main_activity_items(
    status: &DaemonStatus,
    expanded_rooms: &HashSet<String>,
    expanded_calls: bool,
) -> Vec<MainActivityItem> {
    let mut items = Vec::new();
    let engaged_rooms: Vec<String> = status
        .rooms
        .iter()
        .filter(|room| room.engaged)
        .map(|room| room.name.clone())
        .collect();

    for room_name in engaged_rooms {
        let room_peers: Vec<PeerSummary> = visible_voice_peers(status)
            .into_iter()
            .filter(|peer| match &peer.session {
                PeerSessionState::Active { room_name: peer_room, .. } => {
                    peer_room.as_deref().unwrap_or("main") == room_name
                }
                _ => false,
            })
            .cloned()
            .collect();
        items.push(MainActivityItem::RoomGroup {
            room_name: room_name.clone(),
            engaged_users: room_peers.len(),
        });
        if expanded_rooms.contains(&room_name) {
            for peer in room_peers {
                items.push(MainActivityItem::RoomPeer {
                    room_name: room_name.clone(),
                    peer,
                });
            }
        }
    }

    let calls: Vec<PeerSummary> = active_call_peers(status).into_iter().cloned().collect();
    items.push(MainActivityItem::CallsGroup {
        active_calls: calls.len(),
    });
    if expanded_calls {
        for peer in calls {
            items.push(MainActivityItem::CallPeer { peer });
        }
    }

    items
}

pub fn active_call_peers(status: &DaemonStatus) -> Vec<&PeerSummary> {
    let current_room = status.session.room_name.as_deref().unwrap_or("main");
    status
        .peers
        .iter()
        .filter(|peer| {
            matches!(
                &peer.session,
                PeerSessionState::Active { room_name, .. }
                    if room_name.as_deref().unwrap_or("main") != current_room
            )
        })
        .collect()
}

pub fn known_rooms(status: &DaemonStatus) -> Vec<KnownRoomPreview> {
    let current_room = status.session.room_name.as_deref().unwrap_or("main");
    let mut rooms = room_seed_list(status, current_room);

    for peer in visible_voice_peers(status) {
        let room_name = match &peer.session {
            PeerSessionState::Active { room_name, .. } => {
                room_name.clone().unwrap_or_else(|| "main".to_string())
            }
            PeerSessionState::None | PeerSessionState::Handshaking => continue,
        };
        upsert_room(&mut rooms, &room_name, room_name == current_room, true, false);
    }

    for pending in &status.pending_peer_approvals {
        let room_name = pending
            .room_name
            .clone()
            .unwrap_or_else(|| "main".to_string());
        upsert_room(&mut rooms, &room_name, room_name == current_room, false, true);
    }

    rooms.sort_by(|left, right| {
        right
            .is_current
            .cmp(&left.is_current)
            .then_with(|| right.engaged_users.cmp(&left.engaged_users))
            .then_with(|| right.pending_approvals.cmp(&left.pending_approvals))
            .then_with(|| left.name.cmp(&right.name))
    });
    rooms
}

fn room_seed_list(status: &DaemonStatus, current_room: &str) -> Vec<KnownRoomPreview> {
    if status.rooms.is_empty() {
        return vec![KnownRoomPreview {
            name: current_room.to_string(),
            engaged_users: 0,
            pending_approvals: 0,
            is_current: true,
        }];
    }

    status
        .rooms
        .iter()
        .map(|room| seeded_room_preview(room, current_room))
        .collect()
}

fn seeded_room_preview(room: &RoomSummary, current_room: &str) -> KnownRoomPreview {
    KnownRoomPreview {
        name: room.name.clone(),
        engaged_users: 0,
        pending_approvals: 0,
        is_current: room.engaged || room.name == current_room,
    }
}

fn upsert_room(
    rooms: &mut Vec<KnownRoomPreview>,
    room_name: &str,
    is_current: bool,
    engaged_user: bool,
    pending_approval: bool,
) {
    if let Some(room) = rooms.iter_mut().find(|room| room.name == room_name) {
        room.is_current |= is_current;
        room.engaged_users += usize::from(engaged_user);
        room.pending_approvals += usize::from(pending_approval);
    } else {
        rooms.push(KnownRoomPreview {
            name: room_name.to_string(),
            engaged_users: usize::from(engaged_user),
            pending_approvals: usize::from(pending_approval),
            is_current,
        });
    }
}

pub fn adjust_percent(current: u8, delta: i16) -> u8 {
    let next = i16::from(current) + delta;
    next.clamp(0, 200) as u8
}

pub fn best_known_peer_address(peer: &KnownPeerSummary) -> Option<String> {
    peer.last_dial_addr
        .clone()
        .or_else(|| peer.addresses.first().cloned())
}

pub fn ranked_fallback_candidates(
    status: &DaemonStatus,
    peer: &KnownPeerSummary,
) -> Vec<FallbackCandidatePreview> {
    let mut addresses = Vec::new();
    if let Some(last_dial_addr) = &peer.last_dial_addr {
        addresses.push(last_dial_addr.clone());
    }
    addresses.extend(peer.addresses.iter().cloned());

    let mut deduped = Vec::new();
    for address in addresses {
        if !deduped.contains(&address) {
            deduped.push(address);
        }
    }

    deduped.sort_by(|left, right| {
        let left_key = candidate_rank(status, peer, left);
        let right_key = candidate_rank(status, peer, right);
        right_key.cmp(&left_key).then_with(|| left.cmp(right))
    });

    deduped
        .into_iter()
        .map(|address| {
            let path_label = address_path_label(&address);
            let (successes, failures) = path_score(status, path_label);
            FallbackCandidatePreview {
                is_last_dial: peer.last_dial_addr.as_deref() == Some(address.as_str()),
                address,
                path_label,
                score_delta: successes as i64 - failures as i64,
                successes,
                failures,
            }
        })
        .collect()
}

pub fn parse_ui_config(default_control_addr: &str) -> UiConfig {
    let mut control_addr = default_control_addr.to_string();
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

pub fn short_id(peer_id: &str) -> &str {
    peer_id.get(0..12).unwrap_or(peer_id)
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

fn candidate_rank(
    status: &DaemonStatus,
    peer: &KnownPeerSummary,
    address: &str,
) -> (i64, i64, i64) {
    let path_label = address_path_label(address);
    let (successes, failures) = path_score(status, path_label);
    let last_peer_boost = status
        .network
        .path_scores
        .iter()
        .find(|score| score.path == path_label)
        .and_then(|score| score.last_peer_id.as_deref())
        .map(|last_peer| i64::from(last_peer == peer.peer_id))
        .unwrap_or_default();
    let last_dial_boost = i64::from(peer.last_dial_addr.as_deref() == Some(address));
    (
        successes as i64 - failures as i64,
        last_peer_boost,
        last_dial_boost,
    )
}

fn path_score(status: &DaemonStatus, path_label: &str) -> (u64, u64) {
    status
        .network
        .path_scores
        .iter()
        .find(|score| score.path == path_label)
        .map(|score| (score.successes, score.failures))
        .unwrap_or((0, 0))
}

fn address_path_label(address: &str) -> &'static str {
    if address.contains("/p2p-circuit/") {
        "libp2p-relay"
    } else {
        "libp2p-direct"
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use voicers_core::{
        AudioBackend, AudioEngineStage, AudioSummary, NetworkSummary, OutputStrategy,
        PathScoreSummary, RoomSummary, SessionSummary,
    };

    #[test]
    fn ranked_fallback_candidates_prefer_scored_direct_paths() {
        let peer_id = "peer-a";
        let status = test_status(
            vec![
                PathScoreSummary {
                    path: "libp2p-direct".to_string(),
                    successes: 5,
                    failures: 1,
                    last_peer_id: Some(peer_id.to_string()),
                },
                PathScoreSummary {
                    path: "libp2p-relay".to_string(),
                    successes: 1,
                    failures: 4,
                    last_peer_id: Some(peer_id.to_string()),
                },
            ],
            vec![KnownPeerSummary {
                peer_id: peer_id.to_string(),
                display_name: "peer-a".to_string(),
                addresses: vec![
                    format!("/ip4/198.51.100.10/tcp/4001/p2p/{peer_id}"),
                    format!("/ip4/203.0.113.20/tcp/4001/p2p/relay/p2p-circuit/p2p/{peer_id}"),
                ],
                last_dial_addr: Some(format!(
                    "/ip4/203.0.113.20/tcp/4001/p2p/relay/p2p-circuit/p2p/{peer_id}"
                )),
                connected: false,
                pinned: true,
                seen: true,
                whitelisted: false,
                trusted_contact: false,
            }],
        );

        let ranked = ranked_fallback_candidates(&status, &status.network.friends[0]);
        assert_eq!(ranked[0].path_label, "libp2p-direct");
        assert_eq!(ranked[1].path_label, "libp2p-relay");
    }

    #[test]
    fn ranked_fallback_candidates_dedup_last_dial_address() {
        let peer_id = "peer-b";
        let address = format!("/ip4/198.51.100.10/tcp/4001/p2p/{peer_id}");
        let status = test_status(
            Vec::new(),
            vec![KnownPeerSummary {
                peer_id: peer_id.to_string(),
                display_name: "peer-b".to_string(),
                addresses: vec![address.clone()],
                last_dial_addr: Some(address.clone()),
                connected: false,
                pinned: true,
                seen: true,
                whitelisted: false,
                trusted_contact: false,
            }],
        );

        let ranked = ranked_fallback_candidates(&status, &status.network.friends[0]);
        assert_eq!(ranked.len(), 1);
        assert!(ranked[0].is_last_dial);
    }

    #[test]
    fn known_rooms_are_seeded_from_durable_room_state() {
        let mut status = test_status(Vec::new(), Vec::new());
        status.session.room_name = Some("alpha".to_string());
        status.rooms = vec![
            RoomSummary {
                name: "alpha".to_string(),
                engaged: true,
                roles: Vec::new(),
                members: Vec::new(),
                current_invite: None,
            },
            RoomSummary {
                name: "beta".to_string(),
                engaged: false,
                roles: Vec::new(),
                members: Vec::new(),
                current_invite: None,
            },
        ];
        status.peers.push(PeerSummary {
            peer_id: "peer-a".to_string(),
            display_name: "Peer A".to_string(),
            address: "<unknown>".to_string(),
            muted: false,
            output_volume_percent: 100,
            output_bus: "peer_bus_01".to_string(),
            transport: voicers_core::PeerTransportState::Connected,
            session: PeerSessionState::Active {
                room_name: Some("beta".to_string()),
                display_name: "Peer A".to_string(),
            },
            media: voicers_core::PeerMediaState {
                stream_state: voicers_core::MediaStreamState::Idle,
                sent_packets: 0,
                received_packets: 0,
                tx_level_rms: 0.0,
                rx_level_rms: 0.0,
                lost_packets: 0,
                late_packets: 0,
                concealed_frames: 0,
                drift_corrections: 0,
                queued_packets: 0,
                decoded_frames: 0,
                queued_samples: 0,
                last_sequence: None,
                route_via: None,
            },
        });

        let rooms = known_rooms(&status);
        assert_eq!(rooms.len(), 2);
        assert_eq!(rooms[0].name, "alpha");
        assert!(rooms[0].is_current);
        assert_eq!(rooms[1].name, "beta");
        assert_eq!(rooms[1].engaged_users, 1);
    }

    fn test_status(
        path_scores: Vec<PathScoreSummary>,
        known_peers: Vec<KnownPeerSummary>,
    ) -> DaemonStatus {
        let mut status = DaemonStatus {
            daemon_version: "0.1.0".to_string(),
            control_addr: "127.0.0.1:7767".to_string(),
            local_peer_id: "local".to_string(),
            session: SessionSummary {
                room_name: None,
                display_name: "local".to_string(),
                self_muted: false,
            },
            rooms: Vec::new(),
            network: NetworkSummary {
                implementation: "libp2p".to_string(),
                transport_stage: "testing".to_string(),
                nat_status: "testing".to_string(),
                listen_addrs: Vec::new(),
                external_addrs: Vec::new(),
                observed_addrs: Vec::new(),
                stun_addrs: Vec::new(),
                selected_media_path: "libp2p-request-response".to_string(),
                webrtc_connection_state: "disabled".to_string(),
                path_scores,
                saved_peer_addrs: Vec::new(),
                known_peers,
                friends: Vec::new(),
                seen_users: Vec::new(),
                discovered_peers: Vec::new(),
                ignored_peer_ids: Vec::new(),
                direct_call_invite: None,
            },
            audio: AudioSummary {
                backend: AudioBackend::Unknown,
                output_strategy: OutputStrategy::LogicalPeerBusesOnly,
                output_backend: "none".to_string(),
                capture_device: None,
                available_capture_devices: Vec::new(),
                sample_rate_hz: Some(48_000),
                engine: AudioEngineStage::QueueingOnly,
                frame_size_ms: Some(20),
                codec: Some("opus".to_string()),
                source: Some("test".to_string()),
                input_gain_percent: 100,
            },
            peers: Vec::new(),
            pending_peer_approvals: Vec::new(),
            notes: Vec::new(),
        };
        status.network.refresh_user_views();
        status
    }
}
