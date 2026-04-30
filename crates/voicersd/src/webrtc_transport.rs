use std::{collections::HashMap, sync::Arc};

use anyhow::{Context, Result};
use bytes::Bytes;
use tokio::sync::{mpsc, oneshot, Mutex};
use voicers_core::{MediaFrame, WebRtcSignal};
#[cfg(test)]
use webrtc::api::setting_engine::SettingEngine;
use webrtc::{
    api::{interceptor_registry::register_default_interceptors, media_engine::MediaEngine, API},
    data_channel::{
        data_channel_init::RTCDataChannelInit, data_channel_message::DataChannelMessage,
        data_channel_state::RTCDataChannelState, RTCDataChannel,
    },
    ice_transport::{ice_candidate::RTCIceCandidateInit, ice_server::RTCIceServer},
    interceptor::registry::Registry,
    peer_connection::{
        configuration::RTCConfiguration, sdp::session_description::RTCSessionDescription,
        RTCPeerConnection,
    },
};
#[cfg(test)]
use webrtc_ice::{mdns::MulticastDnsMode, network_type::NetworkType};

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct WebRtcTransportConfig {
    pub stun_servers: Vec<String>,
    pub turn_servers: Vec<TurnServerConfig>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TurnServerConfig {
    pub url: String,
    pub username: String,
    pub credential: String,
}

#[derive(Debug, Clone)]
pub struct OutgoingWebRtcSignal {
    pub peer_id: String,
    pub signal: WebRtcSignal,
}

#[derive(Debug, Clone)]
pub struct IncomingWebRtcMediaFrame {
    pub peer_id: String,
    pub frame: MediaFrame,
}

#[derive(Debug, Clone)]
pub struct WebRtcConnectionStateUpdate {
    pub peer_id: String,
    pub state: String,
}

#[derive(Clone)]
pub struct WebRtcHandle {
    command_tx: mpsc::Sender<WebRtcCommand>,
}

enum WebRtcCommand {
    CreateOffer {
        peer_id: String,
        response_tx: oneshot::Sender<Result<WebRtcSignal>>,
    },
    HandleRemoteSignal {
        peer_id: String,
        signal: WebRtcSignal,
        response_tx: oneshot::Sender<Result<Vec<WebRtcSignal>>>,
    },
    SendMediaFrame {
        peer_id: String,
        frame: MediaFrame,
        response_tx: oneshot::Sender<Result<()>>,
    },
}

struct WebRtcRuntime {
    api: API,
    config: WebRtcTransportConfig,
    peers: HashMap<String, Arc<RTCPeerConnection>>,
    data_channels: SharedDataChannels,
    outgoing_tx: mpsc::Sender<OutgoingWebRtcSignal>,
    incoming_media_tx: mpsc::Sender<IncomingWebRtcMediaFrame>,
    connection_state_tx: mpsc::Sender<WebRtcConnectionStateUpdate>,
}

type SharedDataChannels = Arc<Mutex<HashMap<String, Arc<RTCDataChannel>>>>;

pub fn start(
    config: WebRtcTransportConfig,
) -> Result<(
    WebRtcHandle,
    mpsc::Receiver<OutgoingWebRtcSignal>,
    mpsc::Receiver<IncomingWebRtcMediaFrame>,
    mpsc::Receiver<WebRtcConnectionStateUpdate>,
)> {
    start_with_api(config, build_api()?)
}

fn start_with_api(
    config: WebRtcTransportConfig,
    api: API,
) -> Result<(
    WebRtcHandle,
    mpsc::Receiver<OutgoingWebRtcSignal>,
    mpsc::Receiver<IncomingWebRtcMediaFrame>,
    mpsc::Receiver<WebRtcConnectionStateUpdate>,
)> {
    let (command_tx, command_rx) = mpsc::channel(32);
    let (outgoing_tx, outgoing_rx) = mpsc::channel(64);
    let (incoming_media_tx, incoming_media_rx) = mpsc::channel(64);
    let (connection_state_tx, connection_state_rx) = mpsc::channel(64);

    tokio::spawn(run_runtime(
        WebRtcRuntime {
            api,
            config,
            peers: HashMap::new(),
            data_channels: Arc::new(Mutex::new(HashMap::new())),
            outgoing_tx,
            incoming_media_tx,
            connection_state_tx,
        },
        command_rx,
    ));

    Ok((
        WebRtcHandle { command_tx },
        outgoing_rx,
        incoming_media_rx,
        connection_state_rx,
    ))
}

impl WebRtcHandle {
    pub async fn create_offer(&self, peer_id: String) -> Result<WebRtcSignal> {
        let (response_tx, response_rx) = oneshot::channel();
        self.command_tx
            .send(WebRtcCommand::CreateOffer {
                peer_id,
                response_tx,
            })
            .await
            .context("failed to send WebRTC create-offer command")?;

        response_rx
            .await
            .context("WebRTC runtime dropped create-offer response")?
    }

    pub async fn handle_remote_signal(
        &self,
        peer_id: String,
        signal: WebRtcSignal,
    ) -> Result<Vec<WebRtcSignal>> {
        let (response_tx, response_rx) = oneshot::channel();
        self.command_tx
            .send(WebRtcCommand::HandleRemoteSignal {
                peer_id,
                signal,
                response_tx,
            })
            .await
            .context("failed to send WebRTC remote-signal command")?;

        response_rx
            .await
            .context("WebRTC runtime dropped remote-signal response")?
    }

    pub async fn send_media_frame(&self, peer_id: String, frame: MediaFrame) -> Result<()> {
        let (response_tx, response_rx) = oneshot::channel();
        self.command_tx
            .send(WebRtcCommand::SendMediaFrame {
                peer_id,
                frame,
                response_tx,
            })
            .await
            .context("failed to send WebRTC media-frame command")?;

        response_rx
            .await
            .context("WebRTC runtime dropped media-frame response")?
    }
}

async fn run_runtime(mut runtime: WebRtcRuntime, mut command_rx: mpsc::Receiver<WebRtcCommand>) {
    while let Some(command) = command_rx.recv().await {
        match command {
            WebRtcCommand::CreateOffer {
                peer_id,
                response_tx,
            } => {
                let response = runtime.create_offer(peer_id).await;
                let _ = response_tx.send(response);
            }
            WebRtcCommand::HandleRemoteSignal {
                peer_id,
                signal,
                response_tx,
            } => {
                let response = runtime.handle_remote_signal(peer_id, signal).await;
                let _ = response_tx.send(response);
            }
            WebRtcCommand::SendMediaFrame {
                peer_id,
                frame,
                response_tx,
            } => {
                let response = runtime.send_media_frame(&peer_id, frame).await;
                let _ = response_tx.send(response);
            }
        }
    }
}

impl WebRtcRuntime {
    async fn create_offer(&mut self, peer_id: String) -> Result<WebRtcSignal> {
        let peer = self.peer_connection(&peer_id).await?;
        let channel = peer
            .create_data_channel("voicers-media", Some(RTCDataChannelInit::default()))
            .await
            .context("failed to create WebRTC data channel")?;
        self.register_data_channel(&peer_id, channel).await;
        let offer = peer
            .create_offer(None)
            .await
            .context("failed to create WebRTC offer")?;
        #[cfg(test)]
        let mut gathering_complete = peer.gathering_complete_promise().await;
        peer.set_local_description(offer.clone())
            .await
            .context("failed to set local WebRTC offer")?;

        #[cfg(test)]
        {
            let _ = gathering_complete.recv().await;
            let offer = peer
                .local_description()
                .await
                .context("WebRTC local offer was not set")?;
            Ok(WebRtcSignal::Offer { sdp: offer.sdp })
        }
        #[cfg(not(test))]
        {
            Ok(WebRtcSignal::Offer { sdp: offer.sdp })
        }
    }

    async fn handle_remote_signal(
        &mut self,
        peer_id: String,
        signal: WebRtcSignal,
    ) -> Result<Vec<WebRtcSignal>> {
        match signal {
            WebRtcSignal::Offer { sdp } => {
                let peer = self.peer_connection(&peer_id).await?;
                let offer = RTCSessionDescription::offer(sdp)
                    .context("failed to parse remote WebRTC offer")?;
                peer.set_remote_description(offer)
                    .await
                    .context("failed to set remote WebRTC offer")?;

                let answer = peer
                    .create_answer(None)
                    .await
                    .context("failed to create WebRTC answer")?;
                #[cfg(test)]
                let mut gathering_complete = peer.gathering_complete_promise().await;
                peer.set_local_description(answer.clone())
                    .await
                    .context("failed to set local WebRTC answer")?;

                #[cfg(test)]
                {
                    let _ = gathering_complete.recv().await;
                    let answer = peer
                        .local_description()
                        .await
                        .context("WebRTC local answer was not set")?;
                    Ok(vec![WebRtcSignal::Answer { sdp: answer.sdp }])
                }
                #[cfg(not(test))]
                {
                    Ok(vec![WebRtcSignal::Answer { sdp: answer.sdp }])
                }
            }
            WebRtcSignal::Answer { sdp } => {
                let peer = self.peer_connection(&peer_id).await?;
                let answer = RTCSessionDescription::answer(sdp)
                    .context("failed to parse remote WebRTC answer")?;
                peer.set_remote_description(answer)
                    .await
                    .context("failed to set remote WebRTC answer")?;
                Ok(Vec::new())
            }
            WebRtcSignal::IceCandidate {
                candidate,
                sdp_mid,
                sdp_mline_index,
            } => {
                let peer = self.peer_connection(&peer_id).await?;
                peer.add_ice_candidate(RTCIceCandidateInit {
                    candidate,
                    sdp_mid,
                    sdp_mline_index,
                    username_fragment: None,
                })
                .await
                .context("failed to add remote WebRTC ICE candidate")?;
                Ok(Vec::new())
            }
            WebRtcSignal::IceComplete => Ok(Vec::new()),
        }
    }

    async fn send_media_frame(&self, peer_id: &str, frame: MediaFrame) -> Result<()> {
        let channel = {
            let channels = self.data_channels.lock().await;
            channels.get(peer_id).cloned()
        }
        .with_context(|| format!("WebRTC media channel is not available for {peer_id}"))?;

        if channel.ready_state() != RTCDataChannelState::Open {
            anyhow::bail!("WebRTC media channel for {peer_id} is not open");
        }

        let encoded = serde_json::to_vec(&frame).context("failed to encode WebRTC media frame")?;
        channel
            .send(&Bytes::from(encoded))
            .await
            .context("failed to send WebRTC media frame")?;

        Ok(())
    }

    async fn peer_connection(&mut self, peer_id: &str) -> Result<Arc<RTCPeerConnection>> {
        if let Some(peer) = self.peers.get(peer_id) {
            return Ok(Arc::clone(peer));
        }

        let peer = Arc::new(
            self.api
                .new_peer_connection(self.config.rtc_configuration())
                .await
                .context("failed to create WebRTC peer connection")?,
        );
        let outgoing_tx = self.outgoing_tx.clone();
        let candidate_peer_id = peer_id.to_string();
        peer.on_ice_candidate(Box::new(move |candidate| {
            let outgoing_tx = outgoing_tx.clone();
            let peer_id = candidate_peer_id.clone();
            Box::pin(async move {
                let signal = match candidate {
                    Some(candidate) => match candidate.to_json() {
                        Ok(candidate) => WebRtcSignal::IceCandidate {
                            candidate: candidate.candidate,
                            sdp_mid: candidate.sdp_mid,
                            sdp_mline_index: candidate.sdp_mline_index,
                        },
                        Err(_) => return,
                    },
                    None => WebRtcSignal::IceComplete,
                };
                let _ = outgoing_tx
                    .send(OutgoingWebRtcSignal { peer_id, signal })
                    .await;
            })
        }));
        let connection_state_tx = self.connection_state_tx.clone();
        let connection_peer_id = peer_id.to_string();
        peer.on_peer_connection_state_change(Box::new(move |state| {
            let connection_state_tx = connection_state_tx.clone();
            let peer_id = connection_peer_id.clone();
            Box::pin(async move {
                let _ = connection_state_tx
                    .send(WebRtcConnectionStateUpdate {
                        peer_id,
                        state: state.to_string(),
                    })
                    .await;
            })
        }));
        let data_channels = Arc::clone(&self.data_channels);
        let incoming_media_tx = self.incoming_media_tx.clone();
        let data_peer_id = peer_id.to_string();
        peer.on_data_channel(Box::new(move |channel| {
            let data_channels = Arc::clone(&data_channels);
            let incoming_media_tx = incoming_media_tx.clone();
            let peer_id = data_peer_id.clone();
            Box::pin(async move {
                register_data_channel_handlers(&peer_id, Arc::clone(&channel), incoming_media_tx);
                data_channels.lock().await.insert(peer_id, channel);
            })
        }));

        self.peers.insert(peer_id.to_string(), Arc::clone(&peer));
        Ok(peer)
    }

    async fn register_data_channel(&self, peer_id: &str, channel: Arc<RTCDataChannel>) {
        register_data_channel_handlers(
            peer_id,
            Arc::clone(&channel),
            self.incoming_media_tx.clone(),
        );
        self.data_channels
            .lock()
            .await
            .insert(peer_id.to_string(), channel);
    }
}

fn register_data_channel_handlers(
    peer_id: &str,
    channel: Arc<RTCDataChannel>,
    incoming_media_tx: mpsc::Sender<IncomingWebRtcMediaFrame>,
) {
    let peer_id = peer_id.to_string();
    channel.on_message(Box::new(move |message: DataChannelMessage| {
        let incoming_media_tx = incoming_media_tx.clone();
        let peer_id = peer_id.clone();
        Box::pin(async move {
            let Ok(frame) = serde_json::from_slice::<MediaFrame>(message.data.as_ref()) else {
                return;
            };
            let _ = incoming_media_tx
                .send(IncomingWebRtcMediaFrame { peer_id, frame })
                .await;
        })
    }));
}

impl WebRtcTransportConfig {
    pub fn rtc_configuration(&self) -> RTCConfiguration {
        let mut ice_servers = Vec::new();

        if !self.stun_servers.is_empty() {
            ice_servers.push(RTCIceServer {
                urls: self
                    .stun_servers
                    .iter()
                    .map(|server| ice_url("stun", server))
                    .collect(),
                ..Default::default()
            });
        }

        ice_servers.extend(self.turn_servers.iter().map(|server| RTCIceServer {
            urls: vec![ice_url("turn", &server.url)],
            username: server.username.clone(),
            credential: server.credential.clone(),
        }));

        RTCConfiguration {
            ice_servers,
            ..Default::default()
        }
    }
}

fn build_api() -> Result<API> {
    let mut media_engine = MediaEngine::default();
    media_engine
        .register_default_codecs()
        .context("failed to register WebRTC default codecs")?;
    let registry = register_default_interceptors(Registry::new(), &mut media_engine)
        .context("failed to register WebRTC default interceptors")?;

    let builder = webrtc::api::APIBuilder::new()
        .with_media_engine(media_engine)
        .with_interceptor_registry(registry);

    #[cfg(test)]
    let builder = {
        let mut setting_engine = SettingEngine::default();
        setting_engine.set_include_loopback_candidate(true);
        setting_engine.set_ice_multicast_dns_mode(MulticastDnsMode::Disabled);
        setting_engine.set_network_types(vec![NetworkType::Udp4, NetworkType::Udp6]);
        builder.with_setting_engine(setting_engine)
    };

    Ok(builder.build())
}

fn ice_url(default_scheme: &str, value: &str) -> String {
    if value.starts_with("stun:")
        || value.starts_with("stuns:")
        || value.starts_with("turn:")
        || value.starts_with("turns:")
    {
        value.to_string()
    } else {
        format!("{default_scheme}:{value}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::time::{sleep, timeout, Duration, Instant};
    use voicers_core::MediaFrameKind;
    use webrtc_util::vnet::{
        net::{Net, NetConfig},
        router::{Router, RouterConfig},
    };

    #[test]
    fn builds_webrtc_ice_server_configuration() {
        let config = WebRtcTransportConfig {
            stun_servers: vec![
                "stun.l.google.com:19302".to_string(),
                "stun:stun.example.net:3478".to_string(),
            ],
            turn_servers: vec![TurnServerConfig {
                url: "turn.example.net:3478".to_string(),
                username: "user".to_string(),
                credential: "pass".to_string(),
            }],
        };

        let rtc = config.rtc_configuration();

        assert_eq!(rtc.ice_servers.len(), 2);
        assert_eq!(
            rtc.ice_servers[0].urls,
            vec![
                "stun:stun.l.google.com:19302".to_string(),
                "stun:stun.example.net:3478".to_string()
            ]
        );
        assert_eq!(rtc.ice_servers[1].urls, vec!["turn:turn.example.net:3478"]);
        assert_eq!(rtc.ice_servers[1].username, "user");
        assert_eq!(rtc.ice_servers[1].credential, "pass");
    }

    #[tokio::test]
    async fn creates_local_offer() {
        let (handle, _outgoing_rx, _incoming_media_rx, _connection_state_rx) =
            start(WebRtcTransportConfig::default()).unwrap();
        let signal = handle.create_offer("peer-a".to_string()).await.unwrap();

        match signal {
            WebRtcSignal::Offer { sdp } => {
                assert!(sdp.contains("v=0"));
                assert!(sdp.contains("m=application"));
            }
            other => panic!("expected offer, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn vnet_data_channel_transports_media_frame() {
        let (alice_api, bob_api, _router) = vnet_apis().await.unwrap();
        let (alice, mut alice_signals, _alice_media, mut alice_states) =
            start_with_api(WebRtcTransportConfig::default(), alice_api).unwrap();
        let (bob, mut bob_signals, mut bob_media, mut bob_states) =
            start_with_api(WebRtcTransportConfig::default(), bob_api).unwrap();

        let offer = alice.create_offer("bob".to_string()).await.unwrap();
        let WebRtcSignal::Offer { sdp: offer_sdp } = &offer else {
            panic!("expected offer");
        };
        assert!(
            offer_sdp.contains("a=candidate:"),
            "offer has no candidate: {offer_sdp}"
        );
        let replies = bob
            .handle_remote_signal("alice".to_string(), offer)
            .await
            .unwrap();
        for signal in &replies {
            if let WebRtcSignal::Answer { sdp } = signal {
                assert!(
                    sdp.contains("a=candidate:"),
                    "answer has no candidate: {sdp}"
                );
            }
        }
        for signal in replies {
            alice
                .handle_remote_signal("bob".to_string(), signal)
                .await
                .unwrap();
        }

        let frame = MediaFrame {
            sequence: 42,
            timestamp_ms: 1234,
            timestamp_samples: 0,
            frame_kind: MediaFrameKind::AudioOpus,
            sample_rate_hz: 48_000,
            channels: 1,
            frame_duration_ms: 20,
            payload: vec![1, 2, 3, 4],
        };

        let deadline = Instant::now() + Duration::from_secs(5);
        let mut alice_signal_count = 0usize;
        let mut bob_signal_count = 0usize;
        let mut state_log = Vec::new();
        loop {
            while let Ok(signal) = alice_signals.try_recv() {
                alice_signal_count += 1;
                bob.handle_remote_signal("alice".to_string(), signal.signal)
                    .await
                    .unwrap();
            }
            while let Ok(signal) = bob_signals.try_recv() {
                bob_signal_count += 1;
                alice
                    .handle_remote_signal("bob".to_string(), signal.signal)
                    .await
                    .unwrap();
            }
            while let Ok(state) = alice_states.try_recv() {
                state_log.push(format!("alice:{}:{}", state.peer_id, state.state));
            }
            while let Ok(state) = bob_states.try_recv() {
                state_log.push(format!("bob:{}:{}", state.peer_id, state.state));
            }

            if alice
                .send_media_frame("bob".to_string(), frame.clone())
                .await
                .is_ok()
            {
                break;
            }

            assert!(
                Instant::now() < deadline,
                "timed out waiting for WebRTC data channel to open; alice signals: {alice_signal_count}, bob signals: {bob_signal_count}, states: {state_log:?}"
            );
            sleep(Duration::from_millis(20)).await;
        }

        let incoming = timeout(Duration::from_secs(3), bob_media.recv())
            .await
            .expect("timed out waiting for media frame")
            .expect("media channel closed");
        assert_eq!(incoming.peer_id, "alice");
        assert_eq!(incoming.frame.sequence, frame.sequence);
        assert_eq!(incoming.frame.payload, frame.payload);
    }

    async fn vnet_apis() -> Result<(API, API, Arc<Mutex<Router>>)> {
        let router = Arc::new(Mutex::new(Router::new(RouterConfig {
            cidr: "1.2.3.0/24".to_string(),
            ..Default::default()
        })?));
        let alice_net = Arc::new(Net::new(Some(NetConfig {
            static_ips: vec!["1.2.3.4".to_string()],
            ..Default::default()
        })));
        let bob_net = Arc::new(Net::new(Some(NetConfig {
            static_ips: vec!["1.2.3.5".to_string()],
            ..Default::default()
        })));

        attach_net(&router, &alice_net).await?;
        attach_net(&router, &bob_net).await?;
        router.lock().await.start().await?;

        Ok((build_vnet_api(alice_net)?, build_vnet_api(bob_net)?, router))
    }

    async fn attach_net(router: &Arc<Mutex<Router>>, net: &Arc<Net>) -> Result<()> {
        let nic = net.get_nic()?;
        router.lock().await.add_net(Arc::clone(&nic)).await?;
        nic.lock().await.set_router(Arc::clone(router)).await?;
        Ok(())
    }

    fn build_vnet_api(net: Arc<Net>) -> Result<API> {
        let mut media_engine = MediaEngine::default();
        media_engine
            .register_default_codecs()
            .context("failed to register WebRTC default codecs")?;
        let registry = register_default_interceptors(Registry::new(), &mut media_engine)
            .context("failed to register WebRTC default interceptors")?;

        let mut setting_engine = SettingEngine::default();
        setting_engine.set_vnet(Some(net));
        setting_engine.set_ice_multicast_dns_mode(MulticastDnsMode::Disabled);
        setting_engine.set_ice_timeouts(
            Some(Duration::from_secs(1)),
            Some(Duration::from_secs(1)),
            Some(Duration::from_millis(200)),
        );

        Ok(webrtc::api::APIBuilder::new()
            .with_media_engine(media_engine)
            .with_interceptor_registry(registry)
            .with_setting_engine(setting_engine)
            .build())
    }
}
