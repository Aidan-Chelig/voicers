mod peer_book;
mod state;

use std::{path::PathBuf, sync::Arc};

use anyhow::Result;
use tokio::sync::RwLock;
use voicers_core::{
    parse_join_target, AudioBackend, AudioEngineStage, AudioSummary, CompactInviteKind,
    ControlRequest, ControlResponse, DaemonStatus, JoinTarget, NetworkSummary, OutputStrategy,
    RoomPermission, RoomSummary, SessionSummary, DEFAULT_CONTROL_ADDR,
};

use crate::network::update_share_invite;
#[cfg(feature = "webrtc-transport")]
use crate::webrtc_transport::{self, WebRtcTransportConfig};
use crate::{
    media,
    network::{self, NetworkHandle},
    persist::{self, PersistenceHandle},
};
use peer_book::{
    forget_known_peer, rename_known_peer, save_known_peer, seed_invite_hints, set_trusted_contact,
};
use state::{
    build_local_hello, clear_room_invite, create_or_update_room, fresh_invite_code,
    normalize_room_name, now_ms, persisted_state_from_network, persisted_state_from_status,
    room_has_permission, set_room_invite, sync_local_room_memberships,
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
        let config_display_name = config.display_name.clone();
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
                &config_display_name,
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

    pub async fn peer_id(&self) -> String {
        self.state.read().await.local_peer_id.clone()
    }

    pub async fn handle_request(&self, request: ControlRequest) -> ControlResponse {
        match request {
            ControlRequest::GetStatus => ControlResponse::Status(self.status().await),
            ControlRequest::CreateRoom { room_name } => self.handle_create_room(room_name).await,
            ControlRequest::JoinPeer { address } => self.handle_join_peer(address).await,
            ControlRequest::ToggleMuteSelf => self.handle_toggle_mute_self().await,
            ControlRequest::ToggleMutePeer { peer_id } => {
                self.handle_toggle_mute_peer(peer_id).await
            }
            ControlRequest::SetDisplayName { display_name } => {
                self.handle_set_display_name(display_name).await
            }
            ControlRequest::SendWebRtcSignal { peer_id, signal } => {
                self.handle_send_webrtc_signal(peer_id, signal).await
            }
            ControlRequest::StartWebRtcOffer { peer_id } => {
                self.handle_start_webrtc_offer(peer_id).await
            }
            ControlRequest::SetInputGainPercent { percent } => {
                self.handle_set_input_gain_percent(percent).await
            }
            ControlRequest::SelectCaptureDevice { device_name } => {
                self.handle_select_capture_device(device_name).await
            }
            ControlRequest::SetPeerVolumePercent { peer_id, percent } => {
                self.handle_set_peer_volume_percent(peer_id, percent).await
            }
            ControlRequest::SaveKnownPeer { peer_id } => self.handle_save_known_peer(peer_id).await,
            ControlRequest::RenameKnownPeer {
                peer_id,
                display_name,
            } => self.handle_rename_known_peer(peer_id, display_name).await,
            ControlRequest::ForgetKnownPeer { peer_id } => {
                self.handle_forget_known_peer(peer_id).await
            }
            ControlRequest::ApprovePendingPeer { peer_id, whitelist } => {
                self.handle_approve_pending_peer(peer_id, whitelist).await
            }
            ControlRequest::RejectPendingPeer { peer_id } => {
                self.handle_reject_pending_peer(peer_id).await
            }
            ControlRequest::RotateInviteCode => self.handle_rotate_invite_code().await,
            ControlRequest::MarkTrustedContact { peer_id } => {
                self.handle_set_trusted_contact(peer_id, true).await
            }
            ControlRequest::UnmarkTrustedContact { peer_id } => {
                self.handle_set_trusted_contact(peer_id, false).await
            }
        }
    }

    fn persist_network(
        &self,
        local_display_name: String,
        rooms: Vec<RoomSummary>,
        network: NetworkSummary,
    ) -> Result<()> {
        self.persistence.save_full(&persisted_state_from_network(
            local_display_name,
            rooms,
            network,
        ))
    }

    async fn persist_state(&self) -> Result<()> {
        let state = self.state.read().await;
        self.persistence
            .save_full(&persisted_state_from_status(&state))
    }

    async fn seed_invite_hints(&self, invite: voicers_core::CompactInviteV1) {
        let mut state = self.state.write().await;
        seed_invite_hints(&mut state, invite);
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
                route_via: None,
            },
        });
    }

    async fn handle_create_room(&self, room_name: String) -> ControlResponse {
        let normalized_room = normalize_room_name(&room_name);
        let hello = {
            let mut state = self.state.write().await;
            let local_peer_id = state.local_peer_id.clone();
            let display_name = state.session.display_name.clone();
            create_or_update_room(&mut state, &normalized_room, &local_peer_id, &display_name);
            state.session.room_name = Some(normalized_room.clone());
            if normalized_room != "main" {
                let (invite_code, expires_at_ms) = fresh_invite_code(&state.local_peer_id);
                set_room_invite(
                    &mut state,
                    &normalized_room,
                    invite_code,
                    Some(expires_at_ms),
                );
            } else {
                clear_room_invite(&mut state, &normalized_room);
            }
            update_share_invite(&mut state);
            build_local_hello(&state)
        };
        let _ = self.network.broadcast_session_hello(hello).await;
        let _ = self.persist_state().await;
        ControlResponse::Ack {
            message: format!("room set to {normalized_room}"),
        }
    }

    async fn handle_join_peer(&self, address: String) -> ControlResponse {
        let mut adopted_room = None;
        let dial_target = match parse_join_target(&address) {
            JoinTarget::Raw(target) => target,
            JoinTarget::Invite(invite) => {
                let has_addrs = !invite.addrs.is_empty();
                if matches!(invite.kind, CompactInviteKind::Room) {
                    adopted_room = invite.room_name.clone();
                }
                let code_target = match invite.kind {
                    CompactInviteKind::DirectCall => None,
                    CompactInviteKind::Room if !has_addrs => invite
                        .invite_code
                        .clone()
                        .filter(|_| invite.expires_at_ms.unwrap_or(u64::MAX) > now_ms())
                        .or_else(|| invite.room_name.clone()),
                    CompactInviteKind::Room => None,
                };
                let peer_id = invite.peer_id.clone();
                self.seed_invite_hints(invite).await;
                code_target.unwrap_or(peer_id)
            }
        };

        if let Some(room_name) = adopted_room {
            self.adopt_room_invite(room_name).await;
        }

        match self.network.dial(dial_target).await {
            Ok(message) => ControlResponse::Ack { message },
            Err(error) => ControlResponse::Error {
                message: error.to_string(),
            },
        }
    }

    async fn adopt_room_invite(&self, room_name: String) {
        let normalized_room = normalize_room_name(&room_name);
        {
            let mut state = self.state.write().await;
            let local_peer_id = state.local_peer_id.clone();
            let display_name = state.session.display_name.clone();
            create_or_update_room(&mut state, &normalized_room, &local_peer_id, &display_name);
            state.session.room_name = Some(normalized_room);
        }
        let _ = self.persist_state().await;
    }

    async fn handle_toggle_mute_self(&self) -> ControlResponse {
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

    async fn handle_toggle_mute_peer(&self, peer_id: String) -> ControlResponse {
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

    async fn handle_set_display_name(&self, display_name: String) -> ControlResponse {
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
            build_local_hello(&state)
        };

        let _ = self.network.broadcast_session_hello(hello).await;
        let _ = self.persist_state().await;
        ControlResponse::Ack {
            message: format!("nickname set to {new_name}"),
        }
    }

    async fn handle_send_webrtc_signal(
        &self,
        peer_id: String,
        signal: voicers_core::WebRtcSignal,
    ) -> ControlResponse {
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

    async fn handle_start_webrtc_offer(&self, peer_id: String) -> ControlResponse {
        match self.network.start_webrtc_offer(peer_id.clone()).await {
            Ok(()) => ControlResponse::Ack {
                message: format!("started WebRTC offer for {peer_id}"),
            },
            Err(error) => ControlResponse::Error {
                message: error.to_string(),
            },
        }
    }

    async fn handle_set_input_gain_percent(&self, percent: u8) -> ControlResponse {
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

    async fn handle_select_capture_device(&self, device_name: String) -> ControlResponse {
        match self.media.select_capture_device(device_name).await {
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

    async fn handle_set_peer_volume_percent(
        &self,
        peer_id: String,
        percent: u8,
    ) -> ControlResponse {
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

    async fn handle_save_known_peer(&self, peer_id: String) -> ControlResponse {
        let mut state = self.state.write().await;
        let known_peer = match save_known_peer(&mut state, &peer_id) {
            Ok(display_name) => display_name,
            Err(message) => return ControlResponse::Error { message },
        };
        let snapshot = state.network.clone();
        let local_display_name = state.session.display_name.clone();
        let rooms = state.rooms.clone();
        drop(state);
        let _ = self.persist_network(local_display_name, rooms, snapshot);
        ControlResponse::Ack {
            message: format!("{known_peer} added to friends"),
        }
    }

    async fn handle_rename_known_peer(
        &self,
        peer_id: String,
        display_name: String,
    ) -> ControlResponse {
        let new_name = display_name.trim().to_string();
        if new_name.is_empty() {
            return ControlResponse::Error {
                message: "display name cannot be empty".to_string(),
            };
        }
        let mut state = self.state.write().await;
        if let Err(message) = rename_known_peer(&mut state, &peer_id, &new_name) {
            return ControlResponse::Error { message };
        }
        let snapshot = state.network.clone();
        let local_display_name = state.session.display_name.clone();
        let rooms = state.rooms.clone();
        drop(state);
        let _ = self.persist_network(local_display_name, rooms, snapshot);
        ControlResponse::Ack {
            message: format!("friend renamed to {new_name}"),
        }
    }

    async fn handle_forget_known_peer(&self, peer_id: String) -> ControlResponse {
        let mut state = self.state.write().await;
        if let Err(message) = forget_known_peer(&mut state, &peer_id) {
            return ControlResponse::Error { message };
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

    async fn handle_approve_pending_peer(
        &self,
        peer_id: String,
        whitelist: bool,
    ) -> ControlResponse {
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
            Ok(message) => ControlResponse::Ack {
                message: if whitelist {
                    format!("{message}; peer whitelisted")
                } else {
                    message
                },
            },
            Err(error) => ControlResponse::Error {
                message: error.to_string(),
            },
        }
    }

    async fn handle_reject_pending_peer(&self, peer_id: String) -> ControlResponse {
        let response = self.network.reject_pending_peer(peer_id.clone()).await;
        if response.is_ok() {
            let mut state = self.state.write().await;
            state
                .pending_peer_approvals
                .retain(|pending| pending.peer_id != peer_id);
        }
        match response {
            Ok(message) => ControlResponse::Ack { message },
            Err(error) => ControlResponse::Error {
                message: error.to_string(),
            },
        }
    }

    async fn handle_rotate_invite_code(&self) -> ControlResponse {
        let hello_and_invite = {
            let mut state = self.state.write().await;
            if state.session.room_name.as_deref() == Some("main")
                || state
                    .session
                    .room_name
                    .as_deref()
                    .unwrap_or_default()
                    .is_empty()
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
            if !room_has_permission(
                &state,
                &current_room,
                &state.local_peer_id,
                RoomPermission::CreateRoomInvite,
            ) {
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
            update_share_invite(&mut state);
            (build_local_hello(&state), invite_code)
        };

        let _ = self
            .network
            .broadcast_session_hello(hello_and_invite.0)
            .await;
        let _ = self.persist_state().await;
        ControlResponse::Ack {
            message: format!("room invite rotated to {}", hello_and_invite.1),
        }
    }

    async fn handle_set_trusted_contact(
        &self,
        peer_id: String,
        trusted_contact: bool,
    ) -> ControlResponse {
        let hello = {
            let mut state = self.state.write().await;
            if let Err(message) = set_trusted_contact(&mut state, &peer_id, trusted_contact) {
                return ControlResponse::Error { message };
            }
            build_local_hello(&state)
        };
        let snapshot = {
            let state = self.state.read().await;
            (
                state.session.display_name.clone(),
                state.rooms.clone(),
                state.network.clone(),
            )
        };
        let _ = self.persist_network(snapshot.0, snapshot.1, snapshot.2);
        let _ = self.network.broadcast_session_hello(hello).await;
        let action = if trusted_contact {
            "marked as trusted contact"
        } else {
            "unmarked as trusted contact"
        };
        ControlResponse::Ack {
            message: format!("{peer_id} {action}"),
        }
    }
}
