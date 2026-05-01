use anyhow::Result;
use voicersd::{app, cli, control};

#[tokio::main]
async fn main() -> Result<()> {
    let app = app::App::bootstrap(cli::parse_args()).await?;
    control::serve(app).await
}
