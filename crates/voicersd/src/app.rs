use std::{
    path::PathBuf,
    sync::Arc,
    time::{SystemTime, UNIX_EPOCH},
};

use anyhow::Result;
use tokio::sync::RwLock;
use voicers_core::{
    parse_join_target, AudioBackend, AudioEngineStage, AudioSummary, CompactInviteKind,
    CompactInviteV1, ControlRequest, ControlResponse, DaemonStatus, JoinTarget, NetworkSummary,
    OutputStrategy, RoomInviteSummary, RoomMemberSummary, RoomPermission, RoomRoleSummary,
    RoomSummary, SessionHello, SessionSummary, DEFAULT_CONTROL_ADDR,
};

#[cfg(feature = "webrtc-transport")]
use crate::webrtc_transport::{self, WebRtcTransportConfig};
use crate::{
    media,
    network::{self, NetworkHandle},
    persist::{self, PersistedState, PersistenceHandle},
};

#[derive(Debug, Clone)]
pub struct AppConfig {
    pub control_addr: String,
    pub listen_addr: String,
    pub relay_addr: Option<String>,
    pub bootstrap_addrs: Vec<String>,
    pub use_default_bootstrap_addrs: bool,
    pub stun_servers: Vec<String>,
    pub turn_servers: Vec<TurnServerConfig>,
    pub enable_stun: bool,
    pub enable_audio_io: bool,
    pub enable_capture_input: bool,
    pub enable_networking: bool,
    pub display_name: String,
    pub state_path: PathBuf,
}

#[derive(Debug, Clone)]
#[cfg_attr(not(feature = "webrtc-transport"), allow(dead_code))]
pub struct TurnServerConfig {
    pub url: String,
    pub username: String,
    pub credential: String,
}

impl Default for AppConfig {
    fn default() -> Self {
        Self {
            control_addr: DEFAULT_CONTROL_ADDR.to_string(),
            listen_addr: "/ip4/0.0.0.0/tcp/0".to_string(),
            relay_addr: None,
            bootstrap_addrs: Vec::new(),
            use_default_bootstrap_addrs: true,
            stun_servers: Vec::new(),
            turn_servers: Vec::new(),
            enable_stun: true,
            enable_audio_io: true,
            enable_capture_input: true,
            enable_networking: true,
            display_name: "local-user".to_string(),
            state_path: persist::default_state_path(),
        }
    }
}

#[derive(Clone)]
pub struct App {
    state: Arc<RwLock<DaemonStatus>>,
    network: NetworkHandle,
    media: media::MediaHandle,
    persistence: PersistenceHandle,
}

