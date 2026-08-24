//! `cloudflared` detection, spawn/health-check/shutdown, and backend
//! provisioning (SPEC.md "Remote tunnel (Cloudflare Tunnel) integration"
//! §"Tunnel client (daemon)"). Mirrors `engine.rs`'s shape one-for-one
//! rather than reinventing process supervision — shutdown reuses
//! `engine::shutdown` verbatim.
//!
//! STUB NOTE: `provision()` below calls nodelm-web's real
//! `POST /api/account/tunnel` shape (nodelm-web SPEC.md §12), but no live
//! Cloudflare account was available to exercise it end to end in this
//! build — same disclosure standard already used for the BYOK provider
//! lanes. `detect()`/`spawn()`/`check_connectivity()`/shutdown are all
//! real and exercised against whatever `cloudflared` (if any) is on this
//! machine.

use std::path::PathBuf;
use std::process::Stdio;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use tokio::process::{Child, Command};

use crate::config::{nodelm_home, Config};

/// `cloudflared --version` presence check (§"Binary") — fails fast with
/// an install hint instead of letting `spawn()`'s raw `ENOENT` leak out.
pub fn detect(cloudflared_bin: &str) -> anyhow::Result<()> {
    std::process::Command::new(cloudflared_bin)
        .arg("--version")
        .output()
        .map_err(|_| {
            anyhow::anyhow!(
                "cloudflared not found at {cloudflared_bin} — brew install cloudflared, \
                 or set tunnel.cloudflared_bin"
            )
        })?;
    Ok(())
}

fn log_path() -> PathBuf {
    nodelm_home().join("logs").join("tunnel.log")
}

/// Spawns `cloudflared tunnel run --token <token>` — Cloudflare's real
/// remotely-managed-tunnel invocation. `token` is the opaque base64
/// credentials blob nodelm-web's provisioning endpoint returns; this
/// daemon never parses it, only passes it through (§"Process supervision").
pub async fn spawn(cloudflared_bin: &str, token: &str) -> anyhow::Result<Child> {
    let path = log_path();
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let log_file = std::fs::OpenOptions::new().create(true).append(true).open(&path)?;
    let log_file_err = log_file.try_clone()?;

    let child = Command::new(cloudflared_bin)
        .args(["tunnel", "run", "--token", token])
        .stdout(Stdio::from(log_file))
        .stderr(Stdio::from(log_file_err))
        .kill_on_drop(true)
        .spawn()?;
    Ok(child)
}

/// Process-alive-only health signal for the supervisor loop, matching
/// `engine.rs`'s own tiered approach (§"Health check").
pub fn is_alive(child: &mut Child) -> bool {
    matches!(child.try_wait(), Ok(None))
}

/// Best-effort `GET http://127.0.0.1:20241/ready` (cloudflared's default
/// metrics port) with a 1s timeout, used by `nodelm tunnel status` on top
/// of `is_alive` — falls back to "unconfirmed" rather than blocking if
/// that port doesn't answer. This exact port number is unverified against
/// a real `cloudflared` install in this build (§"Health check").
pub async fn check_connectivity() -> bool {
    let Ok(client) = reqwest::Client::builder().timeout(Duration::from_secs(1)).build() else {
        return false;
    };
    client
        .get("http://127.0.0.1:20241/ready")
        .send()
        .await
        .map(|r| r.status().is_success())
        .unwrap_or(false)
}

// ---------------------------------------------------------------------
// Backend provisioning
// ---------------------------------------------------------------------

/// `POST {api_base_url}/api/account/tunnel`'s 201 shape, verbatim
/// (nodelm-web SPEC.md §12).
#[derive(Debug, Deserialize)]
pub struct Provisioned {
    #[allow(dead_code)] // not read yet; kept for parity with the wire shape
    pub tunnel_id: String,
    pub cf_tunnel_id: String,
    pub hostname: String,
    pub token: String,
}

/// Calls nodelm-web to provision a real Cloudflare Tunnel for this
/// device, bearer-authed with the same cloud login token cloud-model
/// routing already uses (§"Tunnel provisioning"). Unverified against a
/// live backend/Cloudflare account — see module STUB NOTE.
pub async fn provision(config: &Config) -> anyhow::Result<Provisioned> {
    let token = crate::cloud::ensure_token()?;
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(config.cloud.request_timeout_secs))
        .build()?;
    let resp = client
        .post(format!("{}/api/account/tunnel", config.cloud.api_base_url))
        .bearer_auth(token)
        .send()
        .await?;
    if !resp.status().is_success() {
        let body = resp.text().await.unwrap_or_default();
        anyhow::bail!("tunnel provisioning failed: {body}");
    }
    Ok(resp.json().await?)
}

// ---------------------------------------------------------------------
// Cloudflare tunnel token storage
// ---------------------------------------------------------------------

fn tunnel_token_path() -> PathBuf {
    nodelm_home().join("tunnel_token.json")
}

/// The raw Cloudflare tunnel token `provision()` returns, persisted 0600
/// (same pattern as `CloudCredentials`) so a daemon restart can re-spawn
/// `cloudflared` without a new Cloudflare API round trip (§"Daemon-restart
/// behavior"). Distinct from `tunnel_auth::TunnelCredentials`, which is
/// the *local* bearer token gating the tunnel listener, not this.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TunnelToken {
    pub token: String,
}

impl TunnelToken {
    pub fn load() -> anyhow::Result<Option<TunnelToken>> {
        match std::fs::read_to_string(tunnel_token_path()) {
            Ok(s) => Ok(Some(serde_json::from_str(&s)?)),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e.into()),
        }
    }

    pub fn save(&self) -> anyhow::Result<()> {
        let path = tunnel_token_path();
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
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detect_reports_missing_binary_cleanly() {
        let err = detect("/definitely/not/a/real/path/cloudflared").unwrap_err();
        assert!(err.to_string().contains("cloudflared not found"));
    }

    /// Guards the provisioning response shape against nodelm-web SPEC.md
    /// §12 drift — this daemon must deserialize it byte-for-byte.
    #[test]
    fn provisioned_deserializes_post_account_tunnel_shape() {
        let raw = r#"{"tunnel_id":"t1","cf_tunnel_id":"cf-uuid","hostname":"x-abcd1234.tunnels.nodelm.dev","tunnel_status":"pending","token":"base64blob"}"#;
        let p: Provisioned = serde_json::from_str(raw).unwrap();
        assert_eq!(p.cf_tunnel_id, "cf-uuid");
        assert_eq!(p.hostname, "x-abcd1234.tunnels.nodelm.dev");
        assert_eq!(p.token, "base64blob");
    }

    // `TunnelToken` resolves its path off the process-global `HOME` env
    // var, same caveat `tunnel_auth`'s tests document — one at a time.
    static HOME_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    #[test]
    fn tunnel_token_round_trips_with_owner_only_perms() {
        let _guard = HOME_LOCK.lock().unwrap();
        let dir = std::env::temp_dir().join(format!("nodelm-tunnel-test-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        std::env::set_var("HOME", &dir);

        assert!(TunnelToken::load().unwrap().is_none());
        TunnelToken { token: "cf-token".into() }.save().unwrap();
        let loaded = TunnelToken::load().unwrap().unwrap();
        assert_eq!(loaded.token, "cf-token");

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(tunnel_token_path()).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o600);
        }

        let _ = std::fs::remove_dir_all(&dir);
    }
}
