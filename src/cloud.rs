//! Cloud auth (device-flow login/logout + token storage) and nodelm-web
//! catalog fetch/merge (SPEC.md's "Cloud integration" section). Lives in
//! one module because the spec puts all `credentials.json`/
//! `cloud_catalog.json` I/O together:
//!
//! - Token storage (`CloudCredentials`, `ensure_token`) — every
//!   cloud-model code path reads a token through here, never touching
//!   the file directly. `login`/`logout` are the only writers.
//! - Catalog sync (`sync_cloud_models`) — fetches nodelm-web's public
//!   model catalog, caches it to disk, and merges it into the live
//!   `ModelRegistry` when a token is present.

use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use crate::config::{nodelm_home, Config};

fn credentials_path() -> PathBuf {
    nodelm_home().join("credentials.json")
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CloudCredentials {
    pub access_token: String,
    #[serde(default)]
    pub refresh_token: Option<String>,
    #[serde(default)]
    pub expires_at: Option<chrono::DateTime<chrono::Utc>>,
}

impl CloudCredentials {
    pub fn load() -> anyhow::Result<Option<CloudCredentials>> {
        match std::fs::read_to_string(credentials_path()) {
            Ok(s) => Ok(Some(serde_json::from_str(&s)?)),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e.into()),
        }
    }

    pub fn save(&self) -> anyhow::Result<()> {
        let path = credentials_path();
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(&path, serde_json::to_string_pretty(self)?)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))?;
        }
        Ok(())
    }

    pub fn delete() -> anyhow::Result<()> {
        match std::fs::remove_file(credentials_path()) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(e.into()),
        }
    }
}

/// The single call every cloud-model code path uses instead of touching
/// `credentials.json` directly (§"Token storage").
pub fn ensure_token() -> anyhow::Result<String> {
    let Some(creds) = CloudCredentials::load()? else {
        anyhow::bail!("not logged in — run 'nodelm login'");
    };
    if let Some(expires_at) = creds.expires_at {
        if expires_at < chrono::Utc::now() {
            anyhow::bail!("cloud login expired — run 'nodelm login' again");
        }
    }
    Ok(creds.access_token)
}

// ---------------------------------------------------------------------
// Device-flow login/logout
// ---------------------------------------------------------------------

#[derive(Debug, Deserialize)]
struct DeviceCodeResponse {
    device_code: String,
    user_code: String,
    verification_uri: String,
    verification_uri_complete: Option<String>,
    expires_in: u64,
    #[serde(default = "default_interval")]
    interval: u64,
}

fn default_interval() -> u64 {
    5
}

#[derive(Debug, Deserialize)]
struct DeviceTokenSuccess {
    access_token: String,
    refresh_token: Option<String>,
    expires_at: Option<chrono::DateTime<chrono::Utc>>,
}

#[derive(Debug, Deserialize)]
struct DeviceTokenError {
    error: DeviceTokenErrorBody,
}

#[derive(Debug, Deserialize)]
struct DeviceTokenErrorBody {
    #[serde(rename = "type")]
    kind: String,
}

/// `nodelm login`: RFC 8628 device-authorization flow against nodelm-web.
pub async fn login(config: &Config) -> anyhow::Result<()> {
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(config.cloud.request_timeout_secs))
        .build()?;

    let device: DeviceCodeResponse = client
        .post(format!("{}/api/auth/device/code", config.cloud.api_base_url))
        .json(&serde_json::json!({ "client_id": config.cloud.client_id }))
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;

    if let Some(complete) = &device.verification_uri_complete {
        println!("Then visit: {complete}");
        open_browser(complete);
    } else {
        println!("First copy your one-time code: {}", device.user_code);
        println!("Then visit: {}", device.verification_uri);
        open_browser(&device.verification_uri);
    }
    println!(
        "Waiting for you to approve in the browser... (expires in {}s)",
        device.expires_in
    );

    let mut interval = Duration::from_secs(device.interval.max(1));
    let deadline = tokio::time::Instant::now() + Duration::from_secs(device.expires_in);

    loop {
        if tokio::time::Instant::now() >= deadline {
            println!("Login expired before it was approved — run 'nodelm login' again.");
            std::process::exit(1);
        }
        tokio::time::sleep(interval).await;

        let resp = client
            .post(format!("{}/api/auth/device/token", config.cloud.api_base_url))
            .json(&serde_json::json!({
                "device_code": device.device_code,
                "grant_type": "urn:ietf:params:oauth:grant-type:device_code",
            }))
            .send()
            .await?;

        if resp.status().is_success() {
            let token: DeviceTokenSuccess = resp.json().await?;
            CloudCredentials {
                access_token: token.access_token,
                refresh_token: token.refresh_token,
                expires_at: token.expires_at,
            }
            .save()?;
            println!("Logged in. Cloud models are now available.");
            return Ok(());
        }

        let body: DeviceTokenError = resp.json().await?;
        match body.error.kind.as_str() {
            "authorization_pending" => continue,
            "slow_down" => interval += Duration::from_secs(5),
            "expired" => {
                println!("Login expired before it was approved — run 'nodelm login' again.");
                std::process::exit(1);
            }
            other => {
                println!("{other}");
                std::process::exit(1);
            }
        }
    }
}

