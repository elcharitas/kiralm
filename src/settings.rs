//! `~/.kiralm/settings.json` -- UI preferences (composer defaults,
//! notifications), edited from Settings > General and auto-saved.
//! Separate from state.json's daemon-managed model/project/tunnel
//! state, and from config.toml's process-level config: this file is
//! pure UI/UX preference, safe to reset without touching either of
//! those.

use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

use crate::config::kiralm_home;

pub fn settings_path() -> PathBuf {
    kiralm_home().join("settings.json")
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    pub default_permission: String,
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
}

impl Default for Settings {
    fn default() -> Self {
        Settings {
            default_permission: "read-only".to_string(),
            send_on_enter: true,
            notify_on_finish: false,
            default_hot_on_pull: false,
            line_height: "normal".to_string(),
            font: "default".to_string(),
            sidebar_collapsed: false,
            collapsed_workspaces: Vec::new(),
            auto_compact: true,
        }
    }
}

impl Settings {
    pub fn load() -> Settings {
        Self::load_from(&settings_path())
    }

    pub fn load_from(path: &Path) -> Settings {
        match std::fs::read_to_string(path) {
            Ok(s) => serde_json::from_str(&s).unwrap_or_default(),
            Err(_) => Settings::default(),
        }
    }

    /// Atomic rewrite, same posture as state.rs's `State::save`: write
    /// to a tmp file, then rename over the target so a crash mid-write
    /// never leaves a torn file.
    pub fn save(&self) -> anyhow::Result<()> {
        self.save_to(&settings_path())
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
