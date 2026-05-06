#[path = "network/media_transport.rs"]
mod media_transport;
#[path = "network/invite.rs"]
mod invite;
#[path = "network/session.rs"]
mod session;
#[path = "network/swarm_events.rs"]
mod swarm_events;
#[path = "network/stun.rs"]
mod stun;

use std::{
    collections::{HashMap, HashSet},
    time::Duration,
};

use anyhow::{Context, Result};
use futures::StreamExt;
use libp2p::{
    connection_limits, dcutr, identify, kad, kad::store::MemoryStore, multiaddr::Protocol, noise,
    ping, relay, request_response, swarm::NetworkBehaviour, tcp, tls, upnp, yamux, Multiaddr,
    PeerId, StreamProtocol, SwarmBuilder,
};
use tokio::sync::{mpsc, oneshot, RwLock};
use voicers_core::{
    DaemonStatus, KnownPeerSummary, MediaAck, MediaRequest, MediaResponse, MediaStreamState,
    PathScoreSummary, PeerSessionState, PeerTransportState, SessionHello, SessionRequest,
    SessionResponse, WebRtcSignal,
};

use std::sync::Arc;

use self::media_transport::{
    address_path_name, resolve_ranked_dial_target, resolve_route, select_media_path,
    send_frame_libp2p, send_frame_relayed_libp2p, Route,
};
use self::invite::{
    decode_rendezvous_record, is_shareable_multiaddr, maybe_publish_rendezvous_record,
    rendezvous_record_key, room_rendezvous_record_key, seed_rendezvous_invite, shareable_address,
};
use self::session::{
    approve_pending_peer, apply_approved_session_hello, apply_session_hello, get_or_insert_peer,
    is_peer_whitelisted, queue_pending_peer_approval, reject_pending_peer, schedule_media_tick,
    send_session_hello, should_gate_peer_approval, should_surface_approved_peer,
    should_surface_connected_peer, upsert_peer,
};
pub(crate) use self::invite::update_share_invite;
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
const SESSION_REQUEST_TIMEOUT_SECS: u64 = 5 * 60;
const RENDEZVOUS_RECORD_PREFIX: &str = "/voicers/rendezvous/v1/";
const DEFAULT_ROOM_NAMESPACE: &str = "main";
const DEFAULT_BOOTSTRAP_ADDRS: &[&str] =
    &["/ip4/104.131.131.82/tcp/4001/p2p/QmaCpDMGvV2BGHeYERUEnRQAwe3N8SzbUtfsmvsqQLuvuJ"];
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
            .send(NetworkCommand::ApprovePendingPeer {
                peer_id,
                response_tx,
            })
            .await
            .context("failed to send approve-peer command to network task")?;

        response_rx
            .await
            .context("network task dropped approve-peer response")?
    }

    pub async fn reject_pending_peer(&self, peer_id: String) -> Result<String> {
        let (response_tx, response_rx) = oneshot::channel();
        self.command_tx
            .send(NetworkCommand::RejectPendingPeer {
                peer_id,
                response_tx,
            })
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
    display_name: &str,
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
                        .with_max_established_outgoing(Some(MAX_ESTABLISHED_OUTGOING_CONNECTIONS))
                        .with_max_established_incoming(Some(MAX_ESTABLISHED_INCOMING_CONNECTIONS))
                        .with_max_pending_outgoing(Some(MAX_PENDING_OUTGOING_CONNECTIONS))
                        .with_max_pending_incoming(Some(MAX_PENDING_INCOMING_CONNECTIONS)),
                ),
                relay,
                dcutr: dcutr::Behaviour::new(local_peer_id),
                kad: kad::Behaviour::new(local_peer_id, store),
                identify: identify::Behaviour::new(
                    identify::Config::new(PROTOCOL_VERSION.to_string(), key.public())
                        .with_agent_version(display_name.to_string()),
                ),
                ping: ping::Behaviour::new(
                    ping::Config::new().with_interval(Duration::from_secs(5)),
                ),
                upnp: upnp::tokio::Behaviour::default(),
                session: request_response::cbor::Behaviour::new(
                    [(SESSION_PROTOCOL, request_response::ProtocolSupport::Full)],
                    request_response::Config::default()
                        .with_request_timeout(Duration::from_secs(SESSION_REQUEST_TIMEOUT_SECS)),
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
                swarm_events::handle_swarm_event(
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
                swarm_events::handle_swarm_event(
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
        NetworkCommand::ApprovePendingPeer {
            peer_id,
            response_tx,
        } => {
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
        NetworkCommand::RejectPendingPeer {
            peer_id,
            response_tx,
        } => {
            let response =
                reject_pending_peer(state, swarm, pending_inbound_approvals, &peer_id).await;
            let _ = response_tx.send(response);
        }
        NetworkCommand::SendMediaFrame { peer_id } => {
            if active_media_flows.contains(&peer_id) {
                let route = {
                    let s = state.read().await;
                    resolve_route(&s, relay_offers, &peer_id)
                };
                match route {
                    Route::Direct => {
                        #[cfg(feature = "webrtc-transport")]
                        send_next_media_frame_webrtc_first(state, swarm, webrtc, media, &peer_id)
                            .await;
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
                            push_note(
                                state,
                                format!(
                                    "self-loop guard: refusing relayed frame to self for {peer_id}"
                                ),
                            )
                            .await;
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
            pending_rendezvous_lookups
                .insert(query_id, PendingRendezvousLookup::PeerId(peer_id.clone()));
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

fn is_supported_dial_addr(address: &Multiaddr) -> bool {
    let mut has_host = false;
    let mut has_tcp = false;
    let mut has_udp = false;
    let mut has_quic = false;
    let mut has_websocket = false;

    for protocol in address.iter() {
        match protocol {
            Protocol::Ip4(_)
            | Protocol::Ip6(_)
            | Protocol::Dns(_)
            | Protocol::Dns4(_)
            | Protocol::Dns6(_) => {
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
    use super::invite::{best_direct_call_invite, is_shareable_multiaddr, shareable_address};
    use super::{is_supported_dial_addr, should_surface_connected_peer, MAX_SHARE_INVITE_ADDRS};
    use libp2p::Multiaddr;
    use voicers_core::{
        parse_join_target, AudioBackend, AudioEngineStage, AudioSummary, DaemonStatus, JoinTarget,
        NetworkSummary, OutputStrategy, PeerSummary, PendingPeerApprovalSummary, SessionSummary,
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
