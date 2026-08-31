//! `/api/*` routes. Mirrors the CLI subcommand set plus what the UI
//! needs.

use axum::extract::{Path, Query, State};
use axum::http::{header, StatusCode, Uri};
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use futures_util::stream::Stream;
use rust_embed::RustEmbed;
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::convert::Infallible;

/// The built UI (`ui/dist/`), embedded at compile time. Run
/// `npm run build` in `ui/` before building the daemon.
// Cargo won't notice ui/dist/ changes on its own (no
// rerun-if-changed) -- touch this file or `cargo clean -p kiralm`
// after rebuilding the UI. Add a build.rs if this bites often.
#[derive(RustEmbed)]
#[folder = "ui/dist/"]
struct Assets;

/// Static-file fallback: serves the embedded SPA for any path that isn't
/// `/v1/*` or `/api/*` (those already have their own routes and 404 on a
/// genuine miss rather than falling through to index.html here).
pub async fn static_handler(uri: Uri) -> Response {
    let path = uri.path().trim_start_matches('/');
    if path.starts_with("v1") || path.starts_with("api") {
        return err(StatusCode::NOT_FOUND, "not found");
    }
    serve_asset(path).unwrap_or_else(|| {
        serve_asset("index.html").unwrap_or_else(|| err(StatusCode::NOT_FOUND, "not found"))
    })
}

fn serve_asset(path: &str) -> Option<Response> {
    let asset = Assets::get(path)?;
    let mime = asset.metadata.mimetype();
    Some(([(header::CONTENT_TYPE, mime)], asset.data).into_response())
}

use super::{ensure_loaded, GatewayError, SharedState};
use crate::cloud;
use crate::registry::{ModelSource, ModelState, ModelStatus};
use crate::residency;
use crate::state::ModelManifest;

pub fn routes() -> Router<SharedState> {
    Router::new()
        .route("/api/status", get(status))
        .route("/api/config", post(update_config))
        .route("/api/settings", get(get_settings).put(put_settings))
        .route("/api/models/pull", post(start_pull))
        .route("/api/models/catalog", get(models_catalog))
        .route(
            "/api/models/:id",
            get(get_model).delete(delete_model),
        )
        .route("/api/models/:id/pull", get(pull_progress))
        .route("/api/models/:id/hot", post(set_hot))
        .route("/api/cloud/status", get(cloud_status))
        .route("/api/cloud/login", post(cloud_login))
        .route("/api/cloud/login/progress", get(cloud_login_progress))
        .route("/api/cloud/logout", post(cloud_logout))
        .route("/api/projects", get(list_projects).post(create_project))
        .route("/api/projects/clone", post(clone_project))
        .route("/api/fs/list", get(list_dir))
        .route(
            "/api/projects/:id",
            axum::routing::delete(delete_project).patch(touch_project),
        )
        .route("/api/projects/:id/git/status", get(git_status))
        .route("/api/projects/:id/git/log", get(git_log))
        .route("/api/projects/:id/git/commit", post(git_commit))
        .route(
            "/api/projects/:id/git/workflow/state",
            get(git_workflow_state),
        )
        .route(
            "/api/projects/:id/git/workflow/diff",
            post(git_workflow_diff),
        )
        .route(
            "/api/projects/:id/git/workflow/branches",
            get(git_workflow_branches).post(git_workflow_create_branch),
        )
        .route(
            "/api/projects/:id/git/workflow/branches/switch",
            post(git_workflow_switch_branch),
        )
        .route(
            "/api/projects/:id/git/workflow/stage",
            post(git_workflow_stage),
        )
        .route(
            "/api/projects/:id/git/workflow/unstage",
            post(git_workflow_unstage),
        )
        .route(
            "/api/projects/:id/git/workflow/commit",
            post(git_workflow_commit),
        )
        .route(
            "/api/projects/:id/git/workflow/audit",
            get(git_workflow_audit),
        )
        .route(
            "/api/sessions/:id/generation",
            get(get_generation).delete(stop_generation),
        )
        .route("/api/generations", get(list_generations))
        .route("/api/approvals", get(list_approvals))
        .route("/api/approvals/:id/resolve", post(resolve_approval))
        .route("/api/chats", get(list_chats).post(create_chat))
        .route(
            "/api/chats/:id",
            axum::routing::patch(update_chat).delete(delete_chat),
        )
        .route("/api/skills", get(list_skills).post(create_skill))
        .route("/api/skills/:name", axum::routing::delete(delete_skill))
        .route("/api/skills/:name/enabled", post(set_skill_enabled))
}

fn err(status: StatusCode, message: impl Into<String>) -> Response {
    (status, Json(json!({"error": message.into()}))).into_response()
}

