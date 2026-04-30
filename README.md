# voicers

`voicers` is a peer-to-peer voice application built around a long-running
daemon and a thin local client.

- `voicersd` owns audio, peer identity, libp2p networking, session state, and
  localhost control IPC.
- `voicers` is the terminal UI client that talks to the daemon.
- `voicers-core` holds shared control and status types.

Audio is encoded with Opus. Networking is handled with `libp2p`.

## Development Shell

The repository includes a Nix shell that works on Linux and Apple Silicon Macs.
It uses the Rust toolchain from `nixpkgs` and only enables Linux runtime
libraries on Linux.

```sh
nix-shell
cargo build --workspace --bins
```

On macOS, the daemon uses the default CoreAudio output device through CPAL. On
Linux, the daemon prefers PipeWire and exposes one logical output per peer when
PipeWire is available.

## Build

```sh
cargo build --workspace --bins
```

The binaries are written to `target/debug/`:

- `target/debug/voicersd`
- `target/debug/voicers`

## Run

Start the daemon:

```sh
./target/debug/voicersd --display-name Alice
```

In another terminal, start the TUI client:

```sh
./target/debug/voicers
```

The daemon listens for local control commands on `127.0.0.1:7767` by default.
It also listens for libp2p peer connections on `/ip4/0.0.0.0/tcp/0` unless a
different listen address is provided.

```sh
./target/debug/voicersd \
  --display-name Alice \
  --listen-addr /ip4/0.0.0.0/tcp/4001
```

STUN probes are enabled by default. To use a specific STUN server, pass
`--stun-server` one or more times:

```sh
./target/debug/voicersd \
  --display-name Alice \
  --stun-server stun.example.net:3478
```

To disable STUN probes:

```sh
./target/debug/voicersd --display-name Alice --no-stun
```

## Connecting Peers

The baseline path is explicit dialing by multiaddr. If Alice has a reachable
address, Bob can ask his local daemon to dial it:

```sh
printf '%s\n' '{"JoinPeer":{"address":"/ip4/203.0.113.10/tcp/4001/p2p/ALICE_PEER_ID"}}' \
  | nc 127.0.0.1 7767
```

For peers on separate home networks, direct dialing only works when at least one
peer has a reachable public address, a port forward, or a working automatic port
mapping. The daemon attempts UPnP and records observed or mapped addresses in
its status.

STUN is also used to discover the public UDP endpoint that the NAT assigns to
the daemon. This is useful diagnostics for NAT behavior and future UDP/WebRTC
transport work. It is not advertised as a libp2p dial address today because the
current libp2p transport listens on TCP.

## Two User Runbook

Build the daemon with WebRTC/ICE support on both machines:

```sh
cargo build -p voicersd --features webrtc-transport
cargo build -p voicers
```

Start Alice. If Alice has a forwarded or reachable TCP port, use that port as
the listen address:

```sh
./target/debug/voicersd \
  --display-name Alice \
  --listen-addr /ip4/0.0.0.0/tcp/4001 \
  --stun-server stun.l.google.com:19302 \
  --turn-server turn.example.net:3478,alice,TURN_SECRET
```

Start Bob:

```sh
./target/debug/voicersd \
  --display-name Bob \
  --listen-addr /ip4/0.0.0.0/tcp/0 \
  --stun-server stun.l.google.com:19302 \
  --turn-server turn.example.net:3478,bob,TURN_SECRET
```

Each user can run the TUI in another terminal:

```sh
./target/debug/voicers
```

Alice gets her status and shares `network.share_invite` with Bob. If
`share_invite` is empty, she can share one of the displayed `external_addrs`,
`observed_addrs`, or `listen_addrs` after appending `/p2p/ALICE_PEER_ID`.

```sh
printf '%s\n' '"GetStatus"' | nc 127.0.0.1 7767
```

Bob dials Alice's shared multiaddr:

```sh
printf '%s\n' '{"JoinPeer":{"address":"ALICE_SHARE_INVITE"}}' \
  | nc 127.0.0.1 7767
```

After the libp2p session connects, either side can start WebRTC/ICE for the
media data path. Bob can get Alice's peer id from status, then create the offer:

```sh
printf '%s\n' '"GetStatus"' | nc 127.0.0.1 7767
```

```sh
printf '%s\n' '{"StartWebRtcOffer":{"peer_id":"ALICE_PEER_ID"}}' \
  | nc 127.0.0.1 7767
```

