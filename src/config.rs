//! `~/.nodelm/config.toml` load/parse/defaults. Hand-edited, static (§3).

use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

/// `~/.nodelm` — everything nodelm owns lives under here.
pub fn nodelm_home() -> PathBuf {
    dirs::home_dir()
        .expect("no home directory found")
        .join(".nodelm")
}

pub fn default_config_path() -> PathBuf {
    nodelm_home().join("config.toml")
}

fn default_unix_socket() -> PathBuf {
    nodelm_home().join("nodelm.sock")
}

fn default_model_dir() -> PathBuf {
    nodelm_home().join("models")
}

fn default_http_listen() -> String {
    "127.0.0.1:11535".to_string()
}

fn default_llama_server_bin() -> String {
    // ponytail: naive default path, real install should set this explicitly
    "/opt/homebrew/bin/llama-server".to_string()
}

fn default_port_range() -> (u16, u16) {
    (8700, 8799)
}

/// On-disk config shape. Every field optional so a hand-edited partial
/// file still parses; defaults fill the rest (§3).
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
struct RawConfig {
    ram_headroom_gb: Option<f64>,
    model_dir: Option<PathBuf>,
    unix_socket: Option<PathBuf>,
    http_listen: Option<String>,
    llama_server_bin: Option<String>,
    llama_server_port_range: Option<(u16, u16)>,
}

#[derive(Debug, Clone)]
pub struct Config {
    /// None = auto-tiered default (2GB if total RAM <= 8GB else 1.5GB), §5.1.
    pub ram_headroom_gb: Option<f64>,
    pub model_dir: PathBuf,
    pub unix_socket: PathBuf,
    pub http_listen: String,
    pub llama_server_bin: String,
    pub llama_server_port_range: (u16, u16),
}

impl Default for Config {
    fn default() -> Self {
        Config {
            ram_headroom_gb: None,
            model_dir: default_model_dir(),
            unix_socket: default_unix_socket(),
            http_listen: default_http_listen(),
            llama_server_bin: default_llama_server_bin(),
            llama_server_port_range: default_port_range(),
        }
    }
}

impl Config {
    /// Loads `~/.nodelm/config.toml`, falling back to defaults for any
    /// field that's absent or if the file doesn't exist at all.
    pub fn load() -> anyhow::Result<Config> {
        Config::load_from(&default_config_path())
    }

    pub fn load_from(path: &Path) -> anyhow::Result<Config> {
        let raw: RawConfig = match std::fs::read_to_string(path) {
            Ok(s) => toml::from_str(&s)?,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => RawConfig::default(),
            Err(e) => return Err(e.into()),
        };

        if let Some(gb) = raw.ram_headroom_gb {
            if gb < 1.0 {
                anyhow::bail!(
                    "ram_headroom_gb = {gb} is below the hard floor of 1GB; refusing to start"
                );
            }
        }

        let defaults = Config::default();
        Ok(Config {
            ram_headroom_gb: raw.ram_headroom_gb,
            model_dir: raw.model_dir.unwrap_or(defaults.model_dir),
            unix_socket: raw.unix_socket.unwrap_or(defaults.unix_socket),
            http_listen: raw.http_listen.unwrap_or(defaults.http_listen),
            llama_server_bin: raw.llama_server_bin.unwrap_or(defaults.llama_server_bin),
            llama_server_port_range: raw
                .llama_server_port_range
                .unwrap_or(defaults.llama_server_port_range),
        })
    }
}
