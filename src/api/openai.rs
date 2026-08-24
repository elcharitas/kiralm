//! `POST /v1/chat/completions`, `GET /v1/models` (§2.1).

use axum::body::Body;
use axum::extract::State;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde_json::json;

use super::{ensure_loaded, resolve_model, GatewayError, SharedState};
use crate::state::ModelManifest;

pub fn routes() -> Router<SharedState> {
    Router::new()
        .route("/v1/chat/completions", post(chat_completions))
        .route("/v1/models", get(list_models))
}

fn openai_err(status: StatusCode, kind: &str, message: impl Into<String>, param: Option<&str>) -> Response {
    (
        status,
        Json(json!({"error": {
            "message": message.into(),
            "type": kind,
            "param": param,
            "code": serde_json::Value::Null,
        }})),
    )
        .into_response()
}

async fn list_models(State(app): State<SharedState>) -> Json<serde_json::Value> {
    let data: Vec<_> = app
        .registry
        .list()
        .into_iter()
        .map(|m| {
            let pulled_at = ModelManifest::load(&m.gguf_path)
                .map(|man| man.pulled_at.timestamp())
                .unwrap_or(0);
            json!({
                "id": m.canonical_id,
                "object": "model",
                "created": pulled_at,
                "owned_by": "nodelm",
            })
        })
        .collect();
    Json(json!({"object": "list", "data": data}))
}

/// Thin reverse proxy (§2.1): reads only the top-level `model` field,
/// resolves it, ensures a child is loaded, forwards the body
/// byte-for-byte. Non-streaming responses get `model` normalized back to
/// the client's requested name; streaming is a pure byte-copy.
async fn chat_completions(
    State(app): State<SharedState>,
    Json(body): Json<serde_json::Value>,
) -> Response {
    let Some(requested_model) = body.get("model").and_then(|v| v.as_str()).map(str::to_string)
    else {
        return openai_err(
            StatusCode::BAD_REQUEST,
            "invalid_request_error",
            "missing required field 'model'",
            Some("model"),
        );
    };

    let Some(canonical) = resolve_model(&app, &requested_model) else {
        return openai_err(
            StatusCode::NOT_FOUND,
            "model_not_found",
            format!("model '{requested_model}' not found"),
            Some("model"),
        );
    };

    let port = match ensure_loaded(&app, &canonical).await {
        Ok(p) => p,
        Err(GatewayError::CachedUnsupported(msg)) => {
            return openai_err(StatusCode::BAD_REQUEST, "invalid_request_error", msg, None)
        }
        Err(GatewayError::InsufficientRam(msg)) => {
            return openai_err(StatusCode::SERVICE_UNAVAILABLE, "invalid_request_error", msg, None)
        }
        Err(GatewayError::LoadFailed(msg)) => {
            return openai_err(StatusCode::INTERNAL_SERVER_ERROR, "api_error", msg, None)
        }
    };

    let is_stream = body.get("stream").and_then(|v| v.as_bool()).unwrap_or(false);
    let client = reqwest::Client::new();
    let url = format!("http://127.0.0.1:{port}/v1/chat/completions");
    let upstream = match client.post(&url).json(&body).send().await {
        Ok(r) => r,
        Err(e) => {
            return openai_err(
                StatusCode::INTERNAL_SERVER_ERROR,
                "api_error",
                e.to_string(),
                None,
            )
        }
    };

    let status = StatusCode::from_u16(upstream.status().as_u16()).unwrap_or(StatusCode::OK);

    if is_stream {
        // Pure byte-copy, no re-parsing (§2.1).
        let stream = upstream.bytes_stream();
        Response::builder()
            .status(status)
            .header("content-type", "text/event-stream")
            .header("cache-control", "no-cache")
            .body(Body::from_stream(stream))
            .unwrap_or_else(|_| StatusCode::INTERNAL_SERVER_ERROR.into_response())
    } else {
        let mut json = match upstream.json::<serde_json::Value>().await {
            Ok(v) => v,
            Err(e) => {
                return openai_err(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "api_error",
                    e.to_string(),
                    None,
                )
            }
        };
        if let Some(obj) = json.as_object_mut() {
            obj.insert("model".to_string(), serde_json::Value::String(requested_model));
        }
        (status, Json(json)).into_response()
    }
}
