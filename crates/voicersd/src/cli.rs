use crate::app::{AppConfig, TurnServerConfig};

pub fn parse_args() -> AppConfig {
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
            "--no-bootstrap" => {
                config.use_default_bootstrap_addrs = false;
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
            "--no-audio" => {
                config.enable_audio_io = false;
            }
            "--no-capture" => {
                config.enable_capture_input = false;
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
