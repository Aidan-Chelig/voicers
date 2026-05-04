#[path = "common/mod.rs"]
mod common;

use std::{fs, path::PathBuf, time::Duration};

use anyhow::{anyhow, Result};
use voicers_core::{encode_room_compact_invite, ControlRequest, ControlResponse, DaemonStatus, PeerSessionState, PeerTransportState};
use voicersd::app::{App, AppConfig};

struct NetworkHarness {
    app: App,
    pub peer_id: String,
    state_path: PathBuf,
}

impl NetworkHarness {
    async fn spawn(display_name: &str) -> Result<Self> {
        let state_path = common::unique_state_path(display_name);
        let app = App::bootstrap(AppConfig {
            control_addr: "127.0.0.1:7767".to_string(),
            listen_addr: "/ip4/127.0.0.1/tcp/0".to_string(),
            relay_addr: None,
            bootstrap_addrs: vec![],
            use_default_bootstrap_addrs: false,
            stun_servers: vec![],
            turn_servers: vec![],
            enable_stun: false,
            enable_audio_io: false,
            enable_capture_input: false,
            enable_networking: true,
            display_name: display_name.to_string(),
            state_path: state_path.clone(),
        })
        .await?;
        let peer_id = app.peer_id().await;
        Ok(Self { app, peer_id, state_path })
    }

    async fn status(&self) -> Result<DaemonStatus> {
        match common::send_request(&self.app, ControlRequest::GetStatus).await? {
            ControlResponse::Status(s) => Ok(s),
            other => Err(anyhow!("expected Status, got {other:?}")),
        }
    }

    async fn request(&self, req: ControlRequest) -> Result<ControlResponse> {
        common::send_request(&self.app, req).await
    }

    async fn listen_addr(&self) -> Result<String> {
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                let status = self.status().await?;
                if let Some(addr) = status
                    .network
                    .listen_addrs
                    .iter()
                    .find(|a| a.contains("/ip4/127.0.0.1/tcp/") && !a.contains("p2p-circuit"))
                {
                    let full = if addr.contains("/p2p/") {
                        addr.clone()
                    } else {
                        format!("{addr}/p2p/{}", self.peer_id)
                    };
                    return Ok(full);
                }
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
        })
        .await
        .map_err(|_| anyhow!("timed out waiting for listen addr"))?
    }

    async fn wait_for_peer(&self, peer_id: &str, timeout: Duration) -> Result<()> {
        tokio::time::timeout(timeout, async {
            loop {
                let status = self.status().await?;
                // Auto-approve any pending inbound connection from this peer so that both
                // sides of a direct-dial test can reach Connected state.
                if status
                    .pending_peer_approvals
                    .iter()
                    .any(|p| p.peer_id == peer_id)
                {
                    let _ = self
                        .request(ControlRequest::ApprovePendingPeer {
                            peer_id: peer_id.to_string(),
                            whitelist: false,
                        })
                        .await;
                }
                if status.peers.iter().any(|p| {
                    p.peer_id == peer_id && matches!(p.transport, PeerTransportState::Connected)
                }) {
                    return Ok(());
                }
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
        })
        .await
        .map_err(|_| anyhow!("timed out waiting for peer {peer_id}"))?
    }

    async fn wait_for_pending(&self, peer_id: &str, timeout: Duration) -> Result<()> {
        tokio::time::timeout(timeout, async {
            loop {
                let status = self.status().await?;
                if status
                    .pending_peer_approvals
                    .iter()
                    .any(|p| p.peer_id == peer_id)
                {
                    return Ok(());
                }
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
        })
        .await
        .map_err(|_| anyhow!("timed out waiting for pending approval from {peer_id}"))?
    }
}

impl Drop for NetworkHarness {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.state_path);
    }
}

#[tokio::test]
async fn two_peers_connect() -> Result<()> {
    let alice = NetworkHarness::spawn("alice").await?;
    let bob = NetworkHarness::spawn("bob").await?;

    let bob_addr = bob.listen_addr().await?;
    common::expect_ack(
        alice
            .request(ControlRequest::JoinPeer { address: bob_addr })
            .await?,
    )?;

    alice
        .wait_for_peer(&bob.peer_id, Duration::from_secs(5))
        .await?;
    bob.wait_for_peer(&alice.peer_id, Duration::from_secs(5))
        .await?;
    Ok(())
}

