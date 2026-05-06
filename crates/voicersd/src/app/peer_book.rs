use voicers_core::{
    CompactInviteV1, DaemonStatus, KnownPeerSummary, PeerSessionState, PeerSummary,
    PeerTransportState,
};

pub fn save_known_peer(state: &mut DaemonStatus, peer_id: &str) -> Result<String, String> {
    let live_peer = state
        .peers
        .iter()
        .find(|peer| peer.peer_id == peer_id)
        .cloned();
    state.network.ignored_peer_ids.retain(|id| id != peer_id);

    let display_name = if let Some(existing) = state
        .network
        .known_peers
        .iter_mut()
        .find(|peer| peer.peer_id == peer_id)
    {
        existing.pinned = true;
        if let Some(live_peer) = &live_peer {
            apply_live_peer_metadata(existing, live_peer);
        }
        existing.display_name.clone()
    } else if let Some(live_peer) = live_peer {
        state
            .network
            .known_peers
            .push(known_peer_from_live_peer(&live_peer));
        live_peer.display_name
    } else {
        return Err("peer not found".to_string());
    };

    state.network.refresh_user_views();
    Ok(display_name)
}

pub fn rename_known_peer(
    state: &mut DaemonStatus,
    peer_id: &str,
    display_name: &str,
) -> Result<(), String> {
    let mut renamed = false;

    if let Some(known_peer) = state
        .network
        .known_peers
        .iter_mut()
        .find(|peer| peer.peer_id == peer_id)
    {
        known_peer.display_name = display_name.to_string();
        known_peer.pinned = true;
        renamed = true;
    }

    if let Some(peer) = state.peers.iter_mut().find(|peer| peer.peer_id == peer_id) {
        peer.display_name = display_name.to_string();
        renamed = true;
    }

    if !renamed {
        return Err("known peer not found".to_string());
    }

    state.network.refresh_user_views();
    Ok(())
}

pub fn forget_known_peer(state: &mut DaemonStatus, peer_id: &str) -> Result<(), String> {
    let Some(known_peer) = state
        .network
        .known_peers
        .iter_mut()
        .find(|peer| peer.peer_id == peer_id)
    else {
        return Err("known peer not found".to_string());
    };

    known_peer.pinned = false;
    state.network.refresh_user_views();
    if !state
        .network
        .ignored_peer_ids
        .iter()
        .any(|id| id == peer_id)
    {
        state.network.ignored_peer_ids.push(peer_id.to_string());
    }
    Ok(())
}

pub fn set_trusted_contact(
    state: &mut DaemonStatus,
    peer_id: &str,
    trusted_contact: bool,
) -> Result<(), String> {
    if let Some(peer) = state
        .network
        .known_peers
        .iter_mut()
        .find(|peer| peer.peer_id == peer_id)
    {
        peer.trusted_contact = trusted_contact;
        state.network.refresh_user_views();
        Ok(())
    } else {
        Err("peer not found in known peers".to_string())
    }
}

pub fn seed_invite_hints(state: &mut DaemonStatus, invite: CompactInviteV1) {
    state
        .network
        .ignored_peer_ids
        .retain(|id| id != &invite.peer_id);

    let peer_index = state
        .network
        .known_peers
        .iter()
        .position(|peer| peer.peer_id == invite.peer_id)
        .unwrap_or_else(|| {
            state.network.known_peers.push(KnownPeerSummary {
                peer_id: invite.peer_id.clone(),
                display_name: format!(
                    "peer {}",
                    invite.peer_id.get(0..12).unwrap_or(&invite.peer_id)
                ),
                addresses: Vec::new(),
                last_dial_addr: None,
                connected: false,
                pinned: false,
                seen: false,
                whitelisted: false,
                trusted_contact: false,
            });
            state.network.known_peers.len() - 1
        });

    for address in invite.addrs {
        let address = address.trim().to_string();
        if address.is_empty() {
            continue;
        }
        if !state.network.saved_peer_addrs.contains(&address) {
            state.network.saved_peer_addrs.push(address.clone());
        }
        let known_peer = &mut state.network.known_peers[peer_index];
        if !known_peer.addresses.contains(&address) {
            known_peer.addresses.push(address.clone());
        }
        if known_peer.last_dial_addr.is_none() {
            known_peer.last_dial_addr = Some(address);
        }
    }

    state.network.refresh_user_views();
}

fn known_peer_from_live_peer(live_peer: &PeerSummary) -> KnownPeerSummary {
    KnownPeerSummary {
        peer_id: live_peer.peer_id.clone(),
        display_name: live_peer.display_name.clone(),
        addresses: if live_peer.address != "<unknown>" {
            vec![live_peer.address.clone()]
        } else {
            Vec::new()
        },
        last_dial_addr: (live_peer.address != "<unknown>").then_some(live_peer.address.clone()),
        connected: matches!(live_peer.transport, PeerTransportState::Connected),
        pinned: true,
        seen: matches!(live_peer.session, PeerSessionState::Active { .. }),
        whitelisted: false,
        trusted_contact: false,
    }
}

fn apply_live_peer_metadata(known_peer: &mut KnownPeerSummary, live_peer: &PeerSummary) {
    known_peer.display_name = live_peer.display_name.clone();
    known_peer.seen |= matches!(live_peer.session, PeerSessionState::Active { .. });
    known_peer.connected = matches!(live_peer.transport, PeerTransportState::Connected);

    if !live_peer.address.is_empty() && live_peer.address != "<unknown>" {
        if !known_peer.addresses.contains(&live_peer.address) {
            known_peer.addresses.push(live_peer.address.clone());
        }
        known_peer.last_dial_addr = Some(live_peer.address.clone());
    }
}
