//! `~/.kiralm/chats.json` -- chat history, daemon-persisted so it
//! survives a browser reload/relaunch instead of living only in
//! localStorage. Message/tool-step *content* is opaque JSON here: the
//! daemon just stores and returns it verbatim, the UI owns the shape.

use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

use crate::config::kiralm_home;

pub fn chats_path() -> PathBuf {
    kiralm_home().join("chats.json")
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Chat {
    pub id: String,
    pub title: String,
    pub created_at: String,
    pub updated_at: String,
    pub project_id: String,
    pub permission: String,
    #[serde(default)]
    pub messages: serde_json::Value,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct Chats {
    pub chats: Vec<Chat>,
}

impl Chats {
    pub fn load() -> Chats {
        Self::load_from(&chats_path())
    }

    pub fn load_from(path: &Path) -> Chats {
        match std::fs::read_to_string(path) {
            Ok(s) => serde_json::from_str(&s).unwrap_or_default(),
            Err(_) => Chats::default(),
        }
    }

    /// Atomic rewrite, same posture as state.rs's `State::save`.
    pub fn save(&self) -> anyhow::Result<()> {
        self.save_to(&chats_path())
    }

    pub fn save_to(&self, path: &Path) -> anyhow::Result<()> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let tmp = path.with_extension("json.tmp");
        std::fs::write(&tmp, serde_json::to_string_pretty(self)?)?;
        std::fs::rename(&tmp, path)?;
        Ok(())
    }
}
