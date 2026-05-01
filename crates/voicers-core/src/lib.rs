use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine as _};
use serde::{Deserialize, Serialize};

pub const DEFAULT_CONTROL_ADDR: &str = "127.0.0.1:7767";
pub const COMPACT_INVITE_PREFIX: &str = "voicers://join/";

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct CompactInviteV1 {
    pub v: u8,
    pub peer_id: String,
    #[serde(default)]
    pub addrs: Vec<String>,
    #[serde(default)]
    pub invite_code: Option<String>,
    #[serde(default)]
    pub expires_at_ms: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum JoinTarget {
    Raw(String),
    Invite(CompactInviteV1),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DaemonStatus {
    pub daemon_version: String,
    pub control_addr: String,
    pub local_peer_id: String,
    pub session: SessionSummary,
    pub network: NetworkSummary,
    pub audio: AudioSummary,
    pub peers: Vec<PeerSummary>,
    #[serde(default)]
    pub pending_peer_approvals: Vec<PendingPeerApprovalSummary>,
    pub notes: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionSummary {
    pub room_name: Option<String>,
    pub display_name: String,
    pub self_muted: bool,
    #[serde(default)]
    pub invite_code: Option<String>,
    #[serde(default)]
    pub invite_expires_at_ms: Option<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PendingPeerApprovalSummary {
    pub peer_id: String,
    pub display_name: String,
    pub address: String,
    pub room_name: Option<String>,
    #[serde(default)]
    pub known: bool,
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
    #[serde(default)]
    pub whitelisted: bool,
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
    JoinDenied { message: String },
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
    ApprovePendingPeer {
        peer_id: String,
        whitelist: bool,
    },
    RejectPendingPeer {
        peer_id: String,
    },
    RotateInviteCode,
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

pub fn encode_compact_invite(target: &str) -> String {
    let encoded = URL_SAFE_NO_PAD.encode(target.trim().as_bytes());
    format!("{COMPACT_INVITE_PREFIX}{encoded}")
}

pub fn encode_join_namespace_invite(namespace: &str) -> String {
    format!("{COMPACT_INVITE_PREFIX}{}", namespace.trim())
}

pub fn encode_peer_compact_invite(
    peer_id: &str,
    addrs: &[String],
    invite_code: Option<&str>,
    expires_at_ms: Option<u64>,
) -> String {
    let payload = CompactInviteV1 {
        v: 1,
        peer_id: peer_id.trim().to_string(),
        addrs: addrs
            .iter()
            .map(|addr| addr.trim())
            .filter(|addr| !addr.is_empty())
            .map(ToOwned::to_owned)
            .collect(),
        invite_code: invite_code
            .map(str::trim)
            .filter(|code| !code.is_empty())
            .map(ToOwned::to_owned),
        expires_at_ms,
    };
    let encoded = serde_json::to_vec(&payload).expect("compact invite payload should serialize");
    let encoded = URL_SAFE_NO_PAD.encode(encoded);
    format!("{COMPACT_INVITE_PREFIX}{encoded}")
}

pub fn decode_compact_invite(invite: &str) -> Option<String> {
    let payload = invite.trim().strip_prefix(COMPACT_INVITE_PREFIX)?;
    let decoded = URL_SAFE_NO_PAD.decode(payload).ok()?;
    let decoded = String::from_utf8(decoded).ok()?;
    Some(decoded.trim().to_string())
}

pub fn parse_join_target(value: &str) -> JoinTarget {
    let trimmed = value.trim();
    let Some(payload) = trimmed.strip_prefix(COMPACT_INVITE_PREFIX) else {
        return JoinTarget::Raw(trimmed.to_string());
    };

    let Ok(decoded) = URL_SAFE_NO_PAD.decode(payload) else {
        return JoinTarget::Raw(payload.trim().to_string());
    };

    if let Ok(invite) = serde_json::from_slice::<CompactInviteV1>(&decoded) {
        return JoinTarget::Invite(CompactInviteV1 {
            v: invite.v,
            peer_id: invite.peer_id.trim().to_string(),
            addrs: invite
                .addrs
                .into_iter()
                .map(|addr: String| addr.trim().to_string())
                .filter(|addr: &String| !addr.is_empty())
                .collect(),
            invite_code: invite
                .invite_code
                .map(|code| code.trim().to_string())
                .filter(|code| !code.is_empty()),
            expires_at_ms: invite.expires_at_ms,
        });
    }

    match String::from_utf8(decoded) {
        Ok(decoded) => JoinTarget::Raw(decoded.trim().to_string()),
        Err(_) => JoinTarget::Raw(payload.trim().to_string()),
    }
}

pub fn normalize_join_target(value: &str) -> String {
    match parse_join_target(value) {
        JoinTarget::Raw(target) => target,
        JoinTarget::Invite(invite) => invite.peer_id,
    }
}

#[cfg(test)]
mod tests {
    use super::{
        decode_compact_invite, encode_compact_invite, encode_join_namespace_invite,
        encode_peer_compact_invite, normalize_join_target, parse_join_target, CompactInviteV1,
        JoinTarget,
    };

    #[test]
    fn compact_invite_round_trips_raw_multiaddr() {
        let raw = "/ip4/203.0.113.10/tcp/4001/p2p/QmNnooDu7bfjPFoTZYxMNLWUQJyrVwtbZg5gBMjTezGAJN";
        let invite = encode_compact_invite(raw);
        assert!(invite.starts_with("voicers://join/"));
        assert_eq!(decode_compact_invite(&invite).as_deref(), Some(raw));
        assert_eq!(normalize_join_target(&invite), raw);
    }

    #[test]
    fn normalize_join_target_leaves_raw_inputs_unchanged() {
        let raw = "/ip4/198.51.100.10/tcp/4001/p2p/QmbLHAnMoJPWSCR5Zhtx6BHJX9KiKNN6tpvbUcqanj75Nb";
        assert_eq!(normalize_join_target(raw), raw);
    }

    #[test]
    fn peer_invite_round_trips_structured_payload() {
        let invite = encode_peer_compact_invite(
            "12D3KooWExamplePeer",
            &[
                "/ip4/192.168.1.50/tcp/27015/p2p/12D3KooWExamplePeer".to_string(),
                "/dns4/example.net/tcp/4001/p2p/12D3KooWExamplePeer".to_string(),
            ],
            Some("abc123"),
            Some(123456789),
        );
        match parse_join_target(&invite) {
            JoinTarget::Invite(CompactInviteV1 {
                v,
                peer_id,
                addrs,
                invite_code,
                expires_at_ms,
            }) => {
                assert_eq!(v, 1);
                assert_eq!(peer_id, "12D3KooWExamplePeer");
                assert_eq!(addrs.len(), 2);
                assert_eq!(invite_code.as_deref(), Some("abc123"));
                assert_eq!(expires_at_ms, Some(123456789));
            }
            other => panic!("expected structured invite, got {other:?}"),
        }
        assert_eq!(normalize_join_target(&invite), "12D3KooWExamplePeer");
    }

    #[test]
    fn namespace_invite_round_trips_short_code() {
        let invite = encode_join_namespace_invite("abc123");
        match parse_join_target(&invite) {
            JoinTarget::Raw(target) => assert_eq!(target, "abc123"),
            other => panic!("expected raw namespace, got {other:?}"),
        }
        assert_eq!(normalize_join_target(&invite), "abc123");
    }
}
