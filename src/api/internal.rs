//! `/api/*` routes — mirrors the CLI subcommand set plus what the UI
//! needs (§2.5).

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use futures_util::stream::Stream;
use serde::Deserialize;
use serde_json::json;
use std::convert::Infallible;

use super::{ensure_loaded, GatewayError, SharedState};
use crate::engine;
use crate::registry::{ModelState, ModelStatus};
use crate::residency;
use crate::state::ModelManifest;

pub fn routes() -> Router<SharedState> {
    Router::new()
        .route("/api/status", get(status))
        .route("/api/models/pull", post(start_pull))
        .route(
            "/api/models/:id",
            get(get_model).delete(delete_model),
        )
        .route("/api/models/:id/pull", get(pull_progress))
        .route("/api/models/:id/hot", post(set_hot))
}

fn err(status: StatusCode, message: impl Into<String>) -> Response {
    (status, Json(json!({"error": message.into()}))).into_response()
}

/// Resolves a name (alias or canonical) via the in-memory registry +
/// the alias map from state.json (§6).
fn resolve(app: &SharedState, name: &str) -> Option<String> {
    let aliases = app.state.read().aliases.clone();
    app.registry.resolve(name, &aliases)
}

async fn status(State(app): State<SharedState>) -> Json<serde_json::Value> {
    let ram = residency::read_ram_info();
    let headroom = residency::headroom_bytes(ram.total_bytes, app.config.ram_headroom_gb);

    let models: Vec<_> = app
        .registry
        .list()
        .into_iter()
        .map(|m| model_json(&app, &m))
        .collect();

    Json(json!({
        "models": models,
        "ram": {
            "total_bytes": ram.total_bytes,
            "available_bytes": ram.available_bytes,
            "headroom_bytes": headroom,
        },
        "config": {
            "ram_headroom_gb": app.config.ram_headroom_gb,
            "model_dir": app.config.model_dir,
        },
    }))
}

fn model_json(app: &SharedState, m: &ModelState) -> serde_json::Value {
    let aliases: Vec<String> = app
        .state
        .read()
        .aliases
        .iter()
        .filter(|(_, v)| *v == &m.canonical_id)
        .map(|(k, _)| k.clone())
        .collect();
    json!({
        "id": m.canonical_id,
        "aliases": aliases,
        "size_bytes": m.size_bytes,
        "quant": m.canonical_id.rsplit(':').next().unwrap_or(""),
        "hot": m.hot,
        "loaded": matches!(m.status, ModelStatus::Loaded),
        "rss_bytes": m.pid.and_then(residency::read_rss_bytes),
        "verdict": m.verdict.tier.as_str(),
        "verdict_reason": m.verdict.reason,
        "verdict_tier": m.verdict.tier.as_str(),
    })
}

async fn get_model(State(app): State<SharedState>, Path(id): Path<String>) -> Response {
    let Some(canonical) = resolve(&app, &id) else {
        return err(StatusCode::NOT_FOUND, format!("model '{id}' not found"));
    };
    let Some(ms) = app.registry.get_clone(&canonical) else {
        return err(StatusCode::NOT_FOUND, format!("model '{id}' not found"));
    };
    let manifest = ModelManifest::load(&ms.gguf_path).ok();
    Json(json!({
        "state": model_json(&app, &ms),
        "manifest": manifest,
    }))
    .into_response()
}

#[derive(Deserialize)]
struct PullRequest {
    #[serde(rename = "ref")]
    r#ref: String,
}

async fn start_pull(State(app): State<SharedState>, Json(body): Json<PullRequest>) -> Response {
    let resolved = match crate::pull::resolve(&body.r#ref).await {
        Ok(r) => r,
        Err(e) => return err(StatusCode::BAD_REQUEST, e.to_string()),
    };
    let canonical_id = resolved.canonical_id.clone();
    let tx = app.pull_channel(&canonical_id);
    let config = app.config.clone();
    let app_for_task = app.clone();

    tokio::spawn(async move {
        match crate::pull::download_and_verify(&resolved, &config, &app_for_task.state, Some(tx.clone())).await {
            Ok(()) => {
                // Load-verification may still have found the model
                // unsupported — that's still success from the pipeline's
                // point of view (§8 closing note), so register it either
                // way: a fresh manifest now exists on disk for it.
                if let Ok(manifest) =
                    ModelManifest::load(&app_for_task.config.model_dir.join(&resolved.filename))
                {
                    super::register_model(&app_for_task, &manifest);
                }
            }
            Err(e) => {
                let _ = tx.send(crate::pull::PullProgress {
                    canonical_id: resolved.canonical_id.clone(),
                    phase: "failed".to_string(),
                    bytes_downloaded: 0,
                    total_bytes: 0,
                    message: Some(e.to_string()),
                });
            }
        }
    });

    Json(json!({"id": canonical_id})).into_response()
}

