//! `llama-server` child process supervisor: spawn, flag selection, port
//! allocation, health-check, log piping, graceful shutdown. SPEC.md §4.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use anyhow::{bail, Context, Result};
use sha1::{Digest, Sha1};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::process::Child;

use crate::config::Config;

const HEALTH_POLL_INTERVAL: Duration = Duration::from_millis(200);
const HEALTH_TIMEOUT: Duration = Duration::from_secs(30);
const SHUTDOWN_GRACE: Duration = Duration::from_secs(5);

// Flat per-token KV-cache estimate for now (manifest has no n_layer/
// n_embd yet); revisit once pull.rs captures those GGUF fields.
const KV_BYTES_PER_TOKEN_F16: u64 = 128 * 1024;
const KV_BYTES_PER_TOKEN_Q8: u64 = 64 * 1024;
const MIN_CONTEXT: u32 = 4096;

pub struct SpawnRequest {
    pub canonical_id: String,
    pub gguf_path: PathBuf,
    pub is_moe: bool,
    pub size_bytes: u64,
    pub context_length_trained: u32,
    pub hot: bool,
    /// Only set if the user explicitly paired a draft model; never
    /// auto-guessed from filename.
    pub draft_model_path: Option<PathBuf>,
}

pub struct SpawnOutcome {
    pub child: Child,
    pub port: u16,
    pub pid: u32,
}

/// SPEC.md §4.3: automatic flag selection.
pub fn build_args(req: &SpawnRequest, port: u16, total_ram_bytes: u64, headroom_bytes: u64) -> Vec<String> {
    let usable_ram = total_ram_bytes.saturating_sub(headroom_bytes);
    let quantized_kv = total_ram_bytes <= 8 * 1024 * 1024 * 1024;

    let mut args = vec![
        "-m".to_string(),
        req.gguf_path.to_string_lossy().to_string(),
        "--port".to_string(),
        port.to_string(),
        "--host".to_string(),
        "127.0.0.1".to_string(),
        "-fa".to_string(),
        "on".to_string(),
        "-ngl".to_string(),
        "99".to_string(),
    ];

    if req.is_moe && req.size_bytes > usable_ram {
        args.push("--override-tensor".to_string());
        args.push(".*exps.*=CPU".to_string());
    }

    if quantized_kv {
        args.push("-ctk".to_string());
        args.push("q8_0".to_string());
        args.push("-ctv".to_string());
        args.push("q8_0".to_string());
    }

    let remaining_after_headroom = usable_ram.saturating_sub(req.size_bytes);
    let ctx = estimate_context_size(req.context_length_trained, remaining_after_headroom, quantized_kv);
    args.push("-c".to_string());
    args.push(ctx.to_string());

    if let Some(draft) = &req.draft_model_path {
        args.push("-md".to_string());
        args.push(draft.to_string_lossy().to_string());
    }

    if req.hot {
        args.push("--mlock".to_string());
    }

    args
}

fn estimate_context_size(context_length_trained: u32, remaining_ram_bytes: u64, quantized_kv: bool) -> u32 {
    let per_token = if quantized_kv { KV_BYTES_PER_TOKEN_Q8 } else { KV_BYTES_PER_TOKEN_F16 };
    let max_by_ram = (remaining_ram_bytes / per_token.max(1)) as u32;
    context_length_trained.min(max_by_ram.max(MIN_CONTEXT)).max(MIN_CONTEXT)
}

pub fn sha1_hex(input: &str) -> String {
    let mut hasher = Sha1::new();
    hasher.update(input.as_bytes());
    hex::encode(hasher.finalize())
}

pub fn log_path(nodelm_home: &Path, canonical_id: &str) -> PathBuf {
    nodelm_home.join("logs").join(format!("{}.log", sha1_hex(canonical_id)))
}

/// Bind-test each port in range; if bound (stale child, unrelated local
/// service), try the next rather than hard-failing. SPEC.md §4.1.
pub async fn allocate_port(range: (u16, u16), in_use: &HashSet<u16>) -> Result<u16> {
    for port in range.0..=range.1 {
        if in_use.contains(&port) {
            continue;
        }
        if tokio::net::TcpListener::bind(("127.0.0.1", port)).await.is_ok() {
            return Ok(port);
        }
    }
    bail!("no free port in range {}-{}", range.0, range.1)
}

/// Spawn `llama-server` for `req`. Stdout/stderr go to the per-model log
/// file, never the daemon's own stdout. SPEC.md §4.4.
pub async fn spawn(
    req: &SpawnRequest,
    cfg: &Config,
    nodelm_home: &Path,
    total_ram_bytes: u64,
    headroom_bytes: u64,
    ports_in_use: &HashSet<u16>,
) -> Result<SpawnOutcome> {
    let port = allocate_port(cfg.llama_server_port_range, ports_in_use).await?;
    let args = build_args(req, port, total_ram_bytes, headroom_bytes);

    let log_file_path = log_path(nodelm_home, &req.canonical_id);
    if let Some(parent) = log_file_path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let log_out = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&log_file_path)
        .with_context(|| format!("opening log file {}", log_file_path.display()))?;
    let log_err = log_out.try_clone()?;

    let child = tokio::process::Command::new(&cfg.llama_server_bin)
        .args(&args)
        .stdout(Stdio::from(log_out))
        .stderr(Stdio::from(log_err))
        .stdin(Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .with_context(|| format!("spawning {}", cfg.llama_server_bin.display()))?;

    let pid = child.id().context("spawned child has no pid")?;
    Ok(SpawnOutcome { child, port, pid })
}