impl App {
    pub async fn bootstrap(config: AppConfig) -> Result<Self> {
        let backend = AudioBackend::current();
        let persistence = PersistenceHandle::new(config.state_path.clone());
        let persisted = persistence.load().unwrap_or_default();
        let mut network = NetworkSummary {
            implementation: "libp2p".to_string(),
            transport_stage: "starting libp2p swarm".to_string(),
            nat_status: "detecting local reachability".to_string(),
            listen_addrs: Vec::new(),
            external_addrs: Vec::new(),
            observed_addrs: Vec::new(),
            stun_addrs: Vec::new(),
            selected_media_path: "libp2p-request-response".to_string(),
            webrtc_connection_state: "disabled".to_string(),
            path_scores: persisted.path_scores,
            saved_peer_addrs: persisted.known_peer_addrs,
            known_peers: persisted.known_peers,
            friends: Vec::new(),
            seen_users: Vec::new(),
            discovered_peers: Vec::new(),
            ignored_peer_ids: persisted.ignored_peer_ids,
            direct_call_invite: persisted.last_share_invite,
        };
        network.refresh_user_views();
        let initial_display_name = persisted
            .local_display_name
            .clone()
            .unwrap_or(config.display_name);
        let rooms = persisted.rooms;

        let state = Arc::new(RwLock::new(DaemonStatus {
            daemon_version: env!("CARGO_PKG_VERSION").to_string(),
            control_addr: config.control_addr.clone(),
            local_peer_id: "<starting>".to_string(),
            session: SessionSummary {
                room_name: None,
                display_name: initial_display_name,
                self_muted: false,
            },
            rooms,
            network,
            audio: AudioSummary {
                output_strategy: OutputStrategy::for_backend(&backend),
                backend,
                output_backend: "logical-buses-only".to_string(),
                capture_device: None,
                available_capture_devices: Vec::new(),
                sample_rate_hz: Some(48_000),
                engine: AudioEngineStage::CapturePending,
                frame_size_ms: Some(20),
                codec: Some("opus".to_string()),
                source: Some("starting".to_string()),
                input_gain_percent: 100,
            },
            peers: Vec::new(),
            pending_peer_approvals: Vec::new(),
            notes: vec![
                "Linux-first milestone: daemon owns per-peer logical buses.".to_string(),
                "PipeWire node publication is planned after the transport and decode path."
                    .to_string(),
                format!("state file {}", config.state_path.display()),
            ],
        }));
        let media = media::start(
            Arc::clone(&state),
            config.enable_audio_io,
            config.enable_capture_input,
        )
        .await;
        let network = if config.enable_networking {
            #[cfg(feature = "webrtc-transport")]
            let (webrtc, webrtc_signals, webrtc_media_frames, webrtc_connection_states) =
                webrtc_transport::start(WebRtcTransportConfig {
                    stun_servers: config.stun_servers.clone(),
                    turn_servers: config
                        .turn_servers
                        .iter()
                        .map(|server| webrtc_transport::TurnServerConfig {
                            url: server.url.clone(),
                            username: server.username.clone(),
                            credential: server.credential.clone(),
                        })
                        .collect(),
                })?;
            network::start(
                Arc::clone(&state),
                &config.listen_addr,
                config.relay_addr.as_deref(),
                &config.bootstrap_addrs,
                config.use_default_bootstrap_addrs,
                &config.stun_servers,
                config.enable_stun,
                #[cfg(feature = "webrtc-transport")]
                Some(webrtc),
                #[cfg(feature = "webrtc-transport")]
                webrtc_signals,
                #[cfg(feature = "webrtc-transport")]
                webrtc_media_frames,
                #[cfg(feature = "webrtc-transport")]
                webrtc_connection_states,
                media.clone(),
                persistence.clone(),
            )
            .await?
        } else {
            {
                let mut state = state.write().await;
                state.network.transport_stage = "network disabled for tests".to_string();
                state.network.nat_status = "not probing reachability".to_string();
                state.network.listen_addrs.clear();
                state.network.external_addrs.clear();
                state.network.observed_addrs.clear();
                state.network.stun_addrs.clear();
                state
                    .notes
                    .push("daemon networking disabled for this instance".to_string());
            }
            network::NetworkBootstrap {
                peer_id: "test-local-peer".to_string(),
                handle: network::NetworkHandle::noop(),
            }
        };

        {
            let mut state = state.write().await;
            state.local_peer_id = network.peer_id;
            let local_peer_id = state.local_peer_id.clone();
            let display_name = state.session.display_name.clone();
            sync_local_room_memberships(&mut state.rooms, &local_peer_id, &display_name);
            #[cfg(feature = "webrtc-transport")]
            {
                state.network.webrtc_connection_state = "idle".to_string();
            }
        }

        Ok(Self {
            state,
            network: network.handle,
            media,
            persistence,
        })
    }

    pub async fn status(&self) -> DaemonStatus {
        self.state.read().await.clone()
    }

