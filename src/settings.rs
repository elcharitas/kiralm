//! `~/.kiralm/settings.json` -- UI preferences (composer defaults,
//! notifications), edited from Settings > General and auto-saved.
//! Separate from state.json's daemon-managed model/project/tunnel
//! state, and from config.toml's process-level config: this file is
//! pure UI/UX preference, safe to reset without touching either of
//! those.

use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

use crate::config::kiralm_home;

const SCHEMA_VERSION: u32 = 1;

pub fn settings_path() -> PathBuf {
    kiralm_home().join("settings.json")
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    pub default_permission: String,
    /// User-authored instructions applied to every agent turn, below
    /// KiraLM's immutable core prompt and above workspace/chat layers.
    pub custom_instructions: String,
    pub send_on_enter: bool,
    pub notify_on_finish: bool,
    pub default_hot_on_pull: bool,
    /// "compact" | "normal" | "relaxed", applied as a CSS custom
    /// property, not a fixed value baked into index.css.
    pub line_height: String,
    /// "default" (the bundled Open Sans build) or a real system font
    /// family name from the browser's Local Font Access API.
    pub font: String,
    pub sidebar_collapsed: bool,
    /// Project ids whose chat list is collapsed in the sidebar.
    pub collapsed_workspaces: Vec<String>,
    /// Whether the UI should summarize a chat's older messages once it
    /// grows large, instead of always resending the full history. The
    /// summarization itself is client-driven (a plain chat-completion
    /// call), this flag just gates whether it fires automatically.
    pub auto_compact: bool,
    pub runtime_dock_collapsed: bool,
    pub runtime_dock_height: u32,
    pub runtime_dock_tab: String,
}

impl Default for Settings {
    fn default() -> Self {
        Settings {
            default_permission: "read-only".to_string(),
            custom_instructions: String::new(),
            send_on_enter: true,
            notify_on_finish: false,
            default_hot_on_pull: false,
            line_height: "normal".to_string(),
            font: "default".to_string(),
            sidebar_collapsed: false,
            collapsed_workspaces: Vec::new(),
            auto_compact: true,
            runtime_dock_collapsed: true,
            runtime_dock_height: 280,
            runtime_dock_tab: "terminal".to_string(),
        }
    }
}

impl Settings {
    pub fn load() -> anyhow::Result<Settings> {
        Self::load_from(&settings_path())
    }

    pub fn load_from(path: &Path) -> anyhow::Result<Settings> {
        Ok(crate::storage::load_versioned(path, SCHEMA_VERSION, migrate)?.unwrap_or_default())
    }

    /// Atomic rewrite, same posture as state.rs's `State::save`: write
    /// to a tmp file, then rename over the target so a crash mid-write
    /// never leaves a torn file.
    pub fn save(&self) -> anyhow::Result<()> {
        self.save_to(&settings_path())
    }

    pub fn save_to(&self, path: &Path) -> anyhow::Result<()> {
        crate::storage::save_versioned(path, SCHEMA_VERSION, self)
    }
}

fn migrate(version: u32, value: serde_json::Value) -> anyhow::Result<serde_json::Value> {
    match version {
        0 => Ok(value),
        _ => anyhow::bail!("no settings migration from schema version {version}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn legacy_settings_default_custom_instructions() {
        let mut value = serde_json::to_value(Settings::default()).unwrap();
        value.as_object_mut().unwrap().remove("custom_instructions");
        let settings: Settings = serde_json::from_value(value).unwrap();
        assert!(settings.custom_instructions.is_empty());
    }
}