The daemons exchange the WebRTC offer, answer, and ICE candidates over the
existing libp2p session channel. If ICE opens the `voicers-media` data channel,
audio frames prefer `webrtc-data-channel`; otherwise the daemon keeps using the
libp2p request-response media path.

For two peers behind NAT with no direct reachable TCP port, run or choose a
known libp2p circuit relay and start Alice with `--relay-addr`:

```sh
./target/debug/voicersd \
  --display-name Alice \
  --listen-addr /ip4/0.0.0.0/tcp/0 \
  --relay-addr /ip4/RELAY_PUBLIC_IP/tcp/4001/p2p/RELAY_PEER_ID \
  --stun-server stun.l.google.com:19302 \
  --turn-server turn.example.net:3478,alice,TURN_SECRET
```

Alice then shares the relayed address:

```text
/ip4/RELAY_PUBLIC_IP/tcp/4001/p2p/RELAY_PEER_ID/p2p-circuit/p2p/ALICE_PEER_ID
```

Bob dials that relayed address with the same `JoinPeer` command, then starts
WebRTC with `StartWebRtcOffer`.

## Relay And Hole Punching

`voicersd` supports libp2p relay client reservations, DCUtR hole punching, DNS
multiaddrs, STUN NAT observation, and Kademlia DHT bootstrap.

To reserve through a known relay:

```sh
./target/debug/voicersd \
  --display-name Alice \
  --listen-addr /ip4/0.0.0.0/tcp/0 \
  --relay-addr /ip4/RELAY_PUBLIC_IP/tcp/4001/p2p/RELAY_PEER_ID
```

Alice can then advertise a relayed address:

```text
/ip4/RELAY_PUBLIC_IP/tcp/4001/p2p/RELAY_PEER_ID/p2p-circuit/p2p/ALICE_PEER_ID
```

Bob can dial that address through his local daemon:

```sh
printf '%s\n' '{"JoinPeer":{"address":"/ip4/RELAY_PUBLIC_IP/tcp/4001/p2p/RELAY_PEER_ID/p2p-circuit/p2p/ALICE_PEER_ID"}}' \
  | nc 127.0.0.1 7767
```

When both peers connect through a relay that supports the required libp2p
protocols, DCUtR can coordinate a direct connection attempt. If the hole punch
succeeds, the connection moves from relayed transport to direct transport.

## DHT Bootstrap

By default, the daemon bootstraps against the public libp2p/IPFS bootstrap
peers. This lets it discover relay-capable peers on the public DHT and attempt a
small number of automatic relay reservations.

```sh
./target/debug/voicersd --display-name Alice
```

Custom bootstrap peers can be supplied with repeated `--bootstrap-addr`
arguments:

```sh
./target/debug/voicersd \
  --display-name Alice \
  --bootstrap-addr /ip4/203.0.113.20/tcp/4001/p2p/BOOTSTRAP_PEER_ID \
  --bootstrap-addr /dnsaddr/bootstrap.example.net/p2p/ANOTHER_BOOTSTRAP_PEER_ID
```

The current DHT support is for bootstrap, routing-table discovery, relay
candidate discovery, and relay reservation attempts. It does not yet provide
room-name or user-name rendezvous. For now, peers still need to exchange a
specific multiaddr out of band or through a future rendezvous layer.

## STUN Servers

STUN servers are lightweight public UDP services that answer binding requests
with the public socket address they observe. They are commonly run by RTC,
VoIP, and NAT traversal infrastructure operators. `voicersd` ships with a small
default public STUN server list, and deployments can replace it with
project-controlled servers using `--stun-server`.

Example with two custom STUN servers:

```sh
./target/debug/voicersd \
  --display-name Alice \
  --stun-server stun1.example.net:3478 \
  --stun-server stun2.example.net:3478
```

STUN by itself does not relay traffic and does not guarantee that another peer
can dial in. It tells the daemon what public UDP endpoint the NAT exposed for a
probe, and the same STUN server list is also passed into the optional WebRTC
ICE transport. Relay, DCUtR, and TURN still handle cases where direct NAT
traversal fails.

## TURN Servers

TURN servers relay UDP media when direct ICE candidates cannot connect. They are
heavier than STUN servers because they carry traffic, so production deployments
should use project-controlled credentials instead of relying on public servers.

