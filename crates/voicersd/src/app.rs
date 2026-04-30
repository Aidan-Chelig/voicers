use std::{path::PathBuf, sync::Arc};

use anyhow::Result;
use tokio::sync::RwLock;
use voicers_core::{
    AudioBackend, AudioEngineStage, AudioSummary, ControlRequest, ControlResponse, DaemonStatus,
    NetworkSummary, OutputStrategy, SessionHello, SessionSummary, DEFAULT_CONTROL_ADDR,
};

use crate::{
    media,
    network::{self, NetworkHandle},
    persist::{self, PersistenceHandle},
};

#[derive(Debug, Clone)]
pub struct AppConfig {
    pub control_addr: String,
    pub listen_addr: String,
    pub display_name: String,
    pub state_path: PathBuf,
}

impl Default for AppConfig {
    fn default() -> Self {
        Self {
            control_addr: DEFAULT_CONTROL_ADDR.to_string(),
            listen_addr: "/ip4/0.0.0.0/tcp/0".to_string(),
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
}

impl App {
    pub async fn bootstrap(config: AppConfig) -> Result<Self> {
        let backend = AudioBackend::current();
        let persistence = PersistenceHandle::new(config.state_path.clone());
        let persisted = persistence.load().unwrap_or_default();

        let state = Arc::new(RwLock::new(DaemonStatus {
            daemon_version: env!("CARGO_PKG_VERSION").to_string(),
            control_addr: config.control_addr.clone(),
            local_peer_id: "<starting>".to_string(),
            session: SessionSummary {
                room_name: None,
                display_name: config.display_name,
                self_muted: false,
            },
            network: NetworkSummary {
                implementation: "libp2p".to_string(),
                transport_stage: "starting libp2p swarm".to_string(),
                nat_status: "detecting local reachability".to_string(),
                listen_addrs: Vec::new(),
                external_addrs: Vec::new(),
                observed_addrs: Vec::new(),
                saved_peer_addrs: persisted.known_peer_addrs,
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
        let network =
            network::start(Arc::clone(&state), &config.listen_addr, media.clone(), persistence)?;

        state.write().await.local_peer_id = network.peer_id;

        Ok(Self {
            state,
            network: network.handle,
            media,
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
        }
    }
}
