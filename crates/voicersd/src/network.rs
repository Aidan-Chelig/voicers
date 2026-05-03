#[path = "network/media_transport.rs"]
mod media_transport;
#[path = "network/stun.rs"]
mod stun;

use std::{
    collections::{HashMap, HashSet},
    env,
    time::Duration,
};

use anyhow::{Context, Result};
use futures::StreamExt;
use libp2p::{
    connection_limits, dcutr, identify, kad, kad::store::MemoryStore, multiaddr::Protocol,
    noise, ping, relay, request_response, swarm::NetworkBehaviour, swarm::SwarmEvent, tcp, tls,
    upnp, yamux, Multiaddr, PeerId, StreamProtocol, SwarmBuilder,
};
use serde_json;
use tokio::sync::{mpsc, oneshot, RwLock};
use voicers_core::{
    encode_peer_compact_invite, encode_room_compact_invite, CompactInviteKind, CompactInviteV1,
    DaemonStatus, KnownPeerSummary, MediaAck, MediaRequest, MediaResponse, MediaStreamState,
    PathScoreSummary, PeerMediaState, PeerSessionState, PeerSummary, PeerTransportState,
    SessionHello, SessionRequest, SessionResponse, WebRtcSignal,
};

use std::sync::Arc;

use self::media_transport::{
    address_path_name, resolve_ranked_dial_target, resolve_route, select_media_path,
    send_frame_libp2p, send_frame_relayed_libp2p, Route,
};
#[cfg(feature = "webrtc-transport")]
use self::media_transport::{
    handle_incoming_webrtc_frame, handle_webrtc_connection_state, send_frame_with_fallback,
};
use self::stun::run_stun_probes;

#[cfg(feature = "webrtc-transport")]
use crate::webrtc_transport::{
    IncomingWebRtcMediaFrame, OutgoingWebRtcSignal, WebRtcConnectionStateUpdate, WebRtcHandle,
};
use crate::{media::MediaHandle, persist::PersistenceHandle};

const PROTOCOL_VERSION: &str = "/voicers/0.1.0";
const SESSION_PROTOCOL: StreamProtocol = StreamProtocol::new("/voicers/session/0.1.0");
const MEDIA_PROTOCOL: StreamProtocol = StreamProtocol::new("/voicers/media/0.1.0");
const MAX_AUTO_RELAY_CANDIDATES: usize = 4;
const MAX_ESTABLISHED_CONNECTIONS: u32 = 32;
const MAX_ESTABLISHED_OUTGOING_CONNECTIONS: u32 = 24;
const MAX_ESTABLISHED_INCOMING_CONNECTIONS: u32 = 12;
const MAX_PENDING_OUTGOING_CONNECTIONS: u32 = 16;
const MAX_PENDING_INCOMING_CONNECTIONS: u32 = 8;
const RENDEZVOUS_RECORD_PREFIX: &str = "/voicers/rendezvous/v1/";
const DEFAULT_ROOM_NAMESPACE: &str = "main";
const DEFAULT_BOOTSTRAP_ADDRS: &[&str] = &[
    "/ip4/104.131.131.82/tcp/4001/p2p/QmaCpDMGvV2BGHeYERUEnRQAwe3N8SzbUtfsmvsqQLuvuJ",
];
const DEFAULT_STUN_SERVERS: &[&str] = &[
    "stun.l.google.com:19302",
    "stun1.l.google.com:19302",
    "stun.cloudflare.com:3478",
];
const MAX_SHARE_INVITE_ADDRS: usize = 4;
fn relay_reservation_addr(mut relay_addr: Multiaddr) -> Result<Multiaddr> {
    let has_relay_peer = relay_addr
        .iter()
        .any(|protocol| matches!(protocol, Protocol::P2p(_)));
    if !has_relay_peer {
        anyhow::bail!("relay address is missing /p2p/<relay-peer-id>");
    }

    let has_circuit = relay_addr
        .iter()
        .any(|protocol| matches!(protocol, Protocol::P2pCircuit));
    if !has_circuit {
        relay_addr.push(Protocol::P2pCircuit);
    }

    Ok(relay_addr)
}

fn split_peer_addr(mut address: Multiaddr) -> Result<(PeerId, Multiaddr)> {
    match address.pop() {
        Some(Protocol::P2p(peer_id)) => Ok((peer_id, address)),
        _ => anyhow::bail!("multiaddr must end with /p2p/<peer-id>"),
    }
}

fn relay_addr_for_peer(address: Multiaddr, peer_id: PeerId) -> Multiaddr {
    if address
        .iter()
        .any(|protocol| matches!(protocol, Protocol::P2p(_)))
    {
        address
    } else {
        address.with(Protocol::P2p(peer_id))
    }
}

#[derive(Clone)]
pub struct NetworkHandle {
    command_tx: mpsc::Sender<NetworkCommand>,
}

impl NetworkHandle {
    pub fn noop() -> Self {
        let (command_tx, command_rx) = mpsc::channel(1);
        drop(command_rx);
        Self { command_tx }
    }

    pub async fn dial(&self, address: String) -> Result<String> {
        let (response_tx, response_rx) = oneshot::channel();
        self.command_tx
            .send(NetworkCommand::Dial {
                address,
                response_tx,
            })
            .await
            .context("failed to send dial command to network task")?;

        response_rx
            .await
            .context("network task dropped dial response")?
    }

    pub async fn broadcast_session_hello(&self, hello: SessionHello) -> Result<()> {
        let (response_tx, response_rx) = oneshot::channel();
        self.command_tx
            .send(NetworkCommand::BroadcastSessionHello { hello, response_tx })
            .await
            .context("failed to send session broadcast command to network task")?;

        response_rx
            .await
            .context("network task dropped session broadcast response")?
    }

    pub async fn send_webrtc_signal(&self, peer_id: String, signal: WebRtcSignal) -> Result<()> {
        let (response_tx, response_rx) = oneshot::channel();
        self.command_tx
            .send(NetworkCommand::SendWebRtcSignal {
                peer_id,
                signal,
                response_tx,
            })
            .await
            .context("failed to send WebRTC signal command to network task")?;

        response_rx
            .await
            .context("network task dropped WebRTC signal response")?
    }

    pub async fn start_webrtc_offer(&self, peer_id: String) -> Result<()> {
        let (response_tx, response_rx) = oneshot::channel();
        self.command_tx
            .send(NetworkCommand::StartWebRtcOffer {
                peer_id,
                response_tx,
            })
            .await
            .context("failed to send WebRTC offer command to network task")?;

        response_rx
            .await
            .context("network task dropped WebRTC offer response")?
    }

    pub async fn approve_pending_peer(&self, peer_id: String) -> Result<String> {
        let (response_tx, response_rx) = oneshot::channel();
        self.command_tx
            .send(NetworkCommand::ApprovePendingPeer { peer_id, response_tx })
            .await
            .context("failed to send approve-peer command to network task")?;

        response_rx
            .await
            .context("network task dropped approve-peer response")?
    }

    pub async fn reject_pending_peer(&self, peer_id: String) -> Result<String> {
        let (response_tx, response_rx) = oneshot::channel();
        self.command_tx
            .send(NetworkCommand::RejectPendingPeer { peer_id, response_tx })
            .await
            .context("failed to send reject-peer command to network task")?;

        response_rx
            .await
            .context("network task dropped reject-peer response")?
    }
}

pub struct NetworkBootstrap {
    pub peer_id: String,
    pub handle: NetworkHandle,
}

#[derive(Debug, Clone)]
struct PendingDial {
    current_address: String,
    fallback_addresses: Vec<String>,
}

#[derive(Debug, Clone)]
enum PendingRendezvousLookup {
    PeerId(String),
    Room {
        room_name: String,
        resolved_peers: HashSet<String>,
    },
}

struct PendingInboundApproval {
    hello: SessionHello,
    channel: request_response::ResponseChannel<SessionResponse>,
}

enum NetworkCommand {
    Dial {
        address: String,
        response_tx: oneshot::Sender<Result<String>>,
    },
    BroadcastSessionHello {
        hello: SessionHello,
        response_tx: oneshot::Sender<Result<()>>,
    },
    SendWebRtcSignal {
        peer_id: String,
        signal: WebRtcSignal,
        response_tx: oneshot::Sender<Result<()>>,
    },
    StartWebRtcOffer {
        peer_id: String,
        response_tx: oneshot::Sender<Result<()>>,
    },
    ApprovePendingPeer {
        peer_id: String,
        response_tx: oneshot::Sender<Result<String>>,
    },
    RejectPendingPeer {
        peer_id: String,
        response_tx: oneshot::Sender<Result<String>>,
    },
    SendMediaFrame {
        peer_id: String,
    },
}

#[derive(NetworkBehaviour)]
struct VoicersBehaviour {
    limits: connection_limits::Behaviour,
    relay: relay::client::Behaviour,
    dcutr: dcutr::Behaviour,
    kad: kad::Behaviour<MemoryStore>,
    identify: identify::Behaviour,
    ping: ping::Behaviour,
    upnp: upnp::tokio::Behaviour,
    session: request_response::cbor::Behaviour<SessionRequest, SessionResponse>,
    media: request_response::cbor::Behaviour<MediaRequest, MediaResponse>,
}

