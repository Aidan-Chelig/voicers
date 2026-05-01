mod server;
mod voice;

#[tokio::main]
pub async fn main() -> Result<(), anyhow::Error> {
    let voice_rx = voice::start().await.unwrap();
    let _ = server::start(voice_rx).await;
    Ok(())
}
