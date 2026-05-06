use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use libp2p::request_response;
use tokio::sync::{mpsc, RwLock};
use voicers_core::{
    DaemonStatus, MediaStreamState, PeerMediaState, PeerSessionState, PeerSummary,
    PeerTransportState, SessionHello, SessionRequest, SessionResponse,
};

use crate::media::MediaHandle;

use super::media_transport::{self, select_media_path};
use super::{
    insert_unique_addr, push_note, update_known_peer, NetworkCommand, PendingInboundApproval,
    VoicersBehaviour,
};

pub(super) fn schedule_media_tick(command_tx: mpsc::Sender<NetworkCommand>, peer_id: String) {
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(20)).await;
        let _ = command_tx
            .send(NetworkCommand::SendMediaFrame { peer_id })
            .await;
    });
}

pub(super) async fn upsert_peer(
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

pub(super) fn get_or_insert_peer<'a>(
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

pub(super) async fn send_session_hello(
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

pub(super) async fn should_gate_peer_approval(
    state: &Arc<RwLock<DaemonStatus>>,
    peer_id: &str,
) -> bool {
    let state = state.read().await;
    !state
        .network
        .known_peers
        .iter()
        .find(|peer| peer.peer_id == peer_id)
        .map(|peer| peer.whitelisted)
        .unwrap_or(false)
}

pub(super) async fn is_peer_whitelisted(
    state: &Arc<RwLock<DaemonStatus>>,
    peer_id: &str,
) -> bool {
    let state = state.read().await;
    state
        .network
        .known_peers
        .iter()
        .find(|peer| peer.peer_id == peer_id)
        .map(|peer| peer.whitelisted)
        .unwrap_or(false)
}

pub(super) async fn queue_pending_peer_approval(
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
            state
                .pending_peer_approvals
                .push(voicers_core::PendingPeerApprovalSummary {
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

pub(super) async fn apply_approved_session_hello(
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
                .filter(|peer| peer.trusted_contact)
                .map(|peer| peer.peer_id.clone())
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

pub(super) async fn approve_pending_peer(
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

pub(super) async fn reject_pending_peer(
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

pub(super) async fn apply_session_hello(
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

pub(super) fn should_surface_connected_peer(
    state: &DaemonStatus,
    peer_id: &str,
    had_pending_dial: bool,
) -> bool {
    had_pending_dial || should_surface_peer_metadata(state, peer_id)
}

pub(super) fn should_surface_peer_metadata(state: &DaemonStatus, peer_id: &str) -> bool {
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

pub(super) fn should_surface_approved_peer(state: &DaemonStatus, peer_id: &str) -> bool {
    state.peers.iter().any(|peer| peer.peer_id == peer_id)
        || state
            .network
            .known_peers
            .iter()
            .any(|peer| peer.peer_id == peer_id)
}
