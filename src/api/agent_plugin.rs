//! `/api/agent-plugins*` -- install/list/uninstall a single Agent
//! Plugins 1.0 plugin from a local path or git URL. agent_plugin.rs
//! owns the file-side work; this layer's job is the same live-registry
//! wiring api/marketplace.rs does for its own installs.

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::{Json, Router};
use serde::{Deserialize, Serialize};
use serde_json::json;

use super::SharedState;
use crate::agent_plugin;
use crate::mcp;

pub fn routes() -> Router<SharedState> {
    Router::new()
        .route("/api/agent-plugins", get(list_plugins).post(install_plugin))
        .route("/api/agent-plugins/:name", axum::routing::delete(uninstall_plugin))
}

fn err(status: StatusCode, message: impl Into<String>) -> Response {
    (status, Json(json!({"error": message.into()}))).into_response()
}

async fn list_plugins(State(_app): State<SharedState>) -> Response {
    Json(json!({"plugins": agent_plugin::list_installed()})).into_response()
}

#[derive(Deserialize)]
struct InstallRequest {
    source: String,
}

#[derive(Serialize)]
struct InstallResponse {
    plugin: agent_plugin::InstalledAgentPlugin,
    skills: Vec<String>,
    mcp_servers: Vec<String>,
    warnings: Vec<String>,
}

async fn install_plugin(State(app): State<SharedState>, Json(body): Json<InstallRequest>) -> Response {
    let result = match agent_plugin::install(&body.source).await {
        Ok(r) => r,
        Err(e) => return err(StatusCode::BAD_REQUEST, e),
    };
    for server_name in &result.outcome.mcp_servers {
        if let Some(cfg) = mcp::get_config(server_name) {
            app.mcp.connect(&cfg).await;
        }
    }
    Json(InstallResponse {
        plugin: result.plugin,
        skills: result.outcome.skills,
        mcp_servers: result.outcome.mcp_servers,
        warnings: result.outcome.warnings,
    })
    .into_response()
}

async fn uninstall_plugin(State(app): State<SharedState>, Path(name): Path<String>) -> Response {
    let outcome = match agent_plugin::uninstall(&name) {
        Ok(o) => o,
        Err(e) => return err(StatusCode::NOT_FOUND, e),
    };
    for server_name in &outcome.mcp_servers {
        app.mcp.disconnect(server_name);
    }
    Json(outcome).into_response()
}
