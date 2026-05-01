# Voicers P2P Daemon Plan

## Product Direction

`voicersd` becomes the long-running local daemon. It owns:

- local audio device integration
- peer identity and networking
- per-peer logical output buses
- session state
- local control IPC for UI clients

`voicers` becomes a thin TUI client. It owns:

- room/session workflow
- device and routing controls
- peer list and health display
- operator actions like mute, join, leave, and diagnostics

This split keeps audio and networking alive independently of the terminal.

## Platform Priority

Linux and macOS are active targets.

- Linux/PipeWire is the primary backend for exposing one logical output per peer as routable nodes for recording software.
- Linux/JACK can become a compatibility backend.
- macOS/CoreAudio is active through CPAL and mixes decoded peer audio into the default output device.
- Windows stays in scope, but the daemon keeps a portable logical per-peer bus model so platform-specific exposure can land later without changing the core session model.

## Networking Direction

The project uses `libp2p`.

Implemented:

- daemon-owned `libp2p` identity and peer ID
- TCP transport with Noise authentication and Yamux multiplexing
- DNS multiaddr support
- explicit peer dialing by multiaddr
- localhost-only control IPC for the TUI
- peer/session state in the daemon
- UPnP external address discovery
- STUN reflexive UDP endpoint discovery
- observed-address tracking from swarm events
- circuit-relay client reservations and relayed dialing
- DCUtR hole-punch coordination
- Kademlia DHT bootstrap and routing-table discovery

Still pending:

- DHT-backed room or user rendezvous
- project-controlled relay-server mode
- NAT-PMP/PCP support
- project-controlled TURN deployment automation
- reconnect policy that uses durable address and path scores
- PipeWire-native peer output publication

Current transport strategy:

1. Listen locally on TCP and accept direct libp2p connections.
2. Prefer explicit direct multiaddr dialing when a reachable address exists.
3. Attempt automatic router port mapping with UPnP.
4. Probe STUN servers and surface the observed UDP reflexive endpoint for NAT diagnostics.
5. Surface observed, external, mapped, STUN, and relayed addresses in daemon status.
6. Bootstrap against DHT peers and learn relay-capable candidates.
7. Reserve through an explicitly configured relay or a limited number of discovered relay-capable peers.
8. Use relayed addresses when direct reachability is not available.
9. Let DCUtR attempt a direct hole punch for relayed connections.

The current networking layer establishes transport, session handshake, peer
status, relay reservation, hole-punch plumbing, WebRTC signalling, and a WebRTC
data-channel media path with libp2p media fallback.

## NAT Resilience Roadmap

The maximum-resilience target is an ICE-style ladder rather than more one-off
NAT probes. `webrtc-rs` is the preferred implementation because it already
contains the protocol pieces this project would otherwise have to maintain:
ICE, STUN, TURN, DTLS, SCTP data channels, RTP, and SRTP.

Execution order:

1. Keep libp2p as the daemon identity, DHT, relay, and signalling plane.
2. Done: add a feature-gated `webrtc-rs` transport module that can build ICE server configuration from daemon STUN/TURN settings.
3. Done: add offer/answer and trickle ICE message types to the daemon control/session protocol.
4. Done: create WebRTC peer connections, generate offers/answers, and exchange generated signalling over the existing libp2p request-response channel.
5. Done: send Opus frames over the WebRTC data channel when it is open, with the current libp2p request-response media path as fallback.
6. Done: expose the selected media path, WebRTC connection state, and persisted path score counters in daemon/TUI status.
7. Done: add TURN credential configuration. Project-controlled TURN deployment automation remains external.
8. Keep libp2p circuit relay available as an application-level fallback and bootstrap path.

## Media Path Unification Plan

The WebRTC path and the libp2p path should converge behind one daemon-owned
media transport abstraction. The caller should ask to send a `MediaFrame` to a
peer, and the network layer should choose the best currently viable path. Code
outside the network module should not need to know whether the frame left over a
WebRTC data channel or libp2p request-response.

Current state:

- Done: both paths consume and produce the same `MediaFrame` type.
- Done: incoming frames from both paths enter the same jitter/decode pipeline.
- Done: outgoing media prefers the WebRTC data channel when it is open.
- Done: libp2p request-response remains the fallback while WebRTC is disabled,
  connecting, failed, or closed.
- Done: daemon status exposes `selected_media_path`, `webrtc_connection_state`,
  and persisted path score counters.
- Done: an internal media-transport boundary now owns path naming, WebRTC-first
  fallback policy, and path-status updates instead of branching directly inside
  the media tick loop.

