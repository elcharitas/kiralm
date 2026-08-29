//! `~/.kiralm/config.toml` load/parse/defaults. Hand-edited, static.

use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

/// `~/.kiralm`. Everything kiralm owns lives under here.
pub fn kiralm_home() -> PathBuf {
    if let Some(path) = std::env::var_os("KIRALM_HOME") {
        return PathBuf::from(path);
    }
    dirs::home_dir()
        .expect("no home directory found")
        .join(".kiralm")
}

pub fn default_config_path() -> PathBuf {
    kiralm_home().join("config.toml")
}

// `HOME` is process-global, so any test that swaps it (tunnel.rs's,
// skills.rs's) must serialize against every other one, not just its
// own module's tests -- a single shared lock, not one per module.
#[cfg(test)]
pub(crate) static HOME_ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

fn default_unix_socket() -> PathBuf {
    kiralm_home().join("kiralm.sock")
}

fn default_model_dir() -> PathBuf {
    kiralm_home().join("models")
}

fn default_http_listen() -> String {
    "127.0.0.1:11535".to_string()
}

fn default_llama_server_bin() -> String {
    // Naive default path, real install should set this explicitly
    "/opt/homebrew/bin/llama-server".to_string()
}

fn default_port_range() -> (u16, u16) {
    (8700, 8799)
}

fn default_cache_quota_gb() -> f64 {
    50.0
}

fn default_optimization_profile() -> String {
    "balanced".to_string()
}

fn default_cloud_api_base_url() -> String {
    "http://localhost:3003".to_string()
}

fn default_cloud_client_id() -> String {
    "kiralm-cli".to_string()
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
    // Naive default path, same posture as llama_server_bin above
    "/opt/homebrew/bin/cloudflared".to_string()
}

fn default_tunnel_listen() -> String {
    "127.0.0.1:11536".to_string()
}

fn default_dsh_bin() -> String {
    // Bare command, not a homebrew-style absolute path: dsh is
    // installed via npm/pnpm global, which already puts it on PATH.
    "dsh".to_string()
}

/// `[cloud]` table. Same all-`Option` + one-default-fn-per-field
/// pattern as the rest of this file. Never mirrored into state.json
/// (hand-edited static config).
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
    /// calls only. The background `GET /api/models` fetch uses a fixed
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

/// `[tunnel]` table. Same all-`Option` + default-fn pattern as the
/// rest of this file.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
struct RawTunnelConfig {
    cloudflared_bin: Option<String>,
    listen: Option<String>,
}

#[derive(Debug, Clone)]
pub struct TunnelConfig {
    pub cloudflared_bin: String,
    /// Second HTTP listener, tunnel-origin traffic only.
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

/// `[dsh]` table. Same all-`Option` + default-fn pattern as the rest of
/// this file.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
struct RawDshConfig {
    bin: Option<String>,
}

#[derive(Debug, Clone)]
pub struct DshConfig {
    pub bin: String,
}

impl Default for DshConfig {
    fn default() -> Self {
        DshConfig { bin: default_dsh_bin() }
    }
}

/// On-disk config shape. Every field optional so a hand-edited partial
/// file still parses; defaults fill the rest.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
struct RawConfig {
    ram_headroom_gb: Option<f64>,
    model_dir: Option<PathBuf>,
    unix_socket: Option<PathBuf>,
    http_listen: Option<String>,
    llama_server_bin: Option<String>,
    llama_server_port_range: Option<(u16, u16)>,
    cache_quota_gb: Option<f64>,
    optimization_profile: Option<String>,
    #[serde(default)]
    cloud: RawCloudConfig,
    #[serde(default)]
    tunnel: RawTunnelConfig,
    #[serde(default)]
    dsh: RawDshConfig,
}

#[derive(Debug, Clone)]
pub struct Config {
    /// None = auto-tiered default (2GB if total RAM <= 8GB else 1.5GB).
    pub ram_headroom_gb: Option<f64>,
    pub model_dir: PathBuf,
    pub unix_socket: PathBuf,
    pub http_listen: String,
    pub llama_server_bin: String,
    pub llama_server_port_range: (u16, u16),
    pub cache_quota_gb: f64,
    pub optimization_profile: String,
    pub cloud: CloudConfig,
    pub tunnel: TunnelConfig,
    pub dsh: DshConfig,
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
            cache_quota_gb: default_cache_quota_gb(),
            optimization_profile: default_optimization_profile(),
            cloud: CloudConfig::default(),
            tunnel: TunnelConfig::default(),
            dsh: DshConfig::default(),
        }
    }
}

impl Config {
    /// Loads `~/.kiralm/config.toml`, falling back to defaults for any
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
        if raw.cache_quota_gb.is_some_and(|gb| gb < 1.0) {
            anyhow::bail!("cache_quota_gb must be at least 1GB");
        }
        if let Some(profile) = raw.optimization_profile.as_deref() {
            if crate::engine::optimization::OptimizationProfile::parse(profile)
                .is_none()
            {
                anyhow::bail!(
                    "optimization_profile must be balanced, throughput, latency, or memory"
                );
            }
        }

        let defaults = Config::default();
        let unix_socket = std::env::var_os("KIRALM_UNIX_SOCKET")
            .map(PathBuf::from)
            .or(raw.unix_socket)
            .unwrap_or(defaults.unix_socket);
        let cloud_api_base_url = std::env::var("KIRALM_CLOUD_API_BASE_URL")
            .ok()
            .or(raw.cloud.api_base_url)
            .unwrap_or(defaults.cloud.api_base_url);
        Ok(Config {
            ram_headroom_gb: raw.ram_headroom_gb,
            model_dir: raw.model_dir.unwrap_or(defaults.model_dir),
            unix_socket,
            http_listen: raw.http_listen.unwrap_or(defaults.http_listen),
            llama_server_bin: raw.llama_server_bin.unwrap_or(defaults.llama_server_bin),
            llama_server_port_range: raw
                .llama_server_port_range
                .unwrap_or(defaults.llama_server_port_range),
            cache_quota_gb: raw
                .cache_quota_gb
                .unwrap_or(defaults.cache_quota_gb),
            optimization_profile: raw
                .optimization_profile
                .unwrap_or(defaults.optimization_profile),
            cloud: CloudConfig {
                api_base_url: cloud_api_base_url,
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
            dsh: DshConfig {
                bin: raw.dsh.bin.unwrap_or(defaults.dsh.bin),
            },
        })
    }
}
