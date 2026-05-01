# Rooms And Roles Plan

## Why This Needs A Real Migration

The current daemon is built around a single active room:

- `SessionSummary.room_name` is a single `Option<String>`
- `SessionSummary.invite_code` is a single code
- `SessionHello.room_name` is a single room field
- rendezvous publication currently publishes one room namespace at a time
- the TUI `Rooms` page is a derived view over live state, not a true membership model

What you want is materially different:

- a user can belong to many rooms at once
- invites are per-room
- roles are per-room
- permissions are additive through roles
- only admins can create invites
- roles themselves are creatable by admins
- room engagement is a TUI concern layered over a broader room membership model

That means we should not keep stretching the single-room model. We need explicit room membership state.

## Product Model

### Room

A room is a durable object with:

- stable room id
- display name
- local membership state
- role definitions
- invite definitions
- per-member role assignments

### Member

A member is identified by peer id and has:

- display name if known
- assigned roles for that room

### Role

A role is:

- room-local
- named
- an additive set of permissions

Roles are not hard-coded to one global meaning. `admin` is just the first built-in role we seed.

### Permission

Permissions should be stored explicitly, not inferred from role names.

Initial permissions:

- `create_invite`
- `create_role`
- `edit_role`
- `assign_role`
- `remove_role`

We can add more later without changing the model.

### Invite

An invite is:

- one of two distinct concepts:
  - 1:1 call invite
  - room invite

These must not be treated as the same thing.

#### 1:1 Call Invite

A 1:1 call invite is:

- tied to a person/session edge, not a room
- intended to establish a direct user-to-user call relationship
- not governed by room roles
- not a room membership action

#### Room Invite

A room invite is:

- tied to exactly one room
- created by a user with permission
- shareable independently of other rooms
- the only thing that should cause a remote user to join that room

## UX Semantics

### Rooms Page

The TUI `Rooms` page should show durable local room memberships, not just rooms inferred from current traffic.

A room has one of two TUI states:

- engaged
- not engaged

Engaged means:

- the local user is currently participating in that room in the TUI sense
- its active users appear in the engaged-users view

Not engaged means:

- the user is still a member
- the room still exists locally
- its invite and member/role metadata are still visible
- it is just not the active foreground engagement context

### Calls Page

The `Calls` page should show active user sessions that are not explained by current engaged room state.

That means:

- active direct user sessions outside engaged rooms
- not arbitrary connected transport peers
- not discovery/routing records

### Invite Creation

Invites should be created from an explicit context, not globally.

For room invites:

- only members with the `admin` role can create room invites

For 1:1 call invites:

- no room role is required because the invite is not a room operation
- it should resolve into a call/session relationship, not room membership

### Roles UX

For the first implementation:

- each new room gets a built-in `admin` role
- the room creator gets `admin`
- other members start with no roles
- no roles means ordinary member

Later:

- admins can create additional roles
- admins can assign/remove roles

## Current Code Constraints

### Single-room state

Current single-room fields:

- `voicers_core::SessionSummary.room_name`
- `voicers_core::SessionSummary.invite_code`
- `voicers_core::SessionSummary.invite_expires_at_ms`
- `voicers_core::SessionHello.room_name`

These need to stop being the canonical room model.

### Invite/rendezvous coupling

Current invite publication in `network.rs` assumes:

- one active invite model at a time
- one optional invite code at a time
- one room namespace publication path at a time

This must split into:

- direct/person invite handling for 1:1 calls
- per-room publication state for room invites

### Peer session state

Current peer session state stores one active room per peer session:

- `PeerSessionState::Active { room_name, display_name }`

That is still acceptable for one session edge at a time, but the local user model must support many room memberships at once.

## Proposed Data Model

### Core Types

Add new shared types in `voicers-core`:

- `RoomId`
  likely `String` initially
- `Permission`
  enum
- `RoleSummary`
  role name + permissions
- `RoomMemberSummary`
  peer id, display name, assigned role names
- `RoomInviteSummary`
  invite code / share invite / expiry / creator
