use std::{path::PathBuf, sync::Arc};

use anyhow::Result;
use tokio::sync::RwLock;
use voicers_core::{
    AudioBackend, AudioEngineStage, AudioSummary, ControlRequest, ControlResponse, DaemonStatus,
    NetworkSummary, OutputStrategy, SessionHello, SessionSummary, DEFAULT_CONTROL_ADDR,
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
    pub stun_servers: Vec<String>,
    pub turn_servers: Vec<TurnServerConfig>,
    pub enable_stun: bool,
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
            stun_servers: Vec::new(),
            turn_servers: Vec::new(),
            enable_stun: true,
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
        let initial_display_name = persisted
            .local_display_name
            .clone()
            .unwrap_or(config.display_name);

        let state = Arc::new(RwLock::new(DaemonStatus {
            daemon_version: env!("CARGO_PKG_VERSION").to_string(),
            control_addr: config.control_addr.clone(),
            local_peer_id: "<starting>".to_string(),
            session: SessionSummary {
                room_name: None,
                display_name: initial_display_name,
                self_muted: false,
            },
            network: NetworkSummary {
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
                ignored_peer_ids: persisted.ignored_peer_ids,
                share_invite: persisted.last_share_invite,
            },
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
            notes: vec![
                "Linux-first milestone: daemon owns per-peer logical buses.".to_string(),
                "PipeWire node publication is planned after the transport and decode path."
                    .to_string(),
                format!("state file {}", config.state_path.display()),
            ],
        }));
        let media = media::start(Arc::clone(&state)).await;
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
        let network = network::start(
            Arc::clone(&state),
            &config.listen_addr,
            config.relay_addr.as_deref(),
            &config.bootstrap_addrs,
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
        )?;

        {
            let mut state = state.write().await;
            state.local_peer_id = network.peer_id;
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
                state.session.room_name = Some(room_name.clone());
                let hello = SessionHello {
                    room_name: state.session.room_name.clone(),
                    display_name: state.session.display_name.clone(),
                };
                drop(state);
                let _ = self.network.broadcast_session_hello(hello).await;
                ControlResponse::Ack {
                    message: format!("room set to {room_name}"),
                }
            }
            ControlRequest::JoinPeer { address } => match self.network.dial(address).await {
                Ok(message) => ControlResponse::Ack { message },
                Err(error) => ControlResponse::Error {
                    message: error.to_string(),
                },
            },
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
                    if let Some(live_peer) = &live_peer {
                        existing.display_name = live_peer.display_name.clone();
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
                        });
                    live_peer.display_name
                } else {
                    return ControlResponse::Error {
                        message: "peer not found".to_string(),
                    };
                };
                let snapshot = state.network.clone();
                let local_display_name = state.session.display_name.clone();
                drop(state);
                let _ = self.persist_network(local_display_name, snapshot);
                ControlResponse::Ack {
                    message: format!("{known_peer} saved"),
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
                let snapshot = state.network.clone();
                let local_display_name = state.session.display_name.clone();
                drop(state);
                let _ = self.persist_network(local_display_name, snapshot);
                ControlResponse::Ack {
                    message: format!("peer renamed to {new_name}"),
                }
            }
            ControlRequest::ForgetKnownPeer { peer_id } => {
                let mut state = self.state.write().await;
                let before = state.network.known_peers.len();
                state
                    .network
                    .known_peers
                    .retain(|peer| peer.peer_id != peer_id);
                if before == state.network.known_peers.len() {
                    return ControlResponse::Error {
                        message: "known peer not found".to_string(),
                    };
                }
                if !state.network.ignored_peer_ids.contains(&peer_id) {
                    state.network.ignored_peer_ids.push(peer_id);
                }
                let snapshot = state.network.clone();
                let local_display_name = state.session.display_name.clone();
                drop(state);
                let _ = self.persist_network(local_display_name, snapshot);
                ControlResponse::Ack {
                    message: "known peer forgotten".to_string(),
                }
            }
        }
    }

    fn persist_network(&self, local_display_name: String, network: NetworkSummary) -> Result<()> {
        self.persistence.save_full(&PersistedState {
            local_display_name: Some(local_display_name),
            known_peer_addrs: network.saved_peer_addrs,
            known_peers: network.known_peers,
            ignored_peer_ids: network.ignored_peer_ids,
            last_share_invite: network.share_invite,
            path_scores: network.path_scores,
        })
    }

    async fn persist_state(&self) -> Result<()> {
        let state = self.state.read().await;
        self.persistence.save_full(&PersistedState {
            local_display_name: Some(state.session.display_name.clone()),
            known_peer_addrs: state.network.saved_peer_addrs.clone(),
            known_peers: state.network.known_peers.clone(),
            ignored_peer_ids: state.network.ignored_peer_ids.clone(),
            last_share_invite: state.network.share_invite.clone(),
            path_scores: state.network.path_scores.clone(),
        })
    }
}
