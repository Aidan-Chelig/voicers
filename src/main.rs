use tokio;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::stream::{Stream, StreamExt};
use tokio::net::TcpListener;
use tokio::sync::{watch, mpsc, Mutex};
use std::sync::Arc;
use std::collections::HashMap;
use anyhow::Error;

mod server;
mod voice;

#[tokio::main]
pub async fn main() -> Result<(), anyhow::Error> {
    let voice_rx = voice::start().await.unwrap();
    let _ = server::start(voice_rx).await;
    Ok(())
}