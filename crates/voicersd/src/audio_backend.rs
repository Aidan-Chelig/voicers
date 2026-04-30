use std::sync::Arc;

use anyhow::{Context, Result};
use futures::future::BoxFuture;
use tokio::sync::{mpsc, RwLock};
use voicers_core::{DaemonStatus, OutputStrategy};

#[derive(Clone)]
pub struct OutputBackendHandle {
    command_tx: mpsc::Sender<BackendCommand>,
}

impl OutputBackendHandle {
    pub async fn register_peer_output(
        &self,
        peer_id: String,
        output_bus: String,
        display_name: String,
    ) -> Result<()> {
        self.command_tx
            .send(BackendCommand::RegisterPeerOutput {
                peer_id,
                output_bus,
                display_name,
            })
            .await
            .context("failed to register peer output")
    }

    pub async fn remove_peer_output(&self, peer_id: String) -> Result<()> {
        self.command_tx
            .send(BackendCommand::RemovePeerOutput { peer_id })
            .await
            .context("failed to remove peer output")
    }

    pub async fn push_peer_pcm(&self, peer_id: String, pcm: Vec<f32>) -> Result<()> {
        self.command_tx
            .send(BackendCommand::PushPeerPcm { peer_id, pcm })
            .await
            .context("failed to push peer pcm")
    }
}

enum BackendCommand {
    RegisterPeerOutput {
        peer_id: String,
        output_bus: String,
        display_name: String,
    },
    RemovePeerOutput {
        peer_id: String,
    },
    PushPeerPcm {
        peer_id: String,
        pcm: Vec<f32>,
    },
}

pub async fn start(state: Arc<RwLock<DaemonStatus>>) -> OutputBackendHandle {
    let backend = select_backend(&state).await;

    let (command_tx, mut command_rx) = mpsc::channel(128);
    tokio::spawn(async move {
        let mut backend = backend;
        while let Some(command) = command_rx.recv().await {
            if let Err(error) = backend.handle(command).await {
                eprintln!("audio backend error: {error:#}");
            }
        }
    });

    OutputBackendHandle { command_tx }
}

async fn select_backend(state: &Arc<RwLock<DaemonStatus>>) -> Box<dyn OutputBackend> {
    #[cfg(target_os = "linux")]
    {
        let mut state = state.write().await;
        state.audio.output_backend = "pipewire-native".to_string();
        state.audio.output_strategy = OutputStrategy::PipeWirePeerNodesActive;
        insert_unique_note(
            &mut state,
            "publishing per-peer PipeWire playback nodes with the native backend".to_string(),
        );
        return Box::new(pipewire_native::Runtime::new());
    }

    #[cfg(target_os = "macos")]
    {
        let mut state = state.write().await;
        match coreaudio_cpal::Runtime::new() {
            Ok(backend) => {
                state.audio.output_backend = "coreaudio-cpal".to_string();
                state.audio.output_strategy = OutputStrategy::CoreAudioMixedOutputActive;
                insert_unique_note(
                    &mut state,
                    "playing decoded peer audio through the macOS CoreAudio output device"
                        .to_string(),
                );
                Box::new(backend)
            }
            Err(error) => {
                state.audio.output_backend = "logical-buses-only".to_string();
                state.audio.output_strategy = OutputStrategy::LogicalPeerBusesOnly;
                insert_unique_note(
                    &mut state,
                    format!("CoreAudio output unavailable, decoded audio stays queued: {error}"),
                );
                Box::new(null::Runtime::new())
            }
        }
    }

    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    {
        let mut state = state.write().await;
        state.audio.output_backend = "logical-buses-only".to_string();
        state.audio.output_strategy = OutputStrategy::LogicalPeerBusesOnly;
        insert_unique_note(
            &mut state,
            "audio backend fallback active: decoded peer audio stays in logical queues only"
                .to_string(),
        );
        Box::new(null::Runtime::new())
    }
}

trait OutputBackend: Send {
    fn handle<'a>(&'a mut self, command: BackendCommand) -> BoxFuture<'a, Result<()>>;
}

#[cfg(not(target_os = "linux"))]
mod null {
    use super::{BackendCommand, OutputBackend, Result};
    use futures::{future::BoxFuture, FutureExt};
    use std::collections::HashSet;

    pub struct Runtime {
        peers: HashSet<String>,
    }

    impl Runtime {
        pub fn new() -> Self {
            Self {
                peers: HashSet::new(),
            }
        }

