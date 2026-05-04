use std::{
    path::PathBuf,
    time::{SystemTime, UNIX_EPOCH},
};

use anyhow::{anyhow, Result};
use futures::{SinkExt, StreamExt};
use tokio::io::{duplex, AsyncRead, AsyncWrite};
use tokio_util::codec::{Framed, LinesCodec};
use voicers_core::{ControlRequest, ControlResponse};
use voicersd::{app::App, control};

pub async fn send_request(app: &App, request: ControlRequest) -> Result<ControlResponse> {
    let (client, server) = duplex(4096);
    let app = app.clone();
    let server_task = tokio::spawn(async move { control::serve_stream(app, server).await });
    let mut framed = framed(client);
    framed
        .send(serde_json::to_string(&request)?)
        .await
        .map_err(|e| anyhow!("failed to send control request: {e}"))?;
    let line = framed
        .next()
        .await
        .ok_or_else(|| anyhow!("daemon closed control socket"))??;
    drop(framed);
    server_task.await??;
    Ok(serde_json::from_str(&line)?)
}

pub fn expect_ack(response: ControlResponse) -> Result<()> {
    match response {
        ControlResponse::Ack { .. } => Ok(()),
        ControlResponse::Error { message } => Err(anyhow!("daemon returned error: {message}")),
        other => Err(anyhow!("expected ack response, got {other:?}")),
    }
}

pub fn framed<T>(stream: T) -> Framed<T, LinesCodec>
where
    T: AsyncRead + AsyncWrite + Unpin,
{
    Framed::new(stream, LinesCodec::new())
}

pub fn unique_state_path(name: &str) -> PathBuf {
    let stamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    std::env::temp_dir().join(format!("voicersd-test-{name}-{stamp}.json"))
}