    pub async fn handle_request(&self, request: ControlRequest) -> ControlResponse {
        match request {
            ControlRequest::GetStatus => ControlResponse::Status(self.status().await),
            ControlRequest::CreateRoom { room_name } => {
                let mut state = self.state.write().await;
                let normalized_room = normalize_room_name(&room_name);
                let local_peer_id = state.local_peer_id.clone();
                let display_name = state.session.display_name.clone();
                engage_room(&mut state, &normalized_room, &local_peer_id, &display_name);
                state.session.room_name = Some(normalized_room.clone());
                if normalized_room != "main" && !normalized_room.is_empty() {
                    let (invite_code, expires_at_ms) = fresh_invite_code(&state.local_peer_id);
                    set_room_invite(
                        &mut state,
                        &normalized_room,
                        invite_code.clone(),
                        Some(expires_at_ms),
                    );
                } else {
                    clear_room_invite(&mut state, &normalized_room);
                }
                let hello = SessionHello {
                    room_name: state.session.room_name.clone(),
                    display_name: state.session.display_name.clone(),
                };
                drop(state);
                let _ = self.network.broadcast_session_hello(hello).await;
                let _ = self.persist_state().await;
                ControlResponse::Ack {
                    message: format!("room set to {normalized_room}"),
                }
            }
            ControlRequest::JoinPeer { address } => {
                let dial_target = match parse_join_target(&address) {
                    JoinTarget::Raw(target) => target,
                    JoinTarget::Invite(invite) => {
                        let code_target = match invite.kind {
                            CompactInviteKind::DirectCall => None,
                            CompactInviteKind::Room => invite
                                .invite_code
                                .clone()
                                .filter(|_| invite.expires_at_ms.unwrap_or(u64::MAX) > now_ms())
                                .or_else(|| invite.room_name.clone()),
                        };
                        let peer_id = invite.peer_id.clone();
                        self.seed_invite_hints(invite).await;
                        code_target.unwrap_or(peer_id)
                    }
                };
                match self.network.dial(dial_target).await {
                    Ok(message) => ControlResponse::Ack { message },
                    Err(error) => ControlResponse::Error {
                        message: error.to_string(),
                    },
                }
            }
            ControlRequest::ToggleMuteSelf => {
                let mut state = self.state.write().await;
                state.session.self_muted = !state.session.self_muted;
                let label = if state.session.self_muted {
                    "self muted"
                } else {
                    "self unmuted"
                };

                ControlResponse::Ack {
                    message: label.to_string(),
                }
            }
            ControlRequest::ToggleMutePeer { peer_id } => {
                let mut state = self.state.write().await;

                match state.peers.iter_mut().find(|peer| peer.peer_id == peer_id) {
                    Some(peer) => {
                        peer.muted = !peer.muted;
                        let label = if peer.muted { "muted" } else { "unmuted" };

                        ControlResponse::Ack {
                            message: format!("{} {label}", peer.display_name),
                        }
                    }
                    None => ControlResponse::Error {
                        message: "peer not found".to_string(),
                    },
                }
            }
            ControlRequest::SetDisplayName { display_name } => {
                let new_name = display_name.trim().to_string();
                if new_name.is_empty() {
                    return ControlResponse::Error {
                        message: "display name cannot be empty".to_string(),
                    };
                }
                let hello = {
                    let mut state = self.state.write().await;
                    state.session.display_name = new_name.clone();
                    let local_peer_id = state.local_peer_id.clone();
                    let display_name = state.session.display_name.clone();
                    sync_local_room_memberships(&mut state.rooms, &local_peer_id, &display_name);
                    SessionHello {
                        room_name: state.session.room_name.clone(),
                        display_name: state.session.display_name.clone(),
                    }
                };
                let _ = self.network.broadcast_session_hello(hello).await;
                let _ = self.persist_state().await;
                ControlResponse::Ack {
                    message: format!("nickname set to {new_name}"),
                }
            }
            ControlRequest::SendWebRtcSignal { peer_id, signal } => {
                let signal_kind = signal.kind();
                match self
                    .network
                    .send_webrtc_signal(peer_id.clone(), signal)
                    .await
                {
                    Ok(()) => ControlResponse::Ack {
                        message: format!("sent WebRTC {signal_kind} signal to {peer_id}"),
                    },
                    Err(error) => ControlResponse::Error {
                        message: error.to_string(),
                    },
                }
            }
            ControlRequest::StartWebRtcOffer { peer_id } => {
                match self.network.start_webrtc_offer(peer_id.clone()).await {
                    Ok(()) => ControlResponse::Ack {
                        message: format!("started WebRTC offer for {peer_id}"),
                    },
                    Err(error) => ControlResponse::Error {
                        message: error.to_string(),
                    },
                }
            }
            ControlRequest::SetInputGainPercent { percent } => {
                let percent = percent.min(200);
                match self.media.set_input_gain_percent(percent).await {
                    Ok(()) => {
                        let mut state = self.state.write().await;
                        state.audio.input_gain_percent = percent;
                        ControlResponse::Ack {
                            message: format!("input gain set to {percent}%"),
                        }
                    }
                    Err(error) => ControlResponse::Error {
                        message: error.to_string(),
                    },
                }
            }
            ControlRequest::SelectCaptureDevice { device_name } => {
                match self.media.select_capture_device(device_name.clone()).await {
                    Ok(selected_name) => {
                        let mut state = self.state.write().await;
                        state.audio.capture_device = Some(selected_name.clone());
                        ControlResponse::Ack {
                            message: format!("capture device set to {selected_name}"),
                        }
                    }
                    Err(error) => ControlResponse::Error {
                        message: error.to_string(),
                    },
                }
            }
            ControlRequest::SetPeerVolumePercent { peer_id, percent } => {
                let percent = percent.min(200);
                let mut state = self.state.write().await;
                match state.peers.iter_mut().find(|peer| peer.peer_id == peer_id) {
                    Some(peer) => {
                        peer.output_volume_percent = percent;
                        ControlResponse::Ack {
                            message: format!("{} volume set to {percent}%", peer.display_name),
                        }
                    }
                    None => ControlResponse::Error {
                        message: "peer not found".to_string(),
                    },
                }
            }
            ControlRequest::SaveKnownPeer { peer_id } => {
                let mut state = self.state.write().await;
                let live_peer = state
                    .peers
                    .iter()
                    .find(|peer| peer.peer_id == peer_id)
                    .cloned();
                state.network.ignored_peer_ids.retain(|id| id != &peer_id);
                let known_peer = if let Some(existing) = state
                    .network
                    .known_peers
                    .iter_mut()
                    .find(|peer| peer.peer_id == peer_id)
                {
                    existing.pinned = true;
                    existing.seen |= matches!(
                        existing.connected,
                        true
                    ) && false;
                    if let Some(live_peer) = &live_peer {
                        existing.display_name = live_peer.display_name.clone();
                        existing.seen |= matches!(
                            live_peer.session,
                            voicers_core::PeerSessionState::Active { .. }
                        );
                        if !live_peer.address.is_empty() && live_peer.address != "<unknown>" {
                            if !existing.addresses.contains(&live_peer.address) {
                                existing.addresses.push(live_peer.address.clone());
                            }
                            existing.last_dial_addr = Some(live_peer.address.clone());
                        }
                    }
                    existing.display_name.clone()
                } else if let Some(live_peer) = live_peer {
                    state
                        .network
                        .known_peers
                        .push(voicers_core::KnownPeerSummary {
                            peer_id: live_peer.peer_id.clone(),
                            display_name: live_peer.display_name.clone(),
                            addresses: if live_peer.address != "<unknown>" {
                                vec![live_peer.address.clone()]
                            } else {
                                Vec::new()
                            },
                            last_dial_addr: (live_peer.address != "<unknown>")
                                .then_some(live_peer.address.clone()),
                            connected: matches!(
                                live_peer.transport,
                                voicers_core::PeerTransportState::Connected
                            ),
                            pinned: true,
                            seen: matches!(
                                live_peer.session,
                                voicers_core::PeerSessionState::Active { .. }
                            ),
                            whitelisted: false,
                        });
                    live_peer.display_name
                } else {
                    return ControlResponse::Error {
                        message: "peer not found".to_string(),
                    };
                };
                state.network.refresh_user_views();
                let snapshot = state.network.clone();
                let local_display_name = state.session.display_name.clone();
                let rooms = state.rooms.clone();
                drop(state);
                let _ = self.persist_network(local_display_name, rooms, snapshot);
                ControlResponse::Ack {
                    message: format!("{known_peer} added to friends"),
                }
            }
            ControlRequest::RenameKnownPeer {
                peer_id,
                display_name,
            } => {
                let new_name = display_name.trim().to_string();
                if new_name.is_empty() {
                    return ControlResponse::Error {
                        message: "display name cannot be empty".to_string(),
                    };
                }
                let mut state = self.state.write().await;
                let mut renamed = false;
                if let Some(known_peer) = state
                    .network
                    .known_peers
                    .iter_mut()
                    .find(|peer| peer.peer_id == peer_id)
                {
                    known_peer.display_name = new_name.clone();
                    known_peer.pinned = true;
                    renamed = true;
                }
                if let Some(peer) = state.peers.iter_mut().find(|peer| peer.peer_id == peer_id) {
                    peer.display_name = new_name.clone();
                    renamed = true;
                }
                if !renamed {
                    return ControlResponse::Error {
                        message: "known peer not found".to_string(),
                    };
                }
                state.network.refresh_user_views();
                let snapshot = state.network.clone();
                let local_display_name = state.session.display_name.clone();
                let rooms = state.rooms.clone();
                drop(state);
                let _ = self.persist_network(local_display_name, rooms, snapshot);
                ControlResponse::Ack {
                    message: format!("friend renamed to {new_name}"),
                }
            }
            ControlRequest::ForgetKnownPeer { peer_id } => {
                let mut state = self.state.write().await;
                let Some(known_peer) = state
                    .network
                    .known_peers
                    .iter_mut()
                    .find(|peer| peer.peer_id == peer_id)
                else {
                    return ControlResponse::Error {
                        message: "known peer not found".to_string(),
                    };
                };
                known_peer.pinned = false;
                state.network.refresh_user_views();
                if !state.network.ignored_peer_ids.contains(&peer_id) {
                    state.network.ignored_peer_ids.push(peer_id);
                }
                let snapshot = state.network.clone();
                let local_display_name = state.session.display_name.clone();
                let rooms = state.rooms.clone();
                drop(state);
                let _ = self.persist_network(local_display_name, rooms, snapshot);
                ControlResponse::Ack {
                    message: "friend removed".to_string(),
                }
            }
            ControlRequest::ApprovePendingPeer { peer_id, whitelist } => {
                let response = self.network.approve_pending_peer(peer_id.clone()).await;
                if response.is_ok() && whitelist {
                    {
                        let mut state = self.state.write().await;
                        if let Some(peer) = state
                            .network
                            .known_peers
                            .iter_mut()
                            .find(|peer| peer.peer_id == peer_id)
                        {
                            peer.whitelisted = true;
                        }
                        state.network.refresh_user_views();
                        state
                            .pending_peer_approvals
                            .retain(|pending| pending.peer_id != peer_id);
                    }
                    let _ = self.persist_state().await;
                } else if response.is_ok() {
                    let mut state = self.state.write().await;
                    state
                        .pending_peer_approvals
                        .retain(|pending| pending.peer_id != peer_id);
                }
                match response {
                    Ok(message) => ControlResponse::Ack { message: if whitelist {
                        format!("{message}; peer whitelisted")
                    } else {
                        message
                    }},
                    Err(error) => ControlResponse::Error { message: error.to_string() },
                }
            }
            ControlRequest::RejectPendingPeer { peer_id } => {
                let response = self.network.reject_pending_peer(peer_id.clone()).await;
                if response.is_ok() {
                    let mut state = self.state.write().await;
                    state
                        .pending_peer_approvals
                        .retain(|pending| pending.peer_id != peer_id);
                }
                match response {
                    Ok(message) => ControlResponse::Ack { message },
                    Err(error) => ControlResponse::Error { message: error.to_string() },
                }
            }
            ControlRequest::RotateInviteCode => {
                let mut state = self.state.write().await;
                if state.session.room_name.as_deref() == Some("main")
                    || state.session.room_name.as_deref().unwrap_or_default().is_empty()
                {
                    return ControlResponse::Error {
                        message: "room invite rotation requires a custom room name".to_string(),
                    };
                }
                let current_room = state
                    .session
                    .room_name
                    .clone()
                    .unwrap_or_else(|| "main".to_string());
                if !room_has_permission(&state, &current_room, &state.local_peer_id, RoomPermission::CreateRoomInvite)
                {
                    return ControlResponse::Error {
                        message: "only room admins can create room invites".to_string(),
                    };
                }
                let (invite_code, expires_at_ms) = fresh_invite_code(&state.local_peer_id);
                set_room_invite(
                    &mut state,
                    &current_room,
                    invite_code.clone(),
                    Some(expires_at_ms),
                );
                let hello = SessionHello {
                    room_name: state.session.room_name.clone(),
                    display_name: state.session.display_name.clone(),
                };
                drop(state);
                let _ = self.network.broadcast_session_hello(hello).await;
                let _ = self.persist_state().await;
                ControlResponse::Ack {
                    message: format!("room invite rotated to {invite_code}"),
                }
            }
        }
    }

