use std::{collections::HashSet, time::Duration};

use anyhow::{Context, Result};
use futures::StreamExt;
use libp2p::{
    identify,
    multiaddr::Protocol,
    noise, ping, request_response, swarm::NetworkBehaviour, swarm::SwarmEvent, tcp, upnp, yamux,
    Multiaddr, StreamProtocol, SwarmBuilder,
};
use tokio::sync::{mpsc, oneshot, RwLock};
use voicers_core::{
    DaemonStatus, KnownPeerSummary, MediaRequest, MediaResponse, MediaStreamState,
    PeerMediaState, PeerSessionState, PeerSummary, PeerTransportState, SessionHello,
    SessionRequest, SessionResponse,
};

use std::sync::Arc;

use crate::{media::MediaHandle, persist::PersistenceHandle};

const PROTOCOL_VERSION: &str = "/voicers/0.1.0";
const SESSION_PROTOCOL: StreamProtocol = StreamProtocol::new("/voicers/session/0.1.0");
const MEDIA_PROTOCOL: StreamProtocol = StreamProtocol::new("/voicers/media/0.1.0");

#[derive(Clone)]
pub struct NetworkHandle {
    command_tx: mpsc::Sender<NetworkCommand>,
}

impl NetworkHandle {
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
}

pub struct NetworkBootstrap {
    pub peer_id: String,
    pub handle: NetworkHandle,
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
    SendMediaFrame {
        peer_id: String,
    },
}

#[derive(NetworkBehaviour)]
struct VoicersBehaviour {
    identify: identify::Behaviour,
    ping: ping::Behaviour,
    upnp: upnp::tokio::Behaviour,
    session: request_response::cbor::Behaviour<SessionRequest, SessionResponse>,
    media: request_response::cbor::Behaviour<MediaRequest, MediaResponse>,
}

