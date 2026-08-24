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

fn default_cloud_api_base_url() -> String {
    "http://localhost:3003".to_string()
}

fn default_cloud_client_id() -> String {
    "nodelm-cli".to_string()
}

fn default_cloud_request_timeout_secs() -> u64 {
    10
}

fn default_catalog_sync_interval_secs() -> u64 {
    3600
}

fn default_catalog_sync_on_startup() -> bool {
    true
}

fn default_cloudflared_bin() -> String {
    // ponytail: naive default path, same posture as llama_server_bin above
    "/opt/homebrew/bin/cloudflared".to_string()
}

fn default_tunnel_listen() -> String {
    "127.0.0.1:11536".to_string()
}

/// `[cloud]` table (Cloud integration §"config.toml keys") — same
/// all-`Option` + one-default-fn-per-field pattern as the rest of this
/// file. Never mirrored into state.json (hand-edited static config).
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
struct RawCloudConfig {
    api_base_url: Option<String>,
    client_id: Option<String>,
    request_timeout_secs: Option<u64>,
    catalog_sync_interval_secs: Option<u64>,
    catalog_sync_on_startup: Option<bool>,
}

#[derive(Debug, Clone)]
pub struct CloudConfig {
    pub api_base_url: String,
    pub client_id: String,
    /// Covers device/code, device/token poll, and chat-completion proxy
    /// calls only — the background `GET /api/models` fetch uses a fixed
    /// 3s timeout instead (see `cloud.rs`), deliberately non-configurable.
    pub request_timeout_secs: u64,
    pub catalog_sync_interval_secs: u64,
    pub catalog_sync_on_startup: bool,
}

impl Default for CloudConfig {
    fn default() -> Self {
        CloudConfig {
            api_base_url: default_cloud_api_base_url(),
            client_id: default_cloud_client_id(),
            request_timeout_secs: default_cloud_request_timeout_secs(),
            catalog_sync_interval_secs: default_catalog_sync_interval_secs(),
            catalog_sync_on_startup: default_catalog_sync_on_startup(),
        }
    }
}

/// `[tunnel]` table (§"Tunnel client (daemon)" -- Binary, and
/// §"Remote-request auth" for `listen`) — same all-`Option` +
/// default-fn pattern as the rest of this file.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
struct RawTunnelConfig {
    cloudflared_bin: Option<String>,
    listen: Option<String>,
}

#[derive(Debug, Clone)]
pub struct TunnelConfig {
    pub cloudflared_bin: String,
    /// Second HTTP listener, tunnel-origin traffic only (§"Remote-request auth").
    pub listen: String,
}

impl Default for TunnelConfig {
    fn default() -> Self {
        TunnelConfig {
            cloudflared_bin: default_cloudflared_bin(),
            listen: default_tunnel_listen(),
        }
    }
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
    #[serde(default)]
    cloud: RawCloudConfig,
    #[serde(default)]
    tunnel: RawTunnelConfig,
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
    pub cloud: CloudConfig,
    pub tunnel: TunnelConfig,
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
            cloud: CloudConfig::default(),
            tunnel: TunnelConfig::default(),
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
            cloud: CloudConfig {
                api_base_url: raw.cloud.api_base_url.unwrap_or(defaults.cloud.api_base_url),
                client_id: raw.cloud.client_id.unwrap_or(defaults.cloud.client_id),
                request_timeout_secs: raw
                    .cloud
                    .request_timeout_secs
                    .unwrap_or(defaults.cloud.request_timeout_secs),
                catalog_sync_interval_secs: raw
                    .cloud
                    .catalog_sync_interval_secs
                    .unwrap_or(defaults.cloud.catalog_sync_interval_secs),
                catalog_sync_on_startup: raw
                    .cloud
                    .catalog_sync_on_startup
                    .unwrap_or(defaults.cloud.catalog_sync_on_startup),
            },
            tunnel: TunnelConfig {
                cloudflared_bin: raw.tunnel.cloudflared_bin.unwrap_or(defaults.tunnel.cloudflared_bin),
                listen: raw.tunnel.listen.unwrap_or(defaults.tunnel.listen),
            },
        })
    }
}
