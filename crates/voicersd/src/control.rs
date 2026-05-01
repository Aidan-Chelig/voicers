use anyhow::{Context, Result};
use futures::{SinkExt, StreamExt};
use tokio::{
    io::{AsyncRead, AsyncWrite},
    net::TcpListener,
};
use tokio_util::codec::{Framed, LinesCodec};
use voicers_core::ControlRequest;

use crate::app::App;

pub async fn serve(app: App) -> Result<()> {
    let control_addr = app.status().await.control_addr;
    let listener = TcpListener::bind(&control_addr)
        .await
        .with_context(|| format!("failed to bind control socket at {control_addr}"))?;

    serve_listener(app, listener).await
}

pub async fn serve_listener(app: App, listener: TcpListener) -> Result<()> {
    serve_loop(app, listener).await
}

pub async fn serve_stream<T>(app: App, stream: T) -> Result<()>
where
    T: AsyncRead + AsyncWrite + Unpin,
{
    handle_connection(app, stream).await
}

async fn serve_loop(app: App, listener: TcpListener) -> Result<()> {
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

async fn handle_connection<T>(app: App, stream: T) -> Result<()>
where
    T: AsyncRead + AsyncWrite + Unpin,
{
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