#[tokio::test]
async fn connected_peer_display_name_propagates() -> Result<()> {
    let alice = NetworkHarness::spawn("alice").await?;
    let bob = NetworkHarness::spawn("bob").await?;

    let bob_addr = bob.listen_addr().await?;
    alice
        .request(ControlRequest::JoinPeer { address: bob_addr })
        .await?;
    alice
        .wait_for_peer(&bob.peer_id, Duration::from_secs(5))
        .await?;

    let status = alice.status().await?;
    let bob_peer = status
        .peers
        .iter()
        .find(|p| p.peer_id == bob.peer_id)
        .ok_or_else(|| anyhow!("bob not found in alice's peers"))?;
    assert_eq!(bob_peer.display_name, "bob");
    Ok(())
}

#[tokio::test]
async fn room_state_observed_via_status() -> Result<()> {
    let alice = NetworkHarness::spawn("alice-room").await?;
    common::expect_ack(
        alice
            .request(ControlRequest::CreateRoom {
                room_name: "lab".into(),
            })
            .await?,
    )?;
    let status = alice.status().await?;
    assert_eq!(status.session.room_name.as_deref(), Some("lab"));
    Ok(())
}

// --- Approval / invite tests ---

#[tokio::test]
async fn unknown_peer_requires_approval() -> Result<()> {
    let alice = NetworkHarness::spawn("alice").await?;
    let bob = NetworkHarness::spawn("bob").await?;

    let bob_addr = bob.listen_addr().await?;
    alice
        .request(ControlRequest::JoinPeer { address: bob_addr })
        .await?;

    // Bob should see alice in pending — NOT auto-connected
    bob.wait_for_pending(&alice.peer_id, Duration::from_secs(5))
        .await?;

    let status = bob.status().await?;
    assert!(
        status
            .peers
            .iter()
            .all(|p| p.peer_id != alice.peer_id || !matches!(p.transport, PeerTransportState::Connected)),
        "alice should not be Connected in bob's peers before approval"
    );
    Ok(())
}

#[tokio::test]
async fn approve_peer_completes_connection() -> Result<()> {
    let alice = NetworkHarness::spawn("alice").await?;
    let bob = NetworkHarness::spawn("bob").await?;

    let bob_addr = bob.listen_addr().await?;
    alice
        .request(ControlRequest::JoinPeer { address: bob_addr })
        .await?;

    bob.wait_for_pending(&alice.peer_id, Duration::from_secs(5))
        .await?;

    common::expect_ack(
        bob.request(ControlRequest::ApprovePendingPeer {
            peer_id: alice.peer_id.clone(),
            whitelist: false,
        })
        .await?,
    )?;

    // After approval both sides reach Connected
    alice
        .wait_for_peer(&bob.peer_id, Duration::from_secs(5))
        .await?;

    let bob_status = bob.status().await?;
    assert!(
        bob_status.pending_peer_approvals.is_empty(),
        "pending list should be empty after approval"
    );
    Ok(())
}

#[tokio::test]
async fn reject_peer_removes_from_pending() -> Result<()> {
    let alice = NetworkHarness::spawn("alice").await?;
    let bob = NetworkHarness::spawn("bob").await?;

    let bob_addr = bob.listen_addr().await?;
    alice
        .request(ControlRequest::JoinPeer { address: bob_addr })
        .await?;

    bob.wait_for_pending(&alice.peer_id, Duration::from_secs(5))
        .await?;

    common::expect_ack(
        bob.request(ControlRequest::RejectPendingPeer {
            peer_id: alice.peer_id.clone(),
        })
        .await?,
    )?;

    let bob_status = bob.status().await?;
    assert!(
        bob_status.pending_peer_approvals.is_empty(),
        "pending list should be empty after rejection"
    );
    assert!(
        bob_status
            .peers
            .iter()
            .all(|p| p.peer_id != alice.peer_id || !matches!(p.transport, PeerTransportState::Connected)),
        "alice should not be Connected in bob's peers after rejection"
    );
    Ok(())
}

#[tokio::test]
async fn approve_with_whitelist_sets_flag() -> Result<()> {
    let alice = NetworkHarness::spawn("alice").await?;
    let bob = NetworkHarness::spawn("bob").await?;

    let bob_addr = bob.listen_addr().await?;
    alice
        .request(ControlRequest::JoinPeer { address: bob_addr })
        .await?;
    bob.wait_for_pending(&alice.peer_id, Duration::from_secs(5))
        .await?;

    common::expect_ack(
        bob.request(ControlRequest::ApprovePendingPeer {
            peer_id: alice.peer_id.clone(),
            whitelist: true,
        })
        .await?,
    )?;

    let bob_status = bob.status().await?;
    let alice_known = bob_status
        .network
        .known_peers
        .iter()
        .find(|p| p.peer_id == alice.peer_id)
        .ok_or_else(|| anyhow!("alice not in bob's known_peers after whitelisted approval"))?;
    assert!(
        alice_known.whitelisted,
        "alice should be whitelisted after ApprovePendingPeer with whitelist: true"
    );
    Ok(())
}

