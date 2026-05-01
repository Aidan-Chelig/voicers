#[path = "network/media_transport.rs"]
mod media_transport;
#[path = "network/stun.rs"]
mod stun;

use std::{
    collections::{HashMap, HashSet},
    time::Duration,
};

use anyhow::{Context, Result};
use futures::StreamExt;
use libp2p::{
    dcutr, identify, kad, kad::store::MemoryStore, multiaddr::Protocol, noise, ping, relay,
    request_response, swarm::NetworkBehaviour, swarm::SwarmEvent, tcp, upnp, yamux, Multiaddr,
    PeerId, StreamProtocol, SwarmBuilder,
};
use tokio::sync::{mpsc, oneshot, RwLock};
use voicers_core::{
    encode_compact_invite, DaemonStatus, KnownPeerSummary, MediaRequest, MediaResponse,
    MediaStreamState, PathScoreSummary, PeerMediaState, PeerSessionState, PeerSummary,
    PeerTransportState, SessionHello, SessionRequest, SessionResponse, WebRtcSignal,
};

use std::sync::Arc;

use self::media_transport::{
    address_path_name, resolve_ranked_dial_target, select_media_path, send_frame_libp2p,
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
const DEFAULT_BOOTSTRAP_ADDRS: &[&str] = &[
    "/dnsaddr/bootstrap.libp2p.io/p2p/QmNnooDu7bfjPFoTZYxMNLWUQJyrVwtbZg5gBMjTezGAJN",
    "/dnsaddr/bootstrap.libp2p.io/p2p/QmQCU2EcMqAqQPR2i9bChDtGNJchTbq5TbXJJ16u19uLTa",
    "/dnsaddr/bootstrap.libp2p.io/p2p/QmbLHAnMoJPWSCR5Zhtx6BHJX9KiKNN6tpvbUcqanj75Nb",
    "/dnsaddr/bootstrap.libp2p.io/p2p/QmcZf59bWwK5XFi76CZX8cbJ4BhTzzA3gU1ZjYZcYW3dwt",
    "/ip4/104.131.131.82/tcp/4001/p2p/QmaCpDMGvV2BGHeYERUEnRQAwe3N8SzbUtfsmvsqQLuvuJ",
];
const DEFAULT_STUN_SERVERS: &[&str] = &[
    "stun.l.google.com:19302",
    "stun1.l.google.com:19302",
    "stun.cloudflare.com:3478",
];
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
    SendMediaFrame {
        peer_id: String,
    },
}

#[derive(NetworkBehaviour)]
struct VoicersBehaviour {
    relay: relay::client::Behaviour,
    dcutr: dcutr::Behaviour,
    kad: kad::Behaviour<MemoryStore>,
    identify: identify::Behaviour,
    ping: ping::Behaviour,
    upnp: upnp::tokio::Behaviour,
    session: request_response::cbor::Behaviour<SessionRequest, SessionResponse>,
    media: request_response::cbor::Behaviour<MediaRequest, MediaResponse>,
}

