use std::{
    env, fs,
    path::{Path, PathBuf},
    sync::Arc,
};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use voicers_core::{KnownPeerSummary, NetworkSummary, PathScoreSummary};

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct PersistedState {
    #[serde(default)]
    pub local_display_name: Option<String>,
    #[serde(default)]
    pub known_peer_addrs: Vec<String>,
    #[serde(default)]
    pub known_peers: Vec<KnownPeerSummary>,
    #[serde(default)]
    pub ignored_peer_ids: Vec<String>,
    #[serde(default)]
    pub last_share_invite: Option<String>,
    #[serde(default)]
    pub path_scores: Vec<PathScoreSummary>,
}

#[derive(Clone)]
pub struct PersistenceHandle {
    path: Arc<PathBuf>,
}

impl PersistenceHandle {
    pub fn new(path: PathBuf) -> Self {
        Self {
            path: Arc::new(path),
        }
    }

    pub fn load(&self) -> Result<PersistedState> {
        if !self.path.exists() {
            return Ok(PersistedState::default());
        }

        let contents = fs::read_to_string(self.path.as_ref()).with_context(|| {
            format!("failed to read persisted state at {}", self.path.display())
        })?;
        let state = serde_json::from_str(&contents).with_context(|| {
            format!(
                "failed to decode persisted state at {}",
                self.path.display()
            )
        })?;

        Ok(state)
    }

    pub fn save_network(&self, network: &NetworkSummary) -> Result<()> {
        let existing = self.load().unwrap_or_default();
        let state = PersistedState {
            local_display_name: existing.local_display_name,
            known_peer_addrs: network.saved_peer_addrs.clone(),
            known_peers: network.known_peers.clone(),
            ignored_peer_ids: network.ignored_peer_ids.clone(),
            last_share_invite: network.share_invite.clone(),
            path_scores: network.path_scores.clone(),
        };

        self.save(&state)
    }

    fn save(&self, state: &PersistedState) -> Result<()> {
        if let Some(parent) = self.path.parent() {
            fs::create_dir_all(parent).with_context(|| {
                format!(
                    "failed to create persistence directory {}",
                    parent.display()
                )
            })?;
        }

        let encoded =
            serde_json::to_vec_pretty(state).context("failed to encode persisted state")?;
        let tmp_path = temporary_path(self.path.as_ref());
        fs::write(&tmp_path, encoded).with_context(|| {
            format!(
                "failed to write temporary state file {}",
                tmp_path.display()
            )
        })?;
        fs::rename(&tmp_path, self.path.as_ref()).with_context(|| {
            format!(
                "failed to move persisted state into place at {}",
                self.path.display()
            )
        })?;

        Ok(())
    }

    pub fn save_full(&self, state: &PersistedState) -> Result<()> {
        self.save(state)
    }
}

pub fn default_state_path() -> PathBuf {
    if let Some(config_home) = env::var_os("XDG_CONFIG_HOME") {
        return PathBuf::from(config_home)
            .join("voicers")
            .join("daemon-state.json");
    }

    if let Some(home) = env::var_os("HOME") {
        return PathBuf::from(home)
            .join(".config")
            .join("voicers")
            .join("daemon-state.json");
    }

    PathBuf::from("daemon-state.json")
}

fn temporary_path(path: &Path) -> PathBuf {
    let mut os = path.as_os_str().to_os_string();
    os.push(".tmp");
    PathBuf::from(os)
}
