use std::{
    fs,
    path::PathBuf,
    time::{SystemTime, UNIX_EPOCH},
};

use anyhow::{anyhow, Context, Result};
use futures::{SinkExt, StreamExt};
use tokio::io::{duplex, AsyncRead, AsyncWrite};
use tokio_util::codec::{Framed, LinesCodec};
use voicers_core::{ControlRequest, ControlResponse, DaemonStatus};
use voicersd::{
    app::{App, AppConfig},
    control,
};

struct AppHarness {
    app: App,
    state_path: PathBuf,
}

impl AppHarness {
    async fn spawn(display_name: &str) -> Result<Self> {
        let state_path = unique_state_path(display_name);
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
        match send_request(&self.app, ControlRequest::GetStatus).await? {
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

    expect_ack(
        send_request(
            &daemon.app,
            ControlRequest::CreateRoom {
                room_name: "lab".to_string(),
            },
        )
        .await?,
    )?;
    expect_ack(send_request(&daemon.app, ControlRequest::ToggleMuteSelf).await?)?;

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

    expect_ack(
        send_request(
            &daemon.app,
            ControlRequest::ToggleMutePeer {
                peer_id: "peer-a".to_string(),
            },
        )
        .await?,
    )?;
    expect_ack(
        send_request(
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

async fn send_request(app: &App, request: ControlRequest) -> Result<ControlResponse> {
    let (client, server) = duplex(4096);
    let app = app.clone();
    let server_task = tokio::spawn(async move { control::serve_stream(app, server).await });
    let mut framed = framed(client);
    framed
        .send(serde_json::to_string(&request)?)
        .await
        .context("failed to send control request")?;
    let line = framed
        .next()
        .await
        .ok_or_else(|| anyhow!("daemon closed control socket"))??;
    drop(framed);
    server_task.await??;
    Ok(serde_json::from_str(&line)?)
}

fn expect_ack(response: ControlResponse) -> Result<()> {
    match response {
        ControlResponse::Ack { .. } => Ok(()),
        ControlResponse::Error { message } => Err(anyhow!("daemon returned error: {message}")),
        other => Err(anyhow!("expected ack response, got {other:?}")),
    }
}

fn framed<T>(stream: T) -> Framed<T, LinesCodec>
where
    T: AsyncRead + AsyncWrite + Unpin,
{
    Framed::new(stream, LinesCodec::new())
}

fn unique_state_path(name: &str) -> PathBuf {
    let stamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    std::env::temp_dir().join(format!("voicersd-test-{name}-{stamp}.json"))
}
