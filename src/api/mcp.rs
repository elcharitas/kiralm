//! `/api/mcp/servers*` -- manual MCP server registration plus live
//! connect/disconnect/refresh, mirroring `api/tunnel.rs`'s shape.

use std::collections::HashMap;

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::{Deserialize, Serialize};
use serde_json::json;

use super::SharedState;
use crate::mcp::{self, McpServerConfig};

pub fn routes() -> Router<SharedState> {
    Router::new()
        .route("/api/mcp/servers", get(list_servers).post(add_server))
        .route(
            "/api/mcp/servers/:name",
            axum::routing::patch(patch_server).delete(delete_server),
        )
        .route("/api/mcp/servers/:name/refresh", post(refresh_server))
}

fn err(status: StatusCode, message: impl Into<String>) -> Response {
    (status, Json(json!({"error": message.into()}))).into_response()
}

#[derive(Serialize)]
struct ServerView {
    name: String,
    command: String,
    args: Vec<String>,
    env: HashMap<String, String>,
    enabled: bool,
    connected: bool,
    tool_count: usize,
    error: Option<String>,
}

fn view(app: &SharedState, cfg: McpServerConfig) -> ServerView {
    let status = app.mcp.status(&cfg.name);
    ServerView {
        name: cfg.name,
        command: cfg.command,
        args: cfg.args,
        env: cfg.env,
        enabled: cfg.enabled,
        connected: status.as_ref().map(|s| s.connected).unwrap_or(false),
        tool_count: status.as_ref().map(|s| s.tool_count).unwrap_or(0),
        error: status.and_then(|s| s.error),
    }
}

async fn list_servers(State(app): State<SharedState>) -> Response {
    let servers: Vec<_> = mcp::load_configs().into_iter().map(|c| view(&app, c)).collect();
    Json(json!({"servers": servers})).into_response()
}

#[derive(Deserialize)]
struct AddServerRequest {
    name: String,
    command: String,
    #[serde(default)]
    args: Vec<String>,
    #[serde(default)]
    env: HashMap<String, String>,
    #[serde(default = "default_enabled")]
    enabled: bool,
}

fn default_enabled() -> bool {
    true
}

async fn add_server(State(app): State<SharedState>, Json(body): Json<AddServerRequest>) -> Response {
    let cfg = McpServerConfig {
        name: body.name.trim().to_string(),
        command: body.command,
        args: body.args,
        env: body.env,
        enabled: body.enabled,
        installed_by: None,
    };
    let saved = match mcp::add_config(cfg) {
        Ok(c) => c,
        Err(e) => return err(StatusCode::BAD_REQUEST, e),
    };
    if saved.enabled {
        app.mcp.connect(&saved).await;
    }
    Json(view(&app, saved)).into_response()
}

#[derive(Deserialize)]
struct PatchServerRequest {
    enabled: bool,
}

async fn patch_server(
    State(app): State<SharedState>,
    Path(name): Path<String>,
    Json(body): Json<PatchServerRequest>,
) -> Response {
    let cfg = match mcp::set_config_enabled(&name, body.enabled) {
        Ok(c) => c,
        Err(e) => return err(StatusCode::NOT_FOUND, e),
    };
    if cfg.enabled {
        app.mcp.connect(&cfg).await;
    } else {
        app.mcp.disconnect(&cfg.name);
    }
    Json(view(&app, cfg)).into_response()
}

async fn delete_server(State(app): State<SharedState>, Path(name): Path<String>) -> Response {
    if let Err(e) = mcp::delete_config(&name) {
        return err(StatusCode::NOT_FOUND, e);
    }
    app.mcp.disconnect(&name);
    StatusCode::NO_CONTENT.into_response()
}

async fn refresh_server(State(app): State<SharedState>, Path(name): Path<String>) -> Response {
    let Some(cfg) = mcp::get_config(&name) else {
        return err(StatusCode::NOT_FOUND, format!("mcp server '{name}' not found"));
    };
    if !cfg.enabled {
        return err(StatusCode::BAD_REQUEST, "server is disabled");
    }
    app.mcp.refresh(&cfg).await;
    Json(view(&app, cfg)).into_response()
}
