#[path = "media/dsp.rs"]
mod dsp;
#[path = "media/jitter.rs"]
mod jitter;

use std::{
    collections::{HashMap, VecDeque},
    convert::TryInto,
    f32::consts::PI,
    sync::mpsc as std_mpsc,
    sync::Arc,
    thread,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use anyhow::{anyhow, Context, Result};
use audiopus::{
    coder::{Decoder, Encoder},
    packet::Packet,
    Application, Bitrate, Channels, MutSignals, SampleRate, Signal,
};
use cpal::{
    traits::{DeviceTrait, HostTrait, StreamTrait},
    SampleFormat, SampleRate as CpalSampleRate, Stream, StreamConfig, SupportedStreamConfig,
};
use tokio::{
    sync::{mpsc, oneshot, RwLock},
    time::{self, MissedTickBehavior},
};
use voicers_core::{
    AudioEngineStage, DaemonStatus, MediaAck, MediaFrame, MediaFrameKind, MediaStreamState,
    PeerMediaState,
};

use crate::audio_backend::{self, OutputBackendHandle};

const JITTER_TARGET_DEPTH: usize = 3;
const JITTER_MIN_DEPTH: usize = 2;
const JITTER_MAX_TARGET_DEPTH: usize = 6;
const JITTER_MAX_DEPTH: usize = 8;
const DRIFT_REBASE_THRESHOLD_FRAMES: u64 = 4;
const PLAYOUT_TICK_MS: u64 = 5;
const PLAYOUT_LATE_GRACE_MS: u64 = 5;
const MAX_PLAYOUT_BURST_FRAMES: usize = 4;
const STABLE_PLAYOUT_TICKS_BEFORE_SHRINK: u32 = 50;
const MAX_CAPTURE_QUEUE_DEPTH: usize = 8;
const SAMPLE_RATE_HZ: u32 = 48_000;
const CHANNELS: u8 = 1;
const FRAME_DURATION_MS: u16 = 20;
const FRAME_SAMPLES: usize = 960;
const FRAME_SAMPLES_U64: u64 = FRAME_SAMPLES as u64;
const MAX_PACKET_BYTES: usize = 512;
const TONE_AMPLITUDE: f32 = 0.15;
const TONE_BASE_HZ: f32 = 440.0;
const OPUS_BITRATE_BPS: i32 = 24_000;
const OPUS_EXPECTED_LOSS_PCT: u8 = 8;
const OPUS_COMPLEXITY: u8 = 10;
const HPF_ALPHA: f32 = 0.995;
const NOISE_FLOOR_ATTACK: f32 = 0.02;
const NOISE_FLOOR_DECAY: f32 = 0.001;
const GATE_OPEN_MULTIPLIER: f32 = 2.2;
const GATE_CLOSE_MULTIPLIER: f32 = 1.4;
const GATE_FLOOR_GAIN: f32 = 0.12;
const GATE_GAIN_ATTACK: f32 = 0.35;
const GATE_GAIN_RELEASE: f32 = 0.08;

#[derive(Clone)]
pub struct MediaHandle {
    command_tx: mpsc::Sender<MediaCommand>,
}

impl MediaHandle {
    pub async fn register_peer(&self, peer_id: String) -> Result<()> {
        self.send(MediaCommand::RegisterPeer { peer_id }).await
    }

    pub async fn disconnect_peer(&self, peer_id: String) -> Result<()> {
        self.send(MediaCommand::DisconnectPeer { peer_id }).await
    }

    pub async fn build_next_audio_frame(&self, peer_id: String) -> Result<MediaFrame> {
        let (response_tx, response_rx) = oneshot::channel();
        self.command_tx
            .send(MediaCommand::BuildNextAudioFrame {
                peer_id,
                response_tx,
            })
            .await
            .context("failed to request audio frame from media engine")?;
        response_rx
            .await
            .context("media engine dropped audio frame response")?
    }

    pub async fn handle_incoming_frame(
        &self,
        peer_id: String,
        frame: MediaFrame,
    ) -> Result<MediaAck> {
        let (response_tx, response_rx) = oneshot::channel();
        self.command_tx
            .send(MediaCommand::HandleIncomingFrame {
                peer_id,
                frame,
                response_tx,
            })
            .await
            .context("failed to queue incoming media frame")?;
        response_rx
            .await
            .context("media engine dropped incoming frame response")?
    }

    pub async fn handle_ack(&self, peer_id: String, ack: MediaAck) -> Result<()> {
        self.send(MediaCommand::HandleAck { peer_id, ack }).await
    }

    pub async fn set_input_gain_percent(&self, percent: u8) -> Result<()> {
        let (response_tx, response_rx) = oneshot::channel();
        self.command_tx
            .send(MediaCommand::SetInputGainPercent {
                percent,
                response_tx,
            })
            .await
            .context("failed to send input gain command")?;
        response_rx
            .await
            .context("media engine dropped input gain response")?
    }

    pub async fn select_capture_device(&self, device_name: String) -> Result<String> {
        let (response_tx, response_rx) = oneshot::channel();
        self.command_tx
            .send(MediaCommand::SelectCaptureDevice {
                device_name,
                response_tx,
            })
            .await
            .context("failed to send capture device command")?;
        response_rx
            .await
            .context("media engine dropped capture device response")?
    }

    async fn send(&self, command: MediaCommand) -> Result<()> {
        self.command_tx
            .send(command)
            .await
            .context("failed to send media command")
    }
}

enum MediaCommand {
    RegisterPeer {
        peer_id: String,
    },
    DisconnectPeer {
        peer_id: String,
    },
    BuildNextAudioFrame {
        peer_id: String,
        response_tx: oneshot::Sender<Result<MediaFrame>>,
    },
    HandleIncomingFrame {
        peer_id: String,
        frame: MediaFrame,
        response_tx: oneshot::Sender<Result<MediaAck>>,
    },
    HandleAck {
        peer_id: String,
        ack: MediaAck,
    },
    SetInputGainPercent {
        percent: u8,
        response_tx: oneshot::Sender<Result<()>>,
    },
    SelectCaptureDevice {
        device_name: String,
        response_tx: oneshot::Sender<Result<String>>,
    },
}

struct MediaPeerRuntime {
    next_send_sequence: u64,
    next_send_timestamp_samples: u64,
    decoder: Decoder,
    encoder: Encoder,
    jitter: JitterBuffer,
    decoded_frames: usize,
    phase_radians: f32,
    tone_hz: f32,
    output_registered: bool,
}

struct CaptureChunk {
    sample_rate_hz: u32,
    samples: Vec<f32>,
}

struct CapturePipeline {
    sample_rate_hz: u32,
    input_samples: Vec<f32>,
    output_cursor: f32,
    frame_assembly: Vec<f32>,
    ready_frames: VecDeque<Vec<f32>>,
    last_frame: Vec<f32>,
    voice_processor: VoiceProcessorChain,
    input_gain: f32,
}

struct CaptureBootstrap {
    device_name: String,
    device_sample_rate_hz: u32,
    chunk_rx: mpsc::Receiver<CaptureChunk>,
    runtime: CaptureRuntime,
}

struct CaptureRuntime {
    stop_tx: std_mpsc::Sender<()>,
    thread: Option<thread::JoinHandle<()>>,
}

struct VoiceProcessorChain {
    high_pass: HighPassFilter,
    noise_gate: AdaptiveNoiseGate,
}

struct HighPassFilter {
    alpha: f32,
    prev_input: f32,
    prev_output: f32,
}

struct AdaptiveNoiseGate {
    noise_floor_rms: f32,
    gain: f32,
}

#[derive(Debug)]
struct EncodedPacket {
    timestamp_samples: u64,
    payload: Vec<u8>,
}

struct JitterBuffer {
    pending: std::collections::BTreeMap<u64, EncodedPacket>,
    next_playout_timestamp_samples: Option<u64>,
    next_playout_deadline: Option<Instant>,
    target_depth: usize,
    stable_playout_ticks: u32,
    late_packets: u64,
    lost_packets: u64,
    concealed_frames: u64,
    drift_corrections: u64,
}

#[derive(Debug)]
enum PlayoutDecision {
    Hold,
    Packet(EncodedPacket),
    Fec { recovery_packet: Vec<u8> },
    Conceal,
}

impl Drop for CaptureRuntime {
    fn drop(&mut self) {
        let _ = self.stop_tx.send(());
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

pub async fn start(
    state: Arc<RwLock<DaemonStatus>>,
    enable_audio_io: bool,
    enable_capture_input: bool,
) -> MediaHandle {
    let output_backend = if enable_audio_io {
        audio_backend::start(Arc::clone(&state)).await
    } else {
        audio_backend::start_noop()
    };
    let capture_enabled = enable_audio_io && enable_capture_input;
    let available_capture_devices = if capture_enabled {
        enumerate_input_device_names()
    } else {
        Vec::new()
    };
    let (command_tx, command_rx) = mpsc::channel(64);
    let capture_bootstrap = if capture_enabled {
        match build_capture_stream(None) {
            Ok(bootstrap) => Some(bootstrap),
            Err(error) => {
                let mut state = state.write().await;
                state.audio.engine = AudioEngineStage::Live;
                state.audio.source = Some("generated-tone-fallback".to_string());
                insert_unique_note(
                    &mut state,
                    format!("microphone capture unavailable, using fallback tone: {error}"),
                );
                None
            }
        }
    } else if enable_audio_io {
        let mut state = state.write().await;
        state.audio.engine = AudioEngineStage::Live;
        state.audio.source = Some("generated-tone-fallback".to_string());
        insert_unique_note(
            &mut state,
            "microphone capture disabled for this daemon instance; using fallback tone".to_string(),
        );
        None
    } else {
        let mut state = state.write().await;
        state.audio.engine = AudioEngineStage::QueueingOnly;
        state.audio.source = Some("disabled-for-tests".to_string());
        insert_unique_note(
            &mut state,
            "audio device integration disabled for this daemon instance".to_string(),
        );
        None
    };

    {
        let mut state = state.write().await;
        state.audio.sample_rate_hz = Some(SAMPLE_RATE_HZ);
        state.audio.frame_size_ms = Some(FRAME_DURATION_MS);
        state.audio.codec = Some("opus/20ms-voip-fec + hpf/gate".to_string());
        state.audio.available_capture_devices = available_capture_devices;
        state.audio.input_gain_percent = 100;

        if let Some(capture) = &capture_bootstrap {
            state.audio.capture_device = Some(capture.device_name.clone());
            state.audio.engine = AudioEngineStage::Live;
            state.audio.source = Some("microphone".to_string());
            insert_unique_note(
                &mut state,
                format!(
                    "capturing microphone from {} at {} Hz",
                    capture.device_name, capture.device_sample_rate_hz
                ),
            );
            insert_unique_note(
                &mut state,
                "voice processing active: high-pass filter + adaptive noise gate".to_string(),
            );
        }
    }

    let (capture_rx, capture_runtime) = match capture_bootstrap {
        Some(capture) => (capture.chunk_rx, Some(capture.runtime)),
        None => {
            let (chunk_tx, chunk_rx) = mpsc::channel(1);
            drop(chunk_tx);
            (chunk_rx, None)
        }
    };

    tokio::spawn(run_media_engine(
        state,
        command_rx,
        capture_rx,
        output_backend,
        capture_runtime,
    ));
    MediaHandle { command_tx }
}

async fn run_media_engine(
    state: Arc<RwLock<DaemonStatus>>,
    mut command_rx: mpsc::Receiver<MediaCommand>,
    mut capture_rx: mpsc::Receiver<CaptureChunk>,
    output_backend: OutputBackendHandle,
    mut capture_runtime: Option<CaptureRuntime>,
) {
    let mut peers: HashMap<String, MediaPeerRuntime> = HashMap::new();
    let mut capture = CapturePipeline::new();
    let mut playout_tick = time::interval(Duration::from_millis(PLAYOUT_TICK_MS));
    playout_tick.set_missed_tick_behavior(MissedTickBehavior::Skip);

    loop {
        tokio::select! {
            _ = playout_tick.tick() => {
                drive_playout(&state, &mut peers, &output_backend).await;
            }
            Some(chunk) = capture_rx.recv() => {
                capture.ingest(chunk);
            }
            Some(command) = command_rx.recv() => {
                match command {
                    MediaCommand::RegisterPeer { peer_id } => {
                        if let Err(error) = ensure_peer_runtime(&mut peers, &peer_id) {
                            eprintln!("media register error for {peer_id}: {error:#}");
                            continue;
                        }
                        let mut state = state.write().await;
                        update_peer_media(&mut state, &peer_id, |media| {
                            media.stream_state = MediaStreamState::Primed;
                        });
                    }
                    MediaCommand::DisconnectPeer { peer_id } => {
                        peers.remove(&peer_id);
                        let _ = output_backend.remove_peer_output(peer_id.clone()).await;
                        let mut state = state.write().await;
                        update_peer_media(&mut state, &peer_id, |media| {
                            media.stream_state = MediaStreamState::Idle;
                            media.queued_packets = 0;
                            media.decoded_frames = 0;
                            media.queued_samples = 0;
                        });
                    }
                    MediaCommand::BuildNextAudioFrame {
                        peer_id,
                        response_tx,
                    } => {
                        let result = build_next_audio_frame(
                            &state,
                            &mut peers,
                            &mut capture,
                            &peer_id,
                            capture_runtime.is_some(),
                        )
                        .await;
                        let _ = response_tx.send(result);
                    }
                    MediaCommand::HandleIncomingFrame {
                        peer_id,
                        frame,
                        response_tx,
                    } => {
                        let result = handle_incoming_frame(&state, &mut peers, &peer_id, frame).await;
                        let _ = response_tx.send(result);
                    }
                    MediaCommand::HandleAck { peer_id, ack } => {
                        let mut state = state.write().await;
                        update_peer_media(&mut state, &peer_id, |media| {
                            media.stream_state = MediaStreamState::Active;
                            media.last_sequence = Some(ack.accepted_sequence);
                            media.queued_packets = ack.queue_depth;
                            media.queued_samples = ack.queued_samples;
                        });
                    }
                    MediaCommand::SetInputGainPercent { percent, response_tx } => {
                        capture.set_input_gain(percent_to_gain(percent));
                        let _ = response_tx.send(Ok(()));
                    }
                    MediaCommand::SelectCaptureDevice {
                        device_name,
                        response_tx,
                    } => {
                        let result = switch_capture_device(
                            &state,
                            &mut capture,
                            &mut capture_rx,
                            &mut capture_runtime,
                            &device_name,
                        )
                        .await;
                        let _ = response_tx.send(result);
                    }
                }
            }
            else => break,
        }
    }
}

async fn build_next_audio_frame(
    state: &Arc<RwLock<DaemonStatus>>,
    peers: &mut HashMap<String, MediaPeerRuntime>,
    capture: &mut CapturePipeline,
    peer_id: &str,
    has_live_capture: bool,
) -> Result<MediaFrame> {
    let runtime = ensure_peer_runtime(peers, peer_id)?;
    let sequence = runtime.next_send_sequence;
    runtime.next_send_sequence += 1;
    let timestamp_samples = runtime.next_send_timestamp_samples;
    runtime.next_send_timestamp_samples += FRAME_SAMPLES_U64;

    let pcm = capture
        .take_ready_frame()
        .or_else(|| {
            if has_live_capture {
                capture.last_frame()
            } else {
                None
            }
        })
        .unwrap_or_else(|| {
            if has_live_capture {
                silence_frame()
            } else {
                synthesize_frame(runtime)
            }
        });

    let mut packet = [0_u8; MAX_PACKET_BYTES];
    let encoded_len = runtime
        .encoder
        .encode_float(&pcm, &mut packet)
        .context("failed to encode opus frame")?;

    let frame = MediaFrame {
        sequence,
        timestamp_ms: now_ms(),
        timestamp_samples,
        frame_kind: MediaFrameKind::AudioOpus,
        sample_rate_hz: SAMPLE_RATE_HZ,
        channels: CHANNELS,
        frame_duration_ms: FRAME_DURATION_MS,
        payload: packet[..encoded_len].to_vec(),
    };

    let mut state = state.write().await;
    let tx_level_rms = rms_level(&pcm);
    update_peer_media(&mut state, peer_id, |media| {
        media.sent_packets += 1;
        media.tx_level_rms = tx_level_rms;
        media.last_sequence = Some(sequence);
        if matches!(media.stream_state, MediaStreamState::Idle) {
            media.stream_state = MediaStreamState::Primed;
        }
    });

    Ok(frame)
}

async fn handle_incoming_frame(
    state: &Arc<RwLock<DaemonStatus>>,
    peers: &mut HashMap<String, MediaPeerRuntime>,
    peer_id: &str,
    frame: MediaFrame,
) -> Result<MediaAck> {
    let runtime = ensure_peer_runtime(peers, peer_id)?;

    match frame.frame_kind {
        MediaFrameKind::Probe => {}
        MediaFrameKind::AudioOpus => {
            runtime.jitter.insert(EncodedPacket {
                timestamp_samples: normalize_timestamp_samples(&frame),
                payload: frame.payload,
            });
        }
    }

    let ack = MediaAck {
        accepted_sequence: frame.sequence,
        queue_depth: runtime.jitter.queue_depth(),
        queued_samples: runtime.jitter.queued_samples(),
    };

    let mut state = state.write().await;
    update_peer_media(&mut state, peer_id, |media| {
        media.received_packets += 1;
        media.late_packets = runtime.jitter.late_packets;
        media.lost_packets = runtime.jitter.lost_packets;
        media.concealed_frames = runtime.jitter.concealed_frames;
        media.drift_corrections = runtime.jitter.drift_corrections;
        media.queued_packets = runtime.jitter.queue_depth();
        media.decoded_frames = runtime.decoded_frames;
        media.queued_samples = runtime.jitter.queued_samples();
        media.last_sequence = Some(frame.sequence);
        media.stream_state = runtime.jitter.stream_state();
    });

    Ok(ack)
}

async fn drive_playout(
    state: &Arc<RwLock<DaemonStatus>>,
    peers: &mut HashMap<String, MediaPeerRuntime>,
    output_backend: &OutputBackendHandle,
) {
    let peer_ids: Vec<String> = peers.keys().cloned().collect();
    let now = Instant::now();

    for peer_id in peer_ids {
        let Some(runtime) = peers.get_mut(&peer_id) else {
            continue;
        };

        let mut last_rx_level_rms = None;
        for _ in 0..MAX_PLAYOUT_BURST_FRAMES {
            let pcm = match runtime.jitter.next_playout_decision(now) {
                PlayoutDecision::Hold => None,
                PlayoutDecision::Packet(packet) => {
                    match decode_packet(&mut runtime.decoder, &packet.payload, false) {
                        Ok(pcm) => Some(pcm),
                        Err(error) => {
                            eprintln!("decode error for {peer_id}: {error:#}");
                            decode_packet_loss(&mut runtime.decoder).ok()
                        }
                    }
                }
                PlayoutDecision::Fec { recovery_packet } => {
                    match decode_packet(&mut runtime.decoder, &recovery_packet, true) {
                        Ok(pcm) => Some(pcm),
                        Err(error) => {
                            eprintln!("fec decode error for {peer_id}: {error:#}");
                            decode_packet_loss(&mut runtime.decoder).ok()
                        }
                    }
                }
                PlayoutDecision::Conceal => decode_packet_loss(&mut runtime.decoder).ok(),
            };

            let Some(pcm) = pcm else {
                break;
            };

            let peer_gain = peer_output_gain(state, &peer_id).await;
            let pcm = apply_output_gain(pcm, peer_gain);
            let rx_level_rms = rms_level(&pcm);
            last_rx_level_rms = Some(rx_level_rms);
            runtime.decoded_frames += 1;
            if !runtime.output_registered {
                let (output_bus, display_name) = peer_output_metadata(state, &peer_id).await;
                if output_backend
                    .register_peer_output(peer_id.clone(), output_bus, display_name)
                    .await
                    .is_ok()
                {
                    runtime.output_registered = true;
                }
            }
            let _ = output_backend.push_peer_pcm(peer_id.clone(), pcm).await;
        }

        if let Some(rx_level_rms) = last_rx_level_rms {
            let mut daemon_state = state.write().await;
            update_peer_media(&mut daemon_state, &peer_id, |media| {
                media.rx_level_rms = rx_level_rms;
            });
        }

        let mut daemon_state = state.write().await;
        update_peer_media(&mut daemon_state, &peer_id, |media| {
            media.late_packets = runtime.jitter.late_packets;
            media.lost_packets = runtime.jitter.lost_packets;
            media.concealed_frames = runtime.jitter.concealed_frames;
            media.drift_corrections = runtime.jitter.drift_corrections;
            media.queued_packets = runtime.jitter.queue_depth();
            media.decoded_frames = runtime.decoded_frames;
            media.queued_samples = runtime.jitter.queued_samples();
            media.stream_state = runtime.jitter.stream_state();
        });
    }
}

fn ensure_peer_runtime<'a>(
    peers: &'a mut HashMap<String, MediaPeerRuntime>,
    peer_id: &str,
) -> Result<&'a mut MediaPeerRuntime> {
    if !peers.contains_key(peer_id) {
        let encoder = build_encoder()?;
        let decoder = build_decoder()?;
        peers.insert(
            peer_id.to_string(),
            MediaPeerRuntime {
                next_send_sequence: 1,
                next_send_timestamp_samples: 0,
                decoder,
                encoder,
                jitter: JitterBuffer::new(),
                decoded_frames: 0,
                phase_radians: 0.0,
                tone_hz: TONE_BASE_HZ + peer_tone_offset(peer_id),
                output_registered: false,
            },
        );
    }

    peers
        .get_mut(peer_id)
        .context("media runtime missing after insertion")
}

async fn peer_output_metadata(
    state: &Arc<RwLock<DaemonStatus>>,
    peer_id: &str,
) -> (String, String) {
    let state = state.read().await;
    state
        .peers
        .iter()
        .find(|peer| peer.peer_id == peer_id)
        .map(|peer| (peer.output_bus.clone(), peer.display_name.clone()))
        .unwrap_or_else(|| {
            (
                format!("peer_bus_{}", short_peer_id(peer_id)),
                format!("peer {}", short_peer_id(peer_id)),
            )
        })
}

fn build_encoder() -> Result<Encoder> {
    let mut encoder = Encoder::new(SampleRate::Hz48000, Channels::Mono, Application::Voip)
        .context("failed to construct opus encoder")?;
    encoder
        .set_signal(Signal::Voice)
        .context("failed to set opus signal type")?;
    encoder
        .set_bitrate(Bitrate::BitsPerSecond(OPUS_BITRATE_BPS))
        .context("failed to set opus bitrate")?;
    encoder
        .set_complexity(OPUS_COMPLEXITY)
        .context("failed to set opus complexity")?;
    encoder.set_vbr(true).context("failed to enable opus vbr")?;
    encoder
        .set_inband_fec(true)
        .context("failed to enable opus fec")?;
    encoder
        .set_packet_loss_perc(OPUS_EXPECTED_LOSS_PCT)
        .context("failed to set opus expected loss")?;
    Ok(encoder)
}

fn build_decoder() -> Result<Decoder> {
    Decoder::new(SampleRate::Hz48000, Channels::Mono).context("failed to construct opus decoder")
}

fn decode_packet(decoder: &mut Decoder, payload: &[u8], fec: bool) -> Result<Vec<f32>> {
    let mut decoded = [0.0_f32; FRAME_SAMPLES * CHANNELS as usize * 2];
    let packet: Packet<'_> = payload.try_into().context("received empty opus packet")?;
    let signals: MutSignals<'_, f32> = (&mut decoded[..])
        .try_into()
        .context("failed to wrap decode output buffer")?;
    let decoded_samples = decoder
        .decode_float(Some(packet), signals, fec)
        .context("failed to decode opus frame")?;
    Ok(decoded[..decoded_samples * usize::from(CHANNELS)].to_vec())
}

fn decode_packet_loss(decoder: &mut Decoder) -> Result<Vec<f32>> {
    let mut decoded = [0.0_f32; FRAME_SAMPLES * CHANNELS as usize * 2];
    let signals: MutSignals<'_, f32> = (&mut decoded[..])
        .try_into()
        .context("failed to wrap plc output buffer")?;
    let decoded_samples = decoder
        .decode_float(None, signals, false)
        .context("failed to conceal lost opus frame")?;
    Ok(decoded[..decoded_samples * usize::from(CHANNELS)].to_vec())
}

fn build_capture_stream(preferred_device_name: Option<&str>) -> Result<CaptureBootstrap> {
    let host = cpal::default_host();
    let device = match preferred_device_name {
        Some(name) => find_input_device_by_name(&host, name)
            .with_context(|| format!("capture device not found: {name}"))?,
        None => host
            .default_input_device()
            .ok_or_else(|| anyhow!("no default input device available"))?,
    };
    let device_name = device
        .name()
        .unwrap_or_else(|_| "default-input".to_string());
    let supported = preferred_input_config(&device)?;
    let stream_config = supported.config();
    let sample_rate_hz = supported.sample_rate().0;
    let channels = usize::from(stream_config.channels);
    let sample_format = supported.sample_format();
    let (chunk_tx, chunk_rx) = mpsc::channel(MAX_CAPTURE_QUEUE_DEPTH);
    let (stop_tx, stop_rx) = std_mpsc::channel();
    let runtime = spawn_capture_runtime(
        device,
        device_name.clone(),
        stream_config,
        sample_rate_hz,
        channels,
        sample_format,
        chunk_tx,
        stop_tx,
        stop_rx,
    )?;

    Ok(CaptureBootstrap {
        device_name,
        device_sample_rate_hz: sample_rate_hz,
        chunk_rx,
        runtime,
    })
}

fn spawn_capture_runtime(
    device: cpal::Device,
    device_name: String,
    stream_config: StreamConfig,
    sample_rate_hz: u32,
    channels: usize,
    sample_format: SampleFormat,
    chunk_tx: mpsc::Sender<CaptureChunk>,
    stop_tx: std_mpsc::Sender<()>,
    stop_rx: std_mpsc::Receiver<()>,
) -> Result<CaptureRuntime> {
    let (ready_tx, ready_rx) = std_mpsc::channel();
    let thread = thread::Builder::new()
        .name("voicers-capture".to_string())
        .spawn(move || {
            let startup = build_running_input_stream(
                device,
                &device_name,
                &stream_config,
                channels,
                sample_rate_hz,
                sample_format,
                chunk_tx,
            );

            match startup {
                Ok(stream) => {
                    let _ = ready_tx.send(Ok(()));
                    let _stream = stream;
                    while stop_rx.recv_timeout(Duration::from_millis(250)).is_err() {}
                }
                Err(error) => {
                    let _ = ready_tx.send(Err(error));
                }
            }
        })
        .context("failed to spawn capture runtime thread")?;

    ready_rx
        .recv()
        .context("capture runtime thread exited before reporting readiness")??;

    Ok(CaptureRuntime {
        stop_tx,
        thread: Some(thread),
    })
}

fn build_running_input_stream(
    device: cpal::Device,
    device_name: &str,
    stream_config: &StreamConfig,
    channels: usize,
    sample_rate_hz: u32,
    sample_format: SampleFormat,
    chunk_tx: mpsc::Sender<CaptureChunk>,
) -> Result<Stream> {
    let error_device_name = device_name.to_string();
    let error_callback = move |error| {
        if is_ignorable_input_stream_error(&error) {
            return;
        }
        eprintln!("input stream error on {error_device_name}: {error}");
    };

    let stream = match sample_format {
        SampleFormat::F32 => build_input_stream::<f32>(
            &device,
            stream_config,
            channels,
            sample_rate_hz,
            chunk_tx,
            error_callback,
        )?,
        SampleFormat::F64 => build_input_stream::<f64>(
            &device,
            stream_config,
            channels,
            sample_rate_hz,
            chunk_tx,
            error_callback,
        )?,
        SampleFormat::I8 => build_input_stream::<i8>(
            &device,
            stream_config,
            channels,
            sample_rate_hz,
            chunk_tx,
            error_callback,
        )?,
        SampleFormat::I16 => build_input_stream::<i16>(
            &device,
            stream_config,
            channels,
            sample_rate_hz,
            chunk_tx,
            error_callback,
        )?,
        SampleFormat::I32 => build_input_stream::<i32>(
            &device,
            stream_config,
            channels,
            sample_rate_hz,
            chunk_tx,
            error_callback,
        )?,
        SampleFormat::I64 => build_input_stream::<i64>(
            &device,
            stream_config,
            channels,
            sample_rate_hz,
            chunk_tx,
            error_callback,
        )?,
        SampleFormat::U8 => build_input_stream::<u8>(
            &device,
            stream_config,
            channels,
            sample_rate_hz,
            chunk_tx,
            error_callback,
        )?,
        SampleFormat::U16 => build_input_stream::<u16>(
            &device,
            stream_config,
            channels,
            sample_rate_hz,
            chunk_tx,
            error_callback,
        )?,
        SampleFormat::U32 => build_input_stream::<u32>(
            &device,
            stream_config,
            channels,
            sample_rate_hz,
            chunk_tx,
            error_callback,
        )?,
        SampleFormat::U64 => build_input_stream::<u64>(
            &device,
            stream_config,
            channels,
            sample_rate_hz,
            chunk_tx,
            error_callback,
        )?,
        other => {
            return Err(anyhow!("unsupported input sample format: {other:?}"));
        }
    };

    stream.play().context("failed to start input stream")?;
    Ok(stream)
}

fn preferred_input_config(device: &cpal::Device) -> Result<SupportedStreamConfig> {
    let default = device
        .default_input_config()
        .context("failed to inspect default input config")?;

    let mut supported_configs = device
        .supported_input_configs()
        .context("failed to enumerate supported input configs")?;

    for config_range in supported_configs.by_ref() {
        if config_range.min_sample_rate().0 <= SAMPLE_RATE_HZ
            && config_range.max_sample_rate().0 >= SAMPLE_RATE_HZ
        {
            return Ok(config_range.with_sample_rate(CpalSampleRate(SAMPLE_RATE_HZ)));
        }
    }

    Ok(default)
}

fn build_input_stream<T>(
    device: &cpal::Device,
    stream_config: &StreamConfig,
    channels: usize,
    sample_rate_hz: u32,
    chunk_tx: mpsc::Sender<CaptureChunk>,
    error_callback: impl FnMut(cpal::StreamError) + Send + 'static,
) -> Result<Stream>
where
    T: cpal::SizedSample,
    f32: FromSample<T>,
{
    let stream = device.build_input_stream(
        stream_config,
        move |data: &[T], _| {
            let mono = downmix_to_mono::<T>(data, channels);
            let _ = chunk_tx.try_send(CaptureChunk {
                sample_rate_hz,
                samples: mono,
            });
        },
        error_callback,
        None,
    )?;

    Ok(stream)
}

trait FromSample<T> {
    fn from_sample(value: T) -> Self;
}

impl FromSample<f32> for f32 {
    fn from_sample(value: f32) -> Self {
        value
    }
}

impl FromSample<f64> for f32 {
    fn from_sample(value: f64) -> Self {
        value as f32
    }
}

impl FromSample<i8> for f32 {
    fn from_sample(value: i8) -> Self {
        value as f32 / i8::MAX as f32
    }
}

impl FromSample<i16> for f32 {
    fn from_sample(value: i16) -> Self {
        value as f32 / i16::MAX as f32
    }
}

impl FromSample<i32> for f32 {
    fn from_sample(value: i32) -> Self {
        value as f32 / i32::MAX as f32
    }
}

impl FromSample<i64> for f32 {
    fn from_sample(value: i64) -> Self {
        value as f32 / i64::MAX as f32
    }
}

impl FromSample<u8> for f32 {
    fn from_sample(value: u8) -> Self {
        (value as f32 / u8::MAX as f32) * 2.0 - 1.0
    }
}

impl FromSample<u16> for f32 {
    fn from_sample(value: u16) -> Self {
        (value as f32 / u16::MAX as f32) * 2.0 - 1.0
    }
}

impl FromSample<u32> for f32 {
    fn from_sample(value: u32) -> Self {
        (value as f64 / u32::MAX as f64 * 2.0 - 1.0) as f32
    }
}

impl FromSample<u64> for f32 {
    fn from_sample(value: u64) -> Self {
        (value as f64 / u64::MAX as f64 * 2.0 - 1.0) as f32
    }
}

fn downmix_to_mono<T>(data: &[T], channels: usize) -> Vec<f32>
where
    T: Copy,
    f32: FromSample<T>,
{
    if channels == 0 {
        return Vec::new();
    }

    let mut mono = Vec::with_capacity(data.len() / channels.max(1));
    for frame in data.chunks(channels) {
        let sum = frame.iter().copied().map(f32::from_sample).sum::<f32>();
        mono.push(sum / frame.len() as f32);
    }
    mono
}

impl CapturePipeline {
    fn new() -> Self {
        Self {
            sample_rate_hz: SAMPLE_RATE_HZ,
            input_samples: Vec::new(),
            output_cursor: 0.0,
            frame_assembly: Vec::with_capacity(FRAME_SAMPLES),
            ready_frames: VecDeque::new(),
            last_frame: vec![0.0; FRAME_SAMPLES],
            voice_processor: VoiceProcessorChain::new(),
            input_gain: 1.0,
        }
    }

    fn ingest(&mut self, chunk: CaptureChunk) {
        if chunk.samples.is_empty() {
            return;
        }

        if self.sample_rate_hz != chunk.sample_rate_hz {
            self.sample_rate_hz = chunk.sample_rate_hz;
            self.input_samples.clear();
            self.output_cursor = 0.0;
            self.frame_assembly.clear();
            self.ready_frames.clear();
            self.last_frame.fill(0.0);
            self.voice_processor.reset();
        }

        self.input_samples.extend(chunk.samples);
        self.rebuild_ready_frames();

        while self.ready_frames.len() > MAX_CAPTURE_QUEUE_DEPTH {
            self.ready_frames.pop_front();
        }
    }

    fn take_ready_frame(&mut self) -> Option<Vec<f32>> {
        self.ready_frames.pop_front().inspect(|frame| {
            self.last_frame.clear();
            self.last_frame.extend_from_slice(frame);
        })
    }

    fn last_frame(&self) -> Option<Vec<f32>> {
        if self.last_frame.len() == FRAME_SAMPLES {
            Some(self.last_frame.clone())
        } else {
            None
        }
    }

    fn set_input_gain(&mut self, gain: f32) {
        self.input_gain = gain.clamp(0.0, 2.0);
    }

    fn reset(&mut self) {
        self.sample_rate_hz = SAMPLE_RATE_HZ;
        self.input_samples.clear();
        self.output_cursor = 0.0;
        self.frame_assembly.clear();
        self.ready_frames.clear();
        self.last_frame.fill(0.0);
        self.voice_processor.reset();
    }

    fn rebuild_ready_frames(&mut self) {
        let step = self.sample_rate_hz as f32 / SAMPLE_RATE_HZ as f32;

        while self.input_samples.len() >= 2 {
            let base_index = self.output_cursor.floor() as usize;
            if base_index + 1 >= self.input_samples.len() {
                break;
            }

            let fraction = self.output_cursor - base_index as f32;
            let left = self.input_samples[base_index];
            let right = self.input_samples[base_index + 1];
            self.frame_assembly.push(left + (right - left) * fraction);
            self.output_cursor += step;

            let consumed = self.output_cursor.floor() as usize;
            if consumed > 0 {
                self.input_samples.drain(0..consumed);
                self.output_cursor -= consumed as f32;
            }

            if self.frame_assembly.len() == FRAME_SAMPLES {
                let mut frame =
                    std::mem::replace(&mut self.frame_assembly, Vec::with_capacity(FRAME_SAMPLES));
                self.voice_processor.process_frame(&mut frame);
                if (self.input_gain - 1.0).abs() > f32::EPSILON {
                    for sample in &mut frame {
                        *sample *= self.input_gain;
                    }
                }
                self.ready_frames.push_back(frame);
            }
        }
    }
}

fn synthesize_frame(runtime: &mut MediaPeerRuntime) -> Vec<f32> {
    let mut frame = Vec::with_capacity(FRAME_SAMPLES);
    let phase_step = 2.0 * PI * runtime.tone_hz / SAMPLE_RATE_HZ as f32;

    for _ in 0..FRAME_SAMPLES {
        frame.push(runtime.phase_radians.sin() * TONE_AMPLITUDE);
        runtime.phase_radians += phase_step;
        if runtime.phase_radians >= 2.0 * PI {
            runtime.phase_radians -= 2.0 * PI;
        }
    }

    frame
}

fn silence_frame() -> Vec<f32> {
    vec![0.0; FRAME_SAMPLES]
}

fn peer_tone_offset(peer_id: &str) -> f32 {
    let checksum = peer_id
        .bytes()
        .fold(0_u32, |acc, byte| acc + u32::from(byte));
    (checksum % 120) as f32
}

fn short_peer_id(peer_id: &str) -> &str {
    peer_id.get(0..12).unwrap_or(peer_id)
}

fn percent_to_gain(percent: u8) -> f32 {
    f32::from(percent.min(200)) / 100.0
}

fn apply_output_gain(mut pcm: Vec<f32>, gain: f32) -> Vec<f32> {
    if (gain - 1.0).abs() <= f32::EPSILON {
        return pcm;
    }

    for sample in &mut pcm {
        *sample *= gain;
    }
    pcm
}

async fn peer_output_gain(state: &Arc<RwLock<DaemonStatus>>, peer_id: &str) -> f32 {
    let state = state.read().await;
    state
        .peers
        .iter()
        .find(|peer| peer.peer_id == peer_id)
        .map(|peer| percent_to_gain(peer.output_volume_percent))
        .unwrap_or(1.0)
}

async fn switch_capture_device(
    state: &Arc<RwLock<DaemonStatus>>,
    capture: &mut CapturePipeline,
    capture_rx: &mut mpsc::Receiver<CaptureChunk>,
    capture_runtime: &mut Option<CaptureRuntime>,
    device_name: &str,
) -> Result<String> {
    let bootstrap = build_capture_stream(Some(device_name))?;
    *capture_rx = bootstrap.chunk_rx;
    *capture_runtime = Some(bootstrap.runtime);
    capture.reset();

    let mut state = state.write().await;
    state.audio.capture_device = Some(bootstrap.device_name.clone());
    state.audio.available_capture_devices = enumerate_input_device_names();
    state.audio.engine = AudioEngineStage::Live;
    state.audio.source = Some("microphone".to_string());
    insert_unique_note(
        &mut state,
        format!(
            "switched capture device to {} at {} Hz",
            bootstrap.device_name, bootstrap.device_sample_rate_hz
        ),
    );

    Ok(bootstrap.device_name)
}

fn enumerate_input_device_names() -> Vec<String> {
    let host = cpal::default_host();
    let Ok(devices) = host.input_devices() else {
        return Vec::new();
    };

    let mut names: Vec<String> = devices.filter_map(|device| device.name().ok()).collect();
    names.sort();
    names.dedup();
    names
}

fn find_input_device_by_name(host: &cpal::Host, needle: &str) -> Result<cpal::Device> {
    for device in host
        .input_devices()
        .context("failed to enumerate input devices")?
    {
        let Ok(name) = device.name() else {
            continue;
        };
        if name == needle {
            return Ok(device);
        }
    }

    Err(anyhow!("input device not found: {needle}"))
}

fn update_peer_media(
    state: &mut DaemonStatus,
    peer_id: &str,
    update: impl FnOnce(&mut PeerMediaState),
) {
    if let Some(peer) = state.peers.iter_mut().find(|peer| peer.peer_id == peer_id) {
        update(&mut peer.media);
    }
}

fn insert_unique_note(state: &mut DaemonStatus, note: String) {
    if state
        .notes
        .first()
        .map(|existing| existing == &note)
        .unwrap_or(false)
    {
        return;
    }

    state.notes.insert(0, note);
    state.notes.truncate(8);
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis() as u64)
        .unwrap_or(0)
}

fn rms_level(samples: &[f32]) -> f32 {
    if samples.is_empty() {
        return 0.0;
    }

    let energy = samples.iter().map(|sample| sample * sample).sum::<f32>() / samples.len() as f32;
    energy.sqrt()
}

fn normalize_timestamp_samples(frame: &MediaFrame) -> u64 {
    if frame.timestamp_samples != 0 {
        frame.timestamp_samples
    } else {
        frame.sequence.saturating_sub(1) * FRAME_SAMPLES_U64
    }
}

fn is_ignorable_input_stream_error(error: &cpal::StreamError) -> bool {
    match error {
        cpal::StreamError::BackendSpecific { err } => err
            .description
            .contains("`alsa::poll()` spuriously returned"),
        _ => false,
    }
}
