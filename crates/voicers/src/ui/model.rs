use std::{
    path::{Path, PathBuf},
    time::{Duration, Instant},
};

use voicers_core::{DaemonStatus, KnownPeerSummary, PeerSessionState, PeerSummary};

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
    pub selected_config_item: usize,
    pub selected_known_peer: usize,
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
    Config,
    KnownPeers,
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
            selected_config_item: 0,
            selected_known_peer: 0,
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

    pub fn selected_known_peer(&self) -> Option<&KnownPeerSummary> {
        self.status
            .as_ref()?
            .network
            .known_peers
            .get(self.selected_known_peer)
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

    pub fn clamp_known_peer_selection(&mut self) {
        let len = self
            .status
            .as_ref()
            .map(|status| status.network.known_peers.len())
            .unwrap_or(0);
        if len == 0 {
            self.selected_known_peer = 0;
        } else if self.selected_known_peer >= len {
            self.selected_known_peer = len - 1;
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
        Screen::Main => {
            "q quit | ? help | enter or d join | i show invite | C rotate code | y allow | w whitelist | n reject | m mute | p peers | c config"
        }
        Screen::Config => {
            "q quit | ? help | c back | p peers | u rename self | enter select device | hjkl or arrows navigate"
        }
        Screen::KnownPeers => {
            "q quit | ? help | p back | c config | a save | n rename peer | D forget | enter or d reconnect"
        }
        Screen::Help => "q quit | ? or esc back | review local testing and fallback diagnostics",
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

pub fn local_fallback_test_steps(control_addr: &str) -> Vec<String> {
    let secondary =
        sibling_control_addr(control_addr).unwrap_or_else(|| "127.0.0.1:7768".to_string());
    vec![
        "1. Build once: cargo build -p voicersd -p voicers".to_string(),
        format!("2. Run Alice at `{control_addr}` on `/ip4/127.0.0.1/tcp/4001`."),
        format!("3. Run Bob at `{secondary}` on `/ip4/127.0.0.1/tcp/4002`."),
        "4. Dial Alice once, then save her in Bob's Known Peers screen.".to_string(),
        "5. Restart Alice on `/ip4/127.0.0.1/tcp/4011`.".to_string(),
        "6. Update Bob's saved addresses so Alice has both `4001` and `4011`, with `last_dial_addr` still on `4001`.".to_string(),
        "7. Reconnect from Known Peers. The daemon should fail the stale address and retry the next ranked address automatically.".to_string(),
    ]
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

fn sibling_control_addr(control_addr: &str) -> Option<String> {
    let socket_addr = control_addr.parse::<std::net::SocketAddr>().ok()?;
    Some(format!("{}:{}", socket_addr.ip(), socket_addr.port() + 1))
}

#[cfg(test)]
mod tests {
    use super::*;
    use voicers_core::{
        AudioBackend, AudioEngineStage, AudioSummary, NetworkSummary, OutputStrategy,
        PathScoreSummary, SessionSummary,
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
            }],
        );

        let ranked = ranked_fallback_candidates(&status, &status.network.known_peers[0]);
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
            }],
        );

        let ranked = ranked_fallback_candidates(&status, &status.network.known_peers[0]);
        assert_eq!(ranked.len(), 1);
        assert!(ranked[0].is_last_dial);
    }

    fn test_status(
        path_scores: Vec<PathScoreSummary>,
        known_peers: Vec<KnownPeerSummary>,
    ) -> DaemonStatus {
        DaemonStatus {
            daemon_version: "0.1.0".to_string(),
            control_addr: "127.0.0.1:7767".to_string(),
            local_peer_id: "local".to_string(),
            session: SessionSummary {
                room_name: None,
                display_name: "local".to_string(),
                self_muted: false,
            },
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
                ignored_peer_ids: Vec::new(),
                share_invite: None,
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
            notes: Vec::new(),
        }
    }
}