    fn persist_network(
        &self,
        local_display_name: String,
        rooms: Vec<RoomSummary>,
        network: NetworkSummary,
    ) -> Result<()> {
        self.persistence.save_full(&PersistedState {
            local_display_name: Some(local_display_name),
            rooms,
            known_peer_addrs: network.saved_peer_addrs,
            known_peers: network.known_peers,
            ignored_peer_ids: network.ignored_peer_ids,
            last_share_invite: network.direct_call_invite,
            path_scores: network.path_scores,
        })
    }

    async fn persist_state(&self) -> Result<()> {
        let state = self.state.read().await;
        self.persistence.save_full(&PersistedState {
            local_display_name: Some(state.session.display_name.clone()),
            rooms: state.rooms.clone(),
            known_peer_addrs: state.network.saved_peer_addrs.clone(),
            known_peers: state.network.known_peers.clone(),
            ignored_peer_ids: state.network.ignored_peer_ids.clone(),
            last_share_invite: state.network.direct_call_invite.clone(),
            path_scores: state.network.path_scores.clone(),
        })
    }

    async fn seed_invite_hints(&self, invite: CompactInviteV1) {
        let mut state = self.state.write().await;
        state
            .network
            .ignored_peer_ids
            .retain(|id| id != &invite.peer_id);

        let peer_index = state
            .network
            .known_peers
            .iter()
            .position(|peer| peer.peer_id == invite.peer_id)
            .unwrap_or_else(|| {
                state.network.known_peers.push(voicers_core::KnownPeerSummary {
                    peer_id: invite.peer_id.clone(),
                    display_name: format!(
                        "peer {}",
                        invite.peer_id.get(0..12).unwrap_or(&invite.peer_id)
                    ),
                    addresses: Vec::new(),
                    last_dial_addr: None,
                    connected: false,
                    pinned: false,
                    seen: false,
                    whitelisted: false,
                });
                state.network.known_peers.len() - 1
            });

        for address in invite.addrs {
            let address = address.trim().to_string();
            if address.is_empty() {
                continue;
            }
            if !state.network.saved_peer_addrs.contains(&address) {
                state.network.saved_peer_addrs.push(address.clone());
            }
            let known_peer = &mut state.network.known_peers[peer_index];
            if !known_peer.addresses.contains(&address) {
                known_peer.addresses.push(address.clone());
            }
            if known_peer.last_dial_addr.is_none() {
                known_peer.last_dial_addr = Some(address);
            }
        }
        state.network.refresh_user_views();
    }