pub async fn start(
    state: Arc<RwLock<DaemonStatus>>,
    listen_addr: &str,
    relay_addr: Option<&str>,
    bootstrap_addrs: &[String],
    use_default_bootstrap_addrs: bool,
    stun_servers: &[String],
    enable_stun: bool,
    #[cfg(feature = "webrtc-transport")] webrtc: Option<WebRtcHandle>,
    #[cfg(feature = "webrtc-transport")] webrtc_signals: mpsc::Receiver<OutgoingWebRtcSignal>,
    #[cfg(feature = "webrtc-transport")] webrtc_media_frames: mpsc::Receiver<
        IncomingWebRtcMediaFrame,
    >,
    #[cfg(feature = "webrtc-transport")] webrtc_connection_states: mpsc::Receiver<
        WebRtcConnectionStateUpdate,
    >,
    media: MediaHandle,
    persistence: PersistenceHandle,
) -> Result<NetworkBootstrap> {
    let mut swarm = SwarmBuilder::with_new_identity()
        .with_tokio()
        .with_tcp(
            tcp::Config::default().nodelay(true),
            noise::Config::new,
            yamux::Config::default,
        )?
        .with_quic()
        .with_dns()?
        .with_websocket(
            (tls::Config::new, noise::Config::new),
            yamux::Config::default,
        )
        .await?
        .with_relay_client(noise::Config::new, yamux::Config::default)?
        .with_behaviour(|key, relay| {
            let local_peer_id = key.public().to_peer_id();
            let store = MemoryStore::new(local_peer_id);
            Ok::<_, Box<dyn std::error::Error + Send + Sync>>(VoicersBehaviour {
                limits: connection_limits::Behaviour::new(
                    connection_limits::ConnectionLimits::default()
                        .with_max_established(Some(MAX_ESTABLISHED_CONNECTIONS))
                        .with_max_established_outgoing(Some(
                            MAX_ESTABLISHED_OUTGOING_CONNECTIONS,
                        ))
                        .with_max_established_incoming(Some(
                            MAX_ESTABLISHED_INCOMING_CONNECTIONS,
                        ))
                        .with_max_pending_outgoing(Some(MAX_PENDING_OUTGOING_CONNECTIONS))
                        .with_max_pending_incoming(Some(MAX_PENDING_INCOMING_CONNECTIONS)),
                ),
                relay,
                dcutr: dcutr::Behaviour::new(local_peer_id),
                kad: kad::Behaviour::new(local_peer_id, store),
                identify: identify::Behaviour::new(
                    identify::Config::new(PROTOCOL_VERSION.to_string(), key.public())
                        .with_agent_version("voicersd".to_string()),
                ),
                ping: ping::Behaviour::new(
                    ping::Config::new().with_interval(Duration::from_secs(5)),
                ),
                upnp: upnp::tokio::Behaviour::default(),
                session: request_response::cbor::Behaviour::new(
                    [(SESSION_PROTOCOL, request_response::ProtocolSupport::Full)],
                    request_response::Config::default(),
                ),
                media: request_response::cbor::Behaviour::new(
                    [(MEDIA_PROTOCOL, request_response::ProtocolSupport::Full)],
                    request_response::Config::default(),
                ),
            })
        })?
        .with_swarm_config(|cfg| cfg.with_idle_connection_timeout(Duration::from_secs(60)))
        .build();

    let peer_id = swarm.local_peer_id().to_string();
    let listen_multiaddr: Multiaddr = listen_addr
        .parse()
        .with_context(|| format!("invalid listen multiaddr: {listen_addr}"))?;
    swarm
        .listen_on(listen_multiaddr)
        .context("failed to start libp2p TCP listener")?;

    if let Some(relay_addr) = relay_addr {
        let relay_multiaddr: Multiaddr = relay_addr
            .parse()
            .with_context(|| format!("invalid relay multiaddr: {relay_addr}"))?;
        let reservation_addr = relay_reservation_addr(relay_multiaddr)
            .context("relay multiaddr must include the relay peer id as /p2p/<peer-id>")?;
        swarm
            .listen_on(reservation_addr.clone())
            .with_context(|| format!("failed to reserve relay address {reservation_addr}"))?;
    }

    let bootstrap_sources: Vec<&str> = if bootstrap_addrs.is_empty() && use_default_bootstrap_addrs
    {
        DEFAULT_BOOTSTRAP_ADDRS.to_vec()
    } else {
        bootstrap_addrs.iter().map(String::as_str).collect()
    };

    let mut configured_bootstrap_peers = 0usize;
    for bootstrap_addr in bootstrap_sources {
        match bootstrap_addr
            .parse::<Multiaddr>()
            .map_err(anyhow::Error::from)
            .and_then(split_peer_addr)
        {
            Ok((peer_id, peer_addr)) => {
                if !is_supported_dial_addr(&peer_addr) {
                    eprintln!(
                        "ignoring unsupported bootstrap address {bootstrap_addr}: transport not supported by voicersd"
                    );
                    continue;
                }
                swarm.behaviour_mut().kad.add_address(&peer_id, peer_addr);
                configured_bootstrap_peers += 1;
            }
            Err(error) => {
                eprintln!("ignoring invalid bootstrap address {bootstrap_addr}: {error:#}");
            }
        }
    }

    if configured_bootstrap_peers > 0 {
        let _ = swarm.behaviour_mut().kad.bootstrap();
    }

    if enable_stun {
        let stun_servers = if stun_servers.is_empty() {
            DEFAULT_STUN_SERVERS
                .iter()
                .map(|server| server.to_string())
                .collect()
        } else {
            stun_servers.to_vec()
        };
        tokio::spawn(run_stun_probes(Arc::clone(&state), stun_servers));
    }

    let (command_tx, command_rx) = mpsc::channel(32);
    tokio::spawn(run_network_task(
        state,
        swarm,
        command_tx.clone(),
        command_rx,
        #[cfg(feature = "webrtc-transport")]
        webrtc,
        #[cfg(feature = "webrtc-transport")]
        webrtc_signals,
        #[cfg(feature = "webrtc-transport")]
        webrtc_media_frames,
        #[cfg(feature = "webrtc-transport")]
        webrtc_connection_states,
        media,
        persistence,
    ));

    Ok(NetworkBootstrap {
        peer_id,
        handle: NetworkHandle { command_tx },
    })
}

#[cfg(not(feature = "webrtc-transport"))]
async fn run_network_task(
    state: Arc<RwLock<DaemonStatus>>,
    mut swarm: libp2p::Swarm<VoicersBehaviour>,
    command_tx: mpsc::Sender<NetworkCommand>,
    mut command_rx: mpsc::Receiver<NetworkCommand>,
    media: MediaHandle,
    persistence: PersistenceHandle,
) {
    let mut active_media_flows = HashSet::new();
    let mut attempted_relays = HashSet::new();
    let mut connected_peers = HashSet::new();
    let mut dht_routable_peers = HashSet::new();
    let mut pending_dials = HashMap::new();
    let mut pending_rendezvous_lookups = HashMap::new();
    let mut pending_rendezvous_publications = HashMap::new();
    let mut published_rendezvous_records = HashMap::new();
    let mut pending_inbound_approvals = HashMap::new();
    let mut relay_offers: HashMap<String, Vec<String>> = HashMap::new();

    loop {
        tokio::select! {
            Some(command) = command_rx.recv() => {
                handle_command(
                    &state,
                    &mut swarm,
                    &command_tx,
                    &mut active_media_flows,
                    &connected_peers,
                    &dht_routable_peers,
                    &mut pending_dials,
                    &mut pending_rendezvous_lookups,
                    &mut pending_rendezvous_publications,
                    &mut published_rendezvous_records,
                    &mut pending_inbound_approvals,
                    &relay_offers,
                    &media,
                    command,
                ).await;
            }
            event = swarm.select_next_some() => {
                handle_swarm_event(
                    &state,
                    &mut swarm,
                    &command_tx,
                    &mut active_media_flows,
                    &mut connected_peers,
                    &mut dht_routable_peers,
                    &mut attempted_relays,
                    &mut pending_dials,
                    &mut pending_rendezvous_lookups,
                    &mut pending_rendezvous_publications,
                    &mut published_rendezvous_records,
                    &mut pending_inbound_approvals,
                    &mut relay_offers,
                    &media,
                    &persistence,
                    event,
                ).await;
            }
        }
    }
}

#[cfg(feature = "webrtc-transport")]
async fn run_network_task(
    state: Arc<RwLock<DaemonStatus>>,
    mut swarm: libp2p::Swarm<VoicersBehaviour>,
    command_tx: mpsc::Sender<NetworkCommand>,
    mut command_rx: mpsc::Receiver<NetworkCommand>,
    webrtc: Option<WebRtcHandle>,
    mut webrtc_signals: mpsc::Receiver<OutgoingWebRtcSignal>,
    mut webrtc_media_frames: mpsc::Receiver<IncomingWebRtcMediaFrame>,
    mut webrtc_connection_states: mpsc::Receiver<WebRtcConnectionStateUpdate>,
    media: MediaHandle,
    persistence: PersistenceHandle,
) {
    let mut active_media_flows = HashSet::new();
    let mut attempted_relays = HashSet::new();
    let mut connected_peers = HashSet::new();
    let mut dht_routable_peers = HashSet::new();
    let mut pending_dials = HashMap::new();
    let mut pending_rendezvous_lookups = HashMap::new();
    let mut pending_rendezvous_publications = HashMap::new();
    let mut published_rendezvous_records = HashMap::new();
    let mut pending_inbound_approvals = HashMap::new();
    let mut relay_offers: HashMap<String, Vec<String>> = HashMap::new();

    loop {
        tokio::select! {
            Some(command) = command_rx.recv() => {
                handle_command(
                    &state,
                    &mut swarm,
                    &command_tx,
                    &mut active_media_flows,
                    &connected_peers,
                    &dht_routable_peers,
                    &mut pending_dials,
                    &mut pending_rendezvous_lookups,
                    &mut pending_rendezvous_publications,
                    &mut published_rendezvous_records,
                    &mut pending_inbound_approvals,
                    &relay_offers,
                    &webrtc,
                    &media,
                    command,
                ).await;
            }
            Some(outgoing_signal) = webrtc_signals.recv() => {
                let _ = send_webrtc_signal(
                    &state,
                    &mut swarm,
                    &outgoing_signal.peer_id,
                    outgoing_signal.signal,
                )
                .await;
            }
            Some(incoming_frame) = webrtc_media_frames.recv() => {
                handle_webrtc_media_frame(&state, &media, incoming_frame).await;
            }
            Some(connection_state) = webrtc_connection_states.recv() => {
                handle_webrtc_connection_state(&state, &persistence, connection_state).await;
            }
            event = swarm.select_next_some() => {
                handle_swarm_event(
                    &state,
                    &mut swarm,
                    &command_tx,
                    &mut active_media_flows,
                    &mut connected_peers,
                    &mut dht_routable_peers,
                    &mut attempted_relays,
                    &mut pending_dials,
                    &mut pending_rendezvous_lookups,
                    &mut pending_rendezvous_publications,
                    &mut published_rendezvous_records,
                    &mut pending_inbound_approvals,
                    &mut relay_offers,
                    &webrtc,
                    &media,
                    &persistence,
                    event,
                ).await;
            }
        }
    }
}

async fn handle_command(
    state: &Arc<RwLock<DaemonStatus>>,
    swarm: &mut libp2p::Swarm<VoicersBehaviour>,
    command_tx: &mpsc::Sender<NetworkCommand>,
    active_media_flows: &mut HashSet<String>,
    connected_peers: &HashSet<PeerId>,
    dht_routable_peers: &HashSet<PeerId>,
    pending_dials: &mut HashMap<String, PendingDial>,
    pending_rendezvous_lookups: &mut HashMap<kad::QueryId, PendingRendezvousLookup>,
    pending_rendezvous_publications: &mut HashMap<kad::QueryId, (String, Vec<u8>)>,
    published_rendezvous_records: &mut HashMap<String, Vec<u8>>,
    pending_inbound_approvals: &mut HashMap<String, PendingInboundApproval>,
    relay_offers: &HashMap<String, Vec<String>>,
    #[cfg(feature = "webrtc-transport")] webrtc: &Option<WebRtcHandle>,
    media: &MediaHandle,
    command: NetworkCommand,
) {
    match command {
        NetworkCommand::Dial {
            address,
            response_tx,
        } => {
            let response = dial_peer(
                state,
                swarm,
                pending_dials,
                pending_rendezvous_lookups,
                &address,
            )
            .await;
            let _ = response_tx.send(response);
        }
        NetworkCommand::BroadcastSessionHello { hello, response_tx } => {
            let response = broadcast_session_hello(swarm, hello).await;
            maybe_publish_rendezvous_record(
                state,
                swarm,
                connected_peers,
                dht_routable_peers,
                pending_rendezvous_publications,
                published_rendezvous_records,
            )
            .await;
            let _ = response_tx.send(response);
        }
        NetworkCommand::SendWebRtcSignal {
            peer_id,
            signal,
            response_tx,
        } => {
            let response = send_webrtc_signal(state, swarm, &peer_id, signal).await;
            let _ = response_tx.send(response);
        }
        NetworkCommand::StartWebRtcOffer {
            peer_id,
            response_tx,
        } => {
            #[cfg(feature = "webrtc-transport")]
            let response = start_webrtc_offer(state, swarm, webrtc, &peer_id).await;
            #[cfg(not(feature = "webrtc-transport"))]
            let _ = peer_id;
            #[cfg(not(feature = "webrtc-transport"))]
            let response = Err(anyhow::anyhow!(
                "WebRTC transport is disabled; rebuild with --features webrtc-transport"
            ));
            let _ = response_tx.send(response);
        }
        NetworkCommand::ApprovePendingPeer { peer_id, response_tx } => {
            let response = approve_pending_peer(
                state,
                swarm,
                command_tx,
                active_media_flows,
                pending_inbound_approvals,
                &peer_id,
                media,
            )
            .await;
            let _ = response_tx.send(response);
        }
        NetworkCommand::RejectPendingPeer { peer_id, response_tx } => {
            let response = reject_pending_peer(
                state,
                swarm,
                pending_inbound_approvals,
                &peer_id,
            )
            .await;
            let _ = response_tx.send(response);
        }
        NetworkCommand::SendMediaFrame { peer_id } => {
            if active_media_flows.contains(&peer_id) {
                let route = {
                    let s = state.read().await;
                    resolve_route(&s, relay_offers, &peer_id)
                };
                match route {
                    Route::Direct(_) => {
                        #[cfg(feature = "webrtc-transport")]
                        send_next_media_frame_webrtc_first(state, swarm, webrtc, media, &peer_id).await;
                        #[cfg(not(feature = "webrtc-transport"))]
                        send_next_media_frame(state, swarm, media, &peer_id).await;
                        {
                            let mut s = state.write().await;
                            if let Some(peer) = s.peers.iter_mut().find(|p| p.peer_id == peer_id) {
                                peer.media.route_via = None;
                            }
                        }
                    }
                    Route::Relayed { via, destination } => {
                        let self_peer_id = state.read().await.local_peer_id.clone();
                        if destination.to_string() == self_peer_id {
                            push_note(state, format!("self-loop guard: refusing relayed frame to self for {peer_id}")).await;
                        } else {
                            let via_str = via.to_string();
                            send_frame_relayed_libp2p(state, swarm, media, via, destination).await;
                            let mut s = state.write().await;
                            if let Some(peer) = s.peers.iter_mut().find(|p| p.peer_id == peer_id) {
                                peer.media.route_via = Some(via_str);
                            }
                        }
                    }
                    Route::Unreachable => {
                        push_note(state, format!("no relay available for {peer_id}")).await;
                        active_media_flows.remove(&peer_id);
                        let mut s = state.write().await;
                        if let Some(peer) = s.peers.iter_mut().find(|p| p.peer_id == peer_id) {
                            peer.media.stream_state = MediaStreamState::Idle;
                            peer.media.route_via = None;
                        }
                        return;
                    }
                }
                schedule_media_tick(command_tx.clone(), peer_id);
            }
        }
    }
}