/// Resolves a name (alias or canonical) via the in-memory registry +
/// the alias map from state.json.
fn resolve(app: &SharedState, name: &str) -> Option<String> {
    let aliases = app.state.read().aliases.clone();
    app.registry.resolve(name, &aliases)
}

async fn status(State(app): State<SharedState>) -> Json<serde_json::Value> {
    maybe_refresh_cloud_models(&app);

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
            "cache_quota_gb": app.config.cache_quota_gb,
            "optimization_profile": app.config.optimization_profile,
        },
    }))
}

#[derive(Deserialize)]
struct UpdateConfigRequest {
    ram_headroom_gb: Option<f64>,
    model_dir: String,
    cache_quota_gb: Option<f64>,
    optimization_profile: Option<String>,
}

/// Writes managed runtime keys into `~/.kiralm/config.toml`,
/// editing just those keys of whatever's already on disk (a
/// generic TOML table, not `Config`'s resolved-with-defaults shape, so
/// unrelated hand-edited fields -- cloud/tunnel included -- survive
/// untouched). `Config` itself is loaded once at startup and treated
/// as static (see config.rs's own doc comment), so this never takes
/// effect until the daemon restarts -- the response says so plainly
/// rather than pretending it applied live.
async fn update_config(State(_app): State<SharedState>, Json(body): Json<UpdateConfigRequest>) -> Response {
    let path = crate::config::default_config_path();
    let mut doc: toml::Value = match std::fs::read_to_string(&path) {
        Ok(s) => match toml::from_str(&s) {
            Ok(v) => v,
            Err(e) => return err(StatusCode::INTERNAL_SERVER_ERROR, format!("existing config.toml is invalid: {e}")),
        },
        Err(_) => toml::Value::Table(Default::default()),
    };
    let Some(table) = doc.as_table_mut() else {
        return err(StatusCode::INTERNAL_SERVER_ERROR, "config.toml's top level isn't a table");
    };

    match body.ram_headroom_gb {
        Some(v) => {
            table.insert("ram_headroom_gb".to_string(), toml::Value::Float(v));
        }
        None => {
            table.remove("ram_headroom_gb");
        }
    }
    let model_dir = body.model_dir.trim();
    if model_dir.is_empty() {
        table.remove("model_dir");
    } else {
        table.insert("model_dir".to_string(), toml::Value::String(model_dir.to_string()));
    }
    if let Some(quota) = body.cache_quota_gb {
        if quota < 1.0 {
            return err(StatusCode::BAD_REQUEST, "cache quota must be at least 1GB");
        }
        table.insert("cache_quota_gb".to_string(), toml::Value::Float(quota));
    }
    if let Some(profile) = body.optimization_profile {
        if crate::engine::optimization::OptimizationProfile::parse(&profile)
            .is_none()
        {
            return err(StatusCode::BAD_REQUEST, "unknown optimization profile");
        }
        table.insert(
            "optimization_profile".to_string(),
            toml::Value::String(profile.to_ascii_lowercase()),
        );
    }

    let out = match toml::to_string_pretty(&doc) {
        Ok(s) => s,
        Err(e) => return err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()),
    };
    if let Some(parent) = path.parent() {
        if let Err(e) = std::fs::create_dir_all(parent) {
            return err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string());
        }
    }
    if let Err(e) = std::fs::write(&path, out) {
        return err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string());
    }
    Json(json!({"restart_required": true})).into_response()
}

async fn get_settings(State(app): State<SharedState>) -> Response {
    Json(app.settings.read().clone()).into_response()
}

/// Replaces the whole settings.json in one write -- the UI edits a
/// local draft and sends the full object on Save, so there's no
/// partial-patch merging to get wrong here.
async fn put_settings(
    State(app): State<SharedState>,
    Json(mut body): Json<crate::settings::Settings>,
) -> Response {
    body.custom_instructions = body.custom_instructions.replace("\r\n", "\n");
    if body.custom_instructions.len() > crate::prompts::MAX_USER_INSTRUCTION_BYTES {
        return err(
            StatusCode::BAD_REQUEST,
            format!(
                "custom instructions exceed {} bytes",
                crate::prompts::MAX_USER_INSTRUCTION_BYTES
            ),
        );
    }
    if let Err(e) = body.save() {
        return err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string());
    }
    *app.settings.write() = body.clone();
    Json(body).into_response()
}