/// Read the last `n` lines of a (possibly large) log file for error
/// reporting on a failed/timed-out load.
pub fn tail_log(path: &Path, n: usize) -> String {
    match std::fs::read_to_string(path) {
        Ok(contents) => {
            let lines: Vec<&str> = contents.lines().collect();
            let start = lines.len().saturating_sub(n);
            lines[start..].join("\n")
        }
        Err(_) => String::new(),
    }
}

/// One-shot GET /health probe; true only on an HTTP 200 status line.
async fn probe_health_once(port: u16) -> Result<bool> {
    let mut stream = tokio::net::TcpStream::connect(("127.0.0.1", port)).await?;
    let request = format!("GET /health HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nConnection: close\r\n\r\n");
    stream.write_all(request.as_bytes()).await?;
    let mut buf = Vec::new();
    stream.read_to_end(&mut buf).await?;
    let text = String::from_utf8_lossy(&buf);
    Ok(text.lines().next().unwrap_or("").contains(" 200 "))
}

pub async fn health_check_once(port: u16) -> bool {
    matches!(tokio::time::timeout(Duration::from_millis(1500), probe_health_once(port)).await, Ok(Ok(true)))
}

/// Poll `/health` every 200ms up to 30s. SPEC.md §4.4.
pub async fn wait_until_healthy(port: u16) -> Result<()> {
    let deadline = tokio::time::Instant::now() + HEALTH_TIMEOUT;
    loop {
        if health_check_once(port).await {
            return Ok(());
        }
        if tokio::time::Instant::now() >= deadline {
            bail!("health check timed out after {:?}", HEALTH_TIMEOUT);
        }
        tokio::time::sleep(HEALTH_POLL_INTERVAL).await;
    }
}

/// SIGTERM, wait up to 5s, SIGKILL if still alive. SPEC.md §4.4.
pub async fn shutdown_gracefully(pid: u32, child: &mut Child) {
    use nix::sys::signal::{kill, Signal};
    use nix::unistd::Pid;

    let _ = kill(Pid::from_raw(pid as i32), Signal::SIGTERM);
    if tokio::time::timeout(SHUTDOWN_GRACE, child.wait()).await.is_err() {
        let _ = child.start_kill();
        let _ = child.wait().await;
    }
}

/// Immediate SIGKILL: used when a load attempt must be aborted (health
/// timeout, RSS-over-cap during startup).
pub async fn kill_now(child: &mut Child) {
    let _ = child.start_kill();
    let _ = child.wait().await;
}

#[cfg(test)]
mod tests {
    use super::*;

    const GB: u64 = 1024 * 1024 * 1024;

    fn req(is_moe: bool, size_bytes: u64) -> SpawnRequest {
        SpawnRequest {
            canonical_id: "org/repo:Q4".to_string(),
            gguf_path: PathBuf::from("/tmp/m.gguf"),
            is_moe,
            size_bytes,
            context_length_trained: 32768,
            hot: false,
            draft_model_path: None,
        }
    }

    #[test]
    fn moe_over_ram_gets_expert_cpu_offload() {
        let args = build_args(&req(true, 20 * GB), 8700, 16 * GB, 2 * GB);
        assert!(args.windows(2).any(|w| w[0] == "--override-tensor" && w[1] == ".*exps.*=CPU"));
    }

    #[test]
    fn moe_fitting_comfortably_skips_expert_offload() {
        let args = build_args(&req(true, 4 * GB), 8700, 16 * GB, 2 * GB);
        assert!(!args.iter().any(|a| a == "--override-tensor"));
    }

    #[test]
    fn small_ram_machines_get_quantized_kv_cache() {
        let args = build_args(&req(false, 4 * GB), 8700, 8 * GB, 2 * GB);
        assert!(args.windows(2).any(|w| w[0] == "-ctk" && w[1] == "q8_0"));
    }

    #[test]
    fn large_ram_machines_skip_quantized_kv_cache() {
        let args = build_args(&req(false, 4 * GB), 8700, 32 * GB, 2 * GB);
        assert!(!args.iter().any(|a| a == "-ctk"));
    }

    #[test]
    fn context_size_never_goes_below_floor() {
        // Tiny remaining RAM budget still floors at MIN_CONTEXT, never 0.
        let ctx = estimate_context_size(32768, 1024, false);
        assert_eq!(ctx, MIN_CONTEXT);
    }

    #[test]
    fn context_size_never_exceeds_trained_max() {
        let ctx = estimate_context_size(8192, 1000 * GB, false);
        assert_eq!(ctx, 8192);
    }
}