    #[doc(hidden)]
    pub async fn seed_peer_for_tests(&self, peer_id: &str, display_name: &str, address: &str) {
        let mut state = self.state.write().await;
        state.peers.push(voicers_core::PeerSummary {
            peer_id: peer_id.to_string(),
            display_name: display_name.to_string(),
            address: address.to_string(),
            muted: false,
            output_volume_percent: 100,
            output_bus: "peer_bus_01".to_string(),
            transport: voicers_core::PeerTransportState::Connected,
            session: voicers_core::PeerSessionState::Handshaking,
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
            },
        });
    }
}

fn fresh_invite_code(peer_id: &str) -> (String, u64) {
    let now_ms = now_ms();
    let expires_at_ms = now_ms.saturating_add(60 * 60 * 1000);
    let seed = format!("{peer_id}:{now_ms}");
    let mut hash = std::collections::hash_map::DefaultHasher::new();
    use std::hash::{Hash, Hasher};
    seed.hash(&mut hash);
    let code = format!("{:08x}", hash.finish())[..8].to_ascii_lowercase();
    (code, expires_at_ms)
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis() as u64)
        .unwrap_or_default()
}

fn normalize_room_name(room_name: &str) -> String {
    let normalized = room_name.trim();
    if normalized.is_empty() {
        "main".to_string()
    } else {
        normalized.to_string()
    }
}