#[tokio::test]
async fn room_invite_join_flow() -> Result<()> {
    let host = NetworkHarness::spawn("host").await?;
    let guest = NetworkHarness::spawn("guest").await?;

    // Host creates a room
    common::expect_ack(
        host.request(ControlRequest::CreateRoom {
            room_name: "test-room".into(),
        })
        .await?,
    )?;

    // share_invite filtering excludes loopback addrs in production. Build the invite
    // URL directly from known components so we exercise the parsing/join path.
    let host_listen_addr = host.listen_addr().await?;
    let host_status = host.status().await?;
    let invite = host_status
        .rooms
        .iter()
        .find(|r| r.name == "test-room")
        .and_then(|r| r.current_invite.as_ref())
        .ok_or_else(|| anyhow!("no current_invite on test-room"))?;
    let invite_url = encode_room_compact_invite(
        &host.peer_id,
        "test-room",
        &[host_listen_addr],
        &invite.invite_code,
        invite.expires_at_ms,
    );

    // Guest joins via invite URL
    guest
        .request(ControlRequest::JoinPeer {
            address: invite_url,
        })
        .await?;

    // Host should see guest as pending
    host.wait_for_pending(&guest.peer_id, Duration::from_secs(5))
        .await?;

    // Host approves
    common::expect_ack(
        host.request(ControlRequest::ApprovePendingPeer {
            peer_id: guest.peer_id.clone(),
            whitelist: false,
        })
        .await?,
    )?;

    // Both sides reach Connected
    guest
        .wait_for_peer(&host.peer_id, Duration::from_secs(5))
        .await?;

    let host_status = host.status().await?;
    assert!(
        host_status.peers.iter().any(|p| p.peer_id == guest.peer_id),
        "guest should be in host's peer list after room invite join"
    );
    Ok(())
}

#[tokio::test]
async fn approved_peer_not_re_gated_on_broadcast_hello() -> Result<()> {
    let alice = NetworkHarness::spawn("alice").await?;
    let bob = NetworkHarness::spawn("bob").await?;

    let bob_addr = bob.listen_addr().await?;
    alice
        .request(ControlRequest::JoinPeer { address: bob_addr })
        .await?;

    bob.wait_for_pending(&alice.peer_id, Duration::from_secs(5))
        .await?;

    common::expect_ack(
        bob.request(ControlRequest::ApprovePendingPeer {
            peer_id: alice.peer_id.clone(),
            whitelist: false,
        })
        .await?,
    )?;
    alice
        .wait_for_peer(&bob.peer_id, Duration::from_secs(5))
        .await?;

    common::expect_ack(
        alice
            .request(ControlRequest::SetDisplayName {
                display_name: "alice-renamed".to_string(),
            })
            .await?,
    )?;

    tokio::time::sleep(Duration::from_millis(500)).await;
    let status = bob.status().await?;
    assert!(
        status.pending_peer_approvals.is_empty(),
        "alice was re-gated after display name change: {:?}",
        status.pending_peer_approvals
    );
    Ok(())
}

#[tokio::test]
async fn set_display_name_propagates_to_connected_peer() -> Result<()> {
    let alice = NetworkHarness::spawn("alice").await?;
    let bob = NetworkHarness::spawn("bob").await?;

    let bob_addr = bob.listen_addr().await?;
    alice
        .request(ControlRequest::JoinPeer { address: bob_addr })
        .await?;

    bob.wait_for_pending(&alice.peer_id, Duration::from_secs(5))
        .await?;
    common::expect_ack(
        bob.request(ControlRequest::ApprovePendingPeer {
            peer_id: alice.peer_id.clone(),
            whitelist: true,
        })
        .await?,
    )?;

    alice
        .wait_for_peer(&bob.peer_id, Duration::from_secs(5))
        .await?;
    bob.wait_for_peer(&alice.peer_id, Duration::from_secs(5))
        .await?;

    common::expect_ack(
        alice
            .request(ControlRequest::SetDisplayName {
                display_name: "alice-renamed".to_string(),
            })
            .await?,
    )?;

    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let status = bob.status().await?;
            if status
                .peers
                .iter()
                .any(|p| p.peer_id == alice.peer_id && p.display_name == "alice-renamed")
            {
                return Ok::<_, anyhow::Error>(());
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    })
    .await
    .map_err(|_| anyhow!("timed out waiting for display name propagation to bob"))??;

    Ok(())
}