/// The "merged-catalog read path": always serves the current registry
/// immediately, but kicks off a
/// background re-fetch if the cache is older than
/// `catalog_sync_interval_secs`, the on-demand half of the sync
/// trigger, sibling to the one-shot startup fetch in `main.rs`.
fn maybe_refresh_cloud_models(app: &SharedState) {
    let stale = match cloud::cache_age_secs() {
        Some(age) => age >= app.config.cloud.catalog_sync_interval_secs as i64,
        None => true, // never fetched
    };
    if !stale {
        return;
    }
    let app = app.clone();
    tokio::spawn(async move {
        cloud::sync_cloud_models(&app.config.cloud.api_base_url, &app.registry).await;
    });
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
        "source": match m.source { ModelSource::Local => "local", ModelSource::Cloud => "cloud" },
        "size_bytes": m.size_bytes,
        "quant": m.canonical_id.rsplit(':').next().unwrap_or(""),
        "hot": m.hot,
        "context_length": m.context_length,
        "vision_capable": m.vision_capable,
        "loaded": matches!(m.status, ModelStatus::Loaded),
        "rss_bytes": m.pid.and_then(residency::read_rss_bytes),
        "verdict": m.verdict.as_ref().map(|v| v.tier.as_str()),
        "verdict_reason": m.verdict.as_ref().map(|v| v.reason.clone()),
        "verdict_tier": m.verdict.as_ref().map(|v| v.tier.as_str()),
    })
}

/// Real `kiralm pull` suggestions (kiralm-web's `local_pull` catalog
/// rows, already synced to disk) for the Models settings view's "pick
/// one to pull" list -- replaces a hardcoded array that used to live
/// in the frontend.
async fn models_catalog(State(_app): State<SharedState>) -> Response {
    Json(json!({"data": crate::cloud::cached_pull_suggestions()})).into_response()
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
                // unsupported. That's still success from the pipeline's
                // point of view, so register it either way: a fresh
                // manifest now exists on disk for it.
                if let Ok(manifest) =
                    ModelManifest::load(&app_for_task.config.model_dir.join(&resolved.filename))
                {
                    super::register_model(&app_for_task, &manifest);
                }
                if let Err(error) = super::prune_artifact_cache(&app_for_task) {
                    tracing::warn!("failed to enforce artifact cache quota: {error}");
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
        // Admission check against the sum of all *hot* models' sizes.
        // This is what makes "hot models are never evicted" safe
        // without discovering the overrun later at eviction time.
        let ram = residency::read_ram_info();
        let headroom = residency::headroom_bytes(ram.total_bytes, app.config.ram_headroom_gb);
        let other_hot: u64 = app
            .registry
            .list()
            .into_iter()
            .filter(|m| m.source == crate::registry::ModelSource::Local && m.hot && m.canonical_id != canonical)
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
    if let Err(error) = app.supervisor.set_hot(&canonical, body.on).await {
        return err(StatusCode::INTERNAL_SERVER_ERROR, error.to_string());
    }

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
                | GatewayError::LoadFailed(m)
                | GatewayError::AuthRequired(m) => m,
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

    if let Err(error) = app.supervisor.unload(&canonical).await {
        return err(StatusCode::INTERNAL_SERVER_ERROR, error.to_string());
    }

    let manifest = ModelManifest::load(&ms.gguf_path).ok();
    if let Err(e) = std::fs::remove_file(&ms.gguf_path) {
        if e.kind() != std::io::ErrorKind::NotFound {
            return err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string());
        }
    }
    let _ = std::fs::remove_file(ModelManifest::sidecar_path_for(&ms.gguf_path));
    if let Some(manifest) = manifest {
        if let Ok(cache) = crate::pull::artifact_cache() {
            if let Err(error) =
                cache.remove_reference(&manifest.sha256, &ms.gguf_path)
            {
                tracing::warn!("failed to update artifact cache: {error}");
            }
        }
    }

    app.registry.remove(&canonical);
    {
        let mut state = app.state.write();
        state.models.remove(&canonical);
        if let Err(e) = state.save() {
            return err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string());
        }
    }
    if let Err(error) = super::prune_artifact_cache(&app) {
        tracing::warn!("failed to enforce artifact cache quota: {error}");
    }

    StatusCode::NO_CONTENT.into_response()
}

async fn cloud_status(State(_app): State<SharedState>) -> Json<serde_json::Value> {
    let (logged_in, device) = cloud::cloud_status();
    Json(json!({"logged_in": logged_in, "device": device}))
}

/// Starts the device flow: the code/URL request is fast and synchronous
/// so the response can carry it immediately, then the slow
/// approve-and-poll half runs in a background task, the same
/// resolve()-now/download_and_verify()-in-background split `start_pull`
/// uses.
async fn cloud_login(State(app): State<SharedState>) -> Response {
    let device = match cloud::request_device_code(&app.config).await {
        Ok(d) => d,
        Err(e) => return err(StatusCode::BAD_GATEWAY, e.to_string()),
    };

    let tx = app.login_channel(&device.device_code);
    let config = app.config.clone();
    let device_for_task = device.clone();
    tokio::spawn(async move {
        cloud::poll_device_token(&config, &device_for_task, Some(&tx)).await;
    });

    Json(json!({
        "device_code": device.device_code,
        "user_code": device.user_code,
        "verification_uri": device.verification_uri,
        "verification_uri_complete": device.verification_uri_complete,
        "expires_in": device.expires_in,
    }))
    .into_response()
}