fn engage_room(
    state: &mut DaemonStatus,
    room_name: &str,
    local_peer_id: &str,
    display_name: &str,
) {
    for room in &mut state.rooms {
        room.engaged = room.name == room_name;
    }

    if let Some(room) = state.rooms.iter_mut().find(|room| room.name == room_name) {
        room.engaged = true;
        ensure_admin_role(room);
        ensure_local_member(room, local_peer_id, display_name, true);
        return;
    }

    let mut room = RoomSummary {
        name: room_name.to_string(),
        engaged: true,
        roles: Vec::new(),
        members: Vec::new(),
        current_invite: None,
    };
    ensure_admin_role(&mut room);
    ensure_local_member(&mut room, local_peer_id, display_name, true);
    state.rooms.push(room);
    state.rooms.sort_by(|left, right| left.name.cmp(&right.name));
}

fn set_room_invite(
    state: &mut DaemonStatus,
    room_name: &str,
    invite_code: String,
    expires_at_ms: Option<u64>,
) {
    if let Some(room) = state.rooms.iter_mut().find(|room| room.name == room_name) {
        room.current_invite = Some(RoomInviteSummary {
            invite_code,
            share_invite: None,
            expires_at_ms,
            created_by_peer_id: Some(state.local_peer_id.clone()),
        });
    }
}

