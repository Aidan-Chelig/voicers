use std::{fs, path::PathBuf};

use anyhow::{anyhow, Result};
use voicers_core::{ControlRequest, ControlResponse, DaemonStatus};
use voicersd::app::{App, AppConfig};

#[path = "common/mod.rs"]
mod common;

struct AppHarness {
    app: App,
    state_path: PathBuf,
}

impl AppHarness {
    async fn spawn(display_name: &str) -> Result<Self> {
        let state_path = common::unique_state_path(display_name);
        let app = App::bootstrap(AppConfig {
            control_addr: "127.0.0.1:7767".to_string(),
            listen_addr: "/ip4/127.0.0.1/tcp/0".to_string(),
            relay_addr: None,
            bootstrap_addrs: Vec::new(),
            use_default_bootstrap_addrs: false,
            stun_servers: Vec::new(),
            turn_servers: Vec::new(),
            enable_stun: false,
            enable_audio_io: false,
            enable_capture_input: false,
            enable_networking: false,
            display_name: display_name.to_string(),
            state_path: state_path.clone(),
        })
        .await?;

        Ok(Self { app, state_path })
    }

    async fn status(&self) -> Result<DaemonStatus> {
        match common::send_request(&self.app, ControlRequest::GetStatus).await? {
            ControlResponse::Status(status) => Ok(status),
            other => Err(anyhow!("expected status response, got {other:?}")),
        }
    }
}

impl Drop for AppHarness {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.state_path);
    }
}

#[tokio::test]
async fn control_ipc_updates_room_and_mute_state() -> Result<()> {
    let daemon = AppHarness::spawn("alice").await?;

    let status = daemon.status().await?;
    assert_eq!(status.session.display_name, "alice");
    assert_eq!(
        status.network.selected_media_path,
        "libp2p-request-response"
    );
    assert_eq!(status.network.webrtc_connection_state, "disabled");

    common::expect_ack(
        common::send_request(
            &daemon.app,
            ControlRequest::CreateRoom {
                room_name: "lab".to_string(),
            },
        )
        .await?,
    )?;
    common::expect_ack(common::send_request(&daemon.app, ControlRequest::ToggleMuteSelf).await?)?;

    let status = daemon.status().await?;
    assert_eq!(status.session.room_name.as_deref(), Some("lab"));
    assert!(status.session.self_muted);

    Ok(())
}

#[tokio::test]
async fn local_peer_dial_populates_peer_state_and_keeps_libp2p_fallback() -> Result<()> {
    let daemon = AppHarness::spawn("alice").await?;
    daemon
        .app
        .seed_peer_for_tests("peer-a", "bob", "/ip4/127.0.0.1/tcp/4011")
        .await;

    common::expect_ack(
        common::send_request(
            &daemon.app,
            ControlRequest::ToggleMutePeer {
                peer_id: "peer-a".to_string(),
            },
        )
        .await?,
    )?;
    common::expect_ack(
        common::send_request(
            &daemon.app,
            ControlRequest::SetPeerVolumePercent {
                peer_id: "peer-a".to_string(),
                percent: 135,
            },
        )
        .await?,
    )?;

    let status = daemon.status().await?;
    assert_eq!(
        status.network.selected_media_path,
        "libp2p-request-response"
    );
    assert_eq!(status.network.webrtc_connection_state, "disabled");
    assert_eq!(status.network.transport_stage, "network disabled for tests");
    assert_eq!(status.peers.len(), 1);
    assert!(status.peers[0].muted);
    assert_eq!(status.peers[0].output_volume_percent, 135);

    Ok(())
}

#[tokio::test]
async fn mark_trusted_contact_sets_flag_in_known_peers() -> Result<()> {
    let daemon = AppHarness::spawn("alice").await?;
    daemon
        .app
        .seed_peer_for_tests("peer-a", "bob", "/ip4/127.0.0.1/tcp/4011")
        .await;
    common::expect_ack(
        common::send_request(
            &daemon.app,
            ControlRequest::SaveKnownPeer {
                peer_id: "peer-a".to_string(),
            },
        )
        .await?,
    )?;

    common::expect_ack(
        common::send_request(
            &daemon.app,
            ControlRequest::MarkTrustedContact {
                peer_id: "peer-a".to_string(),
            },
        )
        .await?,
    )?;

    let status = daemon.status().await?;
    let peer = status
        .network
        .known_peers
        .iter()
        .find(|p| p.peer_id == "peer-a")
        .ok_or_else(|| anyhow!("peer-a not found in known_peers"))?;
    assert!(peer.trusted_contact, "expected trusted_contact=true for peer-a");

    Ok(())
}

