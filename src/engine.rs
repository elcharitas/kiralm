//! `llama-server` process management and capability discovery.

pub mod supervisor;

use std::collections::BTreeSet;
use std::io::Read;
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use tokio::process::{Child, Command};
use tokio::time::Instant;

use crate::config::kiralm_home;

const PROBE_TIMEOUT: Duration = Duration::from_secs(2);

const KNOWN_FLAGS: [&str; 19] = [
    "--batch-size",
    "--cache-prompt",
    "--cache-ram",
    "--cache-reuse",
    "--cache-type-k",
    "--cache-type-v",
    "--cpu-moe",
    "--ctx-size",
    "--flash-attn",
    "--metrics",
    "--mlock",
    "--mmap",
    "--model-draft",
    "--n-cpu-moe",
    "--n-gpu-layers",
    "--parallel",
    "--slot-save-path",
    "--slots",
    "--ubatch-size",
];

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct EngineCapabilities {
    pub version: String,
    pub flags: BTreeSet<String>,
}

impl EngineCapabilities {
    pub fn probe(llama_bin: &str) -> EngineCapabilities {
        let version_bin = llama_bin.to_string();
        let version_probe = std::thread::spawn(move || detect_version(&version_bin));
        let help = run_bounded(llama_bin, &["--help"], PROBE_TIMEOUT);
        let version = version_probe
            .join()
            .unwrap_or_else(|_| "unknown".to_string());
        let flags = if help.timed_out {
            BTreeSet::new()
        } else {
            KNOWN_FLAGS
                .iter()
                .filter(|flag| help.text.contains(**flag))
                .map(|flag| (*flag).to_string())
                .collect()
        };
        EngineCapabilities { version, flags }
    }

    pub fn supports(&self, flag: &str) -> bool {
        self.flags.contains(flag)
    }
}

struct BoundedOutput {
    text: String,
    timed_out: bool,
}

/// `llama-server --version`'s output, used to key the verdict cache so
/// an engine upgrade invalidates stale `unsupported` tags.
pub fn detect_version(llama_bin: &str) -> String {
    let output = run_bounded(llama_bin, &["--version"], PROBE_TIMEOUT);
    if output.timed_out {
        return "unknown".to_string();
    }
    output
        .text
        .lines()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .unwrap_or("unknown")
        .to_string()
}

fn run_bounded(program: &str, args: &[&str], timeout: Duration) -> BoundedOutput {
    let mut child = match std::process::Command::new(program)
        .args(args)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
    {
        Ok(child) => child,
        Err(_) => {
            return BoundedOutput {
                text: String::new(),
                timed_out: false,
            };
        }
    };

    let stdout = child.stdout.take();
    let stderr = child.stderr.take();
    let stdout_reader = std::thread::spawn(move || read_pipe(stdout));
    let stderr_reader = std::thread::spawn(move || read_pipe(stderr));
    let deadline = std::time::Instant::now() + timeout;
    let timed_out = loop {
        match child.try_wait() {
            Ok(Some(_)) => break false,
            Ok(None) if std::time::Instant::now() < deadline => {
                std::thread::sleep(Duration::from_millis(10));
            }
            Ok(None) => {
                let _ = child.kill();
                let _ = child.wait();
                break true;
            }
            Err(_) => break false,
        }
    };

    let mut bytes = stdout_reader.join().unwrap_or_default();
    bytes.extend(stderr_reader.join().unwrap_or_default());
    BoundedOutput {
        text: String::from_utf8_lossy(&bytes).into_owned(),
        timed_out,
    }
}

fn read_pipe(pipe: Option<impl Read>) -> Vec<u8> {
    let Some(mut pipe) = pipe else {
        return Vec::new();
    };
    let mut bytes = Vec::new();
    let _ = pipe.read_to_end(&mut bytes);
    bytes
}

pub fn log_path_for(canonical_id: &str) -> PathBuf {
    use sha1::{Digest, Sha1};
    let mut hasher = Sha1::new();
    hasher.update(canonical_id.as_bytes());
    let digest = hex::encode(hasher.finalize());
    kiralm_home().join("logs").join(format!("{digest}.log"))
}

/// Finds a free port in `range` (inclusive), bind-retrying past ports
/// already bound by a stale child or unrelated local service.
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

/// Always-on flags. Planned options arrive through `extra_args`.
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
        // use the GGUF's own chat template (tool-call syntax lives there)
        // instead of llama-server's generic built-in template
        "--jinja".to_string(),
    ]
}

