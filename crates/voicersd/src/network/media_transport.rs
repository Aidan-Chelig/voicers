use libp2p::{multiaddr::Protocol, Multiaddr};
use tokio::sync::RwLock;
use voicers_core::{DaemonStatus, KnownPeerSummary, NetworkSummary};

use std::collections::HashMap;
use std::sync::Arc;

use crate::media::MediaHandle;
#[cfg(feature = "webrtc-transport")]
use crate::webrtc_transport::{WebRtcConnectionStateUpdate, WebRtcHandle};

#[cfg(feature = "webrtc-transport")]
use crate::persist::PersistenceHandle;

#[cfg(feature = "webrtc-transport")]
use super::IncomingWebRtcMediaFrame;
#[cfg(feature = "webrtc-transport")]
use super::{insert_unique_note, persist_network_snapshot, record_path_score};
use super::{push_note, VoicersBehaviour};

pub(super) const LIBP2P_MEDIA_PATH: &str = "libp2p-request-response";
pub(super) const LIBP2P_DIRECT_PATH: &str = "libp2p-direct";
pub(super) const LIBP2P_RELAY_PATH: &str = "libp2p-relay";
#[cfg(feature = "webrtc-transport")]
pub(super) const WEBRTC_MEDIA_PATH: &str = "webrtc-data-channel";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum MediaPath {
    Libp2pRequestResponse,
    #[cfg(feature = "webrtc-transport")]
    WebRtcDataChannel,
}

impl MediaPath {
    pub(super) fn as_str(self) -> &'static str {
        match self {
            Self::Libp2pRequestResponse => LIBP2P_MEDIA_PATH,
            #[cfg(feature = "webrtc-transport")]
            Self::WebRtcDataChannel => WEBRTC_MEDIA_PATH,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct RankedDialTarget {
    pub peer_id: Option<String>,
    pub primary: String,
    pub fallbacks: Vec<String>,
}

#[cfg(feature = "webrtc-transport")]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum WebRtcPathState {
    Disabled,
    Connected,
    NotConnected,
}

pub(super) enum Route {
    Direct,
    Relayed {
        via: libp2p::PeerId,
        destination: libp2p::PeerId,
    },
    Unreachable,
}

pub(super) fn resolve_route(
    state: &DaemonStatus,
    relay_offers: &HashMap<String, Vec<String>>,
    target: &str,
) -> Route {
    let known = state
        .network
        .known_peers
        .iter()
        .find(|p| p.peer_id == target);

    if let Some(peer) = known {
        if peer.connected && target.parse::<libp2p::PeerId>().is_ok() {
            return Route::Direct;
        }
    }

    let mut candidates: Vec<(&KnownPeerSummary, i64)> = state
        .network
        .known_peers
        .iter()
        .filter(|c| {
            c.trusted_contact
                && c.connected
                && relay_offers
                    .get(&c.peer_id)
                    .map_or(false, |list| list.iter().any(|id| id == target))
        })
        .map(|c| {
            let rtt_score = state
                .network
                .path_scores
                .iter()
                .find(|s| s.last_peer_id.as_deref() == Some(&c.peer_id))
                .map(|s| s.successes as i64 - s.failures as i64)
                .unwrap_or(0);
            (c, rtt_score)
        })
        .collect();

    if candidates.is_empty() {
        return Route::Unreachable;
    }

    candidates.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.peer_id.cmp(&b.0.peer_id)));

    let via_str = &candidates[0].0.peer_id;
    if let (Ok(via), Ok(dest)) = (
        via_str.parse::<libp2p::PeerId>(),
        target.parse::<libp2p::PeerId>(),
    ) {
        Route::Relayed {
            via,
            destination: dest,
        }
    } else {
        Route::Unreachable
    }
}

