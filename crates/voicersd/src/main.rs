mod app;
mod audio_backend;
mod control;
mod media;
mod network;
mod persist;
#[cfg(feature = "webrtc-transport")]
mod webrtc_transport;

use anyhow::Result;
use app::{AppConfig, TurnServerConfig};

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
            "--relay-addr" => {
                if let Some(value) = args.next() {
                    config.relay_addr = Some(value);
                }
            }
            "--bootstrap-addr" => {
                if let Some(value) = args.next() {
                    config.bootstrap_addrs.push(value);
                }
            }
            "--stun-server" => {
                if let Some(value) = args.next() {
                    config.stun_servers.push(value);
                }
            }
            "--turn-server" => {
                if let Some(value) = args.next() {
                    if let Some(server) = parse_turn_server(&value) {
                        config.turn_servers.push(server);
                    }
                }
            }
            "--no-stun" => {
                config.enable_stun = false;
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

fn parse_turn_server(value: &str) -> Option<TurnServerConfig> {
    let mut parts = value.splitn(3, ',');
    Some(TurnServerConfig {
        url: parts.next()?.to_string(),
        username: parts.next().unwrap_or_default().to_string(),
        credential: parts.next().unwrap_or_default().to_string(),
    })
}