#[derive(Deserialize)]
struct LoginProgressQuery {
    device_code: String,
}

async fn cloud_login_progress(
    State(app): State<SharedState>,
    Query(q): Query<LoginProgressQuery>,
) -> Sse<impl Stream<Item = Result<Event, Infallible>>> {
    let tx = app.login_channel(&q.device_code);
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

async fn cloud_logout(State(_app): State<SharedState>) -> Response {
    match cloud::logout() {
        Ok(()) => StatusCode::NO_CONTENT.into_response(),
        Err(e) => err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()),
    }
}

async fn list_projects(State(app): State<SharedState>) -> Json<serde_json::Value> {
    let state = app.state.read();
    let mut projects: Vec<_> = state.projects.values().cloned().collect();
    projects.sort_by(|a, b| b.last_active_at.cmp(&a.last_active_at));
    Json(json!({"projects": projects}))
}

#[derive(Deserialize)]
struct ListDirQuery {
    path: Option<String>,
}

#[derive(Serialize)]
struct DirEntry {
    name: String,
    path: String,
}

// Real in-app folder browser for "Add workspace", not a native OS
// dialog -- lists subdirectories of `path` (home dir if omitted) so
// the UI can render its own explorer instead of shelling out to a
// system picker. Directories only, dotfiles hidden, same as most
// "choose a folder" UIs default to.
async fn list_dir(Query(q): Query<ListDirQuery>) -> Response {
    let requested = q.path.filter(|p| !p.trim().is_empty());
    let base = match requested {
        Some(p) => std::path::PathBuf::from(p),
        None => dirs::home_dir().unwrap_or_else(|| std::path::PathBuf::from("/")),
    };
    let base = base.canonicalize().unwrap_or(base);
    if !base.is_dir() {
        return err(StatusCode::BAD_REQUEST, "not a directory");
    }

    let mut entries = Vec::new();
    if let Ok(read) = std::fs::read_dir(&base) {
        for e in read.flatten() {
            let path = e.path();
            if !path.is_dir() {
                continue;
            }
            let name = e.file_name().to_string_lossy().to_string();
            if name.starts_with('.') {
                continue;
            }
            entries.push(DirEntry { name, path: path.display().to_string() });
        }
    }
    entries.sort_by(|a, b| a.name.to_lowercase().cmp(&b.name.to_lowercase()));

    let parent = base.parent().map(|p| p.display().to_string());
    Json(json!({
        "path": base.display().to_string(),
        "parent": parent,
        "entries": entries,
    }))
    .into_response()
}

#[derive(Deserialize)]
struct CreateProjectRequest {
    path: String,
    name: Option<String>,
}

/// Shared tail of create/clone: validate the directory is real, name
/// it (explicit name, else the path's basename), persist, return it.
fn register_project(app: &SharedState, path: &str, name: Option<String>) -> Response {
    if !std::path::Path::new(path).is_dir() {
        return err(StatusCode::BAD_REQUEST, format!("'{path}' is not an existing directory"));
    }
    let name = name
        .map(|n| n.trim().to_string())
        .filter(|n| !n.is_empty())
        .unwrap_or_else(|| {
            std::path::Path::new(path)
                .file_name()
                .and_then(|s| s.to_str())
                .unwrap_or(path)
                .to_string()
        });

    let now = chrono::Utc::now();
    let project = crate::state::Project {
        id: uuid::Uuid::new_v4().to_string(),
        name,
        path: path.to_string(),
        created_at: now,
        last_active_at: now,
    };

    let mut state = app.state.write();
    state.projects.insert(project.id.clone(), project.clone());
    if let Err(e) = state.save() {
        return err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string());
    }
    Json(project).into_response()
}

async fn create_project(State(app): State<SharedState>, Json(body): Json<CreateProjectRequest>) -> Response {
    let path = body.path.trim();
    if path.is_empty() {
        return err(StatusCode::BAD_REQUEST, "path is required");
    }
    register_project(&app, path, body.name)
}

#[derive(Deserialize)]
struct CloneProjectRequest {
    url: String,
    path: String,
    name: Option<String>,
}

