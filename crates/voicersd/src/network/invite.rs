use std::collections::{HashMap, HashSet};
use std::env;
use std::sync::Arc;

use libp2p::{kad, multiaddr::Protocol, Multiaddr, PeerId};
use tokio::sync::RwLock;
use voicers_core::{
    encode_peer_compact_invite, encode_room_compact_invite, CompactInviteKind, CompactInviteV1,
    DaemonStatus,
};

use super::{
    insert_unique_addr, is_supported_dial_addr, push_note, update_known_peer, VoicersBehaviour,
    DEFAULT_ROOM_NAMESPACE, MAX_SHARE_INVITE_ADDRS, RENDEZVOUS_RECORD_PREFIX,
};

pub(super) fn rendezvous_record_key(peer_id: &str) -> String {
    format!("{RENDEZVOUS_RECORD_PREFIX}{peer_id}")
}

pub(super) fn room_rendezvous_record_key(room_name: &str) -> String {
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

pub(super) fn decode_rendezvous_record(payload: &[u8]) -> Option<CompactInviteV1> {
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

pub(super) async fn maybe_publish_rendezvous_record(
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
        match swarm
            .behaviour_mut()
            .kad
            .put_record(record, kad::Quorum::One)
        {
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

pub(super) async fn seed_rendezvous_invite(
    state: &Arc<RwLock<DaemonStatus>>,
    invite: &CompactInviteV1,
) {
    let mut state = state.write().await;
    state
        .network
        .ignored_peer_ids
        .retain(|id| id != &invite.peer_id);
    update_known_peer(&mut state, &invite.peer_id, None, None, false, false);
    for address in &invite.addrs {
        let normalized =
            shareable_address(address, &invite.peer_id).unwrap_or_else(|| address.trim().to_string());
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

pub(crate) fn update_share_invite(state: &mut DaemonStatus) {
    state.network.direct_call_invite = best_direct_call_invite(state).or_else(|| {
        state.network.direct_call_invite.clone().filter(|invite| {
            !invite.is_empty() && shareable_address(invite, &state.local_peer_id).is_some()
        })
    });

    let room_invite_update = state
        .rooms
        .iter()
        .find(|room| room.engaged)
        .and_then(|room| {
            room.current_invite.as_ref().map(|invite| {
                (
                    room.name.clone(),
                    best_room_invite(state, room.name.as_str(), invite),
                )
            })
        });

    if let Some((room_name, share_invite)) = room_invite_update {
        if let Some(room) = state.rooms.iter_mut().find(|room| room.name == room_name) {
            if let Some(current_invite) = room.current_invite.as_mut() {
                current_invite.share_invite = share_invite;
            }
        }
    }
}

pub(super) fn best_direct_call_invite(state: &DaemonStatus) -> Option<String> {
    let local_peer_id = &state.local_peer_id;
    let network = &state.network;
    let mut addrs = Vec::new();

    collect_shareable_invite_addrs(&mut addrs, &network.external_addrs, local_peer_id);
    collect_shareable_invite_addrs(&mut addrs, &network.observed_addrs, local_peer_id);
    collect_shareable_invite_addrs(&mut addrs, &network.listen_addrs, local_peer_id);

    if addrs.is_empty() {
        None
    } else {
        Some(encode_peer_compact_invite(
            local_peer_id,
            &addrs,
            None,
            None,
        ))
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

pub(super) fn shareable_address(address: &str, local_peer_id: &str) -> Option<String> {
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

pub(super) fn is_shareable_multiaddr(address: &Multiaddr) -> bool {
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
                return true;
            }
            _ => {}
        }
    }

    false
}