pub(super) async fn send_frame_relayed_libp2p(
    state: &Arc<RwLock<DaemonStatus>>,
    swarm: &mut libp2p::Swarm<VoicersBehaviour>,
    media: &MediaHandle,
    via: libp2p::PeerId,
    destination: libp2p::PeerId,
) {
    let destination_str = destination.to_string();
    match media.build_next_audio_frame(destination_str.clone()).await {
        Ok(frame) => {
            swarm.behaviour_mut().media.send_request(
                &via,
                voicers_core::MediaRequest::RelayedFrame {
                    destination: destination_str,
                    frame,
                },
            );
        }
        Err(error) => {
            push_note(
                state,
                format!("failed to build audio frame for {destination_str}: {error}"),
            )
            .await;
        }
    }
}

pub(super) async fn send_frame_libp2p(
    state: &Arc<RwLock<DaemonStatus>>,
    swarm: &mut libp2p::Swarm<VoicersBehaviour>,
    media: &MediaHandle,
    peer_id: &str,
) {
    let Ok(peer) = peer_id.parse() else {
        return;
    };

    match media.build_next_audio_frame(peer_id.to_string()).await {
        Ok(frame) => {
            select_media_path(state, MediaPath::Libp2pRequestResponse).await;
            swarm
                .behaviour_mut()
                .media
                .send_request(&peer, voicers_core::MediaRequest::Frame(frame));
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

#[cfg(feature = "webrtc-transport")]
pub(super) async fn send_frame_with_fallback(
    state: &Arc<RwLock<DaemonStatus>>,
    swarm: &mut libp2p::Swarm<VoicersBehaviour>,
    webrtc: Option<&WebRtcHandle>,
    media: &MediaHandle,
    peer_id: &str,
) {
    let Ok(peer) = peer_id.parse() else {
        return;
    };

    match media.build_next_audio_frame(peer_id.to_string()).await {
        Ok(frame) => {
            if prefers_webrtc(state).await {
                if let Some(webrtc) = webrtc {
                    if webrtc
                        .send_media_frame(peer_id.to_string(), frame.clone())
                        .await
                        .is_ok()
                    {
                        set_active_send_path(state, MediaPath::WebRtcDataChannel).await;
                        return;
                    }
                }
            }

            select_media_path(state, MediaPath::Libp2pRequestResponse).await;
            swarm
                .behaviour_mut()
                .media
                .send_request(&peer, voicers_core::MediaRequest::Frame(frame));
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

#[cfg(feature = "webrtc-transport")]
pub(super) async fn handle_incoming_webrtc_frame(
    state: &Arc<RwLock<DaemonStatus>>,
    media: &MediaHandle,
    incoming: IncomingWebRtcMediaFrame,
) {
    match media
        .handle_incoming_frame(incoming.peer_id.clone(), incoming.frame)
        .await
    {
        Ok(_) => {
            let mut state = state.write().await;
            state.network.transport_stage =
                "webrtc data-channel media active; receiving remote Opus frames".to_string();
            state.network.selected_media_path = WEBRTC_MEDIA_PATH.to_string();
        }
        Err(error) => {
            push_note(
                state,
                format!(
                    "WebRTC media handling failed for {}: {error}",
                    incoming.peer_id
                ),
            )
            .await;
        }
    }
}

#[cfg(feature = "webrtc-transport")]
pub(super) async fn handle_webrtc_connection_state(
    state: &Arc<RwLock<DaemonStatus>>,
    persistence: &PersistenceHandle,
    update: WebRtcConnectionStateUpdate,
) {
    {
        let mut state = state.write().await;
        state.network.webrtc_connection_state = format!("{} {}", update.peer_id, update.state);
        match update.state.as_str() {
            "connected" => {
                state.network.selected_media_path = WEBRTC_MEDIA_PATH.to_string();
                record_path_score(
                    &mut state,
                    WEBRTC_MEDIA_PATH,
                    Some(update.peer_id.clone()),
                    true,
                );
            }
            "failed" | "closed" => {
                record_path_score(
                    &mut state,
                    WEBRTC_MEDIA_PATH,
                    Some(update.peer_id.clone()),
                    false,
                );
                if state.network.selected_media_path == WEBRTC_MEDIA_PATH {
                    state.network.selected_media_path = LIBP2P_MEDIA_PATH.to_string();
                }
            }
            _ => {}
        }
        insert_unique_note(
            &mut state,
            format!(
                "WebRTC connection with {} is {}",
                update.peer_id, update.state
            ),
        );
    }
    persist_network_snapshot(state, persistence).await;
}

pub(super) async fn select_media_path(state: &Arc<RwLock<DaemonStatus>>, path: MediaPath) {
    let mut state = state.write().await;
    state.network.selected_media_path = path.as_str().to_string();
}

pub(super) fn resolve_ranked_dial_target(
    network: &NetworkSummary,
    requested: &str,
) -> RankedDialTarget {
    let requested = requested.trim();

    if let Some(known_peer) = network
        .known_peers
        .iter()
        .find(|peer| peer.peer_id == requested)
    {
        let ranked = rank_known_peer_addresses(network, known_peer, None);
        return RankedDialTarget {
            peer_id: Some(known_peer.peer_id.clone()),
            primary: ranked
                .first()
                .cloned()
                .unwrap_or_else(|| requested.to_string()),
            fallbacks: ranked.into_iter().skip(1).collect(),
        };
    }

    if let Ok(multiaddr) = requested.parse::<Multiaddr>() {
        let requested_peer_id = multiaddr_peer_id(&multiaddr).map(|peer_id| peer_id.to_string());
        if let Some(peer_id) = &requested_peer_id {
            if let Some(known_peer) = network
                .known_peers
                .iter()
                .find(|peer| &peer.peer_id == peer_id)
            {
                let ranked = rank_known_peer_addresses(network, known_peer, Some(requested));
                return RankedDialTarget {
                    peer_id: Some(peer_id.clone()),
                    primary: ranked
                        .first()
                        .cloned()
                        .unwrap_or_else(|| requested.to_string()),
                    fallbacks: ranked.into_iter().skip(1).collect(),
                };
            }
            // Peer not yet known — still track peer_id so the connection is surfaced.
            return RankedDialTarget {
                peer_id: Some(peer_id.clone()),
                primary: requested.to_string(),
                fallbacks: Vec::new(),
            };
        }
    }

    if requested.parse::<libp2p::PeerId>().is_ok() {
        return RankedDialTarget {
            peer_id: Some(requested.to_string()),
            primary: requested.to_string(),
            fallbacks: Vec::new(),
        };
    }

    RankedDialTarget {
        peer_id: None,
        primary: requested.to_string(),
        fallbacks: Vec::new(),
    }
}

#[cfg(feature = "webrtc-transport")]
async fn set_active_send_path(state: &Arc<RwLock<DaemonStatus>>, path: MediaPath) {
    let mut state = state.write().await;
    if matches!(path, MediaPath::WebRtcDataChannel) {
        state.network.transport_stage =
            "webrtc data-channel media active; libp2p signalling fallback ready".to_string();
    }
    state.network.selected_media_path = path.as_str().to_string();
}

fn rank_known_peer_addresses(
    network: &NetworkSummary,
    known_peer: &KnownPeerSummary,
    requested: Option<&str>,
) -> Vec<String> {
    let mut candidates = Vec::new();
    if let Some(requested) = requested {
        candidates.push(requested.to_string());
    }
    if let Some(last_dial_addr) = &known_peer.last_dial_addr {
        candidates.push(last_dial_addr.clone());
    }
    candidates.extend(known_peer.addresses.iter().cloned());

    let mut deduped = Vec::new();
    for candidate in candidates {
        if !deduped.contains(&candidate) {
            deduped.push(candidate);
        }
    }

    deduped.sort_by(|left, right| {
        let left_key = dial_candidate_rank(network, &known_peer.peer_id, left, requested);
        let right_key = dial_candidate_rank(network, &known_peer.peer_id, right, requested);
        right_key.cmp(&left_key).then_with(|| left.cmp(right))
    });
    deduped
}

fn dial_candidate_rank(
    network: &NetworkSummary,
    peer_id: &str,
    address: &str,
    requested: Option<&str>,
) -> (i64, i64, i64, i64) {
    let requested_boost = requested
        .map(|candidate| i64::from(candidate == address))
        .unwrap_or_default();
    let last_dial_boost = network
        .known_peers
        .iter()
        .find(|peer| peer.peer_id == peer_id)
        .and_then(|peer| peer.last_dial_addr.as_deref())
        .map(|candidate| i64::from(candidate == address))
        .unwrap_or_default();
    let path_name = address_path_name(address);
    let path_score = network
        .path_scores
        .iter()
        .find(|score| score.path == path_name)
        .map(|score| score.successes as i64 - score.failures as i64)
        .unwrap_or_default();
    let last_peer_boost = network
        .path_scores
        .iter()
        .find(|score| score.path == path_name)
        .and_then(|score| score.last_peer_id.as_deref())
        .map(|last_peer| i64::from(last_peer == peer_id))
        .unwrap_or_default();

    (
        path_score,
        last_peer_boost,
        last_dial_boost,
        requested_boost,
    )
}

pub(super) fn address_path_name(address: &str) -> &'static str {
    address
        .parse::<Multiaddr>()
        .ok()
        .filter(|multiaddr| {
            multiaddr
                .iter()
                .any(|protocol| matches!(protocol, Protocol::P2pCircuit))
        })
        .map(|_| LIBP2P_RELAY_PATH)
        .unwrap_or(LIBP2P_DIRECT_PATH)
}

fn multiaddr_peer_id(address: &Multiaddr) -> Option<libp2p::PeerId> {
    address.iter().find_map(|protocol| match protocol {
        Protocol::P2p(peer_id) => Some(peer_id),
        _ => None,
    })
}

#[cfg(feature = "webrtc-transport")]
async fn prefers_webrtc(state: &Arc<RwLock<DaemonStatus>>) -> bool {
    let state = state.read().await;
    matches!(webrtc_path_state(&state), WebRtcPathState::Connected)
}

#[cfg(feature = "webrtc-transport")]
fn webrtc_path_state(state: &DaemonStatus) -> WebRtcPathState {
    let connection_state = state.network.webrtc_connection_state.trim();
    if connection_state == "disabled" {
        return WebRtcPathState::Disabled;
    }
    if connection_state.ends_with(" connected") || connection_state == "connected" {
        return WebRtcPathState::Connected;
    }
    WebRtcPathState::NotConnected
}

#[cfg(test)]
mod tests {
    use super::*;
    use voicers_core::{
        AudioBackend, AudioEngineStage, AudioSummary, DaemonStatus, KnownPeerSummary,
        NetworkSummary, OutputStrategy, SessionSummary,
    };

    #[cfg(feature = "webrtc-transport")]
    fn daemon_status(webrtc_connection_state: &str) -> DaemonStatus {
        DaemonStatus {
            daemon_version: "0.1.0".to_string(),
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
                transport_stage: "testing".to_string(),
                nat_status: "testing".to_string(),
                listen_addrs: Vec::new(),
                external_addrs: Vec::new(),
                observed_addrs: Vec::new(),
                stun_addrs: Vec::new(),
                selected_media_path: LIBP2P_MEDIA_PATH.to_string(),
                webrtc_connection_state: webrtc_connection_state.to_string(),
                path_scores: Vec::new(),
                saved_peer_addrs: Vec::new(),
                known_peers: Vec::new(),
                friends: Vec::new(),
                seen_users: Vec::new(),
                discovered_peers: Vec::new(),
                ignored_peer_ids: Vec::new(),
                direct_call_invite: None,
            },
            audio: AudioSummary {
                backend: AudioBackend::Unknown,
                output_strategy: OutputStrategy::LogicalPeerBusesOnly,
                output_backend: "none".to_string(),
                capture_device: None,
                available_capture_devices: Vec::new(),
                sample_rate_hz: Some(48_000),
                engine: AudioEngineStage::QueueingOnly,
                frame_size_ms: Some(20),
                codec: Some("opus".to_string()),
                source: Some("test".to_string()),
                input_gain_percent: 100,
            },
            peers: Vec::new(),
            pending_peer_approvals: Vec::new(),
            notes: Vec::new(),
        }
    }

    #[cfg(feature = "webrtc-transport")]
    #[test]
    fn prefers_webrtc_only_when_connection_is_connected() {
        assert!(matches!(
            webrtc_path_state(&daemon_status("peer-a connected")),
            WebRtcPathState::Connected
        ));
        assert!(matches!(
            webrtc_path_state(&daemon_status("idle")),
            WebRtcPathState::NotConnected
        ));
        assert!(matches!(
            webrtc_path_state(&daemon_status("disabled")),
            WebRtcPathState::Disabled
        ));
    }

    #[test]
    fn ranked_dial_target_prefers_scored_direct_address_for_known_peer() {
        let peer_id = "QmNnooDu7bfjPFoTZYxMNLWUQJyrVwtbZg5gBMjTezGAJN";
        let relay_peer_id = "QmQCU2EcMqAqQPR2i9bChDtGNJchTbq5TbXJJ16u19uLTa";
        let mut status = daemon_status_for_ranking();
        status.network.known_peers.push(KnownPeerSummary {
            peer_id: peer_id.to_string(),
            display_name: "peer-a".to_string(),
            addresses: vec![
                format!("/ip4/198.51.100.10/tcp/4001/p2p/{peer_id}"),
                format!("/ip4/203.0.113.20/tcp/4001/p2p/{relay_peer_id}/p2p-circuit/p2p/{peer_id}"),
            ],
            last_dial_addr: Some(format!(
                "/ip4/203.0.113.20/tcp/4001/p2p/{relay_peer_id}/p2p-circuit/p2p/{peer_id}"
            )),
            connected: false,
            pinned: true,
            seen: true,
            whitelisted: false,
            trusted_contact: false,
        });
        status
            .network
            .path_scores
            .push(voicers_core::PathScoreSummary {
                path: LIBP2P_DIRECT_PATH.to_string(),
                successes: 5,
                failures: 1,
                last_peer_id: Some(peer_id.to_string()),
            });
        status
            .network
            .path_scores
            .push(voicers_core::PathScoreSummary {
                path: LIBP2P_RELAY_PATH.to_string(),
                successes: 1,
                failures: 4,
                last_peer_id: Some(peer_id.to_string()),
            });

        let ranked = resolve_ranked_dial_target(&status.network, peer_id);
        assert_eq!(ranked.peer_id.as_deref(), Some(peer_id));
        assert_eq!(
            ranked.primary,
            format!("/ip4/198.51.100.10/tcp/4001/p2p/{peer_id}")
        );
        assert_eq!(ranked.fallbacks.len(), 1);
    }

    #[test]
    fn ranked_dial_target_uses_requested_address_as_candidate_for_known_peer_multiaddr() {
        let peer_id = "QmbLHAnMoJPWSCR5Zhtx6BHJX9KiKNN6tpvbUcqanj75Nb";
        let mut status = daemon_status_for_ranking();
        status.network.known_peers.push(KnownPeerSummary {
            peer_id: peer_id.to_string(),
            display_name: "peer-b".to_string(),
            addresses: vec![format!("/ip4/198.51.100.11/tcp/4001/p2p/{peer_id}")],
            last_dial_addr: None,
            connected: false,
            pinned: true,
            seen: true,
            whitelisted: false,
            trusted_contact: false,
        });

        let ranked = resolve_ranked_dial_target(
            &status.network,
            &format!("/ip4/203.0.113.30/tcp/4001/p2p/{peer_id}"),
        );
        assert_eq!(ranked.peer_id.as_deref(), Some(peer_id));
        assert_eq!(
            ranked.primary,
            format!("/ip4/203.0.113.30/tcp/4001/p2p/{peer_id}")
        );
        assert_eq!(ranked.fallbacks.len(), 1);
    }

    fn make_route_status(peers: Vec<KnownPeerSummary>) -> DaemonStatus {
        let mut status = daemon_status_for_ranking();
        status.network.known_peers = peers;
        status
    }

    const TARGET: &str = "QmNnooDu7bfjPFoTZYxMNLWUQJyrVwtbZg5gBMjTezGAJN";
    const BOB: &str = "QmQCU2EcMqAqQPR2i9bChDtGNJchTbq5TbXJJ16u19uLTa";
    const CAROL: &str = "QmbLHAnMoJPWSCR5Zhtx6BHJX9KiKNN6tpvbUcqanj75Nb";

    fn peer(id: &str, trusted: bool, connected: bool) -> KnownPeerSummary {
        KnownPeerSummary {
            peer_id: id.to_string(),
            display_name: id[..8].to_string(),
            addresses: vec![],
            last_dial_addr: None,
            connected,
            pinned: false,
            seen: true,
            whitelisted: false,
            trusted_contact: trusted,
        }
    }

    #[test]
    fn resolve_route_direct_when_trusted_and_connected() {
        let status = make_route_status(vec![peer(TARGET, true, true)]);
        let offers = HashMap::new();
        let route = resolve_route(&status, &offers, TARGET);
        assert!(matches!(route, Route::Direct));
    }

    #[test]
    fn resolve_route_direct_when_connected_even_if_not_trusted() {
        let status = make_route_status(vec![peer(TARGET, false, true)]);
        let offers = HashMap::new();
        assert!(matches!(
            resolve_route(&status, &offers, TARGET),
            Route::Direct
        ));
    }

    #[test]
    fn resolve_route_unreachable_when_trusted_but_disconnected() {
        let status = make_route_status(vec![peer(TARGET, true, false)]);
        let offers = HashMap::new();
        assert!(matches!(
            resolve_route(&status, &offers, TARGET),
            Route::Unreachable
        ));
    }

    #[test]
    fn resolve_route_relay_when_not_directly_reachable() {
        let status = make_route_status(vec![peer(TARGET, false, false), peer(BOB, true, true)]);
        let mut offers = HashMap::new();
        offers.insert(BOB.to_string(), vec![TARGET.to_string()]);
        let route = resolve_route(&status, &offers, TARGET);
        let expected_via: libp2p::PeerId = BOB.parse().unwrap();
        let expected_dest: libp2p::PeerId = TARGET.parse().unwrap();
        assert!(
            matches!(route, Route::Relayed { via, destination } if via == expected_via && destination == expected_dest)
        );
    }

    #[test]
    fn resolve_route_unreachable_when_relay_not_connected() {
        let status = make_route_status(vec![peer(TARGET, false, false), peer(BOB, true, false)]);
        let mut offers = HashMap::new();
        offers.insert(BOB.to_string(), vec![TARGET.to_string()]);
        assert!(matches!(
            resolve_route(&status, &offers, TARGET),
            Route::Unreachable
        ));
    }

    #[test]
    fn resolve_route_relay_picks_best_path_score() {
        let mut status = make_route_status(vec![
            peer(TARGET, false, false),
            peer(BOB, true, true),
            peer(CAROL, true, true),
        ]);
        status
            .network
            .path_scores
            .push(voicers_core::PathScoreSummary {
                path: LIBP2P_RELAY_PATH.to_string(),
                successes: 5,
                failures: 0,
                last_peer_id: Some(BOB.to_string()),
            });
        status
            .network
            .path_scores
            .push(voicers_core::PathScoreSummary {
                path: LIBP2P_RELAY_PATH.to_string(),
                successes: 1,
                failures: 0,
                last_peer_id: Some(CAROL.to_string()),
            });
        let mut offers = HashMap::new();
        offers.insert(BOB.to_string(), vec![TARGET.to_string()]);
        offers.insert(CAROL.to_string(), vec![TARGET.to_string()]);
        let route = resolve_route(&status, &offers, TARGET);
        let expected_via: libp2p::PeerId = BOB.parse().unwrap();
        assert!(matches!(route, Route::Relayed { via, .. } if via == expected_via));
    }

    #[test]
    fn resolve_route_relay_when_target_not_in_known_peers() {
        let status = make_route_status(vec![peer(BOB, true, true)]);
        let mut offers = HashMap::new();
        offers.insert(BOB.to_string(), vec![TARGET.to_string()]);
        let route = resolve_route(&status, &offers, TARGET);
        let expected_via: libp2p::PeerId = BOB.parse().unwrap();
        assert!(matches!(route, Route::Relayed { via, .. } if via == expected_via));
    }

    #[test]
    fn resolve_route_unreachable_when_relay_offers_wrong_target() {
        let status = make_route_status(vec![peer(TARGET, false, false), peer(BOB, true, true)]);
        let mut offers = HashMap::new();
        offers.insert(BOB.to_string(), vec![CAROL.to_string()]);
        assert!(matches!(
            resolve_route(&status, &offers, TARGET),
            Route::Unreachable
        ));
    }

    #[test]
    fn resolve_route_unreachable_when_trusted_relay_has_empty_offers() {
        let status = make_route_status(vec![peer(TARGET, false, false), peer(BOB, true, true)]);
        let mut offers = HashMap::new();
        offers.insert(BOB.to_string(), vec![]);
        assert!(matches!(
            resolve_route(&status, &offers, TARGET),
            Route::Unreachable
        ));
    }

    #[test]
    fn address_path_name_classifies_direct_tcp_address() {
        let addr = format!("/ip4/198.51.100.10/tcp/4001/p2p/{TARGET}");
        assert_eq!(address_path_name(&addr), LIBP2P_DIRECT_PATH);
    }

    #[test]
    fn address_path_name_classifies_circuit_relay_address() {
        let addr = format!("/ip4/203.0.113.20/tcp/4001/p2p/{BOB}/p2p-circuit/p2p/{TARGET}");
        assert_eq!(address_path_name(&addr), LIBP2P_RELAY_PATH);
    }

    #[test]
    fn resolve_route_relay_tiebreaks_by_peer_id() {
        let status = make_route_status(vec![
            peer(TARGET, false, false),
            peer(BOB, true, true),
            peer(CAROL, true, true),
        ]);
        let mut offers = HashMap::new();
        offers.insert(BOB.to_string(), vec![TARGET.to_string()]);
        offers.insert(CAROL.to_string(), vec![TARGET.to_string()]);
        let route = resolve_route(&status, &offers, TARGET);
        // BOB ("QmQ...") < CAROL ("Qmb...") lexicographically (uppercase 'Q' < lowercase 'b')
        let expected_via: libp2p::PeerId = BOB.parse().unwrap();
        assert!(matches!(route, Route::Relayed { via, .. } if via == expected_via));
    }

    fn daemon_status_for_ranking() -> DaemonStatus {
        DaemonStatus {
            daemon_version: "0.1.0".to_string(),
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
                transport_stage: "testing".to_string(),
                nat_status: "testing".to_string(),
                listen_addrs: Vec::new(),
                external_addrs: Vec::new(),
                observed_addrs: Vec::new(),
                stun_addrs: Vec::new(),
                selected_media_path: LIBP2P_MEDIA_PATH.to_string(),
                webrtc_connection_state: "disabled".to_string(),
                path_scores: Vec::new(),
                saved_peer_addrs: Vec::new(),
                known_peers: Vec::new(),
                friends: Vec::new(),
                seen_users: Vec::new(),
                discovered_peers: Vec::new(),
                ignored_peer_ids: Vec::new(),
                direct_call_invite: None,
            },
            audio: AudioSummary {
                backend: AudioBackend::Unknown,
                output_strategy: OutputStrategy::LogicalPeerBusesOnly,
                output_backend: "none".to_string(),
                capture_device: None,
                available_capture_devices: Vec::new(),
                sample_rate_hz: Some(48_000),
                engine: AudioEngineStage::QueueingOnly,
                frame_size_ms: Some(20),
                codec: Some("opus".to_string()),
                source: Some("test".to_string()),
                input_gain_percent: 100,
            },
            peers: Vec::new(),
            pending_peer_approvals: Vec::new(),
            notes: Vec::new(),
        }
    }
}