/// Real `git clone <url> <path>`, then registers the clone as a project
/// -- what "Pull from git" on the project-home screen drives.
async fn clone_project(State(app): State<SharedState>, Json(body): Json<CloneProjectRequest>) -> Response {
    let url = body.url.trim();
    let path = body.path.trim();
    if url.is_empty() || path.is_empty() {
        return err(StatusCode::BAD_REQUEST, "url and path are required");
    }
    if let Err(e) = crate::git::clone(url, std::path::Path::new(path)).await {
        return err(StatusCode::BAD_REQUEST, e);
    }
    register_project(&app, path, body.name)
}

async fn delete_project(State(app): State<SharedState>, Path(id): Path<String>) -> Response {
    let mut state = app.state.write();
    if state.projects.remove(&id).is_none() {
        return err(StatusCode::NOT_FOUND, format!("project '{id}' not found"));
    }
    if let Err(e) = state.save() {
        return err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string());
    }
    drop(state);

    // Its chats go with it -- an orphaned chat pointing at a deleted
    // project would just 404 on every subsequent touch/git-status call.
    let mut chats = app.chats.write();
    let removed_chat_ids: Vec<_> = chats
        .chats
        .iter()
        .filter(|chat| chat.project_id == id)
        .map(|chat| chat.id.clone())
        .collect();
    chats.chats.retain(|c| c.project_id != id);
    let _ = chats.save();
    drop(chats);
    for chat_id in removed_chat_ids {
        let _ = app
            .attachments
            .release_reference(&format!("chat:{chat_id}"));
    }
    app.git_workflows.write().remove(&id);

    StatusCode::NO_CONTENT.into_response()
}

async fn list_chats(State(app): State<SharedState>) -> Json<serde_json::Value> {
    Json(json!({"chats": app.chats.read().chats}))
}

#[derive(Deserialize)]
struct CreateChatRequest {
    project_id: String,
    permission: String,
}

async fn create_chat(State(app): State<SharedState>, Json(body): Json<CreateChatRequest>) -> Response {
    // Millis + `Z`, byte-identical to JS's toISOString(): the UI sorts
    // these as strings, so one format everywhere or ordering breaks.
    let now = chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true);
    let chat = crate::chats::Chat {
        id: uuid::Uuid::new_v4().to_string(),
        title: "New chat".to_string(),
        created_at: now.clone(),
        updated_at: now,
        project_id: body.project_id,
        permission: crate::tools::Permission::parse(&body.permission)
            .as_str()
            .to_string(),
        system_prompt: String::new(),
        messages: json!([]),
    };
    let mut chats = app.chats.write();
    chats.chats.insert(0, chat.clone());
    if let Err(e) = chats.save() {
        return err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string());
    }
    Json(chat).into_response()
}

#[derive(Deserialize)]
struct UpdateChatRequest {
    title: Option<String>,
    permission: Option<String>,
    system_prompt: Option<String>,
    messages: Option<serde_json::Value>,
}

async fn update_chat(
    State(app): State<SharedState>,
    Path(id): Path<String>,
    Json(mut body): Json<UpdateChatRequest>,
) -> Response {
    if let Some(prompt) = &mut body.system_prompt {
        *prompt = prompt.replace("\r\n", "\n");
        if prompt.len() > crate::prompts::MAX_USER_INSTRUCTION_BYTES {
            return err(
                StatusCode::BAD_REQUEST,
                format!(
                    "chat instructions exceed {} bytes",
                    crate::prompts::MAX_USER_INSTRUCTION_BYTES
                ),
            );
        }
    }
    let mut chats = app.chats.write();
    let Some(chat) = chats.chats.iter_mut().find(|c| c.id == id) else {
        return err(StatusCode::NOT_FOUND, format!("chat '{id}' not found"));
    };
    if let Some(title) = body.title {
        chat.title = title;
    }
    if let Some(permission) = body.permission {
        chat.permission = crate::tools::Permission::parse(&permission)
            .as_str()
            .to_string();
    }
    if let Some(system_prompt) = body.system_prompt {
        chat.system_prompt = system_prompt;
    }
    if let Some(messages) = body.messages {
        chat.messages = messages;
    }
    chat.updated_at = chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true);
    let updated = chat.clone();
    if let Err(e) = chats.save() {
        return err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string());
    }
    Json(updated).into_response()
}

async fn delete_chat(State(app): State<SharedState>, Path(id): Path<String>) -> Response {
    let mut chats = app.chats.write();
    let before = chats.chats.len();
    chats.chats.retain(|c| c.id != id);
    if chats.chats.len() == before {
        return err(StatusCode::NOT_FOUND, format!("chat '{id}' not found"));
    }
    if let Err(e) = chats.save() {
        return err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string());
    }
    app.approvals.forget_chat(&id);
    let _ = app
        .attachments
        .release_reference(&format!("chat:{id}"));
    StatusCode::NO_CONTENT.into_response()
}

