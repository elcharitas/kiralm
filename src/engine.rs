//! `llama-server` spawn/health-check/kill, flag selection, port
//! allocation, log piping (§4).
//!
//! STUB NOTE: this worktree only needed enough of the engine to run the
//! pull load-verification pass (§8 step 4) and to give `serve` something
//! real to construct. The full automatic-flag matrix (§4.3, MoE expert
//! offload, KV cache quant tiering, context-size-vs-remaining-RAM sizing,
//! `-md` draft pairing) and the steady-state health/RSS watcher (§4.4
//! "while loaded") belong to whichever agent owns residency's runtime
//! loop; only the always-on flags and the one-shot health+completion
//! check used at pull time are implemented here.

use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use tokio::process::{Child, Command};
use tokio::time::Instant;

use crate::config::kiralm_home;

/// `llama-server --version`'s output, used to key the verdict cache
/// (§3/§5.2) so an engine upgrade invalidates stale `unsupported` tags.
pub fn detect_version(llama_bin: &str) -> String {
    std::process::Command::new(llama_bin)
        .arg("--version")
        .output()
        .ok()
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "unknown".to_string())
}

pub fn log_path_for(canonical_id: &str) -> PathBuf {
    use sha1::{Digest, Sha1};
    let mut hasher = Sha1::new();
    hasher.update(canonical_id.as_bytes());
    let digest = hex::encode(hasher.finalize());
    kiralm_home().join("logs").join(format!("{digest}.log"))
}

/// Finds a free port in `range` (inclusive), bind-retrying past ports
/// already bound by a stale child or unrelated local service (§4.1).
pub fn allocate_port(range: (u16, u16)) -> anyhow::Result<u16> {
    for port in range.0..=range.1 {
        if TcpListener::bind(("127.0.0.1", port)).is_ok() {
            return Ok(port);
        }
    }
    anyhow::bail!(
        "no free port in configured range {}-{}",
        range.0,
        range.1
    )
}

/// Always-on flags per §4.3. The RAM/MoE/hot-conditional flags (offload,
/// ctx sizing, ctk/ctv, --mlock, -md) are the fuller residency manager's
/// job. See module STUB NOTE.
fn base_args(gguf_path: &Path, port: u16) -> Vec<String> {
    vec![
        "-m".to_string(),
        gguf_path.display().to_string(),
        "--port".to_string(),
        port.to_string(),
        "-fa".to_string(),
        "on".to_string(),
        "-ngl".to_string(),
        "99".to_string(),
    ]
}

/// Spawns `llama-server`, piping stdout/stderr to
/// `~/.kiralm/logs/<sha1(canonical_id)>.log` (never the daemon's own
/// stdout, §4.4).
pub async fn spawn(
    llama_bin: &str,
    gguf_path: &Path,
    canonical_id: &str,
    port: u16,
    extra_args: &[String],
) -> anyhow::Result<Child> {
    let log_path = log_path_for(canonical_id);
    if let Some(parent) = log_path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let log_file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&log_path)?;
    let log_file_err = log_file.try_clone()?;

    let mut args = base_args(gguf_path, port);
    args.extend_from_slice(extra_args);

    let child = Command::new(llama_bin)
        .args(&args)
        .stdout(Stdio::from(log_file))
        .stderr(Stdio::from(log_file_err))
        .kill_on_drop(true)
        .spawn()?;

    Ok(child)
}