/// Spawns `llama-server`, piping stdout/stderr to
/// `~/.kiralm/logs/<sha1(canonical_id)>.log` (never the daemon's own
/// stdout).
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

/// Polls `GET /health` every 200ms up to `timeout` (default 30s).
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

/// Sends one tiny completion to prove the child can actually generate,
/// not just answer /health.
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

/// Graceful shutdown: SIGTERM, wait up to 5s, SIGKILL if still alive.
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

#[cfg(unix)]
fn send_sigterm(pid: u32) {
    unsafe {
        libc::kill(pid as i32, libc::SIGTERM);
    }
}

#[cfg(not(unix))]
fn send_sigterm(_pid: u32) {
    // No SIGTERM equivalent wired for non-unix; shutdown()'s
    // 5s-timeout SIGKILL fallback still applies. macOS-first.
}

/// A failed load-verification pass. `permanent` is the caller's signal
/// for whether this is real evidence the architecture is unsupported
/// (worth caching forever) or just a transient hiccup -- a slow first
/// load, a busy port -- that says nothing about the model itself.
pub struct LoadFailure {
    pub message: String,
    pub permanent: bool,
}

/// Runs the full one-shot load-verification pass used by `pull`:
/// spawn, wait healthy, send a tiny completion, always shut down.
pub async fn verify_load(
    llama_bin: &str,
    gguf_path: &Path,
    canonical_id: &str,
    port_range: (u16, u16),
) -> Result<(), LoadFailure> {
    let port = allocate_port(port_range).map_err(|e| LoadFailure {
        message: e.to_string(),
        permanent: false,
    })?;
    let mut child = spawn(llama_bin, gguf_path, canonical_id, port, &[])
        .await
        .map_err(|e| LoadFailure {
            message: e.to_string(),
            permanent: false,
        })?;

    let health = wait_healthy(port, Duration::from_secs(30)).await;
    // A health-check timeout with the child still alive just means it
    // was slow to load (big model, cold disk cache) -- not architecture
    // evidence. An already-exited child, or a completion request that
    // fails after health passed, is a real functional break.
    let crashed = matches!(child.try_wait(), Ok(Some(_)));
    let result = match health {
        Ok(()) => send_tiny_completion(port)
            .await
            .map_err(|e| (e.to_string(), true)),
        Err(e) => Err((e.to_string(), crashed)),
    };

    // Always tear the probe child down, whether or not it succeeded.
    shutdown(child).await;

    result.map_err(|(_, permanent)| LoadFailure {
        message: if permanent {
            format!(
                "llama-server failed while loading this model, see {}",
                log_path_for(canonical_id).display()
            )
        } else {
            "load check timed out (server was still starting) -- not treated as unsupported"
                .to_string()
        },
        permanent,
    })
}

#[cfg(test)]
mod capability_tests {
    use super::*;

    #[cfg(unix)]
    fn script(body: &str) -> PathBuf {
        use std::os::unix::fs::PermissionsExt;

        let path =
            std::env::temp_dir().join(format!("kiralm-engine-probe-{}", uuid::Uuid::new_v4()));
        std::fs::write(&path, format!("#!/bin/sh\n{body}\n")).unwrap();
        let mut permissions = std::fs::metadata(&path).unwrap().permissions();
        permissions.set_mode(0o700);
        std::fs::set_permissions(&path, permissions).unwrap();
        path
    }

    #[cfg(unix)]
    #[test]
    fn capability_probe_finds_supported_flags() {
        let path = script(
            r#"if [ "$1" = "--version" ]; then
  echo "llama.cpp build 123"
else
  echo "--cache-prompt --cache-reuse --parallel --metrics"
fi"#,
        );

        let capabilities = EngineCapabilities::probe(path.to_str().unwrap());
        assert_eq!(capabilities.version, "llama.cpp build 123");
        assert!(capabilities.supports("--cache-prompt"));
        assert!(capabilities.supports("--cache-reuse"));
        assert!(capabilities.supports("--parallel"));
        assert!(capabilities.supports("--metrics"));
        assert!(!capabilities.supports("--model-draft"));

        let _ = std::fs::remove_file(path);
    }

    #[cfg(unix)]
    #[test]
    fn version_probe_times_out_and_stops_the_child() {
        let path = script("sleep 10");
        let started = std::time::Instant::now();
        let output = run_bounded(
            path.to_str().unwrap(),
            &["--version"],
            Duration::from_millis(50),
        );

        assert!(output.timed_out);
        assert!(started.elapsed() < Duration::from_secs(2));

        let _ = std::fs::remove_file(path);
    }
}
