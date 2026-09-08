//! `~/.kiralm/chats.json` -- chat history, daemon-persisted so it
//! survives a browser reload/relaunch instead of living only in
//! localStorage. Message/tool-step *content* is opaque JSON here: the
//! daemon just stores and returns it verbatim, the UI owns the shape.

use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

use crate::config::kiralm_home;

const SCHEMA_VERSION: u32 = 1;

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
    pub system_prompt: String,
    #[serde(default)]
    pub messages: serde_json::Value,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct Chats {
    pub chats: Vec<Chat>,
}

impl Chats {
    pub fn load() -> anyhow::Result<Chats> {
        Self::load_from(&chats_path())
    }

    pub fn load_from(path: &Path) -> anyhow::Result<Chats> {
        Ok(crate::storage::load_versioned(path, SCHEMA_VERSION, migrate)?.unwrap_or_default())
    }

    /// Atomic rewrite, same posture as state.rs's `State::save`.
    pub fn save(&self) -> anyhow::Result<()> {
        self.save_to(&chats_path())
    }

    pub fn save_to(&self, path: &Path) -> anyhow::Result<()> {
        crate::storage::save_versioned(path, SCHEMA_VERSION, self)
    }
}

fn migrate(version: u32, value: serde_json::Value) -> anyhow::Result<serde_json::Value> {
    match version {
        0 => Ok(value),
        _ => anyhow::bail!("no chats migration from schema version {version}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn legacy_chat_defaults_system_prompt() {
        let chat: Chat = serde_json::from_value(serde_json::json!({
            "id": "chat-1",
            "title": "Legacy",
            "created_at": "2026-01-01T00:00:00.000Z",
            "updated_at": "2026-01-01T00:00:00.000Z",
            "project_id": "project-1",
            "permission": "read-only",
            "messages": []
        }))
        .unwrap();
        assert!(chat.system_prompt.is_empty());
    }
}