- `RoomSummary`
  room id, display name, engaged bool, local roles, members, invites

Add to daemon status:

- `rooms: Vec<RoomSummary>`

Keep current `session.room_name` temporarily during migration, then retire it after the TUI and network stop depending on it.

### Persisted State

Extend `PersistedState` with:

- durable room list
- local room role definitions
- local room member assignments
- per-room invite metadata
- engaged room ids

The persistence layer should own this explicitly rather than inferring it from current network status.

### Local Role Evaluation

Add helper:

- `room_has_permission(room, local_peer_id, permission) -> bool`

Rules:

- permissions come from union of assigned roles
- roles are additive
- no role means no elevated permissions

## Implementation Phases

### Phase 1: Introduce durable room state

Goal:

- make rooms first-class persistent objects

Changes:

- add `RoomSummary`-style state to `voicers-core`
- extend persistence to store rooms
- seed default `admin` role on room creation
- mark creator as admin
- add TUI `Rooms` screen backing from stored room memberships instead of live inference

Notes:

- keep the current single active room behavior temporarily
- `engaged` can still be only one room at first if needed while the data model lands

### Phase 2: Make invites per-room

Goal:

- stop using one daemon-global invite model

Changes:

- split invite handling into two explicit types:
  - 1:1 call invites
  - room invites
- replace `session.invite_code` / `session.invite_expires_at_ms` with per-room invite state for room invites
- require a room id when creating/rotating a room invite
- publish rendezvous records per room invite
- keep direct call invites out of room-role permission checks
- `JoinPeer` invite decoding must preserve whether the target is:
  - a direct call target
  - a room target
  - a raw peer id / raw room namespace fallback during migration

Rules:

- only admin can create a room invite
- each room invite resolves to exactly one room
- each 1:1 call invite resolves to a direct call/session path, not room membership

### Phase 3: Multi-room membership and engagement

Goal:

- allow a local user to belong to many rooms

Changes:

- introduce `engaged_room_ids`
- TUI `Rooms` page toggles engagement rather than switching one global room
- active room views consume only engaged rooms
- non-engaged room memberships remain visible in the room list

Important:

- the TUI notion of engagement can be multi-room even if some transport/session logic remains serialized at first
- do not let the UI imply exclusive membership once the model supports many rooms

### Phase 4: Session and routing updates

Goal:

- make room membership explicit in protocol flows

Changes:

- replace `SessionHello.room_name` with room identity that maps to a durable room record
- ensure inbound approvals are room-specific
- ensure reconnect and invite flows target a room, not just a peer
- ensure 1:1 call invites target a direct call/session flow, not a room join

### Phase 5: Admin role management

Goal:

- allow admins to create and manage roles

Changes:

- daemon requests for create/edit role
- daemon requests for assign/remove role
- TUI room details page for roles and member assignments

Initial UI can be simple:

- select room
- list members
- list roles
- assign role

## Suggested First Slice

The safest first code slice is:

1. add durable room state and persistence
2. seed built-in `admin` on room creation
3. split invite types into 1:1 call invites and room invites at the shared-type/API level
4. move room-invite state from daemon-global to per-room
5. keep one engaged room temporarily while data migrates
6. then expand to multi-room engagement

This avoids trying to change invites, permissions, persistence, and engagement semantics all at once.

## What Should Not Be Done Halfway

Do not partially ship these mismatched states:

- 1:1 call invites and room invites sharing one ambiguous decode path
- per-room invites with only daemon-global room state
- multi-room TUI engagement with only one backend room slot
- role UI with no authoritative permission evaluation in the daemon

Those states will look like the feature exists while the backend still behaves as single-room.

## Immediate Next Task

Implement Phase 1:

- add durable room summaries to shared types
- persist them
- back the TUI `Rooms` page from those stored room summaries
- seed built-in `admin` on room creation

After that, implement Phase 2 so invites become truly room-scoped.
After that, implement Phase 2 so invites become explicitly split between 1:1 calls and rooms, and room invites become truly room-scoped.