/// Buffered-so-far text + running flag for one chat session's server-
/// side generation (see openai.rs's `start_tracked_stream`), so a client
/// that reconnects or polls instead of watching the live stream can
/// catch up. 200s with an empty/not-running body for an unknown id
/// rather than 404, so the UI never needs to special-case "never
/// generated anything" vs "finished".
async fn get_generation(State(app): State<SharedState>, Path(id): Path<String>) -> Response {
    Json(app.generation_snapshot(&id)).into_response()
}

/// Stops a session's in-flight generation (the "Stop" button): actually
/// aborts the background task/upstream connection, not just detaching
/// whichever client happened to be watching.
async fn stop_generation(State(app): State<SharedState>, Path(id): Path<String>) -> Response {
    if app.stop_generation(&id) {
        StatusCode::NO_CONTENT.into_response()
    } else {
        err(StatusCode::NOT_FOUND, format!("no generation in flight for session '{id}'"))
    }
}

/// Session ids with generation currently in flight, for the sidebar to
/// poll so a background session's status dot reflects real state
/// regardless of which session is focused.
async fn list_generations(State(app): State<SharedState>) -> Json<serde_json::Value> {
    Json(json!({"running": app.running_generation_ids()}))
}

#[derive(Deserialize)]
struct ApprovalQuery {
    chat_id: Option<String>,
}

async fn list_approvals(
    State(app): State<SharedState>,
    Query(query): Query<ApprovalQuery>,
) -> Json<serde_json::Value> {
    Json(json!({
        "approvals": app.approvals.list(query.chat_id.as_deref()),
    }))
}

#[derive(Deserialize)]
struct ResolveApprovalRequest {
    decision: crate::approvals::ApprovalDecision,
}

async fn resolve_approval(
    State(app): State<SharedState>,
    Path(id): Path<String>,
    Json(body): Json<ResolveApprovalRequest>,
) -> Response {
    match app.approvals.resolve(&id, body.decision) {
        Ok(()) => StatusCode::NO_CONTENT.into_response(),
        Err(message) => err(StatusCode::NOT_FOUND, message),
    }
}

/// Looks up a registered project's real directory by id, 404ing if it
/// isn't registered rather than falling back to any default path.
#[allow(clippy::result_large_err)]
fn project_path(app: &SharedState, id: &str) -> Result<std::path::PathBuf, Response> {
    let state = app.state.read();
    match state.projects.get(id) {
        Some(p) => Ok(std::path::PathBuf::from(&p.path)),
        None => Err(err(StatusCode::NOT_FOUND, format!("project '{id}' not found"))),
    }
}

/// Real git status for a project's directory: branch +
/// working-tree changes, shelled out to the system `git` binary. Always
/// 200 -- a non-repo project just comes back with `is_repo: false`
/// rather than an error, so the footer can show that plainly instead of
/// guessing from a thrown error.
async fn git_status(State(app): State<SharedState>, Path(id): Path<String>) -> Response {
    let root = match project_path(&app, &id) {
        Ok(p) => p,
        Err(e) => return e,
    };
    Json(crate::git::status(&root).await).into_response()
}

/// Most recent 50 commits for a project's directory. Always 200 -- a
/// non-repo project just comes back with an empty list.
async fn git_log(State(app): State<SharedState>, Path(id): Path<String>) -> Response {
    let root = match project_path(&app, &id) {
        Ok(p) => p,
        Err(e) => return e,
    };
    Json(json!({"commits": crate::git::log(&root, 50).await})).into_response()
}

#[derive(Deserialize)]
struct GitCommitRequest {
    message: String,
}

/// Real `git add -A` + `git commit -m` for a project's directory. 400 on
/// a blank message or a clean working tree, not a raw git error.
async fn git_commit(
    State(app): State<SharedState>,
    Path(id): Path<String>,
    Json(body): Json<GitCommitRequest>,
) -> Response {
    let root = match project_path(&app, &id) {
        Ok(p) => p,
        Err(e) => return e,
    };
    match crate::git::commit(&root, &body.message).await {
        Ok(result) => Json(result).into_response(),
        Err(e) => err(StatusCode::BAD_REQUEST, e),
    }
}

async fn git_workflow_service(
    app: &SharedState,
    id: &str,
) -> Result<std::sync::Arc<crate::git_workflows::GitWorkflowService>, Response> {
    if let Some(service) = app.git_workflows.read().get(id).cloned() {
        return Ok(service);
    }
    let root = project_path(app, id)?;
    let service = crate::git_workflows::GitWorkflowService::open(
        root,
        crate::git_workflows::GitWorkflowLimits::default(),
    )
    .await
    .map_err(git_workflow_error)?;
    let service = std::sync::Arc::new(service);
    let mut services = app.git_workflows.write();
    Ok(services
        .entry(id.to_string())
        .or_insert_with(|| service.clone())
        .clone())
}

