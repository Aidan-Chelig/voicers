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

Linux is the first-class target.

- PipeWire is the primary backend for exposing one logical output per peer as routable nodes for recording software.
- JACK can be a Linux compatibility backend.
- macOS and Windows stay in scope, but the daemon keeps a portable logical per-peer bus model so platform-specific exposure can land later without changing the core session model.

## Networking Direction

This project will use `libp2p`.

Near-term:

- `libp2p` identity and peer IDs
- a daemon-owned swarm abstraction
- explicit peer/session state in the daemon
- localhost-only control IPC for the TUI

Medium-term:

- `libp2p` transport with authenticated encrypted channels
- peer discovery and rendezvous
- NAT traversal strategy
- framed Opus audio over dedicated peer connections/streams

No-relay-first transport strategy:

1. direct dial by explicit multiaddr
2. automatic router port mapping with UPnP first, then NAT-PMP/PCP later
3. surface observed and mapped public addresses in daemon status and the TUI
4. prefer direct transport whenever a reachable address exists
5. add relay and hole-punch coordination later as fallback for the cases direct reachability cannot solve

The first implementation pass does not try to ship final media transport. It establishes the daemon boundaries the real `libp2p` transport will live behind.

## Audio Direction

The audio model is:

- one local capture path
- one decode/jitter path per remote peer
- one logical output bus per remote peer
- one optional local monitor/mix bus

Backend strategy:

- Linux/PipeWire: expose a distinct node per peer
- Linux/JACK: expose a distinct port group per peer
- macOS/CoreAudio: keep logical buses first, then evaluate virtual-device or aggregate-device publication
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
- Linux-first audio/output strategy captured in code and status surfaces

## Phase 1 Execution Steps

1. Freeze scope around a Linux-first daemon plus TUI with `libp2p` in the network layer.
2. Convert the repository into a Cargo workspace.
3. Define daemon IPC and shared state in `voicers-core`.
4. Replace the prototype binary with a stub daemon that owns control state and a `libp2p` identity.
5. Add a simple TUI client that talks to the daemon and exercises the control loop.

## Next Milestones

1. Replace the stub audio service with a modern capture/encode/decode pipeline that works on current `cpal` and isolates real-time callbacks from async work.
2. Introduce a real `libp2p` swarm with transport, peer dialing, and handshake/session orchestration.
3. Add PipeWire-native peer output publication so recording software can see one output per remote peer.
4. Add persistent config, device selection, and reconnect behavior.
5. Add end-to-end tests for daemon control, peer state transitions, and local loopback audio sessions.

## Connectivity Execution Order

1. Keep explicit multiaddr dialing as the baseline path.
2. Add automatic UPnP port mapping and expose the mapped public address.
3. Add observed-address reporting from identify so users can see what remote peers report.
4. Persist successful direct addresses and make reconnect use them first.
5. Add rendezvous, relay, and DCUtR only after the direct-first path is clear in the UI.
