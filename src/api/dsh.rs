//! `/api/dsh/*` -- DeepSeek Harness profile + plugin CRUD, all real
//! filesystem/`dsh` shell-outs (see `crate::dsh`), plus a detection
//! status endpoint for the UI's "not installed" state.

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::json;

use super::SharedState;
use crate::dsh;

pub fn routes() -> Router<SharedState> {
    Router::new()
        .route("/api/dsh/status", get(status))
        .route("/api/dsh/profiles", get(list_profiles).post(create_profile))
        .route("/api/dsh/profiles/:name", axum::routing::delete(delete_profile))
        .route(
            "/api/dsh/profiles/:name/plugins",
            get(list_plugins).post(install_plugin),
        )
        .route(
            "/api/dsh/profiles/:name/plugins/:pkg",
            axum::routing::delete(remove_plugin),
        )
}

fn err(status: StatusCode, message: impl Into<String>) -> Response {
    (status, Json(json!({"error": message.into()}))).into_response()
}

async fn status(State(app): State<SharedState>) -> Response {
    let bin = &app.config.dsh.bin;
    let detected = dsh::detect(bin).is_ok();
    let version = if detected { dsh::version(bin) } else { None };
    Json(json!({"detected": detected, "version": version})).into_response()
}

async fn list_profiles() -> Response {
    Json(json!({"profiles": dsh::list_profiles()})).into_response()
}

#[derive(Deserialize)]
struct CreateProfileRequest {
    name: String,
}

async fn create_profile(Json(body): Json<CreateProfileRequest>) -> Response {
    match dsh::create_profile(body.name.trim()) {
        Ok(()) => Json(json!({"name": body.name.trim()})).into_response(),
        Err(e) => err(StatusCode::BAD_REQUEST, e),
    }
}

async fn delete_profile(Path(name): Path<String>) -> Response {
    match dsh::delete_profile(&name) {
        Ok(()) => StatusCode::NO_CONTENT.into_response(),
        Err(e) => err(StatusCode::NOT_FOUND, e),
    }
}

async fn list_plugins(Path(name): Path<String>) -> Response {
    match dsh::list_plugins(&name) {
        Ok(plugins) => Json(json!({"plugins": plugins})).into_response(),
        Err(e) => err(StatusCode::NOT_FOUND, e),
    }
}

#[derive(Deserialize)]
struct InstallPluginRequest {
    package: String,
}

async fn install_plugin(
    State(app): State<SharedState>,
    Path(name): Path<String>,
    Json(body): Json<InstallPluginRequest>,
) -> Response {
    if let Err(e) = dsh::detect(&app.config.dsh.bin) {
        return err(StatusCode::PRECONDITION_FAILED, e.to_string());
    }
    if let Err(e) = dsh::install_plugin(&app.config.dsh.bin, &name, body.package.trim()).await {
        return err(StatusCode::BAD_GATEWAY, e);
    }
    match dsh::list_plugins(&name) {
        Ok(plugins) => Json(json!({"plugins": plugins})).into_response(),
        Err(e) => err(StatusCode::INTERNAL_SERVER_ERROR, e),
    }
}

async fn remove_plugin(State(app): State<SharedState>, Path((name, pkg)): Path<(String, String)>) -> Response {
    if let Err(e) = dsh::detect(&app.config.dsh.bin) {
        return err(StatusCode::PRECONDITION_FAILED, e.to_string());
    }
    if let Err(e) = dsh::remove_plugin(&app.config.dsh.bin, &name, &pkg).await {
        return err(StatusCode::BAD_GATEWAY, e);
    }
    match dsh::list_plugins(&name) {
        Ok(plugins) => Json(json!({"plugins": plugins})).into_response(),
        Err(e) => err(StatusCode::INTERNAL_SERVER_ERROR, e),
    }
}
