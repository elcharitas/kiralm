//! `/api/plugins/catalog*` -- the unified cloud-hosted plugin catalog:
//! browse, install, uninstall across all three source kinds. Dispatch
//! logic lives in plugin_catalog.rs; this layer's own job is the same
//! live-mcp-registry wiring api/marketplace.rs and api/agent_plugin.rs
//! do for their own installs.

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::{Json, Router};
use serde::Serialize;
use serde_json::json;

use super::SharedState;
use crate::mcp;
use crate::plugin_catalog::{self, CatalogInstallOutcome, CatalogUninstallOutcome, CloudPlugin};

pub fn routes() -> Router<SharedState> {
    Router::new()
        .route("/api/plugins/catalog", get(list_catalog))
        .route("/api/plugins/catalog/:id/install", axum::routing::post(install))
        .route("/api/plugins/catalog/:id", axum::routing::delete(uninstall))
}

fn err(status: StatusCode, message: impl Into<String>) -> Response {
    (status, Json(json!({"error": message.into()}))).into_response()
}

#[derive(Serialize)]
struct CatalogRow {
    #[serde(flatten)]
    plugin: CloudPlugin,
    installed: bool,
}

async fn list_catalog(State(_app): State<SharedState>) -> Response {
    let rows: Vec<CatalogRow> = plugin_catalog::cached_plugins()
        .into_iter()
        .map(|plugin| {
            let installed = plugin_catalog::is_installed(&plugin);
            CatalogRow { plugin, installed }
        })
        .collect();
    Json(json!({"data": rows})).into_response()
}

fn find_row(id: &str) -> Option<CloudPlugin> {
    plugin_catalog::cached_plugins().into_iter().find(|p| p.id == id)
}

async fn install(State(app): State<SharedState>, Path(id): Path<String>) -> Response {
    let Some(row) = find_row(&id) else {
        return err(StatusCode::NOT_FOUND, format!("catalog entry '{id}' not found"));
    };
    let outcome = match plugin_catalog::install(&app.config.dsh.bin, &row).await {
        Ok(o) => o,
        Err(e) => return err(StatusCode::BAD_REQUEST, e),
    };
    match outcome {
        CatalogInstallOutcome::Marketplace(o) => {
            for name in &o.mcp_servers {
                if let Some(cfg) = mcp::get_config(name) {
                    app.mcp.connect(&cfg).await;
                }
            }
            Json(o).into_response()
        }
        CatalogInstallOutcome::AgentPlugin(r) => {
            for name in &r.outcome.mcp_servers {
                if let Some(cfg) = mcp::get_config(name) {
                    app.mcp.connect(&cfg).await;
                }
            }
            Json(json!({
                "plugin": r.plugin,
                "skills": r.outcome.skills,
                "mcp_servers": r.outcome.mcp_servers,
                "warnings": r.outcome.warnings,
            }))
            .into_response()
        }
        CatalogInstallOutcome::Dsh(output) => Json(json!({"output": output})).into_response(),
    }
}

async fn uninstall(State(app): State<SharedState>, Path(id): Path<String>) -> Response {
    let Some(row) = find_row(&id) else {
        return err(StatusCode::NOT_FOUND, format!("catalog entry '{id}' not found"));
    };
    let outcome = match plugin_catalog::uninstall(&app.config.dsh.bin, &row).await {
        Ok(o) => o,
        Err(e) => return err(StatusCode::BAD_REQUEST, e),
    };
    match outcome {
        CatalogUninstallOutcome::Registry(o) => {
            for name in &o.mcp_servers {
                app.mcp.disconnect(name);
            }
            Json(o).into_response()
        }
        CatalogUninstallOutcome::Dsh(output) => Json(json!({"output": output})).into_response(),
    }
}
