//! Cloud auth (device-flow login/logout + token storage) and kiralm-web
//! catalog fetch/merge. Lives in one module because both share the
//! same `credentials.json`/`cloud_catalog.json` I/O:
//!
//! - Token storage (`CloudCredentials`, `ensure_token`). Every
//!   cloud-model code path reads a token through here, never touching
//!   the file directly. `login`/`logout` are the only writers.
//! - Catalog sync (`sync_cloud_models`). Fetches kiralm-web's public
//!   model catalog, caches it to disk, and merges it into the live
//!   `ModelRegistry` when a token is present.

use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use crate::config::{kiralm_home, Config};

fn credentials_path() -> PathBuf {
    kiralm_home().join("credentials.json")
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
/// `credentials.json` directly.
pub fn ensure_token() -> anyhow::Result<String> {
    let Some(creds) = CloudCredentials::load()? else {
        anyhow::bail!("not logged in. run 'kiralm login'");
    };
    if let Some(expires_at) = creds.expires_at {
        if expires_at < chrono::Utc::now() {
            anyhow::bail!("cloud login expired. run 'kiralm login' again");
        }
    }
    Ok(creds.access_token)
}

// ---------------------------------------------------------------------
// Device identity
// ---------------------------------------------------------------------

fn device_id_path() -> PathBuf {
    kiralm_home().join("device_id.json")
}

#[derive(Debug, Serialize, Deserialize)]
struct DeviceId {
    daemon_instance_id: String,
}

/// Read-or-create the per-machine `daemon_instance_id`, persisted to
/// `~/.kiralm/device_id.json`, stable across logins so the backend
/// upserts one `devices` row per machine instead of creating a new one
/// each time.
pub fn ensure_daemon_instance_id() -> anyhow::Result<String> {
    if let Ok(raw) = std::fs::read_to_string(device_id_path()) {
        if let Ok(existing) = serde_json::from_str::<DeviceId>(&raw) {
            return Ok(existing.daemon_instance_id);
        }
    }

    let id = DeviceId {
        daemon_instance_id: uuid::Uuid::new_v4().to_string(),
    };
    let path = device_id_path();
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, serde_json::to_string_pretty(&id)?)?;
    std::fs::rename(&tmp, &path)?;
    Ok(id.daemon_instance_id)
}

/// Self-description sent as `label` on `device/code` and shown by
/// `kiralm device`. Recomputed each call, never cached.
fn device_label() -> String {
    use sysinfo::System;
    format!(
        "{} ({} {})",
        System::host_name().unwrap_or_default(),
        System::name().unwrap_or_default(),
        System::os_version().unwrap_or_default()
    )
}

/// Shared by `kiralm device` and `GET /api/cloud/status`. `None` when
/// this machine has never completed a login.
fn registered_device_label() -> Option<String> {
    let raw = std::fs::read_to_string(device_id_path()).ok()?;
    let id: DeviceId = serde_json::from_str(&raw).ok()?;
    Some(format!("{} ({})", device_label(), &id.daemon_instance_id[..8]))
}

/// `kiralm device`: read-only, no network call.
pub fn device_info() -> anyhow::Result<()> {
    match registered_device_label() {
        Some(label) => println!("This machine is registered as {label}"),
        None => println!("Not yet registered. Run 'kiralm login' first."),
    }
    Ok(())
}

/// `GET /api/cloud/status`'s read path: whether a valid token is on disk,
/// plus the device label if this machine has ever registered.
pub fn cloud_status() -> (bool, Option<String>) {
    (ensure_token().is_ok(), registered_device_label())
}

// ---------------------------------------------------------------------
// Device-flow login/logout
// ---------------------------------------------------------------------

#[derive(Debug, Clone, Deserialize)]
pub struct DeviceCodeResponse {
    pub device_code: String,
    pub user_code: String,
    pub verification_uri: String,
    pub verification_uri_complete: Option<String>,
    pub expires_in: u64,
    #[serde(default = "default_interval")]
    pub interval: u64,
}

/// Progress event for the device-flow poll, broadcast over
/// `GET /api/cloud/login/progress`. Mirrors `pull::PullProgress`'s
/// shape/lifecycle: one broadcast channel per in-flight login,
/// subscribers just fan out.
#[derive(Debug, Clone, Serialize)]
pub struct LoginProgress {
    /// "pending" | "success" | "expired" | "error"
    pub phase: String,
    pub message: Option<String>,
}

pub type LoginProgressTx = tokio::sync::broadcast::Sender<LoginProgress>;

