//! `/api/plugins/catalog*` -- the unified cloud-hosted plugin catalog:
//! browse, install, uninstall across all three source kinds. Live proxy
//! to kiralm-web plus dispatch logic lives in plugin_catalog.rs; this
//! layer's own job is the same live-mcp-registry wiring api/marketplace.rs
//! and api/agent_plugin.rs do for their own installs.

use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::{Json, Router};
use serde::{Deserialize, Serialize};
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

#[derive(Deserialize)]
struct ListCatalogQuery {
    q: Option<String>,
    offset: Option<usize>,
    limit: Option<usize>,
    installed: Option<String>,
}

async fn list_catalog(State(app): State<SharedState>, Query(params): Query<ListCatalogQuery>) -> Response {
    let installed = match params.installed.as_deref() {
        Some("installed") => Some(true),
        Some("uninstalled") => Some(false),
        _ => None,
    };
    let offset = params.offset.unwrap_or(0);
    let limit = params.limit.unwrap_or(plugin_catalog::DEFAULT_CATALOG_LIMIT).min(plugin_catalog::MAX_CATALOG_LIMIT);

    let client = match plugin_catalog::build_client() {
        Ok(c) => c,
        Err(e) => return err(StatusCode::INTERNAL_SERVER_ERROR, e),
    };

    match plugin_catalog::list_catalog_page(&client, &app.config.cloud.api_base_url, params.q.as_deref(), installed, offset, limit)
        .await
    {
        Ok((rows, total)) => {
            let rows: Vec<CatalogRow> =
                rows.into_iter().map(|(plugin, installed)| CatalogRow { plugin, installed }).collect();
            Json(json!({"data": rows, "total": total})).into_response()
        }
        // No cache to fall back to on this live path -- a real error,
        // not a silent empty page.
        Err(e) => err(StatusCode::BAD_GATEWAY, format!("plugin catalog unavailable: {e}")),
    }
}

async fn install(State(app): State<SharedState>, Path(id): Path<String>) -> Response {
    let client = match plugin_catalog::build_client() {
        Ok(c) => c,
        Err(e) => return err(StatusCode::INTERNAL_SERVER_ERROR, e),
    };
    let row = match plugin_catalog::find_row(&client, &app.config.cloud.api_base_url, &id).await {
        Ok(Some(row)) => row,
        Ok(None) => return err(StatusCode::NOT_FOUND, format!("catalog entry '{id}' not found")),
        Err(e) => return err(StatusCode::BAD_GATEWAY, format!("plugin catalog unavailable: {e}")),
    };
    let outcome = match plugin_catalog::install(&app.config.dsh.bin, &row).await {
        Ok(o) => o,
        Err(e) => return err(StatusCode::BAD_REQUEST, e),
    };
    plugin_catalog::mark_installed(&id);
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
    let client = match plugin_catalog::build_client() {
        Ok(c) => c,
        Err(e) => return err(StatusCode::INTERNAL_SERVER_ERROR, e),
    };
    let row = match plugin_catalog::find_row(&client, &app.config.cloud.api_base_url, &id).await {
        Ok(Some(row)) => row,
        Ok(None) => return err(StatusCode::NOT_FOUND, format!("catalog entry '{id}' not found")),
        Err(e) => return err(StatusCode::BAD_GATEWAY, format!("plugin catalog unavailable: {e}")),
    };
    let outcome = match plugin_catalog::uninstall(&app.config.dsh.bin, &row).await {
        Ok(o) => o,
        Err(e) => return err(StatusCode::BAD_REQUEST, e),
    };
    plugin_catalog::mark_uninstalled(&id);
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