#[tokio::test]
async fn room_change_propagates_to_connected_peer() -> Result<()> {
    let alice = NetworkHarness::spawn("alice").await?;
    let bob = NetworkHarness::spawn("bob").await?;

    let bob_addr = bob.listen_addr().await?;
    alice
        .request(ControlRequest::JoinPeer { address: bob_addr })
        .await?;

    bob.wait_for_pending(&alice.peer_id, Duration::from_secs(5))
        .await?;
    common::expect_ack(
        bob.request(ControlRequest::ApprovePendingPeer {
            peer_id: alice.peer_id.clone(),
            whitelist: true,
        })
        .await?,
    )?;

    alice
        .wait_for_peer(&bob.peer_id, Duration::from_secs(5))
        .await?;
    bob.wait_for_peer(&alice.peer_id, Duration::from_secs(5))
        .await?;

    common::expect_ack(
        alice
            .request(ControlRequest::CreateRoom {
                room_name: "lobby".to_string(),
            })
            .await?,
    )?;

    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let status = bob.status().await?;
            if status.peers.iter().any(|p| {
                p.peer_id == alice.peer_id
                    && matches!(
                        &p.session,
                        PeerSessionState::Active {
                            room_name: Some(name),
                            ..
                        } if name == "lobby"
                    )
            }) {
                return Ok::<_, anyhow::Error>(());
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    })
    .await
    .map_err(|_| anyhow!("timed out waiting for room change propagation to bob"))??;

    Ok(())
}

#[tokio::test]
async fn multiple_pending_peers_all_appear_in_pending_list() -> Result<()> {
    let alice = NetworkHarness::spawn("alice").await?;
    let bob = NetworkHarness::spawn("bob").await?;
    let carol = NetworkHarness::spawn("carol").await?;

    let alice_addr = alice.listen_addr().await?;

    bob.request(ControlRequest::JoinPeer {
        address: alice_addr.clone(),
    })
    .await?;
    carol
        .request(ControlRequest::JoinPeer {
            address: alice_addr,
        })
        .await?;

    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let status = alice.status().await?;
            if status.pending_peer_approvals.len() >= 2 {
                return Ok::<_, anyhow::Error>(());
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    })
    .await
    .map_err(|_| anyhow!("timed out waiting for both bob and carol to appear in pending"))??;

    let status = alice.status().await?;
    assert!(
        status
            .pending_peer_approvals
            .iter()
            .any(|p| p.peer_id == bob.peer_id),
        "bob should be in alice's pending_peer_approvals"
    );
    assert!(
        status
            .pending_peer_approvals
            .iter()
            .any(|p| p.peer_id == carol.peer_id),
        "carol should be in alice's pending_peer_approvals"
    );

    Ok(())
}

#[tokio::test]
async fn persisted_known_peer_survives_daemon_restart() -> Result<()> {
    let state_path = common::unique_state_path("persist-test");

    let app1 = App::bootstrap(AppConfig {
        control_addr: "127.0.0.1:7767".to_string(),
        listen_addr: "/ip4/127.0.0.1/tcp/0".to_string(),
        relay_addr: None,
        bootstrap_addrs: vec![],
        use_default_bootstrap_addrs: false,
        stun_servers: vec![],
        turn_servers: vec![],
        enable_stun: false,
        enable_audio_io: false,
        enable_capture_input: false,
        enable_networking: false,
        display_name: "persist-alice".to_string(),
        state_path: state_path.clone(),
    })
    .await?;

    app1.seed_peer_for_tests("peer-x", "xavier", "/ip4/127.0.0.1/tcp/9999")
        .await;
    common::expect_ack(
        common::send_request(&app1, ControlRequest::SaveKnownPeer {
            peer_id: "peer-x".to_string(),
        })
        .await?,
    )?;
    common::expect_ack(
        common::send_request(&app1, ControlRequest::MarkTrustedContact {
            peer_id: "peer-x".to_string(),
        })
        .await?,
    )?;
    drop(app1);

    let app2 = App::bootstrap(AppConfig {
        control_addr: "127.0.0.1:7767".to_string(),
        listen_addr: "/ip4/127.0.0.1/tcp/0".to_string(),
        relay_addr: None,
        bootstrap_addrs: vec![],
        use_default_bootstrap_addrs: false,
        stun_servers: vec![],
        turn_servers: vec![],
        enable_stun: false,
        enable_audio_io: false,
        enable_capture_input: false,
        enable_networking: false,
        display_name: "persist-alice".to_string(),
        state_path: state_path.clone(),
    })
    .await?;

    let response = common::send_request(&app2, ControlRequest::GetStatus).await?;
    let status = match response {
        ControlResponse::Status(s) => s,
        other => return Err(anyhow!("expected Status, got {other:?}")),
    };
    let peer = status
        .network
        .known_peers
        .iter()
        .find(|p| p.peer_id == "peer-x")
        .ok_or_else(|| anyhow!("peer-x not found in known_peers after restart"))?;
    assert!(
        peer.trusted_contact,
        "peer-x should have trusted_contact=true after restart"
    );

    drop(app2);
    let _ = std::fs::remove_file(&state_path);

    Ok(())
}