fn git_workflow_error(error: crate::git_workflows::GitWorkflowError) -> Response {
    use crate::git_workflows::GitWorkflowError;

    let status = match &error {
        GitWorkflowError::InvalidConfiguration
        | GitWorkflowError::InvalidPath { .. }
        | GitWorkflowError::PathEscape { .. }
        | GitWorkflowError::SymlinkEscape { .. }
        | GitWorkflowError::InvalidBranch { .. }
        | GitWorkflowError::InvalidCommitMessage { .. }
        | GitWorkflowError::NotRepository => StatusCode::BAD_REQUEST,
        GitWorkflowError::RepositoryChanged
        | GitWorkflowError::BranchExists { .. }
        | GitWorkflowError::BranchNotFound { .. }
        | GitWorkflowError::DirtyWorktree
        | GitWorkflowError::Conflicts { .. }
        | GitWorkflowError::NothingToCommit => StatusCode::CONFLICT,
        GitWorkflowError::Busy => StatusCode::TOO_MANY_REQUESTS,
        GitWorkflowError::TimedOut => StatusCode::GATEWAY_TIMEOUT,
        GitWorkflowError::OutputLimitExceeded
        | GitWorkflowError::Io { .. }
        | GitWorkflowError::Git { .. } => StatusCode::INTERNAL_SERVER_ERROR,
    };
    let code = error.code().to_string();
    (
        status,
        Json(json!({
            "error": {
                "message": error.to_string(),
                "code": code,
                "detail": error,
            }
        })),
    )
        .into_response()
}

async fn git_workflow_state(
    State(app): State<SharedState>,
    Path(id): Path<String>,
) -> Response {
    let service = match git_workflow_service(&app, &id).await {
        Ok(service) => service,
        Err(error) => return error,
    };
    match service.repository_state().await {
        Ok(state) => Json(state).into_response(),
        Err(error) => git_workflow_error(error),
    }
}

#[derive(Deserialize)]
struct GitPathsRequest {
    #[serde(default)]
    paths: Vec<String>,
}

#[derive(Deserialize)]
struct GitDiffRequest {
    #[serde(default)]
    scope: Option<String>,
    #[serde(default)]
    paths: Vec<String>,
}

async fn git_workflow_diff(
    State(app): State<SharedState>,
    Path(id): Path<String>,
    Json(body): Json<GitDiffRequest>,
) -> Response {
    let scope = match body.scope.as_deref().unwrap_or("all") {
        "all" => crate::git_workflows::DiffScope::All,
        "staged" => crate::git_workflows::DiffScope::Staged,
        "unstaged" => crate::git_workflows::DiffScope::Unstaged,
        _ => return err(StatusCode::BAD_REQUEST, "invalid diff scope"),
    };
    let service = match git_workflow_service(&app, &id).await {
        Ok(service) => service,
        Err(error) => return error,
    };
    match service.diff_preview(scope, &body.paths).await {
        Ok(preview) => Json(preview).into_response(),
        Err(error) => git_workflow_error(error),
    }
}

async fn git_workflow_branches(
    State(app): State<SharedState>,
    Path(id): Path<String>,
) -> Response {
    let service = match git_workflow_service(&app, &id).await {
        Ok(service) => service,
        Err(error) => return error,
    };
    match service.list_branches().await {
        Ok(branches) => Json(json!({"branches": branches})).into_response(),
        Err(error) => git_workflow_error(error),
    }
}

#[derive(Deserialize)]
struct GitBranchRequest {
    name: String,
}

async fn git_workflow_create_branch(
    State(app): State<SharedState>,
    Path(id): Path<String>,
    Json(body): Json<GitBranchRequest>,
) -> Response {
    let service = match git_workflow_service(&app, &id).await {
        Ok(service) => service,
        Err(error) => return error,
    };
    match service.create_branch(&body.name).await {
        Ok(branch) => (StatusCode::CREATED, Json(branch)).into_response(),
        Err(error) => git_workflow_error(error),
    }
}

async fn git_workflow_switch_branch(
    State(app): State<SharedState>,
    Path(id): Path<String>,
    Json(body): Json<GitBranchRequest>,
) -> Response {
    let service = match git_workflow_service(&app, &id).await {
        Ok(service) => service,
        Err(error) => return error,
    };
    match service.switch_branch(&body.name).await {
        Ok(branch) => Json(branch).into_response(),
        Err(error) => git_workflow_error(error),
    }
}