`voicersd` accepts repeated TURN server entries in `url,username,credential`
form:

```sh
./target/debug/voicersd \
  --display-name Alice \
  --stun-server stun1.example.net:3478 \
  --turn-server turn.example.net:3478,alice,TURN_SECRET
```

The URL may include `turn:` or `turns:`. If no scheme is included, the daemon
uses `turn:`.

## WebRTC / ICE Transport

The UDP NAT-traversal layer uses `webrtc-rs` instead of hand-rolling ICE, DTLS,
SCTP, RTP, SRTP, STUN, and TURN behavior. The daemon keeps
`libp2p` for identity, DHT bootstrap, peer discovery, rendezvous, and signalling,
then uses WebRTC peer connections for the media/data path.

The intended connection ladder becomes:

1. Exchange WebRTC offers, answers, and ICE candidates over the existing daemon
   session/signalling path.
2. Try host, server-reflexive STUN, and TURN/relay ICE candidates.
3. Prefer the selected WebRTC media path when ICE connects.
4. Keep libp2p circuit relay/DCUtR as the daemon-level fallback while WebRTC is
   being brought up.

The initial `webrtc-rs` integration is feature-gated as `webrtc-transport` so
the current TCP/libp2p stack stays buildable while the new transport is wired in.

The daemon session protocol can now carry WebRTC signalling messages. A local
client can ask the daemon to create a WebRTC offer for a connected peer:

```sh
printf '%s\n' '{"StartWebRtcOffer":{"peer_id":"REMOTE_PEER_ID"}}' \
  | nc 127.0.0.1 7767
```

The daemon creates an `RTCPeerConnection`, opens a `voicers-media` data channel,
sets the local offer, forwards the offer over the existing libp2p session
channel, and trickles generated ICE candidates over that same channel.

A local client can also manually send an offer, answer, or ICE candidate to a
connected peer:

```sh
printf '%s\n' '{"SendWebRtcSignal":{"peer_id":"REMOTE_PEER_ID","signal":{"Offer":{"sdp":"v=0..."}}}}' \
  | nc 127.0.0.1 7767
```

```sh
printf '%s\n' '{"SendWebRtcSignal":{"peer_id":"REMOTE_PEER_ID","signal":{"IceCandidate":{"candidate":"candidate:...","sdp_mid":"0","sdp_mline_index":0}}}}' \
  | nc 127.0.0.1 7767
```

Remote offers are applied to a peer connection and answered automatically when
the `webrtc-transport` feature is enabled.

When the `voicers-media` data channel is open, outgoing Opus `MediaFrame`s are
sent over WebRTC first. Until that channel is ready, the daemon keeps using the
existing libp2p request-response media path as a fallback. Incoming WebRTC data
channel frames are decoded through the same jitter/decode pipeline as libp2p
media frames. Daemon status exposes the selected media path, WebRTC connection
state, STUN-observed addresses, and persisted path score counters for direct,
relayed, DCUtR, and WebRTC paths.

## Public Relays

There is no stable, canonical list of public libp2p circuit relays that this
project should depend on. Public relays may be rate-limited, unavailable, or
configured not to accept reservations. For reliable use, run at least one
project-controlled bootstrap/relay node and pass it with `--bootstrap-addr` or
`--relay-addr`.

Useful references:

- STUN RFC 8489: <https://www.rfc-editor.org/rfc/rfc8489>
- webrtc-rs: <https://github.com/webrtc-rs/webrtc>
- webrtc crate docs: <https://docs.rs/webrtc/latest/webrtc/>
- libp2p circuit relay: <https://libp2p.io/docs/circuit-relay/>
- IPFS bootstrap peers: <https://docs.ipfs.tech/how-to/modify-bootstrap-list/>

## Current Limitations

- DHT-backed room rendezvous is not implemented yet.
- Relay support is client-side reservation and dialing support, not a bundled
  relay server mode.
- STUN diagnostics and WebRTC ICE server configuration are implemented; the
  libp2p transport itself still listens on TCP.
- The WebRTC transport is behind `webrtc-transport`; TURN configuration, media
  routing, connection-state reporting, and path score persistence are present,
  but production TURN deployment automation is still external.
- Public DHT and public relay behavior is best-effort.
- macOS currently mixes decoded peer audio into the default output device.
  Linux/PipeWire is the path for per-peer routable output nodes.
