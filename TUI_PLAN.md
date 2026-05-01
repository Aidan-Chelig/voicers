# TUI Plan

## Problem

The current TUI and status model blur together several different concepts:

- `status.peers`
  These are live runtime peer sessions and transports.
- `status.network.known_peers`
  These currently mix together:
  - users we explicitly saved
  - users we have spoken with before
  - routing-only peers learned from invites, DHT, and rendezvous
- pending approvals
  These are inbound join requests, not established room participants.

This causes UI bugs and conceptual bugs:

- The devices/config page can show peer-derived controls for entities that are not really "users in my room".
- The existing "Known Peers" screen is overloaded and does not tell the operator whether they are looking at a friend, a previously seen user, or just a routing hint.
- Discovered peers are being treated as if they were people.

That is the wrong mental model for the product.

## Intended Taxonomy

The TUI should distinguish four separate concepts:

### 1. Engaged Users

Definition:
- users currently in your room
- specifically running `voicersd`
- active voice/session participants you are currently interacting with

Backed by:
- `DaemonStatus.peers`, filtered to active room/session participants

UI meaning:
- these are the only users that belong in the live room views
- these are the only users that belong in per-user audio controls on the devices/config page
- these are the only users that should appear under the primary "who is here right now" flows

### 2. Friends

Definition:
- users you explicitly added to your friend list
- stable, user-curated list
- not a transport/routing cache

Backed by:
- a dedicated persisted collection, not the same bucket as discovered peers

UI meaning:
- list only
- explicit save/remove/rename actions
- reconnect/share shortcuts can exist here
- being a friend does not imply currently connected

### 3. Seen Users

Definition:
- users you have previously been in a voice chat with
- historical memory of real user interaction
- not necessarily explicitly saved as friends

Backed by:
- a dedicated persisted collection or a derived view over user interaction history

UI meaning:
- lightweight history list
- useful for reconnect and recognition
- separate from friends because "I talked to them once" is weaker than "I saved them intentionally"

### 4. Peers

Definition:
- network peers discovered via DHT, relay, rendezvous, invite hints, or other transport mechanisms
- may exist only to provide a route to users
- may not represent a human/user relationship at all

Backed by:
- a dedicated routing/discovery collection

UI meaning:
- hidden from the main UX by default
- diagnostic/network screen only
- never presented as if they are current room participants
- never mixed into friends or seen-users lists

## Immediate UI Rule

The devices/config page must only show engaged users.

That means:

- input gain remains global
- capture devices remain local hardware options
- per-user output volume rows must be derived only from engaged users
- discovered peers must never show up there

This is the most urgent correctness rule because the devices page is for active audio control, not network topology.

## Current Code Issues

### `known_peers` is overloaded

Today `KnownPeerSummary` is being used for multiple incompatible purposes:

- saved people
- remembered people
- invite-seeded route targets
- DHT/rendezvous-discovered peer ids

This makes `known_peers` an implementation convenience, not a truthful user-facing model.

Relevant code:

- `crates/voicersd/src/app.rs`
  `SaveKnownPeer` pins a live peer into `known_peers`
- `crates/voicersd/src/app.rs`
  `seed_invite_hints()` inserts invite-discovered peers into `known_peers`
- `crates/voicersd/src/network.rs`
  `should_surface_connected_peer()` and `should_surface_peer_metadata()` use `known_peers` as one of the conditions for surfacing a runtime peer

### The config page uses runtime peers too broadly

The config page currently builds per-peer controls from `status.peers`.

That is closer to correct than using `known_peers`, but it still needs a stronger rule:

- only engaged users should appear there
- not every surfaced runtime peer should be treated as an active room participant

If runtime surfacing includes bootstrap/routing-only peers, the config page will still be wrong.

### The TUI lacks distinct screens for distinct entity types

Right now the TUI mostly has:

- main room/live peers
- config/devices
- known peers

That is not enough separation for the desired model.

## Proposed Data Model Direction

Do not keep using one `KnownPeerSummary` bucket for everything.

Introduce separate status concepts:

- `engaged_users`
  live users in the current room
- `friends`
  explicit saved users
- `seen_users`
  historical real-user contacts
- `discovered_peers`
  routing/discovery entities

These can be introduced incrementally. The daemon does not need a perfect final schema in one step, but the TUI should stop pretending one collection means all four.

### Suggested shapes

These are conceptual, not final Rust definitions:

- `EngagedUserSummary`
  peer id, display name, room/session status, mute state, output volume, active route, maybe speaking/media state
- `FriendSummary`
  peer id, preferred display name, saved addresses/invite hints, last seen, favorite/pinned flag
- `SeenUserSummary`
  peer id, display name, last seen timestamp, last successful address, maybe room history count