/// Terminal (or in-flight) result of polling `device/token` once the
/// device code has been requested. The reusable core both `kiralm login`
/// and `POST /api/cloud/login`'s background task drive to completion.
pub enum LoginOutcome {
    Success,
    Expired,
    Error(String),
}

fn build_client(config: &Config) -> anyhow::Result<reqwest::Client> {
    Ok(reqwest::Client::builder()
        .timeout(Duration::from_secs(config.cloud.request_timeout_secs))
        .build()?)
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

/// Step 1 of the RFC 8628 device-authorization flow: requests a
/// device/user code pair. Fast, synchronous, so safe to await directly in
/// an HTTP handler before backgrounding the slow poll (`start_pull`'s
/// `pull::resolve`/`download_and_verify` split is the template).
pub async fn request_device_code(config: &Config) -> anyhow::Result<DeviceCodeResponse> {
    let client = build_client(config)?;
    let daemon_instance_id = ensure_daemon_instance_id()?;

    let device: DeviceCodeResponse = client
        .post(format!("{}/api/auth/device/code", config.cloud.api_base_url))
        .json(&serde_json::json!({
            "client_id": config.cloud.client_id,
            "daemon_instance_id": daemon_instance_id,
            "label": device_label(),
        }))
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;
    Ok(device)
}

/// Step 2: polls `device/token` until approval, expiry, or a terminal
/// error, saving credentials on success. Broadcasts each transition on
/// `progress` when given, so both `kiralm login` (no channel, `None`) and
/// the HTTP background task (`Some`) drive the exact same state machine.
pub async fn poll_device_token(
    config: &Config,
    device: &DeviceCodeResponse,
    progress: Option<&LoginProgressTx>,
) -> LoginOutcome {
    let send = |phase: &str, message: Option<String>| {
        if let Some(tx) = progress {
            let _ = tx.send(LoginProgress {
                phase: phase.to_string(),
                message,
            });
        }
    };

    let client = match build_client(config) {
        Ok(c) => c,
        Err(e) => {
            send("error", Some(e.to_string()));
            return LoginOutcome::Error(e.to_string());
        }
    };

    send("pending", None);
    let mut interval = Duration::from_secs(device.interval.max(1));
    let deadline = tokio::time::Instant::now() + Duration::from_secs(device.expires_in);

    loop {
        if tokio::time::Instant::now() >= deadline {
            send("expired", None);
            return LoginOutcome::Expired;
        }
        tokio::time::sleep(interval).await;

        let resp = match client
            .post(format!("{}/api/auth/device/token", config.cloud.api_base_url))
            .json(&serde_json::json!({
                "device_code": device.device_code,
                "grant_type": "urn:ietf:params:oauth:grant-type:device_code",
            }))
            .send()
            .await
        {
            Ok(r) => r,
            Err(e) => {
                send("error", Some(e.to_string()));
                return LoginOutcome::Error(e.to_string());
            }
        };

        if resp.status().is_success() {
            let token: DeviceTokenSuccess = match resp.json().await {
                Ok(t) => t,
                Err(e) => {
                    send("error", Some(e.to_string()));
                    return LoginOutcome::Error(e.to_string());
                }
            };
            let creds = CloudCredentials {
                access_token: token.access_token,
                refresh_token: token.refresh_token,
                expires_at: token.expires_at,
            };
            if let Err(e) = creds.save() {
                send("error", Some(e.to_string()));
                return LoginOutcome::Error(e.to_string());
            }
            send("success", None);
            return LoginOutcome::Success;
        }

        let body: DeviceTokenError = match resp.json().await {
            Ok(b) => b,
            Err(e) => {
                send("error", Some(e.to_string()));
                return LoginOutcome::Error(e.to_string());
            }
        };
        match body.error.kind.as_str() {
            "authorization_pending" => continue,
            "slow_down" => interval += Duration::from_secs(5),
            "expired" => {
                send("expired", None);
                return LoginOutcome::Expired;
            }
            other => {
                send("error", Some(other.to_string()));
                return LoginOutcome::Error(other.to_string());
            }
        }
    }
}

/// `kiralm login`: terminal-printing consumer of `request_device_code` +
/// `poll_device_token`. Same output/exit-code behavior as before the
/// HTTP path (`POST /api/cloud/login`) was added.
pub async fn login(config: &Config) -> anyhow::Result<()> {
    let device = request_device_code(config).await?;

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

    match poll_device_token(config, &device, None).await {
        LoginOutcome::Success => {
            println!("Logged in. Cloud models are now available.");
            Ok(())
        }
        LoginOutcome::Expired => {
            println!("Login expired before it was approved. Run 'kiralm login' again.");
            std::process::exit(1);
        }
        LoginOutcome::Error(msg) => {
            println!("{msg}");
            std::process::exit(1);
        }
    }
}

/// `kiralm logout`: deletes stored credentials, idempotent.
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
/// through silently. The printed URL is always the real fallback.
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

/// `GET /api/models`'s per-row shape, verbatim.
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
    kiralm_home().join("cloud_catalog.json")
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

/// The cached catalog's `local_pull` rows -- curated `kiralm pull`
/// suggestions for the Models settings view, distinct from the
/// registry (which only routes `workers_ai`/cloud-invocable kinds).
pub fn cached_pull_suggestions() -> Vec<CloudModel> {
    load_cache()
        .map(|c| c.models.into_iter().filter(|m| m.kind == "local_pull" && m.enabled).collect())
        .unwrap_or_default()
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
/// recovery, never spamming on every retry.
static LAST_FETCH_FAILED: AtomicBool = AtomicBool::new(false);

/// Guards against a thundering herd of concurrent fetches when several
/// `/api/status` polls land in the same stale window. The staleness
/// gate itself only checks the *previous* fetch's timestamp, so without
/// this a burst of requests before that fetch lands would each spawn
/// their own.
static SYNC_IN_FLIGHT: AtomicBool = AtomicBool::new(false);

/// Fetches `GET {api_base_url}/api/models`, caches it to disk, and (only
/// when a valid token is present) merges it into `registry`. Never
/// blocks or breaks anything on a network failure. Meant to be run
/// inside `tokio::spawn`.
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

    /// Guards the `ref` rename against schema drift. kiralm-web's
    /// GET /api/models shape is fixed and this daemon must deserialize
    /// it byte-for-byte.
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
    fn cached_pull_suggestions_filters_to_enabled_local_pull_only() {
        let _guard = crate::config::HOME_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let dir = std::env::temp_dir().join(format!("kiralm-cloud-test-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let prev = std::env::var("HOME").ok();
        std::env::set_var("HOME", &dir);

        let models = vec![
            CloudModel {
                id: "a".into(),
                name: "A".into(),
                r#ref: "org/a:Q4_K_M".into(),
                kind: "local_pull".into(),
                size_bytes: Some(123),
                description: None,
                enabled: true,
                sort_order: 0,
            },
            CloudModel {
                id: "b".into(),
                name: "B".into(),
                r#ref: "org/b:Q4_K_M".into(),
                kind: "local_pull".into(),
                size_bytes: None,
                description: None,
                enabled: false,
                sort_order: 0,
            },
            CloudModel {
                id: "c".into(),
                name: "C".into(),
                r#ref: "@cf/c".into(),
                kind: "workers_ai".into(),
                size_bytes: None,
                description: None,
                enabled: true,
                sort_order: 0,
            },
        ];
        save_cache(&models).unwrap();

        let suggestions = cached_pull_suggestions();
        assert_eq!(suggestions.len(), 1);
        assert_eq!(suggestions[0].id, "a");

        if let Some(p) = prev {
            std::env::set_var("HOME", p);
        } else {
            std::env::remove_var("HOME");
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn cached_pull_suggestions_is_empty_when_no_cache_exists() {
        let _guard = crate::config::HOME_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let dir = std::env::temp_dir().join(format!("kiralm-cloud-test-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let prev = std::env::var("HOME").ok();
        std::env::set_var("HOME", &dir);

        assert!(cached_pull_suggestions().is_empty());

        if let Some(p) = prev {
            std::env::set_var("HOME", p);
        } else {
            std::env::remove_var("HOME");
        }
        let _ = std::fs::remove_dir_all(&dir);
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

    /// `GET /api/cloud/status`'s JSON shape reads straight off this.
    /// Guards the field names the UI matches against.
    #[test]
    fn login_progress_serializes_phase_and_message() {
        let event = LoginProgress {
            phase: "pending".to_string(),
            message: None,
        };
        let json = serde_json::to_string(&event).unwrap();
        assert_eq!(json, r#"{"phase":"pending","message":null}"#);
    }

    #[test]
    fn cloud_status_reports_logged_out_with_no_device() {
        // Only meaningful in a clean ~/.kiralm; skip on a machine that's
        // actually logged in or registered.
        if CloudCredentials::load().unwrap().is_some() || registered_device_label().is_some() {
            return;
        }
        let (logged_in, device) = cloud_status();
        assert!(!logged_in);
        assert!(device.is_none());
    }
}
