use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use anyhow::Context;
use libp2p::{identify, kad, ping, relay, request_response, swarm::SwarmEvent, upnp, Multiaddr, PeerId};
use tokio::sync::{mpsc, RwLock};

use super::*;

pub(super) async fn handle_swarm_event(
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
            let dial_address = match &endpoint {
                libp2p::core::ConnectedPoint::Dialer { .. } => address.clone(),
                libp2p::core::ConnectedPoint::Listener { .. } => String::new(),
            };
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
                let peer = upsert_peer(state, peer_id_string.clone(), dial_address).await;
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
                if should_surface_approved_peer(&state, &peer_id.to_string()) {
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
                        push_note(state, "published rendezvous record to the DHT".to_string())
                            .await;
                    }
                }
                kad::QueryResult::PutRecord(Err(error)) => {
                    pending_rendezvous_publications.remove(&id);
                    push_note(
                        state,
                        format!("rendezvous record publication failed: {error}"),
                    )
                    .await;
                }
                kad::QueryResult::GetRecord(Ok(kad::GetRecordOk::FoundRecord(peer_record))) => {
                    if let Some(invite) = decode_rendezvous_record(&peer_record.record.value) {
                        if let Some(lookup) = pending_rendezvous_lookups.get_mut(&id) {
                            match lookup {
                                PendingRendezvousLookup::PeerId(requested_peer_id) => {
                                    if invite.peer_id == *requested_peer_id
                                        && !invite.addrs.is_empty()
                                    {
                                        seed_rendezvous_invite(state, &invite).await;
                                        let requested_peer_id = requested_peer_id.clone();
                                        pending_rendezvous_lookups.remove(&id);
                                        let ranked = {
                                            let state = state.read().await;
                                            resolve_ranked_dial_target(
                                                &state.network,
                                                &requested_peer_id,
                                            )
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
                                            resolve_ranked_dial_target(
                                                &state.network,
                                                &invite.peer_id,
                                            )
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
                kad::QueryResult::GetRecord(Ok(
                    kad::GetRecordOk::FinishedWithNoAdditionalRecord { .. },
                )) => {
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
                let peer_str = peer.to_string();
                let already_connected = {
                    let s = state.read().await;
                    s.peers.iter().any(|p| {
                        p.peer_id == peer_str
                            && matches!(p.transport, PeerTransportState::Connected)
                    })
                };
                if already_connected || !should_gate_peer_approval(state, &peer_str).await {
                    relay_offers.insert(peer_str.clone(), hello.trusted_contacts.clone());
                    apply_approved_session_hello(
                        state,
                        swarm,
                        command_tx,
                        active_media_flows,
                        media,
                        &peer_str,
                        hello.clone(),
                        channel,
                    )
                    .await;
                    persist_network_snapshot(state, persistence).await;
                } else {
                    queue_pending_peer_approval(
                        state,
                        pending_inbound_approvals,
                        &peer_str,
                        hello,
                        channel,
                    )
                    .await;
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
                                format!(
                                    "media engine failed for relayed frame from {peer}: {error}"
                                ),
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
                        let _ = swarm
                            .behaviour_mut()
                            .media
                            .send_response(channel, MediaResponse::RelayAck { destination, ack });
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
                response:
                    MediaResponse::RelayDenied {
                        destination,
                        reason,
                    },
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
