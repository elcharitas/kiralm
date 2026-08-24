//! HTTP surface (SPEC.md §2). Only the OpenAI-compatible routes are wired
//! here for now; `anthropic.rs` (§2.2) and `internal.rs` (§2.5) land in
//! their own passes and aren't part of this worktree.

pub mod openai;

use std::sync::Arc;

use axum::{
    Router,
    routing::{get, post},
};

use crate::engine::EngineSupervisor;

#[derive(Clone)]
pub struct AppState {
    pub engine: Arc<EngineSupervisor>,
    pub http: reqwest::Client,
}

/// Router serving the OpenAI-compatible surface. Both the Unix-socket and
/// TCP listeners in SPEC §2 route to the same handlers, so callers mount
/// this on whichever `axum::serve` target they need.
pub fn router(engine: Arc<EngineSupervisor>) -> Router {
    let state = AppState {
        engine,
        http: reqwest::Client::new(),
    };
    Router::new()
        .route("/v1/chat/completions", post(openai::chat_completions))
        .route("/v1/models", get(openai::list_models))
        .with_state(state)
}