fn clear_room_invite(state: &mut DaemonStatus, room_name: &str) {
    if let Some(room) = state.rooms.iter_mut().find(|room| room.name == room_name) {
        room.current_invite = None;
    }
}

fn room_has_permission(
    state: &DaemonStatus,
    room_name: &str,
    peer_id: &str,
    permission: RoomPermission,
) -> bool {
    let Some(room) = state.rooms.iter().find(|room| room.name == room_name) else {
        return false;
    };
    let Some(member) = room.members.iter().find(|member| member.peer_id == peer_id) else {
        return false;
    };

    member.roles.iter().any(|role_name| {
        room.roles
            .iter()
            .find(|role| role.name == *role_name)
            .map(|role| role.permissions.iter().any(|candidate| candidate == &permission))
            .unwrap_or(false)
    })
}

fn ensure_admin_role(room: &mut RoomSummary) {
    if room.roles.iter().any(|role| role.name == "admin") {
        return;
    }

    room.roles.push(RoomRoleSummary {
        name: "admin".to_string(),
        permissions: vec![
            RoomPermission::CreateRoomInvite,
            RoomPermission::CreateRole,
            RoomPermission::EditRole,
            RoomPermission::AssignRole,
        ],
    });
}

fn ensure_local_member(
    room: &mut RoomSummary,
    local_peer_id: &str,
    display_name: &str,
    seed_admin: bool,
) {
    let maybe_member = room
        .members
        .iter_mut()
        .find(|member| member.peer_id == local_peer_id || member.peer_id.is_empty());

    match maybe_member {
        Some(member) => {
            member.peer_id = local_peer_id.to_string();
            member.display_name = display_name.to_string();
            if seed_admin && !member.roles.iter().any(|role| role == "admin") {
                member.roles.push("admin".to_string());
            }
        }
        None => room.members.push(RoomMemberSummary {
            peer_id: local_peer_id.to_string(),
            display_name: display_name.to_string(),
            roles: if seed_admin {
                vec!["admin".to_string()]
            } else {
                Vec::new()
            },
        }),
    }
}

fn sync_local_room_memberships(rooms: &mut [RoomSummary], local_peer_id: &str, display_name: &str) {
    for room in rooms {
        ensure_admin_role(room);
        if room.engaged || room.members.iter().any(|member| member.peer_id == local_peer_id) {
            ensure_local_member(room, local_peer_id, display_name, room.members.is_empty());
        } else {
            for member in &mut room.members {
                if member.peer_id == local_peer_id {
                    member.display_name = display_name.to_string();
                }
            }
        }
    }
}