#[tokio::test]
async fn unmark_trusted_contact_clears_flag_in_known_peers() -> Result<()> {
    let daemon = AppHarness::spawn("alice").await?;
    daemon
        .app
        .seed_peer_for_tests("peer-a", "bob", "/ip4/127.0.0.1/tcp/4011")
        .await;
    common::expect_ack(
        common::send_request(
            &daemon.app,
            ControlRequest::SaveKnownPeer {
                peer_id: "peer-a".to_string(),
            },
        )
        .await?,
    )?;

    common::expect_ack(
        common::send_request(
            &daemon.app,
            ControlRequest::MarkTrustedContact {
                peer_id: "peer-a".to_string(),
            },
        )
        .await?,
    )?;
    common::expect_ack(
        common::send_request(
            &daemon.app,
            ControlRequest::UnmarkTrustedContact {
                peer_id: "peer-a".to_string(),
            },
        )
        .await?,
    )?;

    let status = daemon.status().await?;
    let peer = status
        .network
        .known_peers
        .iter()
        .find(|p| p.peer_id == "peer-a")
        .ok_or_else(|| anyhow!("peer-a not found in known_peers"))?;
    assert!(!peer.trusted_contact, "expected trusted_contact=false for peer-a");

    Ok(())
}

#[tokio::test]
async fn mark_trusted_contact_returns_error_for_unknown_peer() -> Result<()> {
    let daemon = AppHarness::spawn("alice").await?;

    let response = common::send_request(
        &daemon.app,
        ControlRequest::MarkTrustedContact {
            peer_id: "nobody".to_string(),
        },
    )
    .await?;

    assert!(
        matches!(response, ControlResponse::Error { .. }),
        "expected Error response, got {response:?}"
    );

    Ok(())
}

#[tokio::test]
async fn approve_pending_peer_returns_error_when_no_pending_peer() -> Result<()> {
    let daemon = AppHarness::spawn("alice").await?;

    let response = common::send_request(
        &daemon.app,
        ControlRequest::ApprovePendingPeer {
            peer_id: "nobody".to_string(),
            whitelist: false,
        },
    )
    .await?;

    assert!(
        matches!(response, ControlResponse::Error { .. }),
        "expected Error response, got {response:?}"
    );

    Ok(())
}

#[tokio::test]
async fn reject_pending_peer_returns_error_when_no_pending_peer() -> Result<()> {
    let daemon = AppHarness::spawn("alice").await?;

    let response = common::send_request(
        &daemon.app,
        ControlRequest::RejectPendingPeer {
            peer_id: "nobody".to_string(),
        },
    )
    .await?;

    assert!(
        matches!(response, ControlResponse::Error { .. }),
        "expected Error response, got {response:?}"
    );

    Ok(())
}

#[tokio::test]
async fn save_known_peer_populates_known_peers() -> Result<()> {
    let daemon = AppHarness::spawn("alice").await?;
    daemon
        .app
        .seed_peer_for_tests("peer-a", "bob", "/ip4/127.0.0.1/tcp/4011")
        .await;

    common::expect_ack(
        common::send_request(
            &daemon.app,
            ControlRequest::SaveKnownPeer {
                peer_id: "peer-a".to_string(),
            },
        )
        .await?,
    )?;

    let status = daemon.status().await?;
    assert!(
        status.network.known_peers.iter().any(|p| p.peer_id == "peer-a"),
        "peer-a should be in known_peers after SaveKnownPeer"
    );

    Ok(())
}

#[tokio::test]
async fn rename_known_peer_updates_display_name() -> Result<()> {
    let daemon = AppHarness::spawn("alice").await?;
    daemon
        .app
        .seed_peer_for_tests("peer-a", "bob", "/ip4/127.0.0.1/tcp/4011")
        .await;

    common::expect_ack(
        common::send_request(
            &daemon.app,
            ControlRequest::SaveKnownPeer {
                peer_id: "peer-a".to_string(),
            },
        )
        .await?,
    )?;
    common::expect_ack(
        common::send_request(
            &daemon.app,
            ControlRequest::RenameKnownPeer {
                peer_id: "peer-a".to_string(),
                display_name: "carol".to_string(),
            },
        )
        .await?,
    )?;

    let status = daemon.status().await?;
    let peer = status
        .network
        .known_peers
        .iter()
        .find(|p| p.peer_id == "peer-a")
        .ok_or_else(|| anyhow!("peer-a not found in known_peers"))?;
    assert_eq!(peer.display_name, "carol");

    Ok(())
}

#[tokio::test]
async fn forget_known_peer_removes_from_known_peers() -> Result<()> {
    let daemon = AppHarness::spawn("alice").await?;
    daemon
        .app
        .seed_peer_for_tests("peer-a", "bob", "/ip4/127.0.0.1/tcp/4011")
        .await;

    common::expect_ack(
        common::send_request(
            &daemon.app,
            ControlRequest::SaveKnownPeer {
                peer_id: "peer-a".to_string(),
            },
        )
        .await?,
    )?;
    common::expect_ack(
        common::send_request(
            &daemon.app,
            ControlRequest::ForgetKnownPeer {
                peer_id: "peer-a".to_string(),
            },
        )
        .await?,
    )?;

    let status = daemon.status().await?;
    assert!(
        !status.network.friends.iter().any(|p| p.peer_id == "peer-a"),
        "peer-a should not be in friends after ForgetKnownPeer"
    );
    assert!(
        status.network.ignored_peer_ids.contains(&"peer-a".to_string()),
        "peer-a should be in ignored_peer_ids after ForgetKnownPeer"
    );

    Ok(())
}

