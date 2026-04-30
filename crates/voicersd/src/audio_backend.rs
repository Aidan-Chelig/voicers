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

    #[cfg(not(target_os = "linux"))]
    {
        let mut state = state.write().await;
        state.audio.output_backend = "logical-buses-only".to_string();
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