        fn handle_command(&mut self, command: BackendCommand) {
            match command {
                BackendCommand::RegisterPeerOutput { peer_id, .. } => {
                    self.peers.insert(peer_id);
                }
                BackendCommand::RemovePeerOutput { peer_id } => {
                    self.peers.remove(&peer_id);
                }
                BackendCommand::PushPeerPcm { .. } => {}
            }
        }
    }

    impl OutputBackend for Runtime {
        fn handle<'a>(&'a mut self, command: BackendCommand) -> BoxFuture<'a, Result<()>> {
            async move {
                self.handle_command(command);
                Ok(())
            }
            .boxed()
        }
    }
}

#[cfg(target_os = "macos")]
mod coreaudio_cpal {
    use super::{BackendCommand, OutputBackend, Result};
    use anyhow::{anyhow, Context};
    use cpal::{
        traits::{DeviceTrait, HostTrait, StreamTrait},
        SampleFormat, SampleRate, Stream, StreamConfig, SupportedStreamConfig,
    };
    use futures::{future::BoxFuture, FutureExt};
    use std::{
        collections::{HashMap, VecDeque},
        sync::{mpsc, Arc, Mutex},
        thread,
        time::Duration,
    };

    const SAMPLE_RATE_HZ: u32 = 48_000;
    const MAX_QUEUED_SAMPLES: usize = SAMPLE_RATE_HZ as usize / 2;
    const MIN_PREROLL_SAMPLES: usize = SAMPLE_RATE_HZ as usize / 5;

    pub struct Runtime {
        peers: SharedPeers,
        stop_tx: mpsc::Sender<()>,
        thread: Option<thread::JoinHandle<()>>,
    }

    type SharedPeers = Arc<Mutex<HashMap<String, PeerQueue>>>;

    struct PeerQueue {
        output_bus: String,
        display_name: String,
        samples: VecDeque<f32>,
        primed: bool,
    }

    impl Runtime {
        pub fn new() -> Result<Self> {
            let peers = Arc::new(Mutex::new(HashMap::new()));
            let (stop_tx, stop_rx) = mpsc::channel();
            let thread = spawn_output_runtime(Arc::clone(&peers), stop_rx)?;
            Ok(Self {
                peers,
                stop_tx,
                thread: Some(thread),
            })
        }

        async fn handle_command(&mut self, command: BackendCommand) -> Result<()> {
            match command {
                BackendCommand::RegisterPeerOutput {
                    peer_id,
                    output_bus,
                    display_name,
                } => {
                    let mut peers = self
                        .peers
                        .lock()
                        .map_err(|_| anyhow!("coreaudio peer queue lock poisoned"))?;
                    peers
                        .entry(peer_id)
                        .and_modify(|peer| {
                            peer.output_bus = output_bus.clone();
                            peer.display_name = display_name.clone();
                        })
                        .or_insert_with(|| PeerQueue {
                            output_bus,
                            display_name,
                            samples: VecDeque::new(),
                            primed: false,
                        });
                }
                BackendCommand::RemovePeerOutput { peer_id } => {
                    let mut peers = self
                        .peers
                        .lock()
                        .map_err(|_| anyhow!("coreaudio peer queue lock poisoned"))?;
                    peers.remove(&peer_id);
                }
                BackendCommand::PushPeerPcm { peer_id, pcm } => {
                    let mut peers = self
                        .peers
                        .lock()
                        .map_err(|_| anyhow!("coreaudio peer queue lock poisoned"))?;
                    let peer = peers.entry(peer_id).or_insert_with(|| PeerQueue {
                        output_bus: "default".to_string(),
                        display_name: "peer".to_string(),
                        samples: VecDeque::new(),
                        primed: false,
                    });
                    peer.samples.extend(pcm);
                    while peer.samples.len() > MAX_QUEUED_SAMPLES {
                        peer.samples.pop_front();
                    }
                }
            }

            Ok(())
        }
    }

    impl Drop for Runtime {
        fn drop(&mut self) {
            let _ = self.stop_tx.send(());
            if let Some(thread) = self.thread.take() {
                let _ = thread.join();
            }
        }
    }

