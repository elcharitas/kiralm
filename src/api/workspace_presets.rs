use std::collections::HashSet;

use axum::extract::{Path, Query, State};
use axum::http::{header, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::json;

use super::SharedState;
use crate::state::WorkspacePresetBinding;
use crate::workspace_presets::{
    ImportMode, WorkspacePreset, WorkspacePresetError, WorkspacePresetInput,
};

pub fn routes() -> Router<SharedState> {
    Router::new()
        .route("/api/workspace-presets", get(list).post(create))
        .route("/api/workspace-presets/import", post(import))
        .route("/api/workspace-presets/export", get(export))
        .route(
            "/api/workspace-presets/:id",
            get(get_one).put(update).delete(delete),
        )
        .route("/api/workspace-presets/:id/duplicate", post(duplicate))
        .route("/api/workspace-presets/:id/apply", post(apply))
}

fn preset_error(error: WorkspacePresetError) -> Response {
    let status = match &error {
        WorkspacePresetError::Invalid(_) => StatusCode::BAD_REQUEST,
        WorkspacePresetError::NotFound(_) => StatusCode::NOT_FOUND,
        WorkspacePresetError::NameConflict(_)
        | WorkspacePresetError::RevisionConflict { .. }
        | WorkspacePresetError::StoreRevisionConflict { .. } => StatusCode::CONFLICT,
        WorkspacePresetError::Storage(_) => StatusCode::INTERNAL_SERVER_ERROR,
    };
    (status, Json(json!({"error": error.to_string()}))).into_response()
}

fn bad_request(message: impl Into<String>) -> Response {
    (
        StatusCode::BAD_REQUEST,
        Json(json!({"error": message.into()})),
    )
        .into_response()
}

async fn list(State(app): State<SharedState>) -> Json<serde_json::Value> {
    Json(json!({
        "revision": app.workspace_presets.revision(),
        "presets": app.workspace_presets.list(),
        "bindings": app.state.read().workspace_preset_bindings,
    }))
}

async fn get_one(State(app): State<SharedState>, Path(id): Path<String>) -> Response {
    match app.workspace_presets.get(&id) {
        Some(preset) => Json(preset).into_response(),
        None => preset_error(WorkspacePresetError::NotFound(id)),
    }
}

async fn create(
    State(app): State<SharedState>,
    Json(input): Json<WorkspacePresetInput>,
) -> Response {
    match app.workspace_presets.create(input) {
        Ok(preset) => (StatusCode::CREATED, Json(preset)).into_response(),
        Err(error) => preset_error(error),
    }
}

#[derive(Deserialize)]
struct UpdateRequest {
    expected_revision: u64,
    preset: WorkspacePresetInput,
}

async fn update(
    State(app): State<SharedState>,
    Path(id): Path<String>,
    Json(body): Json<UpdateRequest>,
) -> Response {
    match app
        .workspace_presets
        .update(&id, body.expected_revision, body.preset)
    {
        Ok(preset) => Json(preset).into_response(),
        Err(error) => preset_error(error),
    }
}

#[derive(Deserialize)]
struct RevisionQuery {
    revision: u64,
}

async fn delete(
    State(app): State<SharedState>,
    Path(id): Path<String>,
    Query(query): Query<RevisionQuery>,
) -> Response {
    match app.workspace_presets.delete(&id, query.revision) {
        Ok(preset) => {
            let mut state = app.state.write();
            let mut next_state = state.clone();
            next_state
                .workspace_preset_bindings
                .retain(|_, binding| binding.preset_id != preset.id);
            if let Err(error) = next_state.save() {
                tracing::warn!("failed to clean workspace preset bindings: {error}");
            } else {
                *state = next_state;
            }
            Json(preset).into_response()
        }
        Err(error) => preset_error(error),
    }
}

#[derive(Deserialize)]
struct DuplicateRequest {
    expected_revision: u64,
    name: String,
}

async fn duplicate(
    State(app): State<SharedState>,
    Path(id): Path<String>,
    Json(body): Json<DuplicateRequest>,
) -> Response {
    match app
        .workspace_presets
        .duplicate(&id, body.expected_revision, &body.name)
    {
        Ok(preset) => (StatusCode::CREATED, Json(preset)).into_response(),
        Err(error) => preset_error(error),
    }
}

async fn export(State(app): State<SharedState>) -> Response {
    match app.workspace_presets.export_json() {
        Ok(body) => ([(header::CONTENT_TYPE, "application/json")], body).into_response(),
        Err(error) => preset_error(error),
    }
}

#[derive(Deserialize)]
struct ImportRequest {
    json: String,
    mode: String,
    expected_store_revision: u64,
}

async fn import(State(app): State<SharedState>, Json(body): Json<ImportRequest>) -> Response {
    let mode = match body.mode.as_str() {
        "merge" => ImportMode::Merge,
        "replace" => ImportMode::Replace,
        _ => return bad_request("import mode must be 'merge' or 'replace'"),
    };
    match app
        .workspace_presets
        .import_json(&body.json, mode, body.expected_store_revision)
    {
        Ok(presets) => Json(json!({"presets": presets})).into_response(),
        Err(error) => preset_error(error),
    }
}

#[derive(Deserialize)]
struct ApplyRequest {
    project_id: String,
    expected_revision: u64,
}

async fn apply(
    State(app): State<SharedState>,
    Path(id): Path<String>,
    Json(body): Json<ApplyRequest>,
) -> Response {
    let preset = match app
        .workspace_presets
        .resolve_for_apply(&id, Some(body.expected_revision))
    {
        Ok(preset) => preset,
        Err(error) => return preset_error(error),
    };
    if let Err(message) = validate_application(&app, &preset, &body.project_id) {
        return bad_request(message);
    }
    let original_mcp = crate::mcp::load_configs();
    let enabled_mcp: HashSet<_> = preset
        .enabled_mcp_servers
        .iter()
        .map(String::as_str)
        .collect();
    let mut next_mcp = original_mcp.clone();
    for config in &mut next_mcp {
        config.enabled = enabled_mcp.contains(config.name.as_str());
    }
    if let Err(error) = crate::mcp::save_configs(&next_mcp) {
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"error": error.to_string()})),
        )
            .into_response();
    }
    {
        let mut state = app.state.write();
        let mut next_state = state.clone();
        next_state.workspace_preset_bindings.insert(
            body.project_id.clone(),
            WorkspacePresetBinding {
                preset_id: preset.id.clone(),
                preset_revision: preset.revision,
            },
        );
        for model in &preset.hot_model_ids {
            next_state.models.entry(model.clone()).or_default().hot = true;
        }
        if let Err(error) = next_state.save() {
            let _ = crate::mcp::save_configs(&original_mcp);
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": error.to_string()})),
            )
                .into_response();
        }
        *state = next_state;
    }

    for config in &next_mcp {
        if config.enabled {
            app.mcp.connect(config).await;
        } else {
            app.mcp.disconnect(&config.name);
        }
    }
    let profile = crate::engine::optimization::OptimizationProfile::parse(
        preset.optimization_profile.as_str(),
    )
    .expect("workspace preset profiles are validated");
    let mut profiled_models = preset.hot_model_ids.clone();
    if let Some(model) = &preset.preferred_model {
        profiled_models.push(model.clone());
    }
    profiled_models.sort();
    profiled_models.dedup();
    for model in profiled_models {
        let _ = app.supervisor.set_optimization_profile(&model, profile);
    }
    for model in &preset.hot_model_ids {
        app.registry.update(model, |entry| entry.hot = true);
        let _ = app.supervisor.set_hot(model, true).await;
        let supervisor = app.supervisor.clone();
        let model = model.clone();
        tokio::spawn(async move {
            let _ = supervisor.acquire(&model).await;
        });
    }
    Json(json!({"preset": preset, "project_id": body.project_id})).into_response()
}