// ── Room join approval gate ──────────────────────────────────────────────────

// An unknown peer joining via room invite must appear in pending_peer_approvals
// on the host — the host must explicitly approve before the guest is connected.
#[tokio::test]
async fn room_join_unknown_guest_requires_approval() -> Result<()> {
    let host = NetworkHarness::spawn("host").await?;
    let guest = NetworkHarness::spawn("guest").await?;

    common::expect_ack(
        host.request(ControlRequest::CreateRoom {
            room_name: "lobby".into(),
        })
        .await?,
    )?;

    let host_listen_addr = host.listen_addr().await?;
    let host_status = host.status().await?;
    let invite = host_status
        .rooms
        .iter()
        .find(|r| r.name == "lobby")
        .and_then(|r| r.current_invite.as_ref())
        .ok_or_else(|| anyhow!("no invite on lobby"))?;
    let invite_url = encode_room_compact_invite(
        &host.peer_id,
        "lobby",
        &[host_listen_addr],
        &invite.invite_code,
        invite.expires_at_ms,
    );

    guest
        .request(ControlRequest::JoinPeer {
            address: invite_url,
        })
        .await?;

    // Host must gate the unknown guest.
    host.wait_for_pending(&guest.peer_id, Duration::from_secs(5))
        .await
        .map_err(|_| anyhow!("guest never appeared in host's pending_peer_approvals — approval gate bypassed"))?;

    // Guest must NOT be connected yet.
    let guest_status = guest.status().await?;
    assert!(
        !guest_status.peers.iter().any(|p| p.peer_id == host.peer_id
            && matches!(p.transport, PeerTransportState::Connected)),
        "guest should not be Connected before host approves"
    );

    Ok(())
}

// After the host approves, the guest reaches Connected and can see the host.
#[tokio::test]
async fn room_join_approved_guest_reaches_connected() -> Result<()> {
    let host = NetworkHarness::spawn("host").await?;
    let guest = NetworkHarness::spawn("guest").await?;

    common::expect_ack(
        host.request(ControlRequest::CreateRoom {
            room_name: "lobby".into(),
        })
        .await?,
    )?;

    let host_listen_addr = host.listen_addr().await?;
    let host_status = host.status().await?;
    let invite = host_status
        .rooms
        .iter()
        .find(|r| r.name == "lobby")
        .and_then(|r| r.current_invite.as_ref())
        .ok_or_else(|| anyhow!("no invite on lobby"))?;
    let invite_url = encode_room_compact_invite(
        &host.peer_id,
        "lobby",
        &[host_listen_addr],
        &invite.invite_code,
        invite.expires_at_ms,
    );

    guest
        .request(ControlRequest::JoinPeer {
            address: invite_url,
        })
        .await?;

    host.wait_for_pending(&guest.peer_id, Duration::from_secs(5))
        .await?;

    common::expect_ack(
        host.request(ControlRequest::ApprovePendingPeer {
            peer_id: guest.peer_id.clone(),
            whitelist: false,
        })
        .await?,
    )?;

    // Guest reaches Connected to host.
    guest
        .wait_for_peer(&host.peer_id, Duration::from_secs(5))
        .await
        .map_err(|_| anyhow!("guest did not reach Connected after host approved"))?;

    // Host has no more pending approvals.
    let host_status = host.status().await?;
    assert!(
        !host_status
            .pending_peer_approvals
            .iter()
            .any(|p| p.peer_id == guest.peer_id),
        "guest should no longer be in host's pending_peer_approvals after approval"
    );

    // Host sees guest as Connected.
    assert!(
        host_status.peers.iter().any(|p| p.peer_id == guest.peer_id
            && matches!(p.transport, PeerTransportState::Connected)),
        "host should see guest as Connected after approval"
    );

    Ok(())
}

