//! Subcommand handlers (§7). Every subcommand other than `serve` is a
//! thin HTTP client over `~/.nodelm/nodelm.sock`, auto-spawning the
//! daemon if the socket isn't live.

use http_body_util::{BodyExt, Full};
use hyper::body::Bytes;
use hyper::{Method, Request, StatusCode};
use hyper_util::client::legacy::Client;
use hyperlocal::{UnixClientExt, UnixConnector, Uri};
use std::io::Write;
use std::time::Duration;

use crate::config::{nodelm_home, Config};

type UnixClient = Client<UnixConnector, Full<Bytes>>;

/// Connects (spawning the daemon first if needed), then issues one
/// request and returns `(status, parsed JSON body)`.
async fn request(
    config: &Config,
    method: Method,
    path: &str,
    body: Option<serde_json::Value>,
) -> anyhow::Result<(StatusCode, serde_json::Value)> {
    ensure_daemon_running(config).await?;
    let client: UnixClient = Client::unix();
    let uri: hyper::Uri = Uri::new(&config.unix_socket, path).into();

    let payload = match &body {
        Some(v) => serde_json::to_vec(v)?,
        None => Vec::new(),
    };
    let mut builder = Request::builder().method(method).uri(uri);
    if body.is_some() {
        builder = builder.header("content-type", "application/json");
    }
    let req = builder.body(Full::new(Bytes::from(payload)))?;

    let resp = client.request(req).await?;
    let status = resp.status();
    let bytes = resp.into_body().collect().await?.to_bytes();
    let value: serde_json::Value = if bytes.is_empty() {
        serde_json::Value::Null
    } else {
        serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null)
    };
    Ok((status, value))
}

/// Reads an SSE (`text/event-stream`) response line by line, invoking
/// `on_data` with each event's raw `data:` payload.
async fn stream_sse(
    config: &Config,
    method: Method,
    path: &str,
    body: Option<serde_json::Value>,
    mut on_data: impl FnMut(&str) -> bool, // return false to stop early
) -> anyhow::Result<()> {
    ensure_daemon_running(config).await?;
    let client: UnixClient = Client::unix();
    let uri: hyper::Uri = Uri::new(&config.unix_socket, path).into();

    let payload = match &body {
        Some(v) => serde_json::to_vec(v)?,
        None => Vec::new(),
    };
    let mut builder = Request::builder().method(method).uri(uri);
    if body.is_some() {
        builder = builder.header("content-type", "application/json");
    }
    let req = builder.body(Full::new(Bytes::from(payload)))?;

    let mut resp = client.request(req).await?;
    if !resp.status().is_success() {
        let bytes = resp.into_body().collect().await?.to_bytes();
        anyhow::bail!("request failed: {}", String::from_utf8_lossy(&bytes));
    }

    let mut buf = String::new();
    while let Some(frame_result) = resp.frame().await {
        let frame = frame_result?;
        if let Some(chunk) = frame.data_ref() {
            buf.push_str(&String::from_utf8_lossy(chunk));
            while let Some(pos) = buf.find("\n\n") {
                let event = buf[..pos].to_string();
                buf.drain(..pos + 2);
                for line in event.lines() {
                    if let Some(data) = line.strip_prefix("data: ").or_else(|| line.strip_prefix("data:")) {
                        if !on_data(data.trim()) {
                            return Ok(());
                        }
                    }
                }
            }
        }
    }
    Ok(())
}