/// Polls `GET /health` every 200ms up to `timeout` (default 30s, §4.4).
pub async fn wait_healthy(port: u16, timeout: Duration) -> anyhow::Result<()> {
    let client = reqwest::Client::new();
    let url = format!("http://127.0.0.1:{port}/health");
    let deadline = Instant::now() + timeout;
    loop {
        if let Ok(resp) = client.get(&url).timeout(Duration::from_secs(2)).send().await {
            if resp.status().is_success() {
                return Ok(());
            }
        }
        if Instant::now() >= deadline {
            anyhow::bail!("timed out waiting for /health after {timeout:?}");
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
}

/// Sends one tiny completion to prove the child can actually generate
/// (§8 step 4), not just answer /health.
pub async fn send_tiny_completion(port: u16) -> anyhow::Result<()> {
    let client = reqwest::Client::new();
    let url = format!("http://127.0.0.1:{port}/v1/chat/completions");
    let body = serde_json::json!({
        "model": "kiralm-load-verify",
        "messages": [{"role": "user", "content": "hi"}],
        "max_tokens": 1,
        "stream": false,
    });
    let resp = client
        .post(&url)
        .json(&body)
        .timeout(Duration::from_secs(30))
        .send()
        .await?;
    if !resp.status().is_success() {
        let text = resp.text().await.unwrap_or_default();
        anyhow::bail!("tiny completion request failed: {text}");
    }
    Ok(())
}

/// Last `n` lines of a log file, used as the captured error text on a
/// failed load (§4.4).
pub fn tail_log(canonical_id: &str, n: usize) -> String {
    let path = log_path_for(canonical_id);
    match std::fs::read_to_string(path) {
        Ok(s) => s
            .lines()
            .rev()
            .take(n)
            .collect::<Vec<_>>()
            .into_iter()
            .rev()
            .collect::<Vec<_>>()
            .join("\n"),
        Err(_) => String::new(),
    }
}

/// Graceful shutdown: SIGTERM, wait up to 5s, SIGKILL if still alive
/// (§4.4).
pub async fn shutdown(mut child: Child) {
    if let Some(pid) = child.id() {
        send_sigterm(pid);
    }
    let waited = tokio::time::timeout(Duration::from_secs(5), child.wait()).await;
    if waited.is_err() {
        let _ = child.start_kill();
        let _ = child.wait().await;
    }
}

/// Same graceful-shutdown shape as `shutdown()`, but for a pid we only
/// have a bare `u32` for (e.g. `DELETE /api/models/:id` on a registry
/// entry that doesn't own the `Child` handle).
pub async fn kill_pid_graceful(pid: u32) {
    send_sigterm(pid);
    tokio::time::sleep(Duration::from_secs(5)).await;
    #[cfg(unix)]
    unsafe {
        // Still alive? SIGKILL. kill(pid, 0) is the standard existence probe.
        if libc::kill(pid as i32, 0) == 0 {
            libc::kill(pid as i32, libc::SIGKILL);
        }
    }
}

#[cfg(unix)]
fn send_sigterm(pid: u32) {
    unsafe {
        libc::kill(pid as i32, libc::SIGTERM);
    }
}

#[cfg(not(unix))]
fn send_sigterm(_pid: u32) {
    // ponytail: no SIGTERM equivalent wired for non-unix; shutdown()'s
    // 5s-timeout SIGKILL fallback still applies. macOS-first per SPEC §10.
}

/// Runs the full one-shot load-verification pass used by `pull` (§8 step
/// 4): spawn, wait healthy, send a tiny completion, always shut down.
/// Returns `Err` with the captured log tail on any failure.
pub async fn verify_load(
    llama_bin: &str,
    gguf_path: &Path,
    canonical_id: &str,
    port_range: (u16, u16),
) -> Result<(), String> {
    let port = allocate_port(port_range).map_err(|e| e.to_string())?;
    let child = spawn(llama_bin, gguf_path, canonical_id, port, &[])
        .await
        .map_err(|e| e.to_string())?;

    let health = wait_healthy(port, Duration::from_secs(30)).await;
    let result = match health {
        Ok(()) => send_tiny_completion(port).await.map_err(|e| e.to_string()),
        Err(e) => Err(e.to_string()),
    };

    // Always tear the probe child down, whether or not it succeeded.
    shutdown(child).await;

    result.map_err(|_| tail_log(canonical_id, 50)).map(|_| ())
}