async fn dial_peer(
    state: &Arc<RwLock<DaemonStatus>>,
    swarm: &mut libp2p::Swarm<VoicersBehaviour>,
    pending_dials: &mut HashMap<String, PendingDial>,
    pending_rendezvous_lookups: &mut HashMap<kad::QueryId, PendingRendezvousLookup>,
    address: &str,
) -> Result<String> {
    let ranked = {
        let state = state.read().await;
        resolve_ranked_dial_target(&state.network, address)
    };

    let multiaddr: Multiaddr = match ranked.primary.parse() {
        Ok(multiaddr) => multiaddr,
        Err(_) if ranked.peer_id.is_some() => {
            let peer_id = ranked.peer_id.expect("checked above");
            let query_id = swarm
                .behaviour_mut()
                .kad
                .get_record(kad::RecordKey::new(&rendezvous_record_key(&peer_id)));
            pending_rendezvous_lookups.insert(query_id, PendingRendezvousLookup::PeerId(peer_id.clone()));
            push_note(
                state,
                format!("resolving rendezvous record for {peer_id} through the DHT"),
            )
            .await;
            return Ok(format!("resolving {peer_id} through the DHT"));
        }
        Err(_) => {
            let room_name = address.trim().to_string();
            let query_id = swarm
                .behaviour_mut()
                .kad
                .get_record(kad::RecordKey::new(&room_rendezvous_record_key(&room_name)));
            pending_rendezvous_lookups.insert(
                query_id,
                PendingRendezvousLookup::Room {
                    room_name: room_name.clone(),
                    resolved_peers: HashSet::new(),
                },
            );
            push_note(
                state,
                format!("resolving room/invite code `{room_name}` through the DHT"),
            )
            .await;
            return Ok(format!("resolving room `{room_name}` through the DHT"));
        }
    };

    dial_ranked_target(state, swarm, pending_dials, ranked, multiaddr).await
}

async fn broadcast_session_hello(
    swarm: &mut libp2p::Swarm<VoicersBehaviour>,
    hello: SessionHello,
) -> Result<()> {
    let peers: Vec<_> = swarm.connected_peers().cloned().collect();

    for peer in peers {
        swarm
            .behaviour_mut()
            .session
            .send_request(&peer, SessionRequest::Hello(hello.clone()));
    }

    Ok(())
}

async fn send_webrtc_signal(
    state: &Arc<RwLock<DaemonStatus>>,
    swarm: &mut libp2p::Swarm<VoicersBehaviour>,
    peer_id: &str,
    signal: WebRtcSignal,
) -> Result<()> {
    let peer_id: PeerId = peer_id
        .parse()
        .with_context(|| format!("invalid peer id for WebRTC signal: {peer_id}"))?;
    let signal_kind = signal.kind();

    swarm
        .behaviour_mut()
        .session
        .send_request(&peer_id, SessionRequest::WebRtcSignal(signal));
    push_note(
        state,
        format!("sent WebRTC {signal_kind} signal to {peer_id}"),
    )
    .await;

    Ok(())
}

#[cfg(feature = "webrtc-transport")]
async fn start_webrtc_offer(
    state: &Arc<RwLock<DaemonStatus>>,
    swarm: &mut libp2p::Swarm<VoicersBehaviour>,
    webrtc: &Option<WebRtcHandle>,
    peer_id: &str,
) -> Result<()> {
    let Some(webrtc) = webrtc else {
        anyhow::bail!("WebRTC runtime is not available");
    };
    let offer = webrtc.create_offer(peer_id.to_string()).await?;
    send_webrtc_signal(state, swarm, peer_id, offer).await?;
    {
        let mut state = state.write().await;
        state.network.transport_stage =
            "tcp+noise+yamux active; WebRTC offer generated; ICE gathering active".to_string();
        insert_unique_note(&mut state, format!("started WebRTC offer for {peer_id}"));
    }
    Ok(())
}

