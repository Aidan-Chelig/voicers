use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use voicers_core::{
    DaemonStatus, NetworkSummary, RoomInviteSummary, RoomMemberSummary, RoomPermission,
    RoomRoleSummary, RoomSummary, SessionHello,
};

use crate::persist::PersistedState;

const DEFAULT_ROOM_NAME: &str = "main";
static INVITE_COUNTER: AtomicU64 = AtomicU64::new(0);

pub fn persisted_state_from_status(state: &DaemonStatus) -> PersistedState {
    PersistedState {
        local_display_name: Some(state.session.display_name.clone()),
        rooms: state.rooms.clone(),
        known_peer_addrs: state.network.saved_peer_addrs.clone(),
        known_peers: state.network.known_peers.clone(),
        ignored_peer_ids: state.network.ignored_peer_ids.clone(),
        last_share_invite: state.network.direct_call_invite.clone(),
        path_scores: state.network.path_scores.clone(),
    }
}

pub fn persisted_state_from_network(
    local_display_name: String,
    rooms: Vec<RoomSummary>,
    network: NetworkSummary,
) -> PersistedState {
    PersistedState {
        local_display_name: Some(local_display_name),
        rooms,
        known_peer_addrs: network.saved_peer_addrs,
        known_peers: network.known_peers,
        ignored_peer_ids: network.ignored_peer_ids,
        last_share_invite: network.direct_call_invite,
        path_scores: network.path_scores,
    }
}

pub fn fresh_invite_code(peer_id: &str) -> (String, u64) {
    let now_ms = now_ms();
    let expires_at_ms = now_ms.saturating_add(60 * 60 * 1000);
    let uniqueness = INVITE_COUNTER.fetch_add(1, Ordering::Relaxed);
    let seed = format!("{peer_id}:{now_ms}:{uniqueness}");
    let mut hash = std::collections::hash_map::DefaultHasher::new();
    use std::hash::{Hash, Hasher};
    seed.hash(&mut hash);
    let code = format!("{:08x}", hash.finish())[..8].to_ascii_lowercase();
    (code, expires_at_ms)
}

pub fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis() as u64)
        .unwrap_or_default()
}

pub fn normalize_room_name(room_name: &str) -> String {
    let normalized = room_name.trim();
    if normalized.is_empty() {
        DEFAULT_ROOM_NAME.to_string()
    } else {
        normalized.to_string()
    }
}

pub fn create_or_update_room(
    state: &mut DaemonStatus,
    room_name: &str,
    local_peer_id: &str,
    display_name: &str,
) {
    for room in &mut state.rooms {
        room.engaged = room.name == room_name;
    }

    if let Some(room) = state.rooms.iter_mut().find(|room| room.name == room_name) {
        room.engaged = true;
        ensure_admin_role(room);
        ensure_local_member(room, local_peer_id, display_name, true);
        return;
    }

    let mut room = RoomSummary {
        name: room_name.to_string(),
        engaged: true,
        roles: Vec::new(),
        members: Vec::new(),
        current_invite: None,
    };
    ensure_admin_role(&mut room);
    ensure_local_member(&mut room, local_peer_id, display_name, true);
    state.rooms.push(room);
    state
        .rooms
        .sort_by(|left, right| left.name.cmp(&right.name));
}

pub fn set_room_invite(
    state: &mut DaemonStatus,
    room_name: &str,
    invite_code: String,
    expires_at_ms: Option<u64>,
) {
    if let Some(room) = state.rooms.iter_mut().find(|room| room.name == room_name) {
        room.current_invite = Some(RoomInviteSummary {
            invite_code,
            share_invite: None,
            expires_at_ms,
            created_by_peer_id: Some(state.local_peer_id.clone()),
        });
    }
}

pub fn clear_room_invite(state: &mut DaemonStatus, room_name: &str) {
    if let Some(room) = state.rooms.iter_mut().find(|room| room.name == room_name) {
        room.current_invite = None;
    }
}

pub fn room_has_permission(
    state: &DaemonStatus,
    room_name: &str,
    peer_id: &str,
    permission: RoomPermission,
) -> bool {
    let Some(room) = state.rooms.iter().find(|room| room.name == room_name) else {
        return false;
    };
    let Some(member) = room.members.iter().find(|member| member.peer_id == peer_id) else {
        return false;
    };

    member.roles.iter().any(|role_name| {
        room.roles
            .iter()
            .find(|role| role.name == *role_name)
            .map(|role| {
                role.permissions
                    .iter()
                    .any(|candidate| candidate == &permission)
            })
            .unwrap_or(false)
    })
}

pub fn sync_local_room_memberships(
    rooms: &mut [RoomSummary],
    local_peer_id: &str,
    display_name: &str,
) {
    for room in rooms {
        ensure_admin_role(room);
        if room.engaged
            || room
                .members
                .iter()
                .any(|member| member.peer_id == local_peer_id)
        {
            ensure_local_member(room, local_peer_id, display_name, room.members.is_empty());
        } else {
            for member in &mut room.members {
                if member.peer_id == local_peer_id {
                    member.display_name = display_name.to_string();
                }
            }
        }
    }
}

pub fn build_local_hello(state: &DaemonStatus) -> SessionHello {
    let trusted_contacts = state
        .network
        .known_peers
        .iter()
        .filter(|peer| peer.trusted_contact)
        .map(|peer| peer.peer_id.clone())
        .collect();
    SessionHello {
        room_name: state.session.room_name.clone(),
        display_name: state.session.display_name.clone(),
        trusted_contacts,
    }
}

fn ensure_admin_role(room: &mut RoomSummary) {
    if room.roles.iter().any(|role| role.name == "admin") {
        return;
    }

    room.roles.push(RoomRoleSummary {
        name: "admin".to_string(),
        permissions: vec![
            RoomPermission::CreateRoomInvite,
            RoomPermission::CreateRole,
            RoomPermission::EditRole,
            RoomPermission::AssignRole,
        ],
    });
}

fn ensure_local_member(
    room: &mut RoomSummary,
    local_peer_id: &str,
    display_name: &str,
    seed_admin: bool,
) {
    let maybe_member = room
        .members
        .iter_mut()
        .find(|member| member.peer_id == local_peer_id || member.peer_id.is_empty());

    match maybe_member {
        Some(member) => {
            member.peer_id = local_peer_id.to_string();
            member.display_name = display_name.to_string();
            if seed_admin && !member.roles.iter().any(|role| role == "admin") {
                member.roles.push("admin".to_string());
            }
        }
        None => room.members.push(RoomMemberSummary {
            peer_id: local_peer_id.to_string(),
            display_name: display_name.to_string(),
            roles: if seed_admin {
                vec!["admin".to_string()]
            } else {
                Vec::new()
            },
        }),
    }
}