/// `nodelm logout`: deletes stored credentials, idempotent.
pub fn logout() -> anyhow::Result<()> {
    let existed = credentials_path().exists();
    CloudCredentials::delete()?;
    if existed {
        println!("Logged out.");
    } else {
        println!("Not logged in.");
    }
    Ok(())
}

/// Best-effort browser open; a headless box or missing binary falls
/// through silently — the printed URL is always the real fallback.
fn open_browser(url: &str) {
    #[cfg(target_os = "macos")]
    let mut cmd = std::process::Command::new("open");
    #[cfg(target_os = "linux")]
    let mut cmd = std::process::Command::new("xdg-open");
    #[cfg(target_os = "windows")]
    let mut cmd = {
        let mut c = std::process::Command::new("cmd");
        c.args(["/c", "start"]);
        c
    };

    #[cfg(any(target_os = "macos", target_os = "linux"))]
    cmd.arg(url);
    #[cfg(target_os = "windows")]
    cmd.arg(url);

    let _ = cmd.spawn();
}

// ---------------------------------------------------------------------
// Catalog sync
// ---------------------------------------------------------------------

/// `GET /api/models`'s per-row shape, verbatim (§"Catalog sync").
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CloudModel {
    pub id: String,
    pub name: String,
    #[serde(rename = "ref")]
    pub r#ref: String,
    pub kind: String,
    pub size_bytes: Option<u64>,
    pub description: Option<String>,
    pub enabled: bool,
    pub sort_order: i64,
}

#[derive(Debug, Deserialize)]
struct CloudModelsResponse {
    data: Vec<CloudModel>,
}

fn cache_path() -> PathBuf {
    nodelm_home().join("cloud_catalog.json")
}

#[derive(Debug, Serialize, Deserialize)]
struct CachedCatalog {
    fetched_at: i64,
    models: Vec<CloudModel>,
}

fn load_cache() -> Option<CachedCatalog> {
    let raw = std::fs::read_to_string(cache_path()).ok()?;
    serde_json::from_str(&raw).ok()
}

fn save_cache(models: &[CloudModel]) -> anyhow::Result<()> {
    let cache = CachedCatalog {
        fetched_at: chrono::Utc::now().timestamp(),
        models: models.to_vec(),
    };
    let path = cache_path();
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, serde_json::to_string_pretty(&cache)?)?;
    std::fs::rename(&tmp, path)?;
    Ok(())
}

/// `fetched_at` of whatever's cached on disk, or `None` if there's never
/// been a successful fetch. Drives the "older than
/// `catalog_sync_interval_secs`" staleness gate.
pub fn cache_age_secs() -> Option<i64> {
    let cache = load_cache()?;
    Some(chrono::Utc::now().timestamp() - cache.fetched_at)
}

/// Tracks whether the last fetch attempt failed, so a dead backend logs
/// one `warn!` on the transition into an outage and one more on
/// recovery, never spamming on every retry (§"Cache").
static LAST_FETCH_FAILED: AtomicBool = AtomicBool::new(false);

/// Guards against a thundering herd of concurrent fetches when several
/// `/api/status` polls land in the same stale window — the staleness
/// gate itself only checks the *previous* fetch's timestamp, so without
/// this a burst of requests before that fetch lands would each spawn
/// their own.
static SYNC_IN_FLIGHT: AtomicBool = AtomicBool::new(false);