/// Spawns `nodelm serve` detached (setsid, own session, so it survives
/// the CLI exiting), redirected to `~/.nodelm/nodelm.log`, then polls
/// the socket for readiness (§7 closing paragraph).
async fn ensure_daemon_running(config: &Config) -> anyhow::Result<()> {
    if tokio::net::UnixStream::connect(&config.unix_socket).await.is_ok() {
        return Ok(());
    }

    std::fs::create_dir_all(nodelm_home())?;
    let log_path = nodelm_home().join("nodelm.log");
    let log_file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&log_path)?;
    let log_file_err = log_file.try_clone()?;

    let exe = std::env::current_exe()?;
    let mut cmd = std::process::Command::new(exe);
    cmd.arg("serve")
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::from(log_file))
        .stderr(std::process::Stdio::from(log_file_err));

    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        // SAFETY: setsid() is async-signal-safe; called post-fork, pre-exec.
        unsafe {
            cmd.pre_exec(|| {
                if libc::setsid() == -1 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }
    }
    cmd.spawn()?;

    for _ in 0..50 {
        if tokio::net::UnixStream::connect(&config.unix_socket).await.is_ok() {
            return Ok(());
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    anyhow::bail!(
        "nodelm serve did not come up in time; check log at {}",
        log_path.display()
    )
}

/// Percent-encodes a single path segment. Canonical ids contain `/` and
/// `:` (§6), which must not be read as extra path segments/scheme by the
/// router or by hyperlocal's `Uri`.
fn encode_path_segment(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => out.push(b as char),
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

fn print_error_and_exit(status: StatusCode, body: &serde_json::Value) -> ! {
    let msg = body
        .get("error")
        .map(|e| {
            if let Some(s) = e.as_str() {
                s.to_string()
            } else {
                e.get("message")
                    .and_then(|m| m.as_str())
                    .unwrap_or("unknown error")
                    .to_string()
            }
        })
        .unwrap_or_else(|| format!("request failed with status {status}"));
    eprintln!("error: {msg}");
    std::process::exit(1);
}

pub async fn pull(config: &Config, r#ref: &str) -> anyhow::Result<()> {
    let (status, body) = request(
        config,
        Method::POST,
        "/api/models/pull",
        Some(serde_json::json!({"ref": r#ref})),
    )
    .await?;
    if !status.is_success() {
        print_error_and_exit(status, &body);
    }
    let id = body["id"].as_str().unwrap_or(r#ref).to_string();
    println!("pulling {id}...");

    let path = format!("/api/models/{}/pull", encode_path_segment(&id));
    let mut last_phase = String::new();
    stream_sse(config, Method::GET, &path, None, |data| {
        let Ok(progress) = serde_json::from_str::<serde_json::Value>(data) else {
            return true;
        };
        let phase = progress["phase"].as_str().unwrap_or("").to_string();
        if phase != last_phase {
            println!("  [{phase}]");
            last_phase = phase.clone();
        }
        if phase == "downloading" {
            let downloaded = progress["bytes_downloaded"].as_u64().unwrap_or(0);
            let total = progress["total_bytes"].as_u64().unwrap_or(0);
            if total > 0 {
                print!("\r  {:.1}%", downloaded as f64 / total as f64 * 100.0);
                let _ = std::io::stdout().flush();
            }
        }
        if phase == "done" {
            println!("\ndone: {id}");
            return false;
        }
        if phase == "failed" {
            let msg = progress["message"].as_str().unwrap_or("load verification failed");
            println!("\n{id} downloaded but is UNSUPPORTED on this machine: {msg}");
            return false;
        }
        true
    })
    .await?;
    Ok(())
}

pub async fn list(config: &Config) -> anyhow::Result<()> {
    let (status, body) = request(config, Method::GET, "/api/status", None).await?;
    if !status.is_success() {
        print_error_and_exit(status, &body);
    }
    println!(
        "{:<40} {:<8} {:<20} {:<6} {:<8}",
        "MODEL", "SOURCE", "VERDICT", "HOT", "LOADED"
    );
    for m in body["models"].as_array().cloned().unwrap_or_default() {
        let is_cloud = m["source"].as_str() == Some("cloud");
        // Cloud rows have no RAM verdict and no local hot/cold state
        // (SPEC.md's Cloud integration §"GET /v1/models and nodelm
        // list") — print "-" rather than routing through a value that
        // was never computed for them.
        let verdict = if is_cloud { "-".to_string() } else { m["verdict"].as_str().unwrap_or("").to_string() };
        let hot = if is_cloud { "-".to_string() } else { m["hot"].as_bool().unwrap_or(false).to_string() };
        println!(
            "{:<40} {:<8} {:<20} {:<6} {:<8}",
            m["id"].as_str().unwrap_or(""),
            m["source"].as_str().unwrap_or("local"),
            verdict,
            hot,
            m["loaded"].as_bool().unwrap_or(false),
        );
    }
    Ok(())
}

pub async fn ps(config: &Config) -> anyhow::Result<()> {
    let (status, body) = request(config, Method::GET, "/api/status", None).await?;
    if !status.is_success() {
        print_error_and_exit(status, &body);
    }
    println!("{:<40} {:<12} {:<10}", "MODEL", "HOT", "RSS_BYTES");
    for m in body["models"].as_array().cloned().unwrap_or_default() {
        if m["loaded"].as_bool().unwrap_or(false) {
            let is_cloud = m["source"].as_str() == Some("cloud");
            let hot = if is_cloud { "-".to_string() } else { m["hot"].as_bool().unwrap_or(false).to_string() };
            let rss = if is_cloud {
                "-".to_string()
            } else {
                m["rss_bytes"]
                    .as_u64()
                    .map(|n| n.to_string())
                    .unwrap_or_else(|| "?".to_string())
            };
            println!("{:<40} {:<12} {:<10}", m["id"].as_str().unwrap_or(""), hot, rss);
        }
    }
    Ok(())
}

pub async fn show(config: &Config, name: &str) -> anyhow::Result<()> {
    let path = format!("/api/models/{}", encode_path_segment(name));
    let (status, body) = request(config, Method::GET, &path, None).await?;
    if !status.is_success() {
        print_error_and_exit(status, &body);
    }
    println!("{}", serde_json::to_string_pretty(&body)?);
    Ok(())
}

pub async fn run(config: &Config, name: &str, prompt: &str) -> anyhow::Result<()> {
    let body = serde_json::json!({
        "model": name,
        "messages": [{"role": "user", "content": prompt}],
        "stream": true,
    });
    let mut got_any = false;
    let result = stream_sse(config, Method::POST, "/v1/chat/completions", Some(body), |data| {
        if data == "[DONE]" {
            return false;
        }
        if let Ok(chunk) = serde_json::from_str::<serde_json::Value>(data) {
            if let Some(delta) = chunk["choices"][0]["delta"]["content"].as_str() {
                print!("{delta}");
                let _ = std::io::stdout().flush();
                got_any = true;
            }
        }
        true
    })
    .await;
    println!();
    result?;
    if !got_any {
        eprintln!("(no output)");
    }
    Ok(())
}

pub async fn hot(config: &Config, name: &str, on: bool) -> anyhow::Result<()> {
    let path = format!("/api/models/{}/hot", encode_path_segment(name));
    let (status, body) = request(config, Method::POST, &path, Some(serde_json::json!({"on": on}))).await?;
    if !status.is_success() {
        print_error_and_exit(status, &body);
    }
    println!("{name}: hot={on}");
    Ok(())
}

pub async fn tunnel_enable(config: &Config) -> anyhow::Result<()> {
    let (status, body) = request(config, Method::POST, "/api/tunnel/enable", None).await?;
    if !status.is_success() {
        print_error_and_exit(status, &body);
    }
    let hostname = body["hostname"].as_str().unwrap_or("");
    println!("Tunnel enabled: https://{hostname}");
    Ok(())
}

pub async fn tunnel_disable(config: &Config) -> anyhow::Result<()> {
    let (status, body) = request(config, Method::POST, "/api/tunnel/disable", None).await?;
    if !status.is_success() && status != StatusCode::NO_CONTENT {
        print_error_and_exit(status, &body);
    }
    println!("Tunnel disabled.");
    Ok(())
}

pub async fn tunnel_status(config: &Config) -> anyhow::Result<()> {
    let (status, body) = request(config, Method::GET, "/api/tunnel/status", None).await?;
    if !status.is_success() {
        print_error_and_exit(status, &body);
    }
    if !body["enabled"].as_bool().unwrap_or(false) {
        println!("Tunnel disabled.");
        return Ok(());
    }
    let hostname = body["hostname"].as_str().unwrap_or("");
    if body["running"].as_bool().unwrap_or(false) {
        println!("Tunnel enabled, connected: https://{hostname}");
    } else {
        println!(
            "Tunnel enabled but cloudflared is not running (crashed?) — \
             try 'nodelm tunnel disable' then 'enable'"
        );
    }
    Ok(())
}

pub async fn rm(config: &Config, name: &str) -> anyhow::Result<()> {
    let path = format!("/api/models/{}", encode_path_segment(name));
    let (status, body) = request(config, Method::DELETE, &path, None).await?;
    if !status.is_success() && status != StatusCode::NO_CONTENT {
        print_error_and_exit(status, &body);
    }
    println!("removed {name}");
    Ok(())
}
