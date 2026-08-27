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
use crate::engine;
use crate::registry::{ModelSource, ModelState, ModelStatus};
use crate::residency;
use crate::state::ModelManifest;

pub fn routes() -> Router<SharedState> {
    Router::new()
        .route("/api/status", get(status))
        .route("/api/config", post(update_config))
        .route("/api/settings", get(get_settings).put(put_settings))
        .route("/api/models/pull", post(start_pull))
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
        .route("/api/projects/:id/git/commit", post(git_commit))
        .route(
            "/api/sessions/:id/generation",
            get(get_generation).delete(stop_generation),
        )
        .route("/api/generations", get(list_generations))
        .route("/api/chats", get(list_chats).post(create_chat))
        .route(
            "/api/chats/:id",
            axum::routing::patch(update_chat).delete(delete_chat),
        )
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
        },
    }))
}

#[derive(Deserialize)]
struct UpdateConfigRequest {
    ram_headroom_gb: Option<f64>,
    model_dir: String,
}

/// Writes `ram_headroom_gb`/`model_dir` into `~/.kiralm/config.toml`,
/// editing just those two keys of whatever's already on disk (a
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
    Json(body): Json<crate::settings::Settings>,
) -> Response {
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
        "loaded": matches!(m.status, ModelStatus::Loaded),
        "rss_bytes": m.pid.and_then(residency::read_rss_bytes),
        "verdict": m.verdict.as_ref().map(|v| v.tier.as_str()),
        "verdict_reason": m.verdict.as_ref().map(|v| v.reason.clone()),
        "verdict_tier": m.verdict.as_ref().map(|v| v.tier.as_str()),
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
                // unsupported. That's still success from the pipeline's
                // point of view, so register it either way: a fresh
                // manifest now exists on disk for it.
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

    // Unload if running (SIGTERM/SIGKILL), by pid. This stub
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

async fn create_project(State(app): State<SharedState>, Json(body): Json<CreateProjectRequest>) -> Response {
    let path = body.path.trim();
    if path.is_empty() {
        return err(StatusCode::BAD_REQUEST, "path is required");
    }
    let name = body
        .name
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

    let name = body
        .name
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
    chats.chats.retain(|c| c.project_id != id);
    let _ = chats.save();

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
    let now = chrono::Utc::now().to_rfc3339();
    let chat = crate::chats::Chat {
        id: uuid::Uuid::new_v4().to_string(),
        title: "New chat".to_string(),
        created_at: now.clone(),
        updated_at: now,
        project_id: body.project_id,
        permission: body.permission,
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
    messages: Option<serde_json::Value>,
}

async fn update_chat(State(app): State<SharedState>, Path(id): Path<String>, Json(body): Json<UpdateChatRequest>) -> Response {
    let mut chats = app.chats.write();
    let Some(chat) = chats.chats.iter_mut().find(|c| c.id == id) else {
        return err(StatusCode::NOT_FOUND, format!("chat '{id}' not found"));
    };
    if let Some(title) = body.title {
        chat.title = title;
    }
    if let Some(permission) = body.permission {
        chat.permission = permission;
    }
    if let Some(messages) = body.messages {
        chat.messages = messages;
    }
    chat.updated_at = chrono::Utc::now().to_rfc3339();
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

/// Looks up a registered project's real directory by id, 404ing if it
/// isn't registered rather than falling back to any default path.
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

#[derive(Deserialize)]
struct GitCommitRequest {
    message: Option<String>,
}

/// `git add -A && git commit && git push`, for real, against the
/// project's real directory -- what the UI's "Commit, push & PR" button
/// drives.
async fn git_commit(
    State(app): State<SharedState>,
    Path(id): Path<String>,
    Json(body): Json<GitCommitRequest>,
) -> Response {
    let root = match project_path(&app, &id) {
        Ok(p) => p,
        Err(e) => return e,
    };
    let message = body
        .message
        .map(|m| m.trim().to_string())
        .filter(|m| !m.is_empty())
        .unwrap_or_else(|| "Update via kiralm".to_string());
    match crate::git::commit_and_push(&root, &message).await {
        Ok(result) => Json(result).into_response(),
        Err(e) => err(StatusCode::BAD_REQUEST, e),
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
