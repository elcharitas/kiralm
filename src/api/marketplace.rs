//! `/api/marketplaces*` -- add/list/refresh/remove marketplaces, list/
//! install/uninstall their plugins. marketplace.rs owns the file-side
//! work; this layer's own job is wiring newly (dis)connected mcp
//! servers into the live registry, same split as api/mcp.rs.

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::{Deserialize, Serialize};
use serde_json::json;

use super::SharedState;
use crate::marketplace::{self, Marketplace, PluginSource};
use crate::mcp;

pub fn routes() -> Router<SharedState> {
    Router::new()
        .route("/api/marketplaces", get(list_marketplaces).post(add_marketplace))
        .route("/api/marketplaces/:slug", axum::routing::delete(remove_marketplace))
        .route("/api/marketplaces/:slug/refresh", post(refresh_marketplace))
        .route("/api/marketplaces/:slug/plugins", get(list_plugins))
        .route("/api/marketplaces/:slug/plugins/:name/install", post(install_plugin))
        .route("/api/marketplaces/:slug/plugins/:name", axum::routing::delete(uninstall_plugin))
}

fn err(status: StatusCode, message: impl Into<String>) -> Response {
    (status, Json(json!({"error": message.into()}))).into_response()
}

async fn list_marketplaces(State(_app): State<SharedState>) -> Response {
    Json(json!({"marketplaces": marketplace::list()})).into_response()
}

#[derive(Deserialize)]
struct AddMarketplaceRequest {
    url: String,
}

async fn add_marketplace(State(_app): State<SharedState>, Json(body): Json<AddMarketplaceRequest>) -> Response {
    match marketplace::add(&body.url).await {
        Ok(m) => Json(m).into_response(),
        Err(e) => err(StatusCode::BAD_REQUEST, e),
    }
}

async fn remove_marketplace(State(_app): State<SharedState>, Path(slug): Path<String>) -> Response {
    match marketplace::remove(&slug) {
        Ok(()) => StatusCode::NO_CONTENT.into_response(),
        Err(e) => err(StatusCode::NOT_FOUND, e),
    }
}

async fn refresh_marketplace(State(_app): State<SharedState>, Path(slug): Path<String>) -> Response {
    match marketplace::refresh(&slug).await {
        Ok(m) => Json(m).into_response(),
        Err(e) => err(StatusCode::BAD_REQUEST, e),
    }
}

#[derive(Serialize)]
struct PluginView {
    name: String,
    source: PluginSource,
    description: Option<String>,
    installed: bool,
}

async fn list_plugins(State(_app): State<SharedState>, Path(slug): Path<String>) -> Response {
    let marketplace: Marketplace = match marketplace::get(&slug) {
        Ok(m) => m,
        Err(e) => return err(StatusCode::NOT_FOUND, e),
    };
    let plugins: Vec<PluginView> = marketplace
        .plugins
        .into_iter()
        .map(|p| PluginView {
            installed: marketplace::is_installed(&slug, &p.name),
            name: p.name,
            source: p.source,
            description: p.description,
        })
        .collect();
    Json(json!({"plugins": plugins})).into_response()
}

async fn install_plugin(State(app): State<SharedState>, Path((slug, name)): Path<(String, String)>) -> Response {
    let outcome = match marketplace::install_plugin(&slug, &name).await {
        Ok(o) => o,
        Err(e) => return err(StatusCode::BAD_REQUEST, e),
    };
    for server_name in &outcome.mcp_servers {
        if let Some(cfg) = mcp::get_config(server_name) {
            app.mcp.connect(&cfg).await;
        }
    }
    Json(outcome).into_response()
}

async fn uninstall_plugin(State(app): State<SharedState>, Path((slug, name)): Path<(String, String)>) -> Response {
    let outcome = marketplace::uninstall_plugin(&slug, &name);
    for server_name in &outcome.mcp_servers {
        app.mcp.disconnect(server_name);
    }
    Json(outcome).into_response()
}
