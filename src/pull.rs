//! HF ref resolution, download+resume+sha256 verify, manifest write,
//! post-pull load-verification pass (§8).

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::io::SeekFrom;
use std::path::PathBuf;
use tokio::io::{AsyncReadExt, AsyncSeekExt, AsyncWriteExt};

use crate::config::Config;
use crate::engine;
use crate::gguf;
use crate::state::{ExpertCount, ModelManifest, State, VerdictCache};

#[derive(Debug, Clone, Serialize)]
pub struct PullProgress {
    pub canonical_id: String,
    /// "resolving" | "downloading" | "verifying_hash" | "verifying_load" | "done" | "failed"
    pub phase: String,
    pub bytes_downloaded: u64,
    pub total_bytes: u64,
    pub message: Option<String>,
}

/// Broadcast, not mpsc: `/api/models/:id/pull` SSE only has a listener
/// while a download row is expanded/active (§2.5) — sends with no
/// subscriber are fine to drop.
pub type ProgressTx = tokio::sync::broadcast::Sender<PullProgress>;

struct ParsedRef {
    org: String,
    repo: String,
    /// Set when the ref was `org/repo:quant`.
    quant: Option<String>,
    /// Set when the ref was a bare `org/repo/file.gguf` path.
    file: Option<String>,
}

/// `<ref>` = `org/repo:quant` or a bare `org/repo/file.gguf` path (§7/§8.1).
fn parse_ref(s: &str) -> anyhow::Result<ParsedRef> {
    if let Some((repo_part, quant)) = s.split_once(':') {
        let mut segs = repo_part.splitn(2, '/');
        let org = segs
            .next()
            .filter(|s| !s.is_empty())
            .ok_or_else(|| anyhow::anyhow!("ref '{s}' is missing an org"))?;
        let repo = segs
            .next()
            .filter(|s| !s.is_empty())
            .ok_or_else(|| anyhow::anyhow!("ref '{s}' is missing a repo (expected org/repo:quant)"))?;
        Ok(ParsedRef {
            org: org.to_string(),
            repo: repo.to_string(),
            quant: Some(quant.to_string()),
            file: None,
        })
    } else {
        let parts: Vec<&str> = s.splitn(3, '/').collect();
        if parts.len() == 3 {
            Ok(ParsedRef {
                org: parts[0].to_string(),
                repo: parts[1].to_string(),
                quant: None,
                file: Some(parts[2].to_string()),
            })
        } else {
            anyhow::bail!(
                "ref '{s}' doesn't parse as org/repo:quant or org/repo/file.gguf"
            )
        }
    }
}

#[derive(Debug, Deserialize, Clone)]
struct HfSibling {
    rfilename: String,
    #[allow(dead_code)]
    size: Option<u64>,
    lfs: Option<HfLfs>,
}

#[derive(Debug, Deserialize, Clone)]
struct HfLfs {
    sha256: Option<String>,
}

#[derive(Debug, Deserialize)]
struct HfModelInfo {
    siblings: Vec<HfSibling>,
}

struct ResolvedFile {
    filename: String,
    expected_sha256: Option<String>,
}

/// Result of the fast, synchronous resolve phase — enough to know the
/// canonical id before the slow download starts (needed because
/// `POST /api/models/pull` must return `{"id": "<canonical_id>"}`
/// immediately, §2.5).
pub struct Resolved {
    pub canonical_id: String,
    pub filename: String,
    org: String,
    repo: String,
    ref_str: String,
    expected_sha256: Option<String>,
}

/// Resolves `<ref>` against the HF repo's file listing (§8 step 1).
/// Fast (one HF API call) — safe to run synchronously in a request
/// handler before spawning the actual download.
pub async fn resolve(ref_str: &str) -> anyhow::Result<Resolved> {
    let parsed = parse_ref(ref_str)?;
    let client = reqwest::Client::builder().user_agent("nodelm/0.1").build()?;
    let resolved = resolve_file(&client, &parsed).await?;
    let quant = parsed
        .quant
        .clone()
        .unwrap_or_else(|| quant_from_filename(&resolved.filename));
    let canonical_id = format!("{}/{}:{}", parsed.org, parsed.repo, quant);
    Ok(Resolved {
        canonical_id,
        org: parsed.org,
        repo: parsed.repo,
        ref_str: ref_str.to_string(),
        filename: resolved.filename,
        expected_sha256: resolved.expected_sha256,
    })
}