pub fn start(
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
        .with_dns()?
        .with_relay_client(noise::Config::new, yamux::Config::default)?
        .with_behaviour(|key, relay| {
            let local_peer_id = key.public().to_peer_id();
            let store = MemoryStore::new(local_peer_id);
            Ok::<_, Box<dyn std::error::Error + Send + Sync>>(VoicersBehaviour {
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
    let mut pending_dials = HashMap::new();

    loop {
        tokio::select! {
            Some(command) = command_rx.recv() => {
                handle_command(
                    &state,
                    &mut swarm,
                    &command_tx,
                    &mut active_media_flows,
                    &mut pending_dials,
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
                    &mut attempted_relays,
                    &mut pending_dials,
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
    let mut pending_dials = HashMap::new();

    loop {
        tokio::select! {
            Some(command) = command_rx.recv() => {
                handle_command(
                    &state,
                    &mut swarm,
                    &command_tx,
                    &mut active_media_flows,
                    &mut pending_dials,
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
                    &mut attempted_relays,
                    &mut pending_dials,
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
    pending_dials: &mut HashMap<String, PendingDial>,
    #[cfg(feature = "webrtc-transport")] webrtc: &Option<WebRtcHandle>,
    media: &MediaHandle,
    command: NetworkCommand,
) {
    match command {
        NetworkCommand::Dial {
            address,
            response_tx,
        } => {
            let response = dial_peer(state, swarm, pending_dials, &address).await;
            let _ = response_tx.send(response);
        }
        NetworkCommand::BroadcastSessionHello { hello, response_tx } => {
            let response = broadcast_session_hello(swarm, hello).await;
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
        NetworkCommand::SendMediaFrame { peer_id } => {
            if active_media_flows.contains(&peer_id) {
                #[cfg(feature = "webrtc-transport")]
                send_next_media_frame_webrtc_first(state, swarm, webrtc, media, &peer_id).await;
                #[cfg(not(feature = "webrtc-transport"))]
                send_next_media_frame(state, swarm, media, &peer_id).await;
                schedule_media_tick(command_tx.clone(), peer_id);
            }
        }
    }
}

async fn dial_peer(
    state: &Arc<RwLock<DaemonStatus>>,
    swarm: &mut libp2p::Swarm<VoicersBehaviour>,
    pending_dials: &mut HashMap<String, PendingDial>,
    address: &str,
) -> Result<String> {
    let ranked = {
        let state = state.read().await;
        resolve_ranked_dial_target(&state.network, address)
    };

    let multiaddr: Multiaddr = ranked
        .primary
        .parse()
        .with_context(|| format!("invalid multiaddr: {}", ranked.primary))?;

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
    attempted_relays: &mut HashSet<PeerId>,
    pending_dials: &mut HashMap<String, PendingDial>,
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
        }
        SwarmEvent::ExternalAddrConfirmed { address } => {
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
        SwarmEvent::ExternalAddrExpired { address } => {
            let mut state = state.write().await;
            let rendered = address.to_string();
            remove_addr(&mut state.network.external_addrs, &rendered);
            remove_addr(&mut state.network.observed_addrs, &rendered);
            update_share_invite(&mut state);
            insert_unique_note(&mut state, format!("external address expired {rendered}"));
        }
        SwarmEvent::ConnectionEstablished {
            peer_id, endpoint, ..
        } => {
            pending_dials.remove(&peer_id.to_string());
            let address = endpoint.get_remote_address().to_string();
            let local_hello = {
                let state = state.read().await;
                SessionHello {
                    room_name: state.session.room_name.clone(),
                    display_name: state.session.display_name.clone(),
                }
            };

            let peer = upsert_peer(state, peer_id.to_string(), address).await;
            {
                let mut state = state.write().await;
                let path = if endpoint.is_relayed() {
                    "libp2p-relay"
                } else {
                    "libp2p-direct"
                };
                record_path_score(&mut state, path, Some(peer.peer_id.clone()), true);
            }
            persist_network_snapshot(state, persistence).await;
            let _ = media.register_peer(peer.peer_id.clone()).await;
            push_note(
                state,
                format!(
                    "{} connection established with {}",
                    if endpoint.is_relayed() {
                        "relayed"
                    } else {
                        "direct"
                    },
                    peer.peer_id
                ),
            )
            .await;
            send_session_hello(swarm, &peer.peer_id, local_hello).await;
        }
        SwarmEvent::ConnectionClosed {
            peer_id,
            num_established,
            ..
        } => {
            {
                let mut state = state.write().await;
                if let Some(peer) = state
                    .peers
                    .iter_mut()
                    .find(|existing| existing.peer_id == peer_id.to_string())
                {
                    peer.transport = if num_established == 0 {
                        PeerTransportState::Disconnected
                    } else {
                        PeerTransportState::Connected
                    };
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
                swarm
                    .behaviour_mut()
                    .kad
                    .add_address(&peer_id, address.clone());
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
                let identified_peer = {
                    let peer = get_or_insert_peer(&mut state, peer_id.to_string(), remote_addr);
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
                );
                if !observed_addr.is_empty() {
                    insert_unique_addr(&mut state.network.observed_addrs, observed_addr.clone());
                    state.network.nat_status = "observed by remote peers".to_string();
                }
                update_share_invite(&mut state);
                insert_unique_note(&mut state, format!("identified peer {}", identified_peer.0));
            }
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
                if is_new_peer {
                    push_note(state, format!("DHT discovered peer {peer}")).await;
                }
            }
            kad::Event::OutboundQueryProgressed { result, .. } => match result {
                kad::QueryResult::Bootstrap(Ok(ok)) => {
                    let mut state = state.write().await;
                    state.network.transport_stage =
                        "tcp+relay+dcutr+dht active; bootstrap in progress".to_string();
                    insert_unique_note(
                        &mut state,
                        format!(
                            "DHT bootstrap query reached {}; {} remaining",
                            ok.peer, ok.num_remaining
                        ),
                    );
                }
                kad::QueryResult::Bootstrap(Err(error)) => {
                    push_note(state, format!("DHT bootstrap failed: {error}")).await;
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
                apply_session_hello(state, peer.to_string(), hello.clone()).await;
                persist_network_snapshot(state, persistence).await;
                let _ = media.register_peer(peer.to_string()).await;

                let response = {
                    let state = state.read().await;
                    SessionResponse::HelloAck(SessionHello {
                        room_name: state.session.room_name.clone(),
                        display_name: state.session.display_name.clone(),
                    })
                };

                if let Err(response) = swarm
                    .behaviour_mut()
                    .session
                    .send_response(channel, response)
                {
                    push_note(
                        state,
                        format!("failed to send session response to {peer}: {response:?}"),
                    )
                    .await;
                } else {
                    push_note(state, format!("session hello received from {peer}")).await;
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
    );
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
    state.network.share_invite =
        best_share_invite(&state.local_peer_id, &state.network).or_else(|| {
            state
                .network
                .share_invite
                .clone()
                .filter(|invite| !invite.is_empty())
        });
}

fn update_known_peer(
    state: &mut DaemonStatus,
    peer_id: &str,
    address: Option<String>,
    display_name: Option<String>,
    connected: bool,
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
}

fn best_share_invite(
    local_peer_id: &str,
    network: &voicers_core::NetworkSummary,
) -> Option<String> {
    [
        &network.external_addrs,
        &network.observed_addrs,
        &network.listen_addrs,
    ]
    .into_iter()
    .flat_map(|addresses| addresses.iter())
    .filter_map(|address| compact_share_invite(address, local_peer_id))
    .next()
}

fn compact_share_invite(address: &str, local_peer_id: &str) -> Option<String> {
    shareable_address(address, local_peer_id).map(|address| encode_compact_invite(&address))
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
    for protocol in address.iter() {
        match protocol {
            Protocol::Ip4(ip) => {
                if ip.is_unspecified() {
                    return false;
                }
                return true;
            }
            Protocol::Ip6(ip) => {
                if ip.is_unspecified() {
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
