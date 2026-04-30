use serde::{Deserialize, Serialize};

pub const DEFAULT_CONTROL_ADDR: &str = "127.0.0.1:7767";

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DaemonStatus {
    pub daemon_version: String,
    pub control_addr: String,
    pub local_peer_id: String,
    pub session: SessionSummary,
    pub network: NetworkSummary,
    pub audio: AudioSummary,
    pub peers: Vec<PeerSummary>,
    pub notes: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionSummary {
    pub room_name: Option<String>,
    pub display_name: String,
    pub self_muted: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NetworkSummary {
    pub implementation: String,
    pub transport_stage: String,
    #[serde(default)]
    pub nat_status: String,
    pub listen_addrs: Vec<String>,
    #[serde(default)]
    pub external_addrs: Vec<String>,
    #[serde(default)]
    pub observed_addrs: Vec<String>,
    #[serde(default)]
    pub stun_addrs: Vec<String>,
    #[serde(default)]
    pub selected_media_path: String,
    #[serde(default)]
    pub webrtc_connection_state: String,
    #[serde(default)]
    pub path_scores: Vec<PathScoreSummary>,
    #[serde(default)]
    pub saved_peer_addrs: Vec<String>,
    #[serde(default)]
    pub known_peers: Vec<KnownPeerSummary>,
    #[serde(default)]
    pub ignored_peer_ids: Vec<String>,
    #[serde(default)]
    pub share_invite: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PathScoreSummary {
    pub path: String,
    pub successes: u64,
    pub failures: u64,
    pub last_peer_id: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct KnownPeerSummary {
    pub peer_id: String,
    pub display_name: String,
    #[serde(default)]
    pub addresses: Vec<String>,
    pub last_dial_addr: Option<String>,
    #[serde(default)]
    pub connected: bool,
    #[serde(default)]
    pub pinned: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AudioSummary {
    pub backend: AudioBackend,
    pub output_strategy: OutputStrategy,
    pub output_backend: String,
    pub capture_device: Option<String>,
    #[serde(default)]
    pub available_capture_devices: Vec<String>,
    pub sample_rate_hz: Option<u32>,
    pub engine: AudioEngineStage,
    pub frame_size_ms: Option<u16>,
    pub codec: Option<String>,
    pub source: Option<String>,
    #[serde(default = "default_hundred")]
    pub input_gain_percent: u8,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PeerSummary {
    pub peer_id: String,
    pub display_name: String,
    pub address: String,
    pub muted: bool,
    #[serde(default = "default_hundred")]
    pub output_volume_percent: u8,
    pub output_bus: String,
    pub transport: PeerTransportState,
    pub session: PeerSessionState,
    pub media: PeerMediaState,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum AudioBackend {
    PipeWire,
    Jack,
    CoreAudio,
    Wasapi,
    Unknown,
}

impl AudioBackend {
    pub fn current() -> Self {
        #[cfg(target_os = "linux")]
        {
            Self::PipeWire
        }

        #[cfg(target_os = "macos")]
        {
            Self::CoreAudio
        }

        #[cfg(target_os = "windows")]
        {
            Self::Wasapi
        }

        #[cfg(not(any(target_os = "linux", target_os = "macos", target_os = "windows")))]
        {
            Self::Unknown
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum OutputStrategy {
    PipeWirePeerNodesPlanned,
    PipeWirePeerNodesActive,
    JackPeerPortsPlanned,
    CoreAudioPeerBusesPlanned,
    CoreAudioMixedOutputActive,
    WasapiPeerBusesPlanned,
    LogicalPeerBusesOnly,
}

impl OutputStrategy {
    pub fn for_backend(backend: &AudioBackend) -> Self {
        match backend {
            AudioBackend::PipeWire => Self::PipeWirePeerNodesPlanned,
            AudioBackend::Jack => Self::JackPeerPortsPlanned,
            AudioBackend::CoreAudio => Self::CoreAudioPeerBusesPlanned,
            AudioBackend::Wasapi => Self::WasapiPeerBusesPlanned,
            AudioBackend::Unknown => Self::LogicalPeerBusesOnly,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum PeerTransportState {
    Planned,
    Connecting,
    Connected,
    Disconnected,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum PeerSessionState {
    None,
    Handshaking,
    Active {
        room_name: Option<String>,
        display_name: String,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum AudioEngineStage {
    Planned,
    QueueingOnly,
    CapturePending,
    Live,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum MediaFrameKind {
    Probe,
    AudioOpus,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MediaFrame {
    pub sequence: u64,
    pub timestamp_ms: u64,
    #[serde(default)]
    pub timestamp_samples: u64,
    pub frame_kind: MediaFrameKind,
    pub sample_rate_hz: u32,
    pub channels: u8,
    pub frame_duration_ms: u16,
    pub payload: Vec<u8>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MediaAck {
    pub accepted_sequence: u64,
    pub queue_depth: usize,
    #[serde(default)]
    pub queued_samples: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum MediaRequest {
    Frame(MediaFrame),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum MediaResponse {
    Ack(MediaAck),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum MediaStreamState {
    Idle,
    Primed,
    Active,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PeerMediaState {
    pub stream_state: MediaStreamState,
    pub sent_packets: u64,
    pub received_packets: u64,
    pub tx_level_rms: f32,
    pub rx_level_rms: f32,
    pub lost_packets: u64,
    pub late_packets: u64,
    pub concealed_frames: u64,
    pub drift_corrections: u64,
    pub queued_packets: usize,
    pub decoded_frames: usize,
    pub queued_samples: usize,
    pub last_sequence: Option<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionHello {
    pub room_name: Option<String>,
    pub display_name: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum WebRtcSignal {
    Offer {
        sdp: String,
    },
    Answer {
        sdp: String,
    },
    IceCandidate {
        candidate: String,
        #[serde(default)]
        sdp_mid: Option<String>,
        #[serde(default)]
        sdp_mline_index: Option<u16>,
    },
    IceComplete,
}

impl WebRtcSignal {
    pub fn kind(&self) -> &'static str {
        match self {
            Self::Offer { .. } => "offer",
            Self::Answer { .. } => "answer",
            Self::IceCandidate { .. } => "ice-candidate",
            Self::IceComplete => "ice-complete",
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum SessionRequest {
    Hello(SessionHello),
    WebRtcSignal(WebRtcSignal),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum SessionResponse {
    HelloAck(SessionHello),
    WebRtcSignalAck { signal_kind: String },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum ControlRequest {
    GetStatus,
    CreateRoom {
        room_name: String,
    },
    JoinPeer {
        address: String,
    },
    ToggleMuteSelf,
    ToggleMutePeer {
        peer_id: String,
    },
    SetInputGainPercent {
        percent: u8,
    },
    SelectCaptureDevice {
        device_name: String,
    },
    SetPeerVolumePercent {
        peer_id: String,
        percent: u8,
    },
    SetDisplayName {
        display_name: String,
    },
    StartWebRtcOffer {
        peer_id: String,
    },
    SendWebRtcSignal {
        peer_id: String,
        signal: WebRtcSignal,
    },
    SaveKnownPeer {
        peer_id: String,
    },
    RenameKnownPeer {
        peer_id: String,
        display_name: String,
    },
    ForgetKnownPeer {
        peer_id: String,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum ControlResponse {
    Status(DaemonStatus),
    Ack { message: String },
    Error { message: String },
}

fn default_hundred() -> u8 {
    100
}