#[tokio::test]
async fn forget_known_peer_returns_error_for_unknown_peer() -> Result<()> {
    let daemon = AppHarness::spawn("alice").await?;

    let response = common::send_request(
        &daemon.app,
        ControlRequest::ForgetKnownPeer {
            peer_id: "nobody".to_string(),
        },
    )
    .await?;

    assert!(
        matches!(response, ControlResponse::Error { .. }),
        "expected Error response, got {response:?}"
    );

    Ok(())
}

#[tokio::test]
async fn rename_known_peer_returns_error_for_unknown_peer() -> Result<()> {
    let daemon = AppHarness::spawn("alice").await?;

    let response = common::send_request(
        &daemon.app,
        ControlRequest::RenameKnownPeer {
            peer_id: "nobody".to_string(),
            display_name: "carol".to_string(),
        },
    )
    .await?;

    assert!(
        matches!(response, ControlResponse::Error { .. }),
        "expected Error response, got {response:?}"
    );

    Ok(())
}

#[tokio::test]
async fn set_display_name_updates_session() -> Result<()> {
    let daemon = AppHarness::spawn("alice").await?;

    common::expect_ack(
        common::send_request(
            &daemon.app,
            ControlRequest::SetDisplayName {
                display_name: "newname".to_string(),
            },
        )
        .await?,
    )?;

    let status = daemon.status().await?;
    assert_eq!(status.session.display_name, "newname");

    Ok(())
}

#[tokio::test]
async fn set_display_name_empty_returns_error() -> Result<()> {
    let daemon = AppHarness::spawn("alice").await?;

    let response = common::send_request(
        &daemon.app,
        ControlRequest::SetDisplayName {
            display_name: "".to_string(),
        },
    )
    .await?;

    assert!(
        matches!(response, ControlResponse::Error { .. }),
        "expected Error response, got {response:?}"
    );

    Ok(())
}

#[tokio::test]
async fn rotate_invite_code_generates_new_code() -> Result<()> {
    let daemon = AppHarness::spawn("alice").await?;

    common::expect_ack(
        common::send_request(
            &daemon.app,
            ControlRequest::CreateRoom {
                room_name: "myroom".to_string(),
            },
        )
        .await?,
    )?;

    let status_before = daemon.status().await?;
    let code_before = status_before
        .rooms
        .iter()
        .find(|r| r.name == "myroom")
        .and_then(|r| r.current_invite.as_ref())
        .map(|i| i.invite_code.clone())
        .ok_or_else(|| anyhow!("no invite code on myroom before rotate"))?;

    common::expect_ack(
        common::send_request(&daemon.app, ControlRequest::RotateInviteCode).await?,
    )?;

    let status_after = daemon.status().await?;
    let code_after = status_after
        .rooms
        .iter()
        .find(|r| r.name == "myroom")
        .and_then(|r| r.current_invite.as_ref())
        .map(|i| i.invite_code.clone())
        .ok_or_else(|| anyhow!("no invite code on myroom after rotate"))?;

    assert_ne!(code_before, code_after, "invite code should change after RotateInviteCode");

    Ok(())
}

#[tokio::test]
async fn rotate_invite_code_returns_error_without_active_room() -> Result<()> {
    let daemon = AppHarness::spawn("alice").await?;

    let response =
        common::send_request(&daemon.app, ControlRequest::RotateInviteCode).await?;

    assert!(
        matches!(response, ControlResponse::Error { .. }),
        "expected Error response, got {response:?}"
    );

    Ok(())
}

#[tokio::test]
async fn toggle_mute_peer_unknown_returns_error() -> Result<()> {
    let daemon = AppHarness::spawn("alice").await?;

    let response = common::send_request(
        &daemon.app,
        ControlRequest::ToggleMutePeer {
            peer_id: "nobody".to_string(),
        },
    )
    .await?;

    assert!(
        matches!(response, ControlResponse::Error { .. }),
        "expected Error response, got {response:?}"
    );

    Ok(())
}

#[tokio::test]
async fn set_peer_volume_unknown_returns_error() -> Result<()> {
    let daemon = AppHarness::spawn("alice").await?;

    let response = common::send_request(
        &daemon.app,
        ControlRequest::SetPeerVolumePercent {
            peer_id: "nobody".to_string(),
            percent: 50,
        },
    )
    .await?;

    assert!(
        matches!(response, ControlResponse::Error { .. }),
        "expected Error response, got {response:?}"
    );

    Ok(())
}

#[tokio::test]
async fn set_input_gain_clamps_at_200() -> Result<()> {
    let daemon = AppHarness::spawn("alice").await?;

    common::expect_ack(
        common::send_request(
            &daemon.app,
            ControlRequest::SetInputGainPercent { percent: 255 },
        )
        .await?,
    )?;

    let status = daemon.status().await?;
    assert!(
        status.audio.input_gain_percent <= 200,
        "input_gain_percent should be clamped to 200, got {}",
        status.audio.input_gain_percent
    );

    Ok(())
}
