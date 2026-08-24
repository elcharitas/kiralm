//! `~/.nodelm/config.toml` load/parse/defaults. SPEC.md §3.

use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use serde::Deserialize;

const GB: f64 = 1024.0 * 1024.0 * 1024.0;
const HEADROOM_FLOOR_GB: f64 = 1.0;

#[derive(Debug, Clone, Deserialize)]
pub struct Config {
    /// Absent = auto-tiered default (2GB if total RAM <= 8GB else 1.5GB).
    pub ram_headroom_gb: Option<f64>,
    #[serde(default = "default_model_dir")]
    pub model_dir: PathBuf,
    #[serde(default = "default_unix_socket")]
    pub unix_socket: PathBuf,
    #[serde(default = "default_http_listen")]
    pub http_listen: String,
    #[serde(default = "default_llama_server_bin")]
    pub llama_server_bin: PathBuf,
    #[serde(default = "default_port_range")]
    pub llama_server_port_range: (u16, u16),
}

fn nodelm_home() -> PathBuf {
    dirs::home_dir().unwrap_or_else(|| PathBuf::from(".")).join(".nodelm")
}
fn default_model_dir() -> PathBuf {
    nodelm_home().join("models")
}
fn default_unix_socket() -> PathBuf {
    nodelm_home().join("nodelm.sock")
}
fn default_http_listen() -> String {
    "127.0.0.1:11535".to_string()
}
fn default_llama_server_bin() -> PathBuf {
    PathBuf::from("/opt/homebrew/bin/llama-server")
}
fn default_port_range() -> (u16, u16) {
    (8700, 8799)
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
    /// Missing file = defaults. Present file must parse and pass validation.
    pub fn load(path: &Path) -> Result<Self> {
        let cfg = if path.exists() {
            let raw = std::fs::read_to_string(path)
                .with_context(|| format!("reading config at {}", path.display()))?;
            toml::from_str(&raw).with_context(|| format!("parsing config at {}", path.display()))?
        } else {
            Config::default()
        };
        cfg.validate()?;
        Ok(cfg)
    }

    fn validate(&self) -> Result<()> {
        if let Some(gb) = self.ram_headroom_gb {
            if gb < HEADROOM_FLOOR_GB {
                bail!(
                    "ram_headroom_gb = {gb} is below the hard floor of {HEADROOM_FLOOR_GB}GB; no override allowed"
                );
            }
        }
        Ok(())
    }

    /// SPEC.md §5.1: headroom subtracted from total RAM once, up front.
    pub fn headroom_bytes(&self, total_ram_bytes: u64) -> u64 {
        let gb = self.ram_headroom_gb.unwrap_or_else(|| {
            if total_ram_bytes <= 8 * (GB as u64) {
                2.0
            } else {
                1.5
            }
        });
        (gb * GB) as u64
    }
}