fn validate_application(
    app: &SharedState,
    preset: &WorkspacePreset,
    project_id: &str,
) -> Result<(), String> {
    let project = app
        .state
        .read()
        .projects
        .get(project_id)
        .cloned()
        .ok_or_else(|| format!("workspace '{project_id}' was not found"))?;
    if preset
        .project_id
        .as_deref()
        .is_some_and(|id| id != project_id)
    {
        return Err("preset belongs to a different workspace".to_string());
    }
    if let Some(model) = &preset.preferred_model {
        if app.registry.get_clone(model).is_none() {
            return Err(format!("preferred model '{model}' was not found"));
        }
    }
    for model in &preset.hot_model_ids {
        if app.registry.get_clone(model).is_none() {
            return Err(format!("hot model '{model}' was not found"));
        }
    }
    let configured: HashSet<_> = crate::mcp::load_configs()
        .into_iter()
        .map(|config| config.name)
        .collect();
    for server in &preset.enabled_mcp_servers {
        if !configured.contains(server) {
            return Err(format!("MCP server '{server}' was not found"));
        }
    }
    let root = std::fs::canonicalize(&project.path)
        .map_err(|_| "workspace root is unavailable".to_string())?;
    for knowledge_root in &preset.knowledge_roots {
        let knowledge_root = std::fs::canonicalize(knowledge_root).map_err(|_| {
            format!(
                "knowledge root '{}' is unavailable",
                knowledge_root.display()
            )
        })?;
        if knowledge_root != root && !knowledge_root.starts_with(&root) {
            return Err(format!(
                "knowledge root '{}' escapes the workspace",
                knowledge_root.display()
            ));
        }
    }
    Ok(())
}
