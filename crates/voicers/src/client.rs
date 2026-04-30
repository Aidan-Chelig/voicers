use anyhow::{anyhow, Context, Result};
use futures::{SinkExt, StreamExt};
use tokio::net::TcpStream;
use tokio_util::codec::{Framed, LinesCodec};
use voicers_core::{ControlRequest, ControlResponse};

pub async fn send_request_to(
    control_addr: &str,
    request: ControlRequest,
) -> Result<ControlResponse> {
    let stream = TcpStream::connect(control_addr)
        .await
        .with_context(|| format!("failed to connect to daemon at {control_addr}"))?;
    let mut framed = Framed::new(stream, LinesCodec::new());
    let encoded = serde_json::to_string(&request).context("failed to encode request")?;

    framed.send(encoded).await?;

    let line = framed
        .next()
        .await
        .ok_or_else(|| anyhow!("daemon closed the control connection"))??;
    let response = serde_json::from_str(&line).context("failed to decode response")?;

    Ok(response)
}
