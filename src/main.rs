//! NodeLM daemon entrypoint.
//!
//! STUB: this binary currently wires up only the Anthropic-compatible
//! `/v1/messages` route (SPEC.md §2.2), the scope of this task. `main.rs`
//! per SPEC.md §1 should also parse subcommands (§7) and dispatch to
//! `cli.rs`/`serve`; that dispatch, plus `/v1/chat/completions`,
//! `/v1/models`, and `/api/*` (owned by other components), are not
//! implemented here.

mod api;
mod registry;
mod residency;

use std::sync::Arc;

use axum::routing::post;
use axum::Router;

use api::AppState;
use registry::ModelRegistry;
use residency::Residency;

#[tokio::main]
async fn main() {
    let state = AppState {
        registry: Arc::new(ModelRegistry::new()),
        residency: Arc::new(Residency::new()),
        http: reqwest::Client::new(),
    };

    let app = Router::new()
        .route("/v1/messages", post(api::anthropic::messages))
        .with_state(state);

    let listener = tokio::net::TcpListener::bind("127.0.0.1:11535")
        .await
        .expect("bind 127.0.0.1:11535");
    println!("nodelm listening on 127.0.0.1:11535 (anthropic /v1/messages only, stub daemon)");
    axum::serve(listener, app).await.expect("serve");
}