Unification target:

1. Introduce an internal `MediaTransport` boundary with `send_frame`,
   `path_state`, and `path_score` operations.
2. Model WebRTC and libp2p media as implementations of that boundary rather
   than as branches inside the media tick loop.
3. Promote path choice into a small policy object that prefers open WebRTC ICE
   paths, falls back to libp2p media, and can later use score history for
   reconnect ordering.
4. Make WebRTC negotiation automatic after a session handshake instead of
   requiring `StartWebRtcOffer` from the control API.
5. Keep libp2p as the identity, discovery, relay, DHT, and signalling plane
   even when WebRTC owns the selected media path.
6. Persist enough per-peer path metadata to reconnect through the last
   successful direct, relayed, or ICE-selected path before trying lower-scored
   alternatives.
7. Add integration coverage for policy behavior: WebRTC open, WebRTC failed,
   libp2p fallback, relay-only session, and reconnect using persisted scores.

This keeps the user-facing model simple: peers connect once, the daemon reports
which media path won, and the audio engine sees one stream of encoded frames
regardless of transport.

## Audio Direction

The audio model is:

- one local capture path
- one decode/jitter path per remote peer
- one logical output bus per remote peer
- one optional local monitor/mix bus

Backend strategy:

- Linux/PipeWire: expose a distinct node per peer
- Linux/JACK: expose a distinct port group per peer
- macOS/CoreAudio: play decoded peer audio through the default output device, while keeping logical peer buses in daemon state
- Windows/WASAPI: keep logical buses first, then evaluate virtual-endpoint publication

## Workspace Layout

- `crates/voicers-core`
  Shared types for control IPC, session state, backend selection, and milestone-safe interfaces.
- `crates/voicersd`
  The daemon binary with control server, `libp2p` node ownership, and future audio/runtime services.
- `crates/voicers`
  The terminal UI client.

## Phase 1 Milestone

Deliver a daemon/TUI scaffold that compiles and proves:

- daemon process boundary
- local control protocol
- shared session model
- `libp2p` identity ownership in the daemon
- platform-specific audio/output strategy captured in code and status surfaces

## Phase 1 Execution Steps

1. Freeze scope around a Linux-first daemon plus TUI with `libp2p` in the network layer.
2. Convert the repository into a Cargo workspace.
3. Define daemon IPC and shared state in `voicers-core`.
4. Replace the prototype binary with a stub daemon that owns control state and a `libp2p` identity.
5. Add a simple TUI client that talks to the daemon and exercises the control loop.

## Next Milestones

1. Replace the stub audio service with a modern capture/encode/decode pipeline that works on current `cpal` and isolates real-time callbacks from async work.
2. Add DHT-backed rendezvous so users can join by room or invite code instead of manually exchanging full multiaddrs.
3. Add a project-controlled relay/bootstrap/STUN/TURN mode for reliable deployments.
4. Add `webrtc-rs` offer/answer and ICE candidate signalling so STUN/TURN results can contribute to direct connectivity instead of status only.
5. Add PipeWire-native peer output publication so recording software can see one output per remote peer.
6. Add persistent config, device selection, address scoring, and reconnect behavior.
7. Add end-to-end tests for daemon control, peer state transitions, STUN observation, relayed dialing, hole punching, and local loopback audio sessions.

## Connectivity Execution Order

1. Done: keep explicit multiaddr dialing as the baseline path.
2. Done: add automatic UPnP port mapping and expose the mapped public address.
3. Done: add observed-address reporting so users can see what the swarm reports.
4. Done: add relay-client reservations and relayed dialing.
5. Done: add DCUtR hole-punch coordination for relayed peers.
6. Done: add Kademlia DHT bootstrap so the daemon can discover relay-capable peers.
7. Done: add STUN probes so the daemon can report reflexive UDP endpoints.
8. Done: add a feature-gated `webrtc-rs` transport scaffold.
9. Done: add WebRTC offer/answer and ICE candidate signalling over the existing session protocol.
10. Done: instantiate WebRTC peer connections and generate local offers, answers, and ICE candidates.
11. Done: route Opus media over the WebRTC data-channel path when available, with libp2p media fallback.
12. Done: expose media path selection and WebRTC connection state in daemon/TUI status.
13. Done: persist score counters for direct, relayed, DCUtR, and WebRTC selected paths.
14. Next: use scored addresses and paths to prioritize reconnect attempts.
15. Next: add rendezvous records on the DHT for room or invite-code discovery.
