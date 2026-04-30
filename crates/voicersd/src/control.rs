use anyhow::{Context, Result};
use futures::{SinkExt, StreamExt};
use tokio::net::{TcpListener, TcpStream};
use tokio_util::codec::{Framed, LinesCodec};
use voicers_core::ControlRequest;

use crate::app::App;

pub async fn serve(app: App) -> Result<()> {
    let control_addr = app.status().await.control_addr;
    let listener = TcpListener::bind(&control_addr)
        .await
        .with_context(|| format!("failed to bind control socket at {control_addr}"))?;

    loop {
        let (stream, _) = listener.accept().await?;
        let app = app.clone();

        tokio::spawn(async move {
            if let Err(error) = handle_connection(app, stream).await {
                eprintln!("control connection error: {error:#}");
            }
        });
    }
}

async fn handle_connection(app: App, stream: TcpStream) -> Result<()> {
    let mut framed = Framed::new(stream, LinesCodec::new());

    while let Some(line) = framed.next().await {
        let line = line?;
        let request: ControlRequest =
            serde_json::from_str(&line).context("failed to decode control request")?;
        let response = app.handle_request(request).await;
        let encoded = serde_json::to_string(&response).context("failed to encode response")?;

        framed.send(encoded).await?;
    }

    Ok(())
}