async fn git_workflow_stage(
    State(app): State<SharedState>,
    Path(id): Path<String>,
    Json(body): Json<GitPathsRequest>,
) -> Response {
    let service = match git_workflow_service(&app, &id).await {
        Ok(service) => service,
        Err(error) => return error,
    };
    match service.stage_paths(&body.paths).await {
        Ok(state) => Json(state).into_response(),
        Err(error) => git_workflow_error(error),
    }
}

async fn git_workflow_unstage(
    State(app): State<SharedState>,
    Path(id): Path<String>,
    Json(body): Json<GitPathsRequest>,
) -> Response {
    let service = match git_workflow_service(&app, &id).await {
        Ok(service) => service,
        Err(error) => return error,
    };
    match service.unstage_paths(&body.paths).await {
        Ok(state) => Json(state).into_response(),
        Err(error) => git_workflow_error(error),
    }
}

#[derive(Deserialize)]
struct GitWorkflowCommitRequest {
    message: String,
}

async fn git_workflow_commit(
    State(app): State<SharedState>,
    Path(id): Path<String>,
    Json(body): Json<GitWorkflowCommitRequest>,
) -> Response {
    let service = match git_workflow_service(&app, &id).await {
        Ok(service) => service,
        Err(error) => return error,
    };
    match service.commit(&body.message).await {
        Ok(commit) => Json(commit).into_response(),
        Err(error) => git_workflow_error(error),
    }
}

#[derive(Deserialize)]
struct GitAuditQuery {
    #[serde(default)]
    after_sequence: u64,
    #[serde(default = "default_git_audit_limit")]
    limit: usize,
}

fn default_git_audit_limit() -> usize {
    50
}

async fn git_workflow_audit(
    State(app): State<SharedState>,
    Path(id): Path<String>,
    Query(query): Query<GitAuditQuery>,
) -> Response {
    let service = match git_workflow_service(&app, &id).await {
        Ok(service) => service,
        Err(error) => return error,
    };
    Json(json!({
        "records": service.audit_records(query.after_sequence, query.limit),
    }))
    .into_response()
}

async fn list_skills(State(_app): State<SharedState>) -> Json<serde_json::Value> {
    Json(json!({"skills": crate::skills::list()}))
}

#[derive(Deserialize)]
struct CreateSkillRequest {
    /// Raw SKILL.md text (paste path). Mutually exclusive with
    /// `folder_path`; markdown wins if somehow both are sent.
    markdown: Option<String>,
    /// A local directory that already contains a SKILL.md (the
    /// folder-browser path, same `/api/fs/list` flow as workspaces).
    folder_path: Option<String>,
}

async fn create_skill(State(_app): State<SharedState>, Json(body): Json<CreateSkillRequest>) -> Response {
    let markdown = body.markdown.as_deref().map(str::trim).filter(|s| !s.is_empty());
    let folder_path = body.folder_path.as_deref().map(str::trim).filter(|s| !s.is_empty());
    let result = match (markdown, folder_path) {
        (Some(md), _) => crate::skills::create_from_markdown(md),
        (None, Some(path)) => crate::skills::create_from_folder(std::path::Path::new(path)),
        (None, None) => return err(StatusCode::BAD_REQUEST, "either 'markdown' or 'folder_path' is required"),
    };
    match result {
        Ok(skill) => Json(skill).into_response(),
        Err(e) => err(StatusCode::BAD_REQUEST, e),
    }
}

#[derive(Deserialize)]
struct SkillEnabledRequest {
    on: bool,
}

async fn set_skill_enabled(
    State(_app): State<SharedState>,
    Path(name): Path<String>,
    Json(body): Json<SkillEnabledRequest>,
) -> Response {
    match crate::skills::set_enabled(&name, body.on) {
        Ok(()) => StatusCode::NO_CONTENT.into_response(),
        Err(e) => err(StatusCode::NOT_FOUND, e),
    }
}

async fn delete_skill(State(_app): State<SharedState>, Path(name): Path<String>) -> Response {
    match crate::skills::delete(&name) {
        Ok(()) => StatusCode::NO_CONTENT.into_response(),
        Err(e) => err(StatusCode::NOT_FOUND, e),
    }
}

/// Bumps `last_active_at` to now. Called by the UI whenever a session
/// under this project is created or updated, so the persisted record
/// tracks real activity rather than just project-creation time.
async fn touch_project(State(app): State<SharedState>, Path(id): Path<String>) -> Response {
    let mut state = app.state.write();
    let Some(project) = state.projects.get_mut(&id) else {
        return err(StatusCode::NOT_FOUND, format!("project '{id}' not found"));
    };
    project.last_active_at = chrono::Utc::now();
    if let Err(e) = state.save() {
        return err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string());
    }
    StatusCode::NO_CONTENT.into_response()
}