async fn resolve_file(client: &reqwest::Client, r: &ParsedRef) -> anyhow::Result<ResolvedFile> {
    let url = format!(
        "https://huggingface.co/api/models/{}/{}?blobs=true",
        r.org, r.repo
    );
    let info: HfModelInfo = client
        .get(&url)
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;

    let candidate = if let Some(file) = &r.file {
        info.siblings.into_iter().find(|s| &s.rfilename == file)
    } else {
        let quant = r.quant.as_deref().unwrap_or_default();
        let ggufs: Vec<_> = info
            .siblings
            .into_iter()
            .filter(|s| s.rfilename.ends_with(".gguf"))
            .collect();
        let matches: Vec<_> = ggufs
            .iter()
            .filter(|s| s.rfilename.to_lowercase().contains(&quant.to_lowercase()))
            .cloned()
            .collect();
        match matches.len() {
            1 => matches.into_iter().next(),
            0 => anyhow::bail!(
                "no .gguf file matching quant '{quant}' in {}/{} (available: {})",
                r.org,
                r.repo,
                ggufs
                    .iter()
                    .map(|s| s.rfilename.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
            _ => anyhow::bail!(
                "quant '{quant}' is ambiguous in {}/{}: {}",
                r.org,
                r.repo,
                matches
                    .iter()
                    .map(|s| s.rfilename.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
        }
    };

    let sibling = candidate.ok_or_else(|| {
        anyhow::anyhow!("no matching .gguf file found in {}/{}", r.org, r.repo)
    })?;

    Ok(ResolvedFile {
        expected_sha256: sibling.lfs.and_then(|l| l.sha256),
        filename: sibling.rfilename,
    })
}

fn quant_from_filename(filename: &str) -> String {
    // Quant tags themselves contain '_' (Q4_K_M, IQ4_XS, ...) but are
    // '-'-delimited from the rest of the filename, so split on '-' only.
    let stem = filename.strip_suffix(".gguf").unwrap_or(filename);
    stem.rsplit('-').next().unwrap_or(stem).to_uppercase()
}

/// Stream-downloads to `<file>.gguf.partial` with resume support (§8
/// step 2), hashing sha256 as bytes arrive so no second full-file read is
/// needed for verification.
async fn download_with_resume(
    client: &reqwest::Client,
    url: &str,
    dest_partial: &PathBuf,
    canonical_id: &str,
    progress: Option<&ProgressTx>,
) -> anyhow::Result<String> {
    let existing_len = tokio::fs::metadata(dest_partial)
        .await
        .map(|m| m.len())
        .unwrap_or(0);

    let mut req = client.get(url);
    if existing_len > 0 {
        req = req.header(reqwest::header::RANGE, format!("bytes={existing_len}-"));
    }
    let resp = req.send().await?.error_for_status()?;
    let resumed = resp.status() == reqwest::StatusCode::PARTIAL_CONTENT;
    let remaining = resp.content_length().unwrap_or(0);
    let total_bytes = if resumed {
        existing_len + remaining
    } else {
        remaining
    };

    let mut file = tokio::fs::OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(false) // resuming appends; the non-resume branch below truncates explicitly
        .open(dest_partial)
        .await?;

    let mut hasher = Sha256::new();
    let mut downloaded = if resumed {
        // Catch the hasher up on bytes already on disk before appending.
        let mut buf = vec![0u8; 1 << 20];
        file.seek(SeekFrom::Start(0)).await?;
        let mut remaining_to_hash = existing_len;
        while remaining_to_hash > 0 {
            let n = file.read(&mut buf).await?;
            if n == 0 {
                break;
            }
            hasher.update(&buf[..n]);
            remaining_to_hash -= n as u64;
        }
        file.seek(SeekFrom::End(0)).await?;
        existing_len
    } else {
        file.set_len(0).await?;
        file.seek(SeekFrom::Start(0)).await?;
        0
    };

    let mut stream = resp.bytes_stream();
    use futures_util::StreamExt;
    // Throttle progress events to ~1MB steps — reqwest's stream chunks
    // are much smaller, and one broadcast send per chunk floods
    // subscribers for no visible UI benefit.
    let emit_every = 1 << 20;
    let mut last_emitted = downloaded;
    while let Some(chunk) = stream.next().await {
        let chunk = chunk?;
        hasher.update(&chunk);
        file.write_all(&chunk).await?;
        downloaded += chunk.len() as u64;
        if let Some(tx) = progress {
            if downloaded - last_emitted >= emit_every || downloaded == total_bytes {
                last_emitted = downloaded;
                let _ = tx.send(PullProgress {
                    canonical_id: canonical_id.to_string(),
                    phase: "downloading".to_string(),
                    bytes_downloaded: downloaded,
                    total_bytes,
                    message: None,
                });
            }
        }
    }
    file.flush().await?;

    Ok(hex::encode(hasher.finalize()))
}

/// Runs the slow half of the pipeline (§8 steps 2-4): download+verify,
/// write manifest, then a real load-verification pass. Split from
/// `resolve()` so a caller (the `/api/models/pull` handler) can return
/// the canonical id immediately and run this in a background task.
pub async fn download_and_verify(
    resolved: &Resolved,
    config: &Config,
    state: &parking_lot::RwLock<State>,
    progress: Option<ProgressTx>,
) -> anyhow::Result<()> {
    let canonical_id = &resolved.canonical_id;
    let client = reqwest::Client::builder().user_agent("nodelm/0.1").build()?;

    std::fs::create_dir_all(&config.model_dir)?;
    let final_path = config.model_dir.join(&resolved.filename);
    let partial_path = config.model_dir.join(format!("{}.partial", resolved.filename));

    let download_url = format!(
        "https://huggingface.co/{}/{}/resolve/main/{}",
        resolved.org, resolved.repo, resolved.filename
    );

    let actual_sha256 = download_with_resume(
        &client,
        &download_url,
        &partial_path,
        canonical_id,
        progress.as_ref(),
    )
    .await?;

    if let Some(tx) = &progress {
        let _ = tx.send(PullProgress {
            canonical_id: canonical_id.clone(),
            phase: "verifying_hash".to_string(),
            bytes_downloaded: 0,
            total_bytes: 0,
            message: None,
        });
    }
    if let Some(expected) = &resolved.expected_sha256 {
        if expected != &actual_sha256 {
            anyhow::bail!(
                "sha256 mismatch for {}: expected {expected}, got {actual_sha256}",
                resolved.filename
            );
        }
    }

    tokio::fs::rename(&partial_path, &final_path).await?;

    let size_bytes = tokio::fs::metadata(&final_path).await?.len();
    let gguf_meta = gguf::read_metadata(&final_path)?;

    let expert_count = match (gguf_meta.expert_count_total, gguf_meta.expert_count_active) {
        (Some(total), active) => Some(ExpertCount {
            total: total as u32,
            active: active.unwrap_or(0) as u32,
        }),
        _ => None,
    };
    let is_moe = expert_count.as_ref().map(|e| e.total > 0).unwrap_or(false);

    let manifest = ModelManifest {
        canonical_id: canonical_id.clone(),
        filename: resolved.filename.clone(),
        sha256: actual_sha256,
        size_bytes,
        architecture: gguf_meta.architecture,
        is_moe,
        expert_count,
        context_length_trained: gguf_meta.context_length.unwrap_or(4096),
        vision_capable: false, // ponytail: no vision detection yet, see gguf.rs note
        source_ref: resolved.ref_str.clone(),
        pulled_at: chrono::Utc::now(),
    };
    manifest.write(&final_path)?;

    if let Some(tx) = &progress {
        let _ = tx.send(PullProgress {
            canonical_id: canonical_id.clone(),
            phase: "verifying_load".to_string(),
            bytes_downloaded: 0,
            total_bytes: 0,
            message: None,
        });
    }

    // §8 step 4: run the same load path residency uses for a real request.
    let load_result = engine::verify_load(
        &config.llama_server_bin,
        &final_path,
        canonical_id,
        config.llama_server_port_range,
    )
    .await;

    // Mutate the live in-memory state (shared with the rest of the
    // daemon via AppState), not a disk-only round-trip — otherwise a
    // freshly-cached unsupported verdict wouldn't take effect until a
    // daemon restart, defeating the point of caching it (§5.2/§8).
    let engine_version = engine::detect_version(&config.llama_server_bin);
    {
        let mut guard = state.write();
        let entry = guard.models.entry(canonical_id.clone()).or_default();
        entry.verdict_cache = Some(match &load_result {
            Ok(()) => VerdictCache {
                engine_version: engine_version.clone(),
                status: "supported".to_string(),
                error: None,
            },
            Err(e) => VerdictCache {
                engine_version: engine_version.clone(),
                status: "unsupported".to_string(),
                error: Some(e.clone()),
            },
        });
        guard.save()?;
    }

    if let Some(tx) = &progress {
        let (phase, message) = match &load_result {
            Ok(()) => ("done".to_string(), None),
            Err(e) => ("failed".to_string(), Some(e.clone())),
        };
        let _ = tx.send(PullProgress {
            canonical_id: canonical_id.clone(),
            phase,
            bytes_downloaded: size_bytes,
            total_bytes: size_bytes,
            message,
        });
    }

    // File and manifest are kept regardless of load outcome (§8 closing
    // note) — an unsupported verdict means this engine build can't load
    // this architecture, not that the download is corrupt.
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_org_repo_quant() {
        let r = parse_ref("unsloth/Qwen3-30B-A3B:Q6_K").unwrap();
        assert_eq!(r.org, "unsloth");
        assert_eq!(r.repo, "Qwen3-30B-A3B");
        assert_eq!(r.quant.as_deref(), Some("Q6_K"));
        assert!(r.file.is_none());
    }

    #[test]
    fn parses_bare_file_path() {
        let r = parse_ref("org/repo/model-Q4_K_M.gguf").unwrap();
        assert_eq!(r.org, "org");
        assert_eq!(r.repo, "repo");
        assert_eq!(r.file.as_deref(), Some("model-Q4_K_M.gguf"));
        assert!(r.quant.is_none());
    }

    #[test]
    fn rejects_unparseable_ref() {
        assert!(parse_ref("just-a-name").is_err());
    }

    #[test]
    fn quant_from_filename_extracts_trailing_token() {
        assert_eq!(quant_from_filename("Qwen3-30B-A3B-Q6_K.gguf"), "Q6_K");
        assert_eq!(quant_from_filename("OLMoE-1B-7B-Instruct-Q4_K_M.gguf"), "Q4_K_M");
    }
}