    impl OutputBackend for Runtime {
        fn handle<'a>(&'a mut self, command: BackendCommand) -> BoxFuture<'a, Result<()>> {
            async move { self.handle_command(command).await }.boxed()
        }
    }

    fn spawn_output_runtime(
        peers: SharedPeers,
        stop_rx: mpsc::Receiver<()>,
    ) -> Result<thread::JoinHandle<()>> {
        let (ready_tx, ready_rx) = mpsc::channel();
        let thread = thread::Builder::new()
            .name("voicers-coreaudio-output".to_string())
            .spawn(move || {
                let startup = build_running_output_stream(peers);
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
            .context("failed to spawn CoreAudio output thread")?;

        ready_rx
            .recv()
            .context("CoreAudio output thread exited before reporting readiness")??;

        Ok(thread)
    }

    fn build_running_output_stream(peers: SharedPeers) -> Result<Stream> {
        let host = cpal::default_host();
        let device = host
            .default_output_device()
            .ok_or_else(|| anyhow!("no default output device available"))?;
        let supported = preferred_output_config(&device)?;
        let stream_config = supported.config();
        let channels = usize::from(stream_config.channels);
        let error_device_name = device
            .name()
            .unwrap_or_else(|_| "default-output".to_string());
        let error_callback = move |error| {
            eprintln!("output stream error on {error_device_name}: {error}");
        };

        let stream = match supported.sample_format() {
            SampleFormat::F32 => build_output_stream::<f32>(
                &device,
                &stream_config,
                channels,
                peers,
                error_callback,
            )?,
            SampleFormat::F64 => build_output_stream::<f64>(
                &device,
                &stream_config,
                channels,
                peers,
                error_callback,
            )?,
            SampleFormat::I8 => {
                build_output_stream::<i8>(&device, &stream_config, channels, peers, error_callback)?
            }
            SampleFormat::I16 => build_output_stream::<i16>(
                &device,
                &stream_config,
                channels,
                peers,
                error_callback,
            )?,
            SampleFormat::I32 => build_output_stream::<i32>(
                &device,
                &stream_config,
                channels,
                peers,
                error_callback,
            )?,
            SampleFormat::I64 => build_output_stream::<i64>(
                &device,
                &stream_config,
                channels,
                peers,
                error_callback,
            )?,
            SampleFormat::U8 => {
                build_output_stream::<u8>(&device, &stream_config, channels, peers, error_callback)?
            }
            SampleFormat::U16 => build_output_stream::<u16>(
                &device,
                &stream_config,
                channels,
                peers,
                error_callback,
            )?,
            SampleFormat::U32 => build_output_stream::<u32>(
                &device,
                &stream_config,
                channels,
                peers,
                error_callback,
            )?,
            SampleFormat::U64 => build_output_stream::<u64>(
                &device,
                &stream_config,
                channels,
                peers,
                error_callback,
            )?,
            other => return Err(anyhow!("unsupported output sample format: {other:?}")),
        };

        stream.play().context("failed to start output stream")?;
        Ok(stream)
    }

    fn preferred_output_config(device: &cpal::Device) -> Result<SupportedStreamConfig> {
        let default = device
            .default_output_config()
            .context("failed to inspect default output config")?;

        let mut supported_configs = device
            .supported_output_configs()
            .context("failed to enumerate supported output configs")?;

        for config_range in supported_configs.by_ref() {
            if config_range.min_sample_rate().0 <= SAMPLE_RATE_HZ
                && config_range.max_sample_rate().0 >= SAMPLE_RATE_HZ
            {
                return Ok(config_range.with_sample_rate(SampleRate(SAMPLE_RATE_HZ)));
            }
        }

        Ok(default)
    }

    fn build_output_stream<T>(
        device: &cpal::Device,
        stream_config: &StreamConfig,
        channels: usize,
        peers: SharedPeers,
        error_callback: impl FnMut(cpal::StreamError) + Send + 'static,
    ) -> Result<Stream>
    where
        T: cpal::SizedSample,
        T: FromOutputSample,
    {
        let stream = device.build_output_stream(
            stream_config,
            move |data: &mut [T], _| fill_output_buffer(data, channels, &peers),
            error_callback,
            None,
        )?;

        Ok(stream)
    }

    fn fill_output_buffer<T>(data: &mut [T], channels: usize, peers: &SharedPeers)
    where
        T: FromOutputSample,
    {
        for sample in data.iter_mut() {
            *sample = T::from_output_sample(0.0);
        }

        let Ok(mut peers) = peers.lock() else {
            return;
        };

        for frame in data.chunks_mut(channels.max(1)) {
            let mixed = peers
                .values_mut()
                .map(next_peer_sample)
                .sum::<f32>()
                .clamp(-1.0, 1.0);
            let sample = T::from_output_sample(mixed);
            for channel in frame {
                *channel = sample;
            }
        }
    }

    fn next_peer_sample(peer: &mut PeerQueue) -> f32 {
        if !peer.primed {
            if peer.samples.len() >= MIN_PREROLL_SAMPLES {
                peer.primed = true;
            } else {
                return 0.0;
            }
        }

        let sample = peer.samples.pop_front().unwrap_or(0.0);
        if peer.samples.len() < MIN_PREROLL_SAMPLES / 2 {
            peer.primed = false;
        }
        sample
    }

    trait FromOutputSample: Copy {
        fn from_output_sample(value: f32) -> Self;
    }

    impl FromOutputSample for f32 {
        fn from_output_sample(value: f32) -> Self {
            value
        }
    }

    impl FromOutputSample for f64 {
        fn from_output_sample(value: f32) -> Self {
            value as f64
        }
    }

    impl FromOutputSample for i8 {
        fn from_output_sample(value: f32) -> Self {
            (value.clamp(-1.0, 1.0) * i8::MAX as f32) as i8
        }
    }

    impl FromOutputSample for i16 {
        fn from_output_sample(value: f32) -> Self {
            (value.clamp(-1.0, 1.0) * i16::MAX as f32) as i16
        }
    }

    impl FromOutputSample for i32 {
        fn from_output_sample(value: f32) -> Self {
            (value.clamp(-1.0, 1.0) * i32::MAX as f32) as i32
        }
    }

    impl FromOutputSample for i64 {
        fn from_output_sample(value: f32) -> Self {
            (value.clamp(-1.0, 1.0) * i64::MAX as f32) as i64
        }
    }

    impl FromOutputSample for u8 {
        fn from_output_sample(value: f32) -> Self {
            ((value.clamp(-1.0, 1.0) + 1.0) * 0.5 * u8::MAX as f32) as u8
        }
    }

    impl FromOutputSample for u16 {
        fn from_output_sample(value: f32) -> Self {
            ((value.clamp(-1.0, 1.0) + 1.0) * 0.5 * u16::MAX as f32) as u16
        }
    }

    impl FromOutputSample for u32 {
        fn from_output_sample(value: f32) -> Self {
            ((value.clamp(-1.0, 1.0) as f64 + 1.0) * 0.5 * u32::MAX as f64) as u32
        }
    }

    impl FromOutputSample for u64 {
        fn from_output_sample(value: f32) -> Self {
            ((value.clamp(-1.0, 1.0) as f64 + 1.0) * 0.5 * u64::MAX as f64) as u64
        }
    }
}

#[cfg(target_os = "linux")]
mod pipewire_native {
    use super::{BackendCommand, OutputBackend, Result};
    use anyhow::{anyhow, Context};
    use futures::{future::BoxFuture, FutureExt};
    use pipewire as pw;
    use pw::{properties::properties, spa};
    use spa::pod::Pod;
    use std::{
        collections::{HashMap, VecDeque},
        io::Cursor,
        sync::{mpsc, Arc, Mutex},
        thread,
    };

    const SAMPLE_RATE_HZ: u32 = 48_000;
    const CHANNELS: u32 = 1;
    const MAX_QUEUED_SAMPLES: usize = SAMPLE_RATE_HZ as usize / 2;
    const MIN_PREROLL_SAMPLES: usize = SAMPLE_RATE_HZ as usize / 5;
    const TARGET_NODE_LATENCY: &str = "960/48000";
    const BYTES_PER_SAMPLE: usize = std::mem::size_of::<f32>();

    pub struct Runtime {
        peers: HashMap<String, PeerNode>,
    }

    impl Runtime {
        pub fn new() -> Self {
            Self {
                peers: HashMap::new(),
            }
        }

        async fn handle_command(&mut self, command: BackendCommand) -> Result<()> {
            match command {
                BackendCommand::RegisterPeerOutput {
                    peer_id,
                    output_bus,
                    display_name,
                } => {
                    self.ensure_peer_node(peer_id, output_bus, display_name)?;
                }
                BackendCommand::RemovePeerOutput { peer_id } => {
                    if let Some(node) = self.peers.remove(&peer_id) {
                        let _ = node.control_tx.send(PeerCommand::Terminate);
                        let _ = node.thread.join();
                    }
                }
                BackendCommand::PushPeerPcm { peer_id, pcm } => {
                    if let Some(node) = self.peers.get(&peer_id) {
                        let mut queue = node
                            .queue
                            .lock()
                            .map_err(|_| anyhow!("pipewire peer queue lock poisoned"))?;
                        for sample in pcm {
                            queue.samples.push_back(sample);
                        }
                        while queue.samples.len() > MAX_QUEUED_SAMPLES {
                            queue.samples.pop_front();
                        }
                    }
                }
            }

            Ok(())
        }

        fn ensure_peer_node(
            &mut self,
            peer_id: String,
            _output_bus: String,
            display_name: String,
        ) -> Result<()> {
            let node_name = format!(
                "{}_{}",
                sanitize_label(&display_name),
                short_peer_id(&peer_id)
            );

            if let Some(existing) = self.peers.get(&peer_id) {
                if existing.node_name == node_name && existing.display_name == display_name {
                    return Ok(());
                }
            }

            if let Some(node) = self.peers.remove(&peer_id) {
                let _ = node.control_tx.send(PeerCommand::Terminate);
                let _ = node.thread.join();
            }

            let node_description = format!("Voicers {}", display_name);
            let queue = Arc::new(Mutex::new(PeerQueue {
                samples: VecDeque::new(),
                primed: false,
            }));
            let (control_tx, thread) =
                spawn_output_stream(Arc::clone(&queue), &node_name, &node_description, "peer")?;

            self.peers.insert(
                peer_id,
                PeerNode {
                    queue,
                    control_tx,
                    thread,
                    node_name,
                    display_name,
                },
            );
            Ok(())
        }
    }

    impl OutputBackend for Runtime {
        fn handle<'a>(&'a mut self, command: BackendCommand) -> BoxFuture<'a, Result<()>> {
            async move { self.handle_command(command).await }.boxed()
        }
    }

    struct PeerNode {
        queue: Arc<Mutex<PeerQueue>>,
        control_tx: pw::channel::Sender<PeerCommand>,
        thread: thread::JoinHandle<()>,
        node_name: String,
        display_name: String,
    }

    struct PeerQueue {
        samples: VecDeque<f32>,
        primed: bool,
    }

    enum PeerCommand {
        Terminate,
    }

    fn spawn_output_stream(
        queue: Arc<Mutex<PeerQueue>>,
        node_name: &str,
        node_description: &str,
        thread_label: &str,
    ) -> Result<(pw::channel::Sender<PeerCommand>, thread::JoinHandle<()>)> {
        let (control_tx, control_rx) = pw::channel::channel::<PeerCommand>();
        let (ready_tx, ready_rx) = mpsc::channel();
        let queue_for_thread = Arc::clone(&queue);
        let node_name = node_name.to_string();
        let node_description = node_description.to_string();
        let thread_label = thread_label.to_string();

        let thread = thread::Builder::new()
            .name(format!("voicers-pw-{thread_label}"))
            .spawn(move || {
                let result = run_peer_stream(
                    control_rx,
                    queue_for_thread,
                    &node_name,
                    &node_description,
                    ready_tx,
                );
                if let Err(error) = result {
                    eprintln!("pipewire {thread_label} stream exited with error: {error:#}");
                }
            })
            .context("failed to spawn pipewire output thread")?;

        ready_rx
            .recv()
            .context("pipewire output thread exited before startup completed")??;

        Ok((control_tx, thread))
    }

    fn run_peer_stream(
        control_rx: pw::channel::Receiver<PeerCommand>,
        queue: Arc<Mutex<PeerQueue>>,
        node_name: &str,
        node_description: &str,
        ready_tx: mpsc::Sender<Result<()>>,
    ) -> Result<()> {
        pw::init();

        let mainloop = match pw::main_loop::MainLoopRc::new(None) {
            Ok(value) => value,
            Err(error) => {
                let _ = ready_tx.send(Err(anyhow!(error.to_string())));
                return Err(error.into());
            }
        };
        let context = match pw::context::ContextRc::new(&mainloop, None) {
            Ok(value) => value,
            Err(error) => {
                let _ = ready_tx.send(Err(anyhow!(error.to_string())));
                return Err(error.into());
            }
        };
        let core = match context.connect_rc(None) {
            Ok(value) => value,
            Err(error) => {
                let _ = ready_tx.send(Err(anyhow!(error.to_string())));
                return Err(error.into());
            }
        };

        let props = properties! {
            *pw::keys::MEDIA_TYPE => "Audio",
            *pw::keys::MEDIA_CATEGORY => "Playback",
            *pw::keys::MEDIA_ROLE => "Communication",
            *pw::keys::NODE_NAME => node_name,
            *pw::keys::NODE_DESCRIPTION => node_description,
            *pw::keys::NODE_LATENCY => TARGET_NODE_LATENCY,
            *pw::keys::AUDIO_CHANNELS => "1",
            *pw::keys::AUDIO_FORMAT => "F32LE",
        };

        let stream = match pw::stream::StreamBox::new(&core, "voicers-peer-output", props) {
            Ok(value) => value,
            Err(error) => {
                let _ = ready_tx.send(Err(anyhow!(error.to_string())));
                return Err(error.into());
            }
        };

        let control = control_rx.attach(mainloop.loop_(), {
            let mainloop = mainloop.clone();
            move |command| match command {
                PeerCommand::Terminate => mainloop.quit(),
            }
        });

        let listener = match stream
            .add_local_listener_with_user_data(queue)
            .process(|stream, queue| {
                let Some(mut buffer) = stream.dequeue_buffer() else {
                    return;
                };

                let datas = buffer.datas_mut();
                if datas.is_empty() {
                    return;
                }

                let data = &mut datas[0];
                let Some(slice) = data.data() else {
                    return;
                };

                fill_output_buffer(slice, queue);
                let filled_size = slice.len();
                let chunk = data.chunk_mut();
                *chunk.offset_mut() = 0;
                *chunk.stride_mut() = BYTES_PER_SAMPLE as _;
                *chunk.size_mut() = filled_size as _;
            })
            .register()
        {
            Ok(value) => value,
            Err(error) => {
                let _ = ready_tx.send(Err(anyhow!(error.to_string())));
                return Err(error.into());
            }
        };

        let mut audio_info = spa::param::audio::AudioInfoRaw::new();
        audio_info.set_format(spa::param::audio::AudioFormat::F32LE);
        audio_info.set_rate(SAMPLE_RATE_HZ);
        audio_info.set_channels(CHANNELS);
        let mut position = [0; spa::param::audio::MAX_CHANNELS];
        position[0] = spa::sys::SPA_AUDIO_CHANNEL_MONO;
        audio_info.set_position(position);

        let values: Vec<u8> = pw::spa::pod::serialize::PodSerializer::serialize(
            Cursor::new(Vec::new()),
            &pw::spa::pod::Value::Object(pw::spa::pod::Object {
                type_: spa::sys::SPA_TYPE_OBJECT_Format,
                id: spa::sys::SPA_PARAM_EnumFormat,
                properties: audio_info.into(),
            }),
        )
        .unwrap()
        .0
        .into_inner();
        let mut params = [Pod::from_bytes(&values).unwrap()];

        if let Err(error) = stream.connect(
            spa::utils::Direction::Output,
            None,
            pw::stream::StreamFlags::AUTOCONNECT
                | pw::stream::StreamFlags::MAP_BUFFERS
                | pw::stream::StreamFlags::RT_PROCESS,
            &mut params,
        ) {
            let _ = ready_tx.send(Err(anyhow!(error.to_string())));
            return Err(error.into());
        }

        let _ = ready_tx.send(Ok(()));
        let _control = control;
        let _listener = listener;
        let _stream = stream;
        mainloop.run();

        Ok(())
    }

    fn fill_output_buffer(slice: &mut [u8], queue: &Arc<Mutex<PeerQueue>>) {
        if let Ok(mut queued) = queue.lock() {
            if !queued.primed {
                if queued.samples.len() >= MIN_PREROLL_SAMPLES {
                    queued.primed = true;
                } else {
                    for chunk in slice.chunks_exact_mut(BYTES_PER_SAMPLE) {
                        chunk.copy_from_slice(&0.0_f32.to_le_bytes());
                    }
                    return;
                }
            }

            for chunk in slice.chunks_exact_mut(BYTES_PER_SAMPLE) {
                let sample = queued.samples.pop_front().unwrap_or(0.0);
                chunk.copy_from_slice(&sample.to_le_bytes());
            }

            if queued.samples.len() < MIN_PREROLL_SAMPLES / 2 {
                queued.primed = false;
            }
        } else {
            for chunk in slice.chunks_exact_mut(BYTES_PER_SAMPLE) {
                chunk.copy_from_slice(&0.0_f32.to_le_bytes());
            }
        }
    }

    fn sanitize_label(peer_id: &str) -> String {
        peer_id
            .chars()
            .map(|ch| if ch.is_ascii_alphanumeric() { ch } else { '_' })
            .collect()
    }

    fn short_peer_id(peer_id: &str) -> &str {
        peer_id.get(0..12).unwrap_or(peer_id)
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
