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

// ---------------------------------------------------------------------
// Typed OpenAI chat-completion wire shapes. The proxy above forwards raw
// JSON, but `anthropic.rs`'s translator (§2.2) needs typed request/
// response/SSE-chunk shapes to build and parse against, since it's a
// real translation, not a byte passthrough.
// ---------------------------------------------------------------------

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(untagged)]
pub enum ChatContent {
    Text(String),
    Parts(Vec<ChatContentPart>),
}

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ChatContentPart {
    Text { text: String },
    ImageUrl { image_url: ImageUrl },
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ImageUrl {
    pub url: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct ChatMessage {
    pub role: String,
    pub content: ChatContent,
}

#[derive(Debug, Clone, Serialize)]
pub struct ChatCompletionRequest {
    pub model: String,
    pub messages: Vec<ChatMessage>,
    pub max_tokens: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub temperature: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub top_p: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub top_k: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stop: Option<Vec<String>>,
    pub stream: bool,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ChatCompletionResponse {
    #[allow(dead_code)]
    pub id: String,
    pub choices: Vec<ChatChoice>,
    pub usage: Option<ChatUsage>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ChatChoice {
    pub message: ChatChoiceMessage,
    pub finish_reason: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ChatChoiceMessage {
    pub content: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ChatUsage {
    pub prompt_tokens: u32,
    pub completion_tokens: u32,
}

/// One `data:` line of the child's chat-completion SSE stream.
#[derive(Debug, Clone, Deserialize)]
pub struct ChatCompletionChunk {
    pub choices: Vec<ChatChunkChoice>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ChatChunkChoice {
    pub delta: ChatChunkDelta,
    pub finish_reason: Option<String>,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct ChatChunkDelta {
    pub content: Option<String>,
}