pub fn start(
    state: Arc<RwLock<DaemonStatus>>,
    listen_addr: &str,
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
        .with_behaviour(|key| {
            Ok::<_, Box<dyn std::error::Error + Send + Sync>>(VoicersBehaviour {
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

    let (command_tx, command_rx) = mpsc::channel(32);
    tokio::spawn(run_network_task(
        state,
        swarm,
        command_tx.clone(),
        command_rx,
        media,
        persistence,
    ));

    Ok(NetworkBootstrap {
        peer_id,
        handle: NetworkHandle { command_tx },
    })
}

async fn run_network_task(
    state: Arc<RwLock<DaemonStatus>>,
    mut swarm: libp2p::Swarm<VoicersBehaviour>,
    command_tx: mpsc::Sender<NetworkCommand>,
    mut command_rx: mpsc::Receiver<NetworkCommand>,
    media: MediaHandle,
    persistence: PersistenceHandle,
) {
    let mut active_media_flows = HashSet::new();

    loop {
        tokio::select! {
            Some(command) = command_rx.recv() => {
                handle_command(&state, &mut swarm, &command_tx, &mut active_media_flows, &media, command).await;
            }
            event = swarm.select_next_some() => {
                handle_swarm_event(&state, &mut swarm, &command_tx, &mut active_media_flows, &media, &persistence, event).await;
            }
        }
    }
}

async fn handle_command(
    state: &Arc<RwLock<DaemonStatus>>,
    swarm: &mut libp2p::Swarm<VoicersBehaviour>,
    command_tx: &mpsc::Sender<NetworkCommand>,
    active_media_flows: &mut HashSet<String>,
    media: &MediaHandle,
    command: NetworkCommand,
) {
    match command {
        NetworkCommand::Dial {
            address,
            response_tx,
        } => {
            let response = dial_peer(state, swarm, &address).await;
            let _ = response_tx.send(response);
        }
        NetworkCommand::BroadcastSessionHello { hello, response_tx } => {
            let response = broadcast_session_hello(swarm, hello).await;
            let _ = response_tx.send(response);
        }
        NetworkCommand::SendMediaFrame { peer_id } => {
            if active_media_flows.contains(&peer_id) {
                send_next_media_frame(state, swarm, media, &peer_id).await;
                schedule_media_tick(command_tx.clone(), peer_id);
            }
        }
    }
}

async fn dial_peer(
    state: &Arc<RwLock<DaemonStatus>>,
    swarm: &mut libp2p::Swarm<VoicersBehaviour>,
    address: &str,
) -> Result<String> {
    let multiaddr: Multiaddr = address
        .parse()
        .with_context(|| format!("invalid multiaddr: {address}"))?;

    swarm
        .dial(multiaddr.clone())
        .with_context(|| format!("failed to dial {multiaddr}"))?;
    push_note(state, format!("dialing {multiaddr}")).await;

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

async fn handle_swarm_event(
    state: &Arc<RwLock<DaemonStatus>>,
    swarm: &mut libp2p::Swarm<VoicersBehaviour>,
    command_tx: &mpsc::Sender<NetworkCommand>,
    active_media_flows: &mut HashSet<String>,
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
                state.network.transport_stage =
                    "tcp+noise+yamux active; direct dial ready; session handshake active; media transport still pending"
                        .to_string();
                insert_unique_note(&mut state, format!("listening on {rendered}"));
            }
            persist_network_snapshot(state, persistence).await;
        }
        SwarmEvent::ConnectionEstablished {
            peer_id, endpoint, ..
        } => {
            let address = endpoint.get_remote_address().to_string();
            let local_hello = {
                let state = state.read().await;
                SessionHello {
                    room_name: state.session.room_name.clone(),
                    display_name: state.session.display_name.clone(),
                }
            };

            let peer = upsert_peer(state, peer_id.to_string(), address).await;
            persist_network_snapshot(state, persistence).await;
            let _ = media.register_peer(peer.peer_id.clone()).await;
            push_note(
                state,
                format!("connection established with {}", peer.peer_id),
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
            push_note(
                state,
                format!(
                    "outgoing connection error{}: {error}",
                    peer_id
                        .map(|peer| format!(" for {peer}"))
                        .unwrap_or_default()
                ),
            )
            .await;
        }
        SwarmEvent::Behaviour(VoicersBehaviourEvent::Identify(identify::Event::Received {
            peer_id,
            info,
            ..
        })) => {
            let remote_addr = info
                .listen_addrs
                .first()
                .map(ToString::to_string)
                .unwrap_or_else(|| "<unknown>".to_string());
            let observed_addr = info.observed_addr.to_string();

            {
                let mut state = state.write().await;
                let identified_peer = {
                    let peer = get_or_insert_peer(&mut state, peer_id.to_string(), remote_addr);
                    peer.display_name = if info.agent_version.is_empty() {
                        format!("peer {}", short_peer_id(&peer.peer_id))
                    } else {
                        info.agent_version
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
        SwarmEvent::Behaviour(VoicersBehaviourEvent::Upnp(
            upnp::Event::ExpiredExternalAddr(address),
        )) => {
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
        SwarmEvent::Behaviour(VoicersBehaviourEvent::Upnp(
            upnp::Event::NonRoutableGateway,
        )) => {
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
                            "tcp+noise+yamux active; direct media transport active"
                                .to_string();
                    }
                    schedule_media_tick(command_tx.clone(), peer.to_string());
                    push_note(state, format!("media flow started for {peer}")).await;
                }
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

async fn send_next_media_frame(
    state: &Arc<RwLock<DaemonStatus>>,
    swarm: &mut libp2p::Swarm<VoicersBehaviour>,
    media: &MediaHandle,
    peer_id: &str,
) {
    if let Ok(peer) = peer_id.parse() {
        match media.build_next_audio_frame(peer_id.to_string()).await {
            Ok(frame) => {
                swarm
                    .behaviour_mut()
                    .media
                    .send_request(&peer, MediaRequest::Frame(frame));
            }
            Err(error) => {
                push_note(
                    state,
                    format!("failed to build audio frame for {peer_id}: {error}"),
                )
                .await;
            }
        }
    }
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
        insert_unique_addr(&mut state.network.saved_peer_addrs, peer_clone.address.clone());
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

fn remove_addr(addresses: &mut Vec<String>, address: &str) {
    addresses.retain(|candidate| candidate != address);
}

fn update_share_invite(state: &mut DaemonStatus) {
    state.network.share_invite =
        best_share_invite(&state.local_peer_id, &state.network).or_else(|| {
            state.network
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
    if state.network.ignored_peer_ids.iter().any(|id| id == peer_id) {
        return;
    }

    let normalized_addr = address.and_then(|addr| shareable_invite(&addr, peer_id));

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

fn best_share_invite(local_peer_id: &str, network: &voicers_core::NetworkSummary) -> Option<String> {
    [
        &network.external_addrs,
        &network.observed_addrs,
        &network.listen_addrs,
    ]
    .into_iter()
    .flat_map(|addresses| addresses.iter())
    .filter_map(|address| shareable_invite(address, local_peer_id))
    .next()
}

fn shareable_invite(address: &str, local_peer_id: &str) -> Option<String> {
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
            Protocol::Dns(_)
            | Protocol::Dns4(_)
            | Protocol::Dns6(_)
            | Protocol::Dnsaddr(_) => return true,
            _ => {}
        }
    }

    false
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