async fn handle_swarm_event(
    state: &Arc<RwLock<DaemonStatus>>,
    swarm: &mut libp2p::Swarm<VoicersBehaviour>,
    command_tx: &mpsc::Sender<NetworkCommand>,
    active_media_flows: &mut HashSet<String>,
    connected_peers: &mut HashSet<PeerId>,
    dht_routable_peers: &mut HashSet<PeerId>,
    attempted_relays: &mut HashSet<PeerId>,
    pending_dials: &mut HashMap<String, PendingDial>,
    pending_rendezvous_lookups: &mut HashMap<kad::QueryId, PendingRendezvousLookup>,
    pending_rendezvous_publications: &mut HashMap<kad::QueryId, (String, Vec<u8>)>,
    published_rendezvous_records: &mut HashMap<String, Vec<u8>>,
    pending_inbound_approvals: &mut HashMap<String, PendingInboundApproval>,
    relay_offers: &mut HashMap<String, Vec<String>>,
    #[cfg(feature = "webrtc-transport")] webrtc: &Option<WebRtcHandle>,
    media: &MediaHandle,
    persistence: &PersistenceHandle,
    event: SwarmEvent<VoicersBehaviourEvent>,
) {
    match event {
        SwarmEvent::NewListenAddr { address, .. } => {
            let rendered = address.to_string();
            {
                let mut state = state.write().await;
                insert_unique_addr(&mut state.network.listen_addrs, rendered.clone());
                update_share_invite(&mut state);
                state.network.transport_stage = if is_relayed_addr(&address) {
                    "tcp+relay+dcutr active; relay reservation ready; media transport still pending"
                        .to_string()
                } else {
                    "tcp+noise+yamux active; direct dial ready; session handshake active; media transport still pending"
                        .to_string()
                };
                insert_unique_note(&mut state, format!("listening on {rendered}"));
            }
            maybe_publish_rendezvous_record(
                state,
                swarm,
                connected_peers,
                dht_routable_peers,
                pending_rendezvous_publications,
                published_rendezvous_records,
            )
            .await;
            persist_network_snapshot(state, persistence).await;
        }
        SwarmEvent::NewExternalAddrCandidate { address } => {
            if is_shareable_multiaddr(&address) {
                swarm.add_external_address(address.clone());
                let mut state = state.write().await;
                let rendered = address.to_string();
                insert_unique_addr(&mut state.network.observed_addrs, rendered.clone());
                update_share_invite(&mut state);
                insert_unique_note(&mut state, format!("confirmed observed address {rendered}"));
            }
            maybe_publish_rendezvous_record(
                state,
                swarm,
                connected_peers,
                dht_routable_peers,
                pending_rendezvous_publications,
                published_rendezvous_records,
            )
            .await;
        }
        SwarmEvent::ExternalAddrConfirmed { address } => {
            {
                let mut state = state.write().await;
                let rendered = address.to_string();
                insert_unique_addr(&mut state.network.external_addrs, rendered.clone());
                state.network.nat_status = if is_relayed_addr(&address) {
                    "relay reservation active".to_string()
                } else {
                    "external address confirmed".to_string()
                };
                update_share_invite(&mut state);
                insert_unique_note(&mut state, format!("external address confirmed {rendered}"));
            }
            maybe_publish_rendezvous_record(
                state,
                swarm,
                connected_peers,
                dht_routable_peers,
                pending_rendezvous_publications,
                published_rendezvous_records,
            )
            .await;
            persist_network_snapshot(state, persistence).await;
        }
        SwarmEvent::ExternalAddrExpired { address } => {
            {
                let mut state = state.write().await;
                let rendered = address.to_string();
                remove_addr(&mut state.network.external_addrs, &rendered);
                remove_addr(&mut state.network.observed_addrs, &rendered);
                update_share_invite(&mut state);
                insert_unique_note(&mut state, format!("external address expired {rendered}"));
            }
            maybe_publish_rendezvous_record(
                state,
                swarm,
                connected_peers,
                dht_routable_peers,
                pending_rendezvous_publications,
                published_rendezvous_records,
            )
            .await;
            persist_network_snapshot(state, persistence).await;
        }
        SwarmEvent::ConnectionEstablished {
            peer_id, endpoint, ..
        } => {
            connected_peers.insert(peer_id);
            let peer_id_string = peer_id.to_string();
            let had_pending_dial = pending_dials.remove(&peer_id_string).is_some();
            let address = endpoint.get_remote_address().to_string();
            let local_hello = {
                let state = state.read().await;
                SessionHello {
                    room_name: state.session.room_name.clone(),
                    display_name: state.session.display_name.clone(),
                    trusted_contacts: state
                        .network
                        .known_peers
                        .iter()
                        .filter(|p| p.trusted_contact)
                        .map(|p| p.peer_id.clone())
                        .collect(),
                }
            };
            let should_surface_peer = {
                let state = state.read().await;
                should_surface_connected_peer(&state, &peer_id_string, had_pending_dial)
            };
            {
                let mut state = state.write().await;
                let path = if endpoint.is_relayed() {
                    "libp2p-relay"
                } else {
                    "libp2p-direct"
                };
                record_path_score(&mut state, path, Some(peer_id_string.clone()), true);
            }
            persist_network_snapshot(state, persistence).await;
            push_note(
                state,
                format!(
                    "{} connection established with {}{}",
                    if endpoint.is_relayed() {
                        "relayed"
                    } else {
                        "direct"
                    },
                    peer_id_string,
                    if should_surface_peer {
                        ""
                    } else {
                        " (transport-only)"
                    }
                ),
            )
            .await;
            if should_surface_peer {
                let peer = upsert_peer(state, peer_id_string.clone(), address).await;
                let _ = media.register_peer(peer.peer_id.clone()).await;
                let should_send_hello = match &endpoint {
                    libp2p::core::ConnectedPoint::Dialer { .. } => true,
                    libp2p::core::ConnectedPoint::Listener { .. } => {
                        is_peer_whitelisted(state, &peer.peer_id).await
                    }
                };
                if should_send_hello {
                    send_session_hello(swarm, &peer.peer_id, local_hello).await;
                }
            }
        }
        SwarmEvent::ConnectionClosed {
            peer_id,
            num_established,
            ..
        } => {
            if num_established == 0 {
                connected_peers.remove(&peer_id);
            }
            {
                let mut state = state.write().await;
                if let Some(index) = state
                    .peers
                    .iter()
                    .position(|existing| existing.peer_id == peer_id.to_string())
                {
                    let remove_peer = num_established == 0
                        && matches!(state.peers[index].session, PeerSessionState::None)
                        && !state
                            .network
                            .known_peers
                            .iter()
                            .any(|existing| existing.peer_id == peer_id.to_string())
                        && !state
                            .pending_peer_approvals
                            .iter()
                            .any(|pending| pending.peer_id == peer_id.to_string());
                    if remove_peer {
                        state.peers.remove(index);
                    } else {
                        let peer = &mut state.peers[index];
                        peer.transport = if num_established == 0 {
                            PeerTransportState::Disconnected
                        } else {
                            PeerTransportState::Connected
                        };
                    }
                }
                if let Some(known_peer) = state
                    .network
                    .known_peers
                    .iter_mut()
                    .find(|existing| existing.peer_id == peer_id.to_string())
                {
                    known_peer.connected = num_established > 0;
                }
                insert_unique_note(&mut state, format!("connection closed with {peer_id}"));
            }
            active_media_flows.remove(&peer_id.to_string());
            if num_established == 0 {
                relay_offers.remove(&peer_id.to_string());
            }
            let _ = media.disconnect_peer(peer_id.to_string()).await;
            persist_network_snapshot(state, persistence).await;
        }
        SwarmEvent::OutgoingConnectionError { peer_id, error, .. } => {
            let mut status = state.write().await;
            let peer_label = peer_id.map(|peer| peer.to_string());
            let failed_path = peer_label
                .as_ref()
                .and_then(|peer| pending_dials.get(peer))
                .map(|pending| address_path_name(&pending.current_address))
                .unwrap_or("libp2p-direct");
            record_path_score(&mut status, failed_path, peer_label.clone(), false);
            insert_unique_note(
                &mut status,
                format!(
                    "outgoing connection error{}: {error}",
                    peer_label
                        .as_deref()
                        .map(|peer| format!(" for {peer}"))
                        .unwrap_or_default()
                ),
            );
            let next_retry = peer_label.as_ref().and_then(|peer| {
                let pending = pending_dials.get_mut(peer)?;
                let next = pending.fallback_addresses.first().cloned()?;
                pending.fallback_addresses.remove(0);
                pending.current_address = next.clone();
                Some((peer.clone(), next, pending.fallback_addresses.clone()))
            });
            drop(status);
            if let Some((peer, next_address, remaining)) = next_retry {
                match next_address.parse::<Multiaddr>() {
                    Ok(multiaddr) => {
                        if swarm.dial(multiaddr.clone()).is_ok() {
                            if remaining.is_empty() {
                                push_note(
                                    state,
                                    format!("retrying {peer} via fallback {multiaddr}"),
                                )
                                .await;
                            } else {
                                push_note(
                                    state,
                                    format!(
                                        "retrying {peer} via fallback {multiaddr}; remaining fallbacks: {}",
                                        remaining.join(", ")
                                    ),
                                )
                                .await;
                            }
                        } else {
                            push_note(
                                state,
                                format!("fallback dial setup failed for {peer} via {multiaddr}"),
                            )
                            .await;
                        }
                    }
                    Err(parse_error) => {
                        push_note(
                            state,
                            format!(
                                "skipping invalid fallback dial address for {peer}: {next_address} ({parse_error})"
                            ),
                        )
                        .await;
                    }
                }
            }
        }
        SwarmEvent::Behaviour(VoicersBehaviourEvent::Identify(identify::Event::Received {
            peer_id,
            info,
            ..
        })) => {
            let listen_addrs = info.listen_addrs.clone();
            let protocols = info.protocols.clone();
            let agent_version = info.agent_version.clone();
            let remote_addr = info
                .listen_addrs
                .first()
                .map(ToString::to_string)
                .unwrap_or_else(|| "<unknown>".to_string());
            let observed_addr = info.observed_addr.to_string();

            for address in &listen_addrs {
                if is_supported_dial_addr(address) {
                    swarm
                        .behaviour_mut()
                        .kad
                        .add_address(&peer_id, address.clone());
                }
            }

            if protocols.contains(&relay::HOP_PROTOCOL_NAME)
                && attempted_relays.len() < MAX_AUTO_RELAY_CANDIDATES
                && attempted_relays.insert(peer_id)
            {
                if let Some(relay_addr) = listen_addrs
                    .iter()
                    .find(|address| is_shareable_multiaddr(address) && !is_relayed_addr(address))
                    .cloned()
                    .map(|address| relay_addr_for_peer(address, peer_id))
                {
                    match relay_reservation_addr(relay_addr.clone()).and_then(|reservation_addr| {
                        swarm
                            .listen_on(reservation_addr.clone())
                            .map(|_| ())
                            .with_context(|| {
                                format!("failed to request relay reservation at {reservation_addr}")
                            })
                    }) {
                        Ok(()) => {
                            push_note(
                                state,
                                format!("requesting relay reservation from {peer_id}"),
                            )
                            .await;
                        }
                        Err(error) => {
                            push_note(
                                state,
                                format!("relay reservation request failed for {peer_id}: {error}"),
                            )
                            .await;
                        }
                    }
                }
            }

            {
                let mut state = state.write().await;
                if should_surface_peer_metadata(&state, &peer_id.to_string()) {
                    let identified_peer = {
                        let peer =
                            get_or_insert_peer(&mut state, peer_id.to_string(), remote_addr);
                        peer.display_name = if agent_version.is_empty() {
                            format!("peer {}", short_peer_id(&peer.peer_id))
                        } else {
                            agent_version
                        };
                        peer.transport = PeerTransportState::Connected;
                        (peer.peer_id.clone(), peer.display_name.clone())
                    };
                    update_known_peer(
                        &mut state,
                        &identified_peer.0,
                        None,
                        Some(identified_peer.1.clone()),
                        true,
                        false,
                    );
                    insert_unique_note(
                        &mut state,
                        format!("identified session peer {}", identified_peer.0),
                    );
                }
                if !observed_addr.is_empty() {
                    insert_unique_addr(&mut state.network.observed_addrs, observed_addr.clone());
                    state.network.nat_status = "observed by remote peers".to_string();
                }
                update_share_invite(&mut state);
            }
            maybe_publish_rendezvous_record(
                state,
                swarm,
                connected_peers,
                dht_routable_peers,
                pending_rendezvous_publications,
                published_rendezvous_records,
            )
            .await;
            persist_network_snapshot(state, persistence).await;
        }
        SwarmEvent::Behaviour(VoicersBehaviourEvent::Identify(identify::Event::Sent {
            peer_id,
            ..
        })) => {
            push_note(state, format!("sent identify info to {peer_id}")).await;
        }
        SwarmEvent::Behaviour(VoicersBehaviourEvent::Identify(identify::Event::Error {
            peer_id,
            error,
            ..
        })) => {
            push_note(state, format!("identify error with {peer_id}: {error}")).await;
        }
        SwarmEvent::Behaviour(VoicersBehaviourEvent::Ping(ping::Event {
            peer,
            result: Ok(rtt),
            ..
        })) => {
            let mut state = state.write().await;
            if let Some(existing) = state
                .peers
                .iter_mut()
                .find(|candidate| candidate.peer_id == peer.to_string())
            {
                existing.transport = PeerTransportState::Connected;
            }
            insert_unique_note(&mut state, format!("ping {peer} {rtt:?}"));
        }
        SwarmEvent::Behaviour(VoicersBehaviourEvent::Ping(ping::Event {
            peer,
            result: Err(error),
            ..
        })) => {
            push_note(state, format!("ping failure with {peer}: {error}")).await;
        }
        SwarmEvent::Behaviour(VoicersBehaviourEvent::Relay(
            relay::client::Event::ReservationReqAccepted {
                relay_peer_id,
                renewal,
                ..
            },
        )) => {
            let mut state = state.write().await;
            state.network.nat_status = "relay reservation accepted".to_string();
            state.network.transport_stage =
                "tcp+relay+dcutr active; relay reservation accepted".to_string();
            record_path_score(
                &mut state,
                "libp2p-relay",
                Some(relay_peer_id.to_string()),
                true,
            );
            insert_unique_note(
                &mut state,
                format!(
                    "relay reservation {} by {relay_peer_id}",
                    if renewal { "renewed" } else { "accepted" }
                ),
            );
        }
        SwarmEvent::Behaviour(VoicersBehaviourEvent::Relay(
            relay::client::Event::OutboundCircuitEstablished { relay_peer_id, .. },
        )) => {
            push_note(
                state,
                format!("outbound relay circuit established through {relay_peer_id}"),
            )
            .await;
        }
        SwarmEvent::Behaviour(VoicersBehaviourEvent::Relay(
            relay::client::Event::InboundCircuitEstablished { src_peer_id, .. },
        )) => {
            push_note(
                state,
                format!("inbound relay circuit established from {src_peer_id}"),
            )
            .await;
        }
        SwarmEvent::Behaviour(VoicersBehaviourEvent::Kad(kad_event)) => match kad_event {
            kad::Event::RoutingUpdated {
                peer, is_new_peer, ..
            } => {
                dht_routable_peers.insert(peer);
                if is_new_peer {
                    push_note(state, format!("DHT discovered peer {peer}")).await;
                    maybe_publish_rendezvous_record(
                        state,
                        swarm,
                        connected_peers,
                        dht_routable_peers,
                        pending_rendezvous_publications,
                        published_rendezvous_records,
                    )
                    .await;
                }
            }
            kad::Event::OutboundQueryProgressed { id, result, .. } => match result {
                kad::QueryResult::Bootstrap(Ok(ok)) => {
                    let mut daemon_state = state.write().await;
                    daemon_state.network.transport_stage =
                        "tcp+relay+dcutr+dht active; bootstrap in progress".to_string();
                    insert_unique_note(
                        &mut daemon_state,
                        format!(
                            "DHT bootstrap query reached {}; {} remaining",
                            ok.peer, ok.num_remaining
                        ),
                    );
                    drop(daemon_state);
                    maybe_publish_rendezvous_record(
                        state,
                        swarm,
                        connected_peers,
                        dht_routable_peers,
                        pending_rendezvous_publications,
                        published_rendezvous_records,
                    )
                    .await;
                }
                kad::QueryResult::Bootstrap(Err(error)) => {
                    push_note(state, format!("DHT bootstrap failed: {error}")).await;
                }
                kad::QueryResult::PutRecord(Ok(_)) => {
                    if let Some((key, payload)) = pending_rendezvous_publications.remove(&id) {
                        published_rendezvous_records.insert(key, payload);
                        push_note(state, "published rendezvous record to the DHT".to_string()).await;
                    }
                }
                kad::QueryResult::PutRecord(Err(error)) => {
                    pending_rendezvous_publications.remove(&id);
                    push_note(state, format!("rendezvous record publication failed: {error}")).await;
                }
                kad::QueryResult::GetRecord(Ok(kad::GetRecordOk::FoundRecord(peer_record))) => {
                    if let Some(invite) = decode_rendezvous_record(&peer_record.record.value) {
                        if let Some(lookup) = pending_rendezvous_lookups.get_mut(&id) {
                            match lookup {
                                PendingRendezvousLookup::PeerId(requested_peer_id) => {
                                    if invite.peer_id == *requested_peer_id && !invite.addrs.is_empty() {
                                        seed_rendezvous_invite(state, &invite).await;
                                        let requested_peer_id = requested_peer_id.clone();
                                        pending_rendezvous_lookups.remove(&id);
                                        let ranked = {
                                            let state = state.read().await;
                                            resolve_ranked_dial_target(&state.network, &requested_peer_id)
                                        };
                                        match ranked.primary.parse::<Multiaddr>() {
                                            Ok(multiaddr) => {
                                                let _ = dial_ranked_target(
                                                    state,
                                                    swarm,
                                                    pending_dials,
                                                    ranked,
                                                    multiaddr,
                                                )
                                                .await;
                                            }
                                            Err(_) => {
                                                push_note(
                                                    state,
                                                    format!(
                                                        "rendezvous resolved {requested_peer_id}, but no dialable address was produced"
                                                    ),
                                                )
                                                .await;
                                            }
                                        }
                                    }
                                }
                                PendingRendezvousLookup::Room {
                                    room_name,
                                    resolved_peers,
                                } => {
                                    if !invite.addrs.is_empty()
                                        && resolved_peers.insert(invite.peer_id.clone())
                                    {
                                        seed_rendezvous_invite(state, &invite).await;
                                        let ranked = {
                                            let state = state.read().await;
                                            resolve_ranked_dial_target(&state.network, &invite.peer_id)
                                        };
                                        match ranked.primary.parse::<Multiaddr>() {
                                            Ok(multiaddr) => {
                                                let _ = dial_ranked_target(
                                                    state,
                                                    swarm,
                                                    pending_dials,
                                                    ranked,
                                                    multiaddr,
                                                )
                                                .await;
                                                push_note(
                                                    state,
                                                    format!(
                                                        "room/invite code `{room_name}` resolved peer {}",
                                                        invite.peer_id
                                                    ),
                                                )
                                                .await;
                                            }
                                            Err(_) => {
                                                push_note(
                                                    state,
                                                    format!(
                                                        "room/invite code `{room_name}` resolved {}, but no dialable address was produced",
                                                        invite.peer_id
                                                    ),
                                                )
                                                .await;
                                            }
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
                kad::QueryResult::GetRecord(Ok(kad::GetRecordOk::FinishedWithNoAdditionalRecord { .. })) => {
                    if let Some(lookup) = pending_rendezvous_lookups.remove(&id) {
                        match lookup {
                            PendingRendezvousLookup::PeerId(requested_peer_id) => {
                                push_note(
                                    state,
                                    format!("DHT rendezvous lookup for {requested_peer_id} finished without a usable record"),
                                )
                                .await;
                            }
                            PendingRendezvousLookup::Room {
                                room_name,
                                resolved_peers,
                            } => {
                                if resolved_peers.is_empty() {
                                    push_note(
                                        state,
                                        format!("room/invite code `{room_name}` finished without any rendezvous matches"),
                                    )
                                    .await;
                                }
                            }
                        }
                    }
                }
                kad::QueryResult::GetRecord(Err(error)) => {
                    if let Some(lookup) = pending_rendezvous_lookups.remove(&id) {
                        match lookup {
                            PendingRendezvousLookup::PeerId(requested_peer_id) => {
                                push_note(
                                    state,
                                    format!("DHT rendezvous lookup failed for {requested_peer_id}: {error}"),
                                )
                                .await;
                            }
                            PendingRendezvousLookup::Room { room_name, .. } => {
                                push_note(
                                    state,
                                    format!("DHT rendezvous lookup failed for room/invite code `{room_name}`: {error}"),
                                )
                                .await;
                            }
                        }
                    }
                }
                _ => {}
            },
            kad::Event::ModeChanged { new_mode } => {
                push_note(state, format!("DHT mode changed to {new_mode:?}")).await;
            }
            _ => {}
        },
        SwarmEvent::Behaviour(VoicersBehaviourEvent::Dcutr(event)) => {
            let mut state = state.write().await;
            match event.result {
                Ok(connection_id) => {
                    state.network.transport_stage =
                        "tcp+relay+dcutr active; direct hole punch established".to_string();
                    record_path_score(
                        &mut state,
                        "libp2p-dcutr-direct",
                        Some(event.remote_peer_id.to_string()),
                        true,
                    );
                    insert_unique_note(
                        &mut state,
                        format!(
                            "hole punch succeeded with {} on connection {:?}",
                            event.remote_peer_id, connection_id
                        ),
                    );
                }
                Err(error) => {
                    record_path_score(
                        &mut state,
                        "libp2p-dcutr-direct",
                        Some(event.remote_peer_id.to_string()),
                        false,
                    );
                    insert_unique_note(
                        &mut state,
                        format!("hole punch failed with {}: {error}", event.remote_peer_id),
                    );
                }
            }
        }
        SwarmEvent::Behaviour(VoicersBehaviourEvent::Upnp(upnp::Event::NewExternalAddr(
            address,
        ))) => {
            {
                let mut state = state.write().await;
                let rendered = address.to_string();
                insert_unique_addr(&mut state.network.external_addrs, rendered.clone());
                state.network.nat_status = "upnp mapped".to_string();
                update_share_invite(&mut state);
                insert_unique_note(&mut state, format!("UPnP mapped {rendered}"));
            }
            maybe_publish_rendezvous_record(
                state,
                swarm,
                connected_peers,
                dht_routable_peers,
                pending_rendezvous_publications,
                published_rendezvous_records,
            )
            .await;
            persist_network_snapshot(state, persistence).await;
        }
        SwarmEvent::Behaviour(VoicersBehaviourEvent::Upnp(upnp::Event::ExpiredExternalAddr(
            address,
        ))) => {
            {
                let mut state = state.write().await;
                let rendered = address.to_string();
                remove_addr(&mut state.network.external_addrs, &rendered);
                if state.network.external_addrs.is_empty() {
                    state.network.nat_status = "upnp mapping expired".to_string();
                }
                update_share_invite(&mut state);
                insert_unique_note(&mut state, format!("UPnP mapping expired for {rendered}"));
            }
            maybe_publish_rendezvous_record(
                state,
                swarm,
                connected_peers,
                dht_routable_peers,
                pending_rendezvous_publications,
                published_rendezvous_records,
            )
            .await;
            persist_network_snapshot(state, persistence).await;
        }
        SwarmEvent::Behaviour(VoicersBehaviourEvent::Upnp(upnp::Event::GatewayNotFound)) => {
            let mut state = state.write().await;
            state.network.nat_status = "no upnp gateway found".to_string();
            insert_unique_note(&mut state, "UPnP gateway not found".to_string());
        }
        SwarmEvent::Behaviour(VoicersBehaviourEvent::Upnp(upnp::Event::NonRoutableGateway)) => {
            let mut state = state.write().await;
            state.network.nat_status = "gateway is not publicly routable".to_string();
            insert_unique_note(
                &mut state,
                "UPnP gateway is not publicly routable".to_string(),
            );
        }
        SwarmEvent::Behaviour(VoicersBehaviourEvent::Session(
            request_response::Event::Message { peer, message, .. },
        )) => match message {
            request_response::Message::Request {
                request: SessionRequest::Hello(hello),
                channel,
                ..
            } => {
                if should_gate_peer_approval(state, &peer.to_string()).await {
                    queue_pending_peer_approval(
                        state,
                        pending_inbound_approvals,
                        &peer.to_string(),
                        hello,
                        channel,
                    )
                    .await;
                } else {
                    relay_offers.insert(peer.to_string(), hello.trusted_contacts.clone());
                    apply_approved_session_hello(
                        state,
                        swarm,
                        command_tx,
                        active_media_flows,
                        media,
                        &peer.to_string(),
                        hello.clone(),
                        channel,
                    )
                    .await;
                    persist_network_snapshot(state, persistence).await;
                }
            }
            request_response::Message::Request {
                request: SessionRequest::WebRtcSignal(signal),
                channel,
                ..
            } => {
                let signal_kind = signal.kind().to_string();
                record_webrtc_signal(state, &peer, &signal).await;
                #[cfg(feature = "webrtc-transport")]
                if let Some(webrtc) = webrtc {
                    match webrtc
                        .handle_remote_signal(peer.to_string(), signal.clone())
                        .await
                    {
                        Ok(reply_signals) => {
                            for reply_signal in reply_signals {
                                let _ = send_webrtc_signal(
                                    state,
                                    swarm,
                                    &peer.to_string(),
                                    reply_signal,
                                )
                                .await;
                            }
                        }
                        Err(error) => {
                            push_note(
                                state,
                                format!(
                                    "WebRTC {signal_kind} handling failed for {peer}: {error:#}"
                                ),
                            )
                            .await;
                        }
                    }
                }

                if let Err(response) = swarm.behaviour_mut().session.send_response(
                    channel,
                    SessionResponse::WebRtcSignalAck {
                        signal_kind: signal_kind.clone(),
                    },
                ) {
                    push_note(
                        state,
                        format!("failed to send WebRTC signal ack to {peer}: {response:?}"),
                    )
                    .await;
                } else {
                    push_note(
                        state,
                        format!("WebRTC {signal_kind} signal received from {peer}"),
                    )
                    .await;
                }
            }
            request_response::Message::Response {
                response: SessionResponse::HelloAck(hello),
                ..
            } => {
                relay_offers.insert(peer.to_string(), hello.trusted_contacts.clone());
                apply_session_hello(state, peer.to_string(), hello).await;
                persist_network_snapshot(state, persistence).await;
                let _ = media.register_peer(peer.to_string()).await;
                push_note(state, format!("session hello acknowledged by {peer}")).await;
                if active_media_flows.insert(peer.to_string()) {
                    {
                        let mut state = state.write().await;
                        state.network.transport_stage =
                            "tcp+noise+yamux active; direct media transport active".to_string();
                    }
                    select_media_path(state, media_transport::MediaPath::Libp2pRequestResponse)
                        .await;
                    schedule_media_tick(command_tx.clone(), peer.to_string());
                    push_note(state, format!("media flow started for {peer}")).await;
                }
            }
            request_response::Message::Response {
                response: SessionResponse::JoinDenied { message },
                ..
            } => {
                push_note(state, format!("join denied by {peer}: {message}")).await;
            }
            request_response::Message::Response {
                response: SessionResponse::WebRtcSignalAck { signal_kind },
                ..
            } => {
                push_note(
                    state,
                    format!("WebRTC {signal_kind} signal acknowledged by {peer}"),
                )
                .await;
            }
        },
        SwarmEvent::Behaviour(VoicersBehaviourEvent::Session(
            request_response::Event::OutboundFailure { peer, error, .. },
        )) => {
            push_note(
                state,
                format!("session outbound failure with {peer}: {error}"),
            )
            .await;
        }
        SwarmEvent::Behaviour(VoicersBehaviourEvent::Session(
            request_response::Event::InboundFailure { peer, error, .. },
        )) => {
            push_note(
                state,
                format!("session inbound failure with {peer}: {error}"),
            )
            .await;
        }
        SwarmEvent::Behaviour(VoicersBehaviourEvent::Session(
            request_response::Event::ResponseSent { peer, .. },
        )) => {
            push_note(state, format!("session response sent to {peer}")).await;
        }
        SwarmEvent::Behaviour(VoicersBehaviourEvent::Media(request_response::Event::Message {
            peer,
            message,
            ..
        })) => match message {
            request_response::Message::Request {
                request: MediaRequest::Frame(frame),
                channel,
                ..
            } => {
                match media
                    .handle_incoming_frame(peer.to_string(), frame.clone())
                    .await
                {
                    Ok(ack) => {
                        if let Err(response) = swarm
                            .behaviour_mut()
                            .media
                            .send_response(channel, MediaResponse::Ack(ack))
                        {
                            push_note(
                                state,
                                format!("failed to send media ack to {peer}: {response:?}"),
                            )
                            .await;
                        }
                    }
                    Err(error) => {
                        push_note(state, format!("media engine failed for {peer}: {error}")).await;
                    }
                }
            }
            request_response::Message::Request {
                request: MediaRequest::RelayedFrame { destination, frame },
                channel,
                ..
            } => {
                let local_peer_id = state.read().await.local_peer_id.clone();
                if destination == local_peer_id {
                    match media
                        .handle_incoming_frame(peer.to_string(), frame.clone())
                        .await
                    {
                        Ok(ack) => {
                            let _ = swarm.behaviour_mut().media.send_response(
                                channel,
                                MediaResponse::RelayAck { destination, ack },
                            );
                        }
                        Err(error) => {
                            push_note(
                                state,
                                format!("media engine failed for relayed frame from {peer}: {error}"),
                            )
                            .await;
                        }
                    }
                } else if let Ok(destination_peer_id) = destination.parse::<PeerId>() {
                    let is_trusted = state
                        .read()
                        .await
                        .network
                        .known_peers
                        .iter()
                        .any(|kp| kp.peer_id == destination && kp.trusted_contact);
                    if connected_peers.contains(&destination_peer_id) && is_trusted {
                        // SECURITY: relay sees frames in cleartext; per-hop encryption deferred to future iteration.
                        swarm.behaviour_mut().media.send_request(
                            &destination_peer_id,
                            MediaRequest::RelayedFrame {
                                destination: destination.clone(),
                                frame: frame.clone(),
                            },
                        );
                        let ack = MediaAck {
                            accepted_sequence: frame.sequence,
                            queue_depth: 0,
                            queued_samples: 0,
                        };
                        let _ = swarm.behaviour_mut().media.send_response(
                            channel,
                            MediaResponse::RelayAck { destination, ack },
                        );
                    } else {
                        let _ = swarm.behaviour_mut().media.send_response(
                            channel,
                            MediaResponse::RelayDenied {
                                destination,
                                reason: "not relaying for that peer".to_string(),
                            },
                        );
                    }
                } else {
                    let _ = swarm.behaviour_mut().media.send_response(
                        channel,
                        MediaResponse::RelayDenied {
                            destination,
                            reason: "not relaying for that peer".to_string(),
                        },
                    );
                }
            }
            request_response::Message::Response {
                response: MediaResponse::Ack(ack),
                ..
            } => {
                if let Err(error) = media.handle_ack(peer.to_string(), ack).await {
                    push_note(
                        state,
                        format!("media ack handling failed for {peer}: {error}"),
                    )
                    .await;
                }
            }
            request_response::Message::Response {
                response: MediaResponse::RelayAck { ack, .. },
                ..
            } => {
                if let Err(error) = media.handle_ack(peer.to_string(), ack).await {
                    push_note(
                        state,
                        format!("media ack handling failed for {peer}: {error}"),
                    )
                    .await;
                }
            }
            request_response::Message::Response {
                response: MediaResponse::RelayDenied { destination, reason },
                ..
            } => {
                push_note(
                    state,
                    format!("relay denied for {destination} via {peer}: {reason}"),
                )
                .await;
            }
        },
        SwarmEvent::Behaviour(VoicersBehaviourEvent::Media(
            request_response::Event::OutboundFailure { peer, error, .. },
        )) => {
            push_note(
                state,
                format!("media outbound failure with {peer}: {error}"),
            )
            .await;
        }
        SwarmEvent::Behaviour(VoicersBehaviourEvent::Media(
            request_response::Event::InboundFailure { peer, error, .. },
        )) => {
            push_note(state, format!("media inbound failure with {peer}: {error}")).await;
        }
        SwarmEvent::Behaviour(VoicersBehaviourEvent::Media(
            request_response::Event::ResponseSent { .. },
        )) => {}
        _ => {}
    }
}

#[cfg(not(feature = "webrtc-transport"))]
async fn send_next_media_frame(
    state: &Arc<RwLock<DaemonStatus>>,
    swarm: &mut libp2p::Swarm<VoicersBehaviour>,
    media: &MediaHandle,
    peer_id: &str,
) {
    send_frame_libp2p(state, swarm, media, peer_id).await;
}

#[cfg(feature = "webrtc-transport")]
async fn send_next_media_frame_webrtc_first(
    state: &Arc<RwLock<DaemonStatus>>,
    swarm: &mut libp2p::Swarm<VoicersBehaviour>,
    webrtc: &Option<WebRtcHandle>,
    media: &MediaHandle,
    peer_id: &str,
) {
    send_frame_with_fallback(state, swarm, webrtc.as_ref(), media, peer_id).await;
}

fn schedule_media_tick(command_tx: mpsc::Sender<NetworkCommand>, peer_id: String) {
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(20)).await;
        let _ = command_tx
            .send(NetworkCommand::SendMediaFrame { peer_id })
            .await;
    });
}

async fn upsert_peer(
    state: &Arc<RwLock<DaemonStatus>>,
    peer_id: String,
    address: String,
) -> PeerSummary {
    let mut state = state.write().await;
    let peer = get_or_insert_peer(&mut state, peer_id, address);
    peer.transport = PeerTransportState::Connected;
    if matches!(peer.session, PeerSessionState::None) {
        peer.session = PeerSessionState::Handshaking;
    }
    let peer_clone = peer.clone();
    if peer_clone.address != "<unknown>" {
        insert_unique_addr(
            &mut state.network.saved_peer_addrs,
            peer_clone.address.clone(),
        );
    }
    update_known_peer(
        &mut state,
        &peer_clone.peer_id,
        Some(peer_clone.address.clone()),
        Some(peer_clone.display_name.clone()),
        true,
        false,
    );
    peer_clone
}

fn get_or_insert_peer<'a>(
    state: &'a mut DaemonStatus,
    peer_id: String,
    address: String,
) -> &'a mut PeerSummary {
    if let Some(index) = state
        .peers
        .iter()
        .position(|existing| existing.peer_id == peer_id)
    {
        let peer = &mut state.peers[index];
        peer.address = address;
        return peer;
    }

    let index = state.peers.len() + 1;
    state.peers.push(PeerSummary {
        peer_id,
        display_name: format!("peer {index}"),
        address,
        muted: false,
        output_volume_percent: 100,
        output_bus: format!("peer_bus_{index:02}"),
        transport: PeerTransportState::Connecting,
        session: PeerSessionState::None,
        media: PeerMediaState {
            stream_state: MediaStreamState::Idle,
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

    state.peers.last_mut().expect("peer inserted")
}

async fn send_session_hello(
    swarm: &mut libp2p::Swarm<VoicersBehaviour>,
    peer_id: &str,
    hello: SessionHello,
) {
    if let Ok(peer_id) = peer_id.parse() {
        swarm
            .behaviour_mut()
            .session
            .send_request(&peer_id, SessionRequest::Hello(hello));
    }
}

async fn should_gate_peer_approval(state: &Arc<RwLock<DaemonStatus>>, peer_id: &str) -> bool {
    let state = state.read().await;
    !state
        .network
        .known_peers
        .iter()
        .find(|peer| peer.peer_id == peer_id)
        .map(|peer| peer.whitelisted)
        .unwrap_or(false)
}

async fn is_peer_whitelisted(state: &Arc<RwLock<DaemonStatus>>, peer_id: &str) -> bool {
    let state = state.read().await;
    state
        .network
        .known_peers
        .iter()
        .find(|peer| peer.peer_id == peer_id)
        .map(|peer| peer.whitelisted)
        .unwrap_or(false)
}

async fn queue_pending_peer_approval(
    state: &Arc<RwLock<DaemonStatus>>,
    pending_inbound_approvals: &mut HashMap<String, PendingInboundApproval>,
    peer_id: &str,
    hello: SessionHello,
    channel: request_response::ResponseChannel<SessionResponse>,
) {
    let (address, known) = {
        let state = state.read().await;
        let address = state
            .peers
            .iter()
            .find(|peer| peer.peer_id == peer_id)
            .map(|peer| peer.address.clone())
            .unwrap_or_else(|| "<unknown>".to_string());
        let known = state
            .network
            .known_peers
            .iter()
            .any(|peer| peer.peer_id == peer_id);
        (address, known)
    };
    pending_inbound_approvals.insert(
        peer_id.to_string(),
        PendingInboundApproval {
            hello: hello.clone(),
            channel,
        },
    );
    {
        let mut state = state.write().await;
        if !state
            .pending_peer_approvals
            .iter()
            .any(|pending| pending.peer_id == peer_id)
        {
            state.pending_peer_approvals.push(voicers_core::PendingPeerApprovalSummary {
                peer_id: peer_id.to_string(),
                display_name: hello.display_name.clone(),
                address,
                room_name: hello.room_name.clone(),
                known,
            });
        }
    }
    push_note(
        state,
        format!("peer {peer_id} is waiting for room approval"),
    )
    .await;
}

async fn apply_approved_session_hello(
    state: &Arc<RwLock<DaemonStatus>>,
    swarm: &mut libp2p::Swarm<VoicersBehaviour>,
    command_tx: &mpsc::Sender<NetworkCommand>,
    active_media_flows: &mut HashSet<String>,
    media: &MediaHandle,
    peer_id: &str,
    hello: SessionHello,
    channel: request_response::ResponseChannel<SessionResponse>,
) {
    apply_session_hello(state, peer_id.to_string(), hello.clone()).await;
    let _ = media.register_peer(peer_id.to_string()).await;
    let response = {
        let state = state.read().await;
        SessionResponse::HelloAck(SessionHello {
            room_name: state.session.room_name.clone(),
            display_name: state.session.display_name.clone(),
            trusted_contacts: state
                .network
                .known_peers
                .iter()
                .filter(|p| p.trusted_contact)
                .map(|p| p.peer_id.clone())
                .collect(),
        })
    };
    if let Err(response) = swarm.behaviour_mut().session.send_response(channel, response) {
        push_note(
            state,
            format!("failed to send session response to {peer_id}: {response:?}"),
        )
        .await;
    } else {
        push_note(state, format!("session hello received from {peer_id}")).await;
    }
    if active_media_flows.insert(peer_id.to_string()) {
        {
            let mut state = state.write().await;
            state.network.transport_stage =
                "tcp+noise+yamux active; direct media transport active".to_string();
            state
                .pending_peer_approvals
                .retain(|pending| pending.peer_id != peer_id);
        }
        select_media_path(state, media_transport::MediaPath::Libp2pRequestResponse).await;
        schedule_media_tick(command_tx.clone(), peer_id.to_string());
        push_note(state, format!("media flow started for {peer_id}")).await;
    }
}

async fn approve_pending_peer(
    state: &Arc<RwLock<DaemonStatus>>,
    swarm: &mut libp2p::Swarm<VoicersBehaviour>,
    command_tx: &mpsc::Sender<NetworkCommand>,
    active_media_flows: &mut HashSet<String>,
    pending_inbound_approvals: &mut HashMap<String, PendingInboundApproval>,
    peer_id: &str,
    media: &MediaHandle,
) -> Result<String> {
    let Some(pending) = pending_inbound_approvals.remove(peer_id) else {
        anyhow::bail!("pending peer not found");
    };
    apply_approved_session_hello(
        state,
        swarm,
        command_tx,
        active_media_flows,
        media,
        peer_id,
        pending.hello,
        pending.channel,
    )
    .await;
    Ok(format!("approved {peer_id}"))
}

async fn reject_pending_peer(
    state: &Arc<RwLock<DaemonStatus>>,
    swarm: &mut libp2p::Swarm<VoicersBehaviour>,
    pending_inbound_approvals: &mut HashMap<String, PendingInboundApproval>,
    peer_id: &str,
) -> Result<String> {
    let Some(pending) = pending_inbound_approvals.remove(peer_id) else {
        anyhow::bail!("pending peer not found");
    };
    let _ = swarm.behaviour_mut().session.send_response(
        pending.channel,
        SessionResponse::JoinDenied {
            message: "room join denied".to_string(),
        },
    );
    {
        let mut state = state.write().await;
        state
            .pending_peer_approvals
            .retain(|pending| pending.peer_id != peer_id);
    }
    Ok(format!("rejected {peer_id}"))
}

async fn apply_session_hello(
    state: &Arc<RwLock<DaemonStatus>>,
    peer_id: String,
    hello: SessionHello,
) {
    let mut state = state.write().await;
    let display_name = hello.display_name.clone();
    let (peer_id, peer_address) = {
        let peer = if let Some(index) = state
            .peers
            .iter()
            .position(|existing| existing.peer_id == peer_id)
        {
            &mut state.peers[index]
        } else {
            get_or_insert_peer(&mut state, peer_id, "<unknown>".to_string())
        };
        peer.display_name = hello.display_name.clone();
        peer.session = PeerSessionState::Active {
            room_name: hello.room_name,
            display_name: hello.display_name,
        };
        peer.transport = PeerTransportState::Connected;
        (peer.peer_id.clone(), peer.address.clone())
    };
    update_known_peer(
        &mut state,
        &peer_id,
        Some(peer_address),
        Some(display_name),
        true,
        true,
    );
}

fn should_surface_connected_peer(
    state: &DaemonStatus,
    peer_id: &str,
    had_pending_dial: bool,
) -> bool {
    had_pending_dial || should_surface_peer_metadata(state, peer_id)
}

fn should_surface_peer_metadata(state: &DaemonStatus, peer_id: &str) -> bool {
    state.peers.iter().any(|peer| peer.peer_id == peer_id)
        || state
            .network
            .known_peers
            .iter()
            .any(|peer| peer.peer_id == peer_id)
        || state
            .pending_peer_approvals
            .iter()
            .any(|pending| pending.peer_id == peer_id)
}

async fn record_webrtc_signal(
    state: &Arc<RwLock<DaemonStatus>>,
    peer_id: &PeerId,
    signal: &WebRtcSignal,
) {
    let mut state = state.write().await;
    state.network.transport_stage =
        "tcp+noise+yamux active; WebRTC signalling active; ICE transport pending".to_string();
    insert_unique_note(
        &mut state,
        format!(
            "WebRTC {} signal queued for peer {}",
            signal.kind(),
            peer_id
        ),
    );
}

async fn push_note(state: &Arc<RwLock<DaemonStatus>>, note: String) {
    let mut state = state.write().await;
    insert_unique_note(&mut state, note);
}

fn insert_unique_note(state: &mut DaemonStatus, note: String) {
    if state
        .notes
        .first()
        .map(|existing| existing == &note)
        .unwrap_or(false)
    {
        return;
    }

    state.notes.insert(0, note);
    state.notes.truncate(8);
}

fn insert_unique_addr(addresses: &mut Vec<String>, address: String) {
    if !addresses.contains(&address) {
        addresses.push(address);
    }
}

async fn dial_ranked_target(
    state: &Arc<RwLock<DaemonStatus>>,
    swarm: &mut libp2p::Swarm<VoicersBehaviour>,
    pending_dials: &mut HashMap<String, PendingDial>,
    ranked: media_transport::RankedDialTarget,
    multiaddr: Multiaddr,
) -> Result<String> {
    swarm
        .dial(multiaddr.clone())
        .with_context(|| format!("failed to dial {multiaddr}"))?;
    if let Some(peer_id) = ranked.peer_id {
        pending_dials.insert(
            peer_id,
            PendingDial {
                current_address: ranked.primary.clone(),
                fallback_addresses: ranked.fallbacks.clone(),
            },
        );
    }
    if ranked.fallbacks.is_empty() {
        push_note(state, format!("dialing {multiaddr}")).await;
    } else {
        push_note(
            state,
            format!(
                "dialing {multiaddr}; ranked fallbacks: {}",
                ranked.fallbacks.join(", ")
            ),
        )
        .await;
    }

    Ok(format!("dialing {multiaddr}"))
}

fn rendezvous_record_key(peer_id: &str) -> String {
    format!("{RENDEZVOUS_RECORD_PREFIX}{peer_id}")
}

fn room_rendezvous_record_key(room_name: &str) -> String {
    format!("{RENDEZVOUS_RECORD_PREFIX}room/{}", room_name.trim())
}

fn current_direct_call_invite(state: &DaemonStatus) -> Option<CompactInviteV1> {
    if state.local_peer_id == "<starting>" || state.local_peer_id.is_empty() {
        return None;
    }

    let addrs: Vec<String> = [
        &state.network.external_addrs,
        &state.network.observed_addrs,
        &state.network.listen_addrs,
    ]
    .into_iter()
    .flat_map(|addresses| addresses.iter())
    .filter_map(|address| shareable_address(address, &state.local_peer_id))
    .fold(Vec::new(), |mut acc, address| {
        if !acc.contains(&address) {
            acc.push(address);
        }
        acc
    });

    if addrs.is_empty() {
        None
    } else {
        Some(CompactInviteV1 {
            v: 1,
            kind: CompactInviteKind::DirectCall,
            peer_id: state.local_peer_id.clone(),
            addrs,
            invite_code: None,
            room_name: None,
            expires_at_ms: None,
        })
    }
}

fn current_rendezvous_records(state: &DaemonStatus) -> Vec<(String, CompactInviteV1)> {
    let Some(direct_call_invite) = current_direct_call_invite(state) else {
        return Vec::new();
    };

    let mut records = vec![(
        rendezvous_record_key(&direct_call_invite.peer_id),
        direct_call_invite.clone(),
    )];
    if let Some(room) = state.rooms.iter().find(|room| room.engaged) {
        if let Some(room_invite) = &room.current_invite {
            let room_name = room.name.trim();
            let invite_code = room_invite.invite_code.trim();
            let invite = CompactInviteV1 {
                v: 1,
                kind: CompactInviteKind::Room,
                peer_id: state.local_peer_id.clone(),
                addrs: direct_call_invite.addrs.clone(),
                invite_code: (!invite_code.is_empty()).then_some(invite_code.to_string()),
                room_name: (!room_name.is_empty()).then_some(room_name.to_string()),
                expires_at_ms: room_invite.expires_at_ms,
            };
            if !invite_code.is_empty() {
                records.push((room_rendezvous_record_key(invite_code), invite.clone()));
            }
            if !room_name.is_empty() && room_name != DEFAULT_ROOM_NAMESPACE {
                records.push((room_rendezvous_record_key(room_name), invite));
            }
        }
    }
    records
}

fn decode_rendezvous_record(payload: &[u8]) -> Option<CompactInviteV1> {
    let invite = serde_json::from_slice::<CompactInviteV1>(payload).ok()?;
    if invite.v != 1 || invite.peer_id.trim().is_empty() {
        return None;
    }
    Some(CompactInviteV1 {
        v: invite.v,
        kind: invite.kind,
        peer_id: invite.peer_id.trim().to_string(),
        addrs: invite
            .addrs
            .into_iter()
            .map(|addr| addr.trim().to_string())
            .filter(|addr| !addr.is_empty())
            .collect(),
        invite_code: invite
            .invite_code
            .map(|code| code.trim().to_string())
            .filter(|code| !code.is_empty()),
        room_name: invite
            .room_name
            .map(|room_name| room_name.trim().to_string())
            .filter(|room_name| !room_name.is_empty()),
        expires_at_ms: invite.expires_at_ms,
    })
}

async fn maybe_publish_rendezvous_record(
    state: &Arc<RwLock<DaemonStatus>>,
    swarm: &mut libp2p::Swarm<VoicersBehaviour>,
    connected_peers: &HashSet<PeerId>,
    dht_routable_peers: &HashSet<PeerId>,
    pending_rendezvous_publications: &mut HashMap<kad::QueryId, (String, Vec<u8>)>,
    published_rendezvous_records: &mut HashMap<String, Vec<u8>>,
) {
    if !has_active_dht_peer(connected_peers, dht_routable_peers) {
        return;
    }

    let state_snapshot = state.read().await;
    let records = current_rendezvous_records(&state_snapshot);
    drop(state_snapshot);

    for (key, invite) in records {
        let Ok(payload) = serde_json::to_vec(&invite) else {
            continue;
        };

        if published_rendezvous_records
            .get(&key)
            .map(|current| current == &payload)
            .unwrap_or(false)
        {
            continue;
        }

        if pending_rendezvous_publications
            .values()
            .any(|(pending_key, current)| pending_key == &key && current == &payload)
        {
            continue;
        }

        let record = kad::Record::new(key.clone().into_bytes(), payload.clone());
        match swarm.behaviour_mut().kad.put_record(record, kad::Quorum::One) {
            Ok(query_id) => {
                pending_rendezvous_publications.insert(query_id, (key, payload));
            }
            Err(error) => {
                push_note(
                    state,
                    format!("failed to queue rendezvous record publication: {error}"),
                )
                .await;
            }
        }
    }
}

fn has_active_dht_peer(
    connected_peers: &HashSet<PeerId>,
    dht_routable_peers: &HashSet<PeerId>,
) -> bool {
    connected_peers
        .iter()
        .any(|peer| dht_routable_peers.contains(peer))
}

async fn seed_rendezvous_invite(state: &Arc<RwLock<DaemonStatus>>, invite: &CompactInviteV1) {
    let mut state = state.write().await;
    state
        .network
        .ignored_peer_ids
        .retain(|id| id != &invite.peer_id);
    update_known_peer(&mut state, &invite.peer_id, None, None, false, false);
    for address in &invite.addrs {
        let normalized = shareable_address(address, &invite.peer_id)
            .unwrap_or_else(|| address.trim().to_string());
        if normalized.is_empty() {
            continue;
        }
        insert_unique_addr(&mut state.network.saved_peer_addrs, normalized.clone());
        if let Some(known_peer) = state
            .network
            .known_peers
            .iter_mut()
            .find(|peer| peer.peer_id == invite.peer_id)
        {
            insert_unique_addr(&mut known_peer.addresses, normalized.clone());
            known_peer.last_dial_addr = Some(normalized.clone());
        }
    }
}

fn record_path_score(state: &mut DaemonStatus, path: &str, peer_id: Option<String>, success: bool) {
    let index = state
        .network
        .path_scores
        .iter()
        .position(|score| score.path == path)
        .unwrap_or_else(|| {
            state.network.path_scores.push(PathScoreSummary {
                path: path.to_string(),
                successes: 0,
                failures: 0,
                last_peer_id: None,
            });
            state.network.path_scores.len() - 1
        });

    let score = &mut state.network.path_scores[index];
    if success {
        score.successes = score.successes.saturating_add(1);
    } else {
        score.failures = score.failures.saturating_add(1);
    }
    score.last_peer_id = peer_id;
}

fn remove_addr(addresses: &mut Vec<String>, address: &str) {
    addresses.retain(|candidate| candidate != address);
}

fn update_share_invite(state: &mut DaemonStatus) {
    state.network.direct_call_invite = best_direct_call_invite(state).or_else(|| {
        state
            .network
            .direct_call_invite
            .clone()
            .filter(|invite| !invite.is_empty() && shareable_address(invite, &state.local_peer_id).is_some())
    });

    let room_invite_update = state
        .rooms
        .iter()
        .find(|room| room.engaged)
        .and_then(|room| {
            room.current_invite
                .as_ref()
                .map(|invite| (room.name.clone(), best_room_invite(state, room.name.as_str(), invite)))
        });

    if let Some((room_name, share_invite)) = room_invite_update {
        if let Some(room) = state.rooms.iter_mut().find(|room| room.name == room_name) {
            if let Some(current_invite) = room.current_invite.as_mut() {
                current_invite.share_invite = share_invite;
            }
        }
    }
}

fn update_known_peer(
    state: &mut DaemonStatus,
    peer_id: &str,
    address: Option<String>,
    display_name: Option<String>,
    connected: bool,
    seen: bool,
) {
    if state
        .network
        .ignored_peer_ids
        .iter()
        .any(|id| id == peer_id)
    {
        return;
    }

    let normalized_addr = address.and_then(|addr| shareable_address(&addr, peer_id));

    let entry = if let Some(existing) = state
        .network
        .known_peers
        .iter_mut()
        .find(|peer| peer.peer_id == peer_id)
    {
        existing
    } else {
        state.network.known_peers.push(KnownPeerSummary {
            peer_id: peer_id.to_string(),
            display_name: display_name
                .clone()
                .unwrap_or_else(|| format!("peer {}", short_peer_id(peer_id))),
            addresses: Vec::new(),
            last_dial_addr: None,
            connected,
            pinned: false,
            seen,
            whitelisted: false,
            trusted_contact: false,
        });
        state
            .network
            .known_peers
            .last_mut()
            .expect("known peer inserted")
    };

    if let Some(name) = display_name {
        entry.display_name = name;
    }
    if let Some(addr) = normalized_addr {
        insert_unique_addr(&mut entry.addresses, addr.clone());
        entry.last_dial_addr = Some(addr);
    }
    entry.connected = connected;
    entry.seen |= seen;
    state.network.refresh_user_views();
}

fn best_direct_call_invite(state: &DaemonStatus) -> Option<String> {
    let local_peer_id = &state.local_peer_id;
    let network = &state.network;
    let mut addrs = Vec::new();

    collect_shareable_invite_addrs(&mut addrs, &network.external_addrs, local_peer_id);
    collect_shareable_invite_addrs(&mut addrs, &network.observed_addrs, local_peer_id);
    collect_shareable_invite_addrs(&mut addrs, &network.listen_addrs, local_peer_id);

    if addrs.is_empty() {
        None
    } else {
        Some(encode_peer_compact_invite(local_peer_id, &addrs, None, None))
    }
}

fn best_room_invite(
    state: &DaemonStatus,
    room_name: &str,
    room_invite: &voicers_core::RoomInviteSummary,
) -> Option<String> {
    let local_peer_id = &state.local_peer_id;
    let network = &state.network;
    let mut addrs = Vec::new();

    collect_shareable_invite_addrs(&mut addrs, &network.external_addrs, local_peer_id);
    collect_shareable_invite_addrs(&mut addrs, &network.observed_addrs, local_peer_id);
    collect_shareable_invite_addrs(&mut addrs, &network.listen_addrs, local_peer_id);

    if addrs.is_empty() || room_invite.invite_code.trim().is_empty() {
        None
    } else {
        Some(encode_room_compact_invite(
            local_peer_id,
            room_name,
            &addrs,
            &room_invite.invite_code,
            room_invite.expires_at_ms,
        ))
    }
}

fn collect_shareable_invite_addrs(
    dest: &mut Vec<String>,
    addresses: &[String],
    local_peer_id: &str,
) {
    for address in addresses {
        if dest.len() >= MAX_SHARE_INVITE_ADDRS {
            break;
        }

        let Some(address) = shareable_address(address, local_peer_id) else {
            continue;
        };

        if dest.contains(&address) {
            continue;
        }

        dest.push(address);
    }
}

fn shareable_address(address: &str, local_peer_id: &str) -> Option<String> {
    if local_peer_id == "<starting>" || local_peer_id.is_empty() {
        return None;
    }

    let multiaddr: Multiaddr = address.parse().ok()?;
    if !is_shareable_multiaddr(&multiaddr) {
        return None;
    }

    if address.contains("/p2p/") {
        Some(address.to_string())
    } else {
        Some(format!("{address}/p2p/{local_peer_id}"))
    }
}

fn is_shareable_multiaddr(address: &Multiaddr) -> bool {
    let allow_loopback = env::var_os("VOICERS_ALLOW_LOOPBACK_INVITES").is_some();
    if !is_supported_dial_addr(address) {
        return false;
    }

    for protocol in address.iter() {
        match protocol {
            Protocol::Ip4(ip) => {
                if ip.is_unspecified() || (ip.is_loopback() && !allow_loopback) {
                    return false;
                }
                return true;
            }
            Protocol::Ip6(ip) => {
                if ip.is_unspecified() || (ip.is_loopback() && !allow_loopback) {
                    return false;
                }
                return true;
            }
            Protocol::Dns(_) | Protocol::Dns4(_) | Protocol::Dns6(_) | Protocol::Dnsaddr(_) => {
                return true
            }
            _ => {}
        }
    }

    false
}

fn is_supported_dial_addr(address: &Multiaddr) -> bool {
    let mut has_host = false;
    let mut has_tcp = false;
    let mut has_udp = false;
    let mut has_quic = false;
    let mut has_websocket = false;

    for protocol in address.iter() {
        match protocol {
            Protocol::Ip4(_) | Protocol::Ip6(_) | Protocol::Dns(_) | Protocol::Dns4(_) | Protocol::Dns6(_) => {
                has_host = true;
            }
            Protocol::Tcp(_) => {
                has_tcp = true;
            }
            Protocol::Udp(_) => {
                has_udp = true;
            }
            Protocol::Quic | Protocol::QuicV1 => {
                has_quic = true;
            }
            Protocol::Ws(_) | Protocol::Wss(_) => {
                has_websocket = true;
            }
            Protocol::Dnsaddr(_)
            | Protocol::Http
            | Protocol::Https
            | Protocol::P2pWebRtcDirect
            | Protocol::P2pWebRtcStar
            | Protocol::WebRTCDirect
            | Protocol::P2pWebSocketStar
            | Protocol::Memory(_)
            | Protocol::Onion(_, _)
            | Protocol::Onion3(_)
            | Protocol::Sctp(_)
            | Protocol::Udt
            | Protocol::Unix(_)
            | Protocol::Utp
            | Protocol::WebTransport
            | Protocol::P2pStardust
            | Protocol::WebRTC => {
                return false;
            }
            _ => {}
        }
    }

    has_host && ((has_tcp && !has_quic) || (has_tcp && has_websocket) || (has_udp && has_quic))
}

fn is_relayed_addr(address: &Multiaddr) -> bool {
    address
        .iter()
        .any(|protocol| matches!(protocol, Protocol::P2pCircuit))
}

async fn persist_network_snapshot(
    state: &Arc<RwLock<DaemonStatus>>,
    persistence: &PersistenceHandle,
) {
    let snapshot = state.read().await.network.clone();
    let _ = persistence.save_network(&snapshot);
}

fn short_peer_id(peer_id: &str) -> &str {
    peer_id.get(0..12).unwrap_or(peer_id)
}

#[cfg(test)]
mod tests {
    use super::{
        best_direct_call_invite, is_shareable_multiaddr, is_supported_dial_addr, shareable_address,
        should_surface_connected_peer, MAX_SHARE_INVITE_ADDRS,
    };
    use libp2p::Multiaddr;
    use voicers_core::{
        parse_join_target, AudioBackend, AudioEngineStage, AudioSummary, DaemonStatus,
        JoinTarget, NetworkSummary, OutputStrategy, PeerSummary, PendingPeerApprovalSummary,
        SessionSummary,
    };

    #[test]
    fn loopback_addresses_are_not_shareable() {
        let loopback: Multiaddr = "/ip4/127.0.0.1/tcp/4001".parse().unwrap();
        assert!(!is_shareable_multiaddr(&loopback));
        assert!(shareable_address("/ip4/127.0.0.1/tcp/4001", "peer-id").is_none());
    }

    #[test]
    fn lan_addresses_remain_shareable() {
        let lan: Multiaddr = "/ip4/192.168.1.50/tcp/27015".parse().unwrap();
        assert!(is_shareable_multiaddr(&lan));
        assert_eq!(
            shareable_address("/ip4/192.168.1.50/tcp/27015", "peer-id").as_deref(),
            Some("/ip4/192.168.1.50/tcp/27015/p2p/peer-id")
        );
    }

    #[test]
    fn unsupported_and_supported_transports_are_classified_correctly() {
        let dnsaddr: Multiaddr =
            "/dnsaddr/bootstrap.libp2p.io/p2p/QmNnooDu7bfjPFoTZYxMNLWUQJyrVwtbZg5gBMjTezGAJN"
                .parse()
                .unwrap();
        let quic: Multiaddr = "/ip4/203.0.113.10/udp/4001/quic-v1".parse().unwrap();
        let wss: Multiaddr = "/dns4/example.net/tcp/443/wss".parse().unwrap();

        assert!(!is_supported_dial_addr(&dnsaddr));
        assert!(is_supported_dial_addr(&quic));
        assert!(is_supported_dial_addr(&wss));
        assert!(!is_shareable_multiaddr(&dnsaddr));
        assert!(is_shareable_multiaddr(&quic));
        assert!(is_shareable_multiaddr(&wss));
    }

    #[test]
    fn only_explicit_or_session_peers_are_surfaced_as_people() {
        let empty = test_status(Vec::new(), Vec::new(), Vec::new());
        assert!(!should_surface_connected_peer(
            &empty,
            "12D3KooWBootstrapPeer",
            false
        ));
        assert!(should_surface_connected_peer(
            &empty,
            "12D3KooWDialedPeer",
            true
        ));

        let known = test_status(
            Vec::new(),
            vec![voicers_core::KnownPeerSummary {
                peer_id: "12D3KooWFriend".to_string(),
                display_name: "Friend".to_string(),
                addresses: Vec::new(),
                last_dial_addr: None,
                connected: false,
                pinned: false,
                seen: true,
                whitelisted: false,
                trusted_contact: false,
            }],
            Vec::new(),
        );
        assert!(should_surface_connected_peer(
            &known,
            "12D3KooWFriend",
            false
        ));

        let pending = test_status(
            Vec::new(),
            Vec::new(),
            vec![PendingPeerApprovalSummary {
                peer_id: "12D3KooWPending".to_string(),
                display_name: "Pending".to_string(),
                address: "<unknown>".to_string(),
                room_name: Some("room".to_string()),
                known: false,
            }],
        );
        assert!(should_surface_connected_peer(
            &pending,
            "12D3KooWPending",
            false
        ));
    }

    #[test]
    fn direct_call_invite_limits_advertised_addresses() {
        let mut state = test_status(Vec::new(), Vec::new(), Vec::new());
        state.local_peer_id = "12D3KooWExamplePeer".to_string();
        state.network.external_addrs = vec![
            "/ip4/198.51.100.10/tcp/4001".to_string(),
            "/ip4/198.51.100.11/tcp/4002".to_string(),
            "/ip4/198.51.100.12/tcp/4003".to_string(),
        ];
        state.network.observed_addrs = vec![
            "/ip4/203.0.113.10/tcp/4101".to_string(),
            "/ip4/203.0.113.11/tcp/4102".to_string(),
        ];
        state.network.listen_addrs = vec![
            "/ip4/192.168.1.10/tcp/4010".to_string(),
            "/ip4/192.168.1.11/tcp/4011".to_string(),
        ];

        let invite = best_direct_call_invite(&state).expect("share invite should exist");
        match parse_join_target(&invite) {
            JoinTarget::Invite(invite) => {
                assert_eq!(invite.addrs.len(), MAX_SHARE_INVITE_ADDRS);
            }
            other => panic!("expected structured invite, got {other:?}"),
        }
    }

    fn test_status(
        peers: Vec<PeerSummary>,
        known_peers: Vec<voicers_core::KnownPeerSummary>,
        pending_peer_approvals: Vec<PendingPeerApprovalSummary>,
    ) -> DaemonStatus {
        DaemonStatus {
            daemon_version: "test".to_string(),
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
                transport_stage: String::new(),
                nat_status: String::new(),
                listen_addrs: Vec::new(),
                external_addrs: Vec::new(),
                observed_addrs: Vec::new(),
                stun_addrs: Vec::new(),
                selected_media_path: String::new(),
                webrtc_connection_state: String::new(),
                path_scores: Vec::new(),
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
                output_backend: String::new(),
                capture_device: None,
                available_capture_devices: Vec::new(),
                sample_rate_hz: None,
                engine: AudioEngineStage::Planned,
                frame_size_ms: None,
                codec: None,
                source: None,
                input_gain_percent: 100,
            },
            peers,
            pending_peer_approvals,
            notes: Vec::new(),
        }
    }
}