- `DiscoveredPeerSummary`
  peer id, discovery source, discovered addresses, last observed time, relay/direct hints

Important rule:

- user-facing collections should model people
- peer-facing collections should model routing endpoints

## Proposed TUI Information Architecture

### Home

Shows:

- current room
- share invite
- pending approvals
- engaged user summary

Should not show:

- friends
- seen users
- discovered peers

### Peers

Rename the conceptual purpose of the current live peers screen to:

- `Engaged Users`

Shows:

- users currently in your room
- active audio/session details
- current transport only as an implementation detail attached to the engaged user

### Devices

Shows:

- input gain
- capture device
- per-engaged-user output controls only

Should not show:

- discovered peers
- inactive users
- friends who are offline
- seen users who are not present

### Friends

New screen.

Shows:

- explicitly saved users only

Actions:

- rename
- remove
- reconnect
- maybe copy/share preferred join info later

### Seen Users

New screen.

Shows:

- users previously encountered in sessions

Actions:

- promote to friend
- reconnect if viable route info exists
- inspect last-seen metadata

### Network / Peers

New advanced/diagnostic screen.

Shows:

- discovered peer ids
- route hints
- DHT/rendezvous origin
- relay/direct path clues

This is the correct place for transport-oriented entities.

This screen should be optional or secondary, because it is for debugging the network rather than running the voice app.

## Behavioral Rules

### Save Friend

Current `SaveKnownPeer` should conceptually become "Add Friend".

Expected behavior:

- only valid for a real engaged user or a real seen user
- should not turn arbitrary discovered peers into friends just because we learned a route hint

### Seen User creation

A user should enter `seen_users` when:

- they completed enough of a real session to count as a human interaction

Examples:

- active room/session handshake completed
- room join approved and user became active

Non-examples:

- DHT lookup returned a peer id
- an invite contained address hints
- a relay was discovered

### Discovered Peer creation

A peer should enter `discovered_peers` when:

- learned from DHT
- learned from rendezvous
- learned from invite hints
- learned from routing metadata

This collection is operational, not social.

## Rollout Plan

### Phase 1: TUI correctness patch

Goal:
- stop showing the wrong entities in the wrong screens

Changes:

- rename the current live "Peers" concept to "Engaged Users" in copy and help text
- ensure the devices/config page only uses engaged users
- relabel the current `Known Peers` screen to reflect what it truly is today
- add explicit warnings in code comments that `known_peers` is transitional and not the final user model

Success criteria:

- no routing-only entity appears in the devices page
- no screen title implies that discovered peers are friends or room participants

### Phase 2: Split saved users from discovered peers

Goal:
- stop storing friends and discovery hints in the same list

Changes:

- add separate persisted collections for:
  - friends
  - discovered peers
- migrate `SaveKnownPeer` to `AddFriend`
- change `seed_invite_hints()` so it seeds discovered peer state instead of friend state

Success criteria:

- invite/DHT discovery no longer mutates the friend list
- friend list only changes from explicit user action

### Phase 3: Add seen-users history

Goal:
- preserve useful human history without forcing everything into friends

Changes:

- create `seen_users`
- add daemon update points when a session becomes meaningfully active
- add TUI screen and promote-to-friend action

Success criteria:

- past conversation partners are visible
- users do not need to friend everyone just to remember them

### Phase 4: Add advanced network diagnostics screen

Goal:
- keep peer/discovery visibility available without polluting the main UX

Changes:

- dedicated network/peers diagnostic screen
- display DHT/rendezvous/relay-discovered entities there
- move path-score and route-debug concepts closer to that screen

Success criteria:

- routing details are available to developers/power users
- normal voice UX remains people-centered

## Naming Recommendation

Preferred user-facing terms:

- `Engaged Users`
- `Friends`
- `Seen Users`
- `Network Peers`

Why "Engaged Users":

- it is explicit that this is not "all peers we know about"
- it maps to "currently participating in my room"
- it avoids overloading the word "peer"

## Non-Goals

This plan does not require:

- removing peer-level transport internals from the daemon
- redesigning libp2p discovery
- changing the invite format immediately

The immediate goal is to stop leaking routing entities into people-oriented UI.

## First Implementation Tasks

1. Audit every TUI screen and list source and label whether it should consume engaged users, friends, seen users, or discovered peers.
2. Change the devices/config page so its per-user controls are built from engaged users only.
3. Rename `Known Peers` in the UI to a transitional name if needed until the data split lands.
4. Add a new daemon-side collection for discovered peers and move invite-hint seeding there.
5. Add a new daemon-side collection for friends and migrate save/remove actions to it.
6. Add seen-user tracking from successful room/session participation.

## Design Principle

The TUI should be organized around people first and routes second.

If an entity exists only to help packets find their destination, it is not a user-facing person record and should not appear in people-centric screens.