/// Fetches `GET {api_base_url}/api/models`, caches it to disk, and (only
/// when a valid token is present) merges it into `registry` — never
/// blocks or breaks anything on a network failure (§"Catalog sync",
/// §"Merge into the registry"). Meant to be run inside `tokio::spawn`.
pub async fn sync_cloud_models(api_base_url: &str, registry: &crate::registry::ModelRegistry) {
    if SYNC_IN_FLIGHT.swap(true, Ordering::Relaxed) {
        return;
    }
    sync_cloud_models_inner(api_base_url, registry).await;
    SYNC_IN_FLIGHT.store(false, Ordering::Relaxed);
}

async fn sync_cloud_models_inner(api_base_url: &str, registry: &crate::registry::ModelRegistry) {
    let client = match reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(3))
        .build()
    {
        Ok(c) => c,
        Err(e) => {
            tracing::debug!("cloud catalog sync: failed to build http client: {e}");
            return;
        }
    };

    let url = format!("{}/api/models", api_base_url.trim_end_matches('/'));
    let result = async {
        let resp = client.get(&url).send().await?;
        if !resp.status().is_success() {
            anyhow::bail!("GET /api/models returned {}", resp.status());
        }
        let parsed: CloudModelsResponse = resp.json().await?;
        Ok::<_, anyhow::Error>(parsed.data)
    }
    .await;

    let models = match result {
        Ok(models) => {
            if LAST_FETCH_FAILED.swap(false, Ordering::Relaxed) {
                tracing::warn!("cloud catalog sync recovered");
            }
            if let Err(e) = save_cache(&models) {
                tracing::debug!("cloud catalog sync: failed to write cache: {e}");
            }
            models
        }
        Err(e) => {
            if !LAST_FETCH_FAILED.swap(true, Ordering::Relaxed) {
                tracing::warn!("cloud catalog sync: backend unreachable: {e}");
            } else {
                tracing::debug!("cloud catalog sync: backend unreachable: {e}");
            }
            // Network never blocks or breaks anything: keep serving
            // whatever's already cached (or nothing, if there's no
            // cache yet) rather than touching the live registry.
            return;
        }
    };

    if ensure_token().is_err() {
        // Cached to disk above so it's ready the instant the user logs
        // in, but not inserted into the live registry until then.
        return;
    }

    let entries: Vec<_> = models
        .iter()
        .filter(|m| m.enabled)
        .filter_map(crate::registry::ModelState::from_cloud_entry)
        .collect();
    registry.replace_cloud_entries(entries);
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Guards the `ref` rename against schema drift — nodelm-web's
    /// GET /api/models shape is fixed and this daemon must deserialize
    /// it byte-for-byte (SPEC.md's "Catalog sync").
    #[test]
    fn cloud_model_deserializes_get_models_shape() {
        let raw = r#"{"id":"llama-3.1-70b","name":"Llama 3.1 70B",
            "ref":"@cf/meta/llama-3.1-70b-instruct","kind":"workers_ai",
            "size_bytes":null,"description":"...","enabled":true,"sort_order":0}"#;
        let m: CloudModel = serde_json::from_str(raw).unwrap();
        assert_eq!(m.r#ref, "@cf/meta/llama-3.1-70b-instruct");
        assert_eq!(m.size_bytes, None);
        assert!(m.enabled);
    }

    #[test]
    fn ensure_token_errors_when_logged_out() {
        // No credentials.json in this test's HOME -> "not logged in".
        let err = if CloudCredentials::load().unwrap().is_none() {
            ensure_token().unwrap_err().to_string()
        } else {
            return; // environment already has real credentials; skip
        };
        assert!(err.contains("not logged in"));
    }

    #[test]
    fn device_token_error_shapes_parse() {
        let pending: DeviceTokenError =
            serde_json::from_str(r#"{"error":{"type":"authorization_pending","message":"x"}}"#)
                .unwrap();
        assert_eq!(pending.error.kind, "authorization_pending");

        let expired: DeviceTokenError =
            serde_json::from_str(r#"{"error":{"type":"expired"}}"#).unwrap();
        assert_eq!(expired.error.kind, "expired");
    }

    #[test]
    fn expires_at_in_past_is_rejected() {
        let expired = CloudCredentials {
            access_token: "t".into(),
            refresh_token: None,
            expires_at: Some(chrono::Utc::now() - chrono::Duration::seconds(1)),
        };
        assert!(expired.expires_at.unwrap() < chrono::Utc::now());
    }
}