async fn pull_progress(
    State(app): State<SharedState>,
    Path(id): Path<String>,
) -> Sse<impl Stream<Item = Result<Event, Infallible>>> {
    let tx = app.pull_channel(&id);
    let rx = tx.subscribe();
    let stream = tokio_stream::wrappers::BroadcastStream::new(rx).filter_map(|msg| async move {
        match msg {
            Ok(progress) => serde_json::to_string(&progress)
                .ok()
                .map(|json| Ok(Event::default().data(json))),
            Err(_) => None, // lagged: drop, next event still arrives
        }
    });
    use futures_util::StreamExt;
    Sse::new(stream).keep_alive(KeepAlive::default())
}

#[derive(Deserialize)]
struct HotRequest {
    on: bool,
}

async fn set_hot(
    State(app): State<SharedState>,
    Path(id): Path<String>,
    Json(body): Json<HotRequest>,
) -> Response {
    let Some(canonical) = resolve(&app, &id) else {
        return err(StatusCode::NOT_FOUND, format!("model '{id}' not found"));
    };
    let Some(mut ms) = app.registry.get_clone(&canonical) else {
        return err(StatusCode::NOT_FOUND, format!("model '{id}' not found"));
    };

    if body.on {
        // Admission check against the sum of all *hot* models' sizes
        // (§5.4) — this is what makes "hot models are never evicted"
        // safe without discovering the overrun later at eviction time.
        let ram = residency::read_ram_info();
        let headroom = residency::headroom_bytes(ram.total_bytes, app.config.ram_headroom_gb);
        let other_hot: u64 = app
            .registry
            .list()
            .into_iter()
            .filter(|m| m.hot && m.canonical_id != canonical)
            .map(|m| m.size_bytes)
            .sum();
        if let Err(shortfall) =
            residency::check_admission(other_hot, ms.size_bytes, ram.total_bytes, headroom)
        {
            return err(
                StatusCode::BAD_REQUEST,
                format!(
                    "won't fit alongside other hot models: need {shortfall} more bytes; \
                     evict a hot model manually or lower headroom"
                ),
            );
        }
    }

    ms.hot = body.on;
    app.registry.insert(ms);

    {
        let mut state = app.state.write();
        state.models.entry(canonical.clone()).or_default().hot = body.on;
        if let Err(e) = state.save() {
            return err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string());
        }
    }

    // hot means resident now, not "resident on next request" -- load
    // eagerly so pinning a model actually delivers the no-cold-start
    // promise instead of just exempting it from eviction later.
    if body.on {
        if let Err(e) = ensure_loaded(&app, &canonical).await {
            let msg = match e {
                GatewayError::CachedUnsupported(m)
                | GatewayError::InsufficientRam(m)
                | GatewayError::LoadFailed(m) => m,
            };
            return err(
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("marked hot but failed to load: {msg}"),
            );
        }
    }

    Json(json!({"id": canonical, "hot": body.on})).into_response()
}

async fn delete_model(State(app): State<SharedState>, Path(id): Path<String>) -> Response {
    let Some(canonical) = resolve(&app, &id) else {
        return err(StatusCode::NOT_FOUND, format!("model '{id}' not found"));
    };
    let Some(ms) = app.registry.get_clone(&canonical) else {
        return err(StatusCode::NOT_FOUND, format!("model '{id}' not found"));
    };

    // Unload if running (SIGTERM/SIGKILL per §4.4), by pid — this stub
    // registry doesn't hold the tokio::process::Child handle, only the
    // pid, which is enough to signal it directly.
    if let (ModelStatus::Loaded, Some(pid)) = (&ms.status, ms.pid) {
        engine::kill_pid_graceful(pid).await;
    }

    if let Err(e) = std::fs::remove_file(&ms.gguf_path) {
        if e.kind() != std::io::ErrorKind::NotFound {
            return err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string());
        }
    }
    let _ = std::fs::remove_file(ModelManifest::sidecar_path_for(&ms.gguf_path));

    app.registry.remove(&canonical);
    {
        let mut state = app.state.write();
        state.models.remove(&canonical);
        if let Err(e) = state.save() {
            return err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string());
        }
    }

    StatusCode::NO_CONTENT.into_response()
}
