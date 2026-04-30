mod app;
mod audio_backend;
mod control;
mod media;
mod network;
mod persist;

use anyhow::Result;
use app::AppConfig;

#[tokio::main]
async fn main() -> Result<()> {
    let app = app::App::bootstrap(parse_args()).await?;
    control::serve(app).await
}

fn parse_args() -> AppConfig {
    let mut config = AppConfig::default();
    let mut args = std::env::args().skip(1);

    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--control-addr" => {
                if let Some(value) = args.next() {
                    config.control_addr = value;
                }
            }
            "--listen-addr" => {
                if let Some(value) = args.next() {
                    config.listen_addr = value;
                }
            }
            "--display-name" => {
                if let Some(value) = args.next() {
                    config.display_name = value;
                }
            }
            "--state-path" => {
                if let Some(value) = args.next() {
                    config.state_path = value.into();
                }
            }
            _ => {}
        }
    }

    config
}