// Rejecting a room-join guest removes them from pending and leaves them disconnected.
#[tokio::test]
async fn room_join_rejected_guest_not_connected() -> Result<()> {
    let host = NetworkHarness::spawn("host").await?;
    let guest = NetworkHarness::spawn("guest").await?;

    common::expect_ack(
        host.request(ControlRequest::CreateRoom {
            room_name: "lobby".into(),
        })
        .await?,
    )?;

    let host_listen_addr = host.listen_addr().await?;
    let host_status = host.status().await?;
    let invite = host_status
        .rooms
        .iter()
        .find(|r| r.name == "lobby")
        .and_then(|r| r.current_invite.as_ref())
        .ok_or_else(|| anyhow!("no invite on lobby"))?;
    let invite_url = encode_room_compact_invite(
        &host.peer_id,
        "lobby",
        &[host_listen_addr],
        &invite.invite_code,
        invite.expires_at_ms,
    );

    guest
        .request(ControlRequest::JoinPeer {
            address: invite_url,
        })
        .await?;

    host.wait_for_pending(&guest.peer_id, Duration::from_secs(5))
        .await?;

    common::expect_ack(
        host.request(ControlRequest::RejectPendingPeer {
            peer_id: guest.peer_id.clone(),
        })
        .await?,
    )?;

    // Give the rejection a moment to propagate, then verify guest is not Connected.
    tokio::time::sleep(Duration::from_millis(300)).await;

    let guest_status = guest.status().await?;
    assert!(
        !guest_status.peers.iter().any(|p| p.peer_id == host.peer_id
            && matches!(p.transport, PeerTransportState::Connected)),
        "rejected guest should not be Connected"
    );

    let host_status = host.status().await?;
    assert!(
        !host_status
            .pending_peer_approvals
            .iter()
            .any(|p| p.peer_id == guest.peer_id),
        "rejected guest should be removed from pending_peer_approvals"
    );

    Ok(())
}

// A whitelisted peer joining via room invite bypasses the approval gate entirely.
// We whitelist the guest via a first connection (approve with whitelist:true), then
// the guest reconnects via a room invite and should be auto-approved.
#[tokio::test]
async fn room_join_whitelisted_guest_auto_approved() -> Result<()> {
    let host = NetworkHarness::spawn("host").await?;
    let guest = NetworkHarness::spawn("guest").await?;

    // First: connect guest directly so host can whitelist them.
    let host_listen_addr = host.listen_addr().await?;
    guest
        .request(ControlRequest::JoinPeer {
            address: host_listen_addr.clone(),
        })
        .await?;
    host.wait_for_pending(&guest.peer_id, Duration::from_secs(5))
        .await?;
    common::expect_ack(
        host.request(ControlRequest::ApprovePendingPeer {
            peer_id: guest.peer_id.clone(),
            whitelist: true,
        })
        .await?,
    )?;
    guest
        .wait_for_peer(&host.peer_id, Duration::from_secs(5))
        .await?;

    // Guest is now whitelisted. Host creates a room.
    common::expect_ack(
        host.request(ControlRequest::CreateRoom {
            room_name: "lobby".into(),
        })
        .await?,
    )?;

    let host_listen_addr = host.listen_addr().await?;
    let host_status = host.status().await?;
    let invite = host_status
        .rooms
        .iter()
        .find(|r| r.name == "lobby")
        .and_then(|r| r.current_invite.as_ref())
        .ok_or_else(|| anyhow!("no invite on lobby"))?;
    let invite_url = encode_room_compact_invite(
        &host.peer_id,
        "lobby",
        &[host_listen_addr],
        &invite.invite_code,
        invite.expires_at_ms,
    );

    guest
        .request(ControlRequest::JoinPeer {
            address: invite_url,
        })
        .await?;

    // Whitelisted guest should reach Connected without any manual approval.
    guest
        .wait_for_peer(&host.peer_id, Duration::from_secs(5))
        .await
        .map_err(|_| anyhow!("whitelisted guest did not auto-connect"))?;

    let host_status = host.status().await?;
    assert!(
        host_status.pending_peer_approvals.is_empty(),
        "whitelisted guest should not appear in pending_peer_approvals"
    );

    Ok(())
}
