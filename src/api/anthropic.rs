//! `POST /v1/messages` (+ SSE translator) — §2.2.
//!
//! STUB: this worktree's task was the CLI + serve wiring + pull
//! pipeline, not the Anthropic translation layer. §2.2 requires a real
//! request/response/SSE-event translation (not a byte proxy — the
//! schemas differ entirely), which is a sizable, separately-spec'd
//! component. Router shape and error envelope match §2.2/§2.4 so the
//! owning agent can drop the real translator in without touching
//! `api/mod.rs`'s wiring.

use axum::extract::State;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::post;
use axum::{Json, Router};
use serde_json::json;

use super::SharedState;

pub fn routes() -> Router<SharedState> {
    Router::new().route("/v1/messages", post(messages))
}

async fn messages(State(_app): State<SharedState>, Json(_body): Json<serde_json::Value>) -> Response {
    (
        StatusCode::NOT_IMPLEMENTED,
        Json(json!({"type": "error", "error": {
            "type": "api_error",
            "message": "nodelm: /v1/messages translation not implemented in this build",
        }})),
    )
        .into_response()
}
